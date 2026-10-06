//! Сведения о туннеле для показа, когда он не подключён: из файла-источника (.conf) или из родного окна.
//! Приватный ключ не хранится: из него сразу вычисляется открытый.

use std::collections::BTreeSet;

use crate::uapi;

mod check;
pub use check::{check, Issue};

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PeerInfo {
    pub public_key: String,
    pub preshared: bool,
    pub endpoint: String,
    pub allowed_ips: Vec<String>,
    pub keepalive: String,
}

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TunnelInfo {
    pub public_key: String,
    pub listen_port: String,
    pub mtu: String,
    pub addresses: Vec<String>,
    pub dns: Vec<String>,
    /// Параметры обфускации AmneziaWG (Jc, Jmin, S1, H1 …) в порядке файла.
    pub awg: Vec<(String, String)>,
    pub peers: Vec<PeerInfo>,
}

/// Ключи интерфейса, которые не показываются как параметры AWG.
const INTERFACE_KEYS: &[&str] = &["privatekey", "listenport", "mtu", "address", "dns", "table", "preup", "postup", "predown", "postdown", "saveconfig", "fwmark"];

/// Разбор текста .conf (WireGuard / AmneziaWG).
pub fn parse(text: &str) -> TunnelInfo {
    let mut info = TunnelInfo::default();
    let mut in_peer = false;
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            in_peer = line.eq_ignore_ascii_case("[Peer]");
            if in_peer {
                info.peers.push(PeerInfo::default());
            }
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        let (key, v) = (k.trim().to_ascii_lowercase(), v.trim());
        let list = || v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect::<Vec<_>>();
        if in_peer {
            let Some(p) = info.peers.last_mut() else { continue };
            match key.as_str() {
                "publickey" => p.public_key = v.to_string(),
                "presharedkey" => p.preshared = !v.is_empty(),
                "endpoint" => p.endpoint = v.to_string(),
                "allowedips" => p.allowed_ips.extend(list()),
                "persistentkeepalive" => p.keepalive = v.to_string(),
                _ => {}
            }
            continue;
        }
        match key.as_str() {
            "privatekey" => info.public_key = base64_decode(v).map(|b| uapi::public_key(&b)).unwrap_or_default(),
            "listenport" => info.listen_port = v.to_string(),
            "mtu" => info.mtu = v.to_string(),
            "address" => info.addresses.extend(list()),
            "dns" => info.dns.extend(list()),
            k if !INTERFACE_KEYS.contains(&k) => info.awg.push((k.trim().to_string(), v.to_string())),
            _ => {}
        }
    }
    // Имена параметров AWG — как в файле (Jc, Jmin, H1), а не в нижнем регистре.
    let originals: Vec<String> = text
        .lines()
        .filter_map(|l| l.split_once('=').map(|(k, _)| k.trim().to_string()))
        .collect();
    for (k, _) in &mut info.awg {
        if let Some(o) = originals.iter().find(|o| o.eq_ignore_ascii_case(k)) {
            *k = o.clone();
        }
    }
    info
}


/// Что туннель занимает в системе: адреса интерфейса и маршрут «весь трафик» (по семействам адресов).
/// Два туннеля с общим адресом или оба на весь трафик одного семейства вместе не работают.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Footprint {
    pub addresses: BTreeSet<String>,
    pub all_v4: bool,
    pub all_v6: bool,
}

impl Footprint {
    pub fn of(info: &TunnelInfo) -> Self {
        let mut f = Self::routes(info.peers.iter().flat_map(|p| &p.allowed_ips));
        f.addresses = info.addresses.iter().map(|a| host(a)).collect();
        f
    }

    /// Только маршруты — у работающего туннеля их видно по каналу состояния, адреса интерфейса — нет.
    pub fn of_status(st: &uapi::Status) -> Self {
        Self::routes(st.peers.iter().flat_map(|p| &p.allowed_ips))
    }

    fn routes<'a>(ips: impl Iterator<Item = &'a String>) -> Self {
        let mut f = Self::default();
        for ip in ips {
            match ip.replace(' ', "").as_str() {
                "0.0.0.0/0" => f.all_v4 = true,
                "::/0" => f.all_v6 = true,
                _ => {}
            }
        }
        f
    }

    pub fn merge(&mut self, other: Self) {
        self.addresses.extend(other.addresses);
        self.all_v4 |= other.all_v4;
        self.all_v6 |= other.all_v6;
    }

    pub fn conflicts(&self, other: &Self) -> bool {
        (self.all_v4 && other.all_v4) || (self.all_v6 && other.all_v6) || !self.addresses.is_disjoint(&other.addresses)
    }
}

/// Адрес без длины префикса: `10.0.0.2/32` → `10.0.0.2`.
fn host(address: &str) -> String {
    address.split('/').next().unwrap_or("").trim().to_ascii_lowercase()
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut bits = 0u32;
    let mut n = 0;
    let mut out = Vec::new();
    for c in s.trim().bytes().filter(|&c| c != b'=') {
        let v = T.iter().position(|&t| t == c)? as u32;
        bits = bits << 6 | v;
        n += 6;
        if n >= 8 {
            n -= 8;
            out.push((bits >> n) as u8);
            bits &= (1 << n) - 1;
        }
    }
    Some(out)
}

/// В конфиге есть команды, которые служба туннеля выполнила бы (PreUp, PostUp, PreDown, PostDown): от SYSTEM.
/// Из окна такие конфиги не принимают ни ядро, ни агент.
pub fn has_scripts(text: &str) -> bool {
    text.lines().any(|line| {
        let key = line.split('=').next().unwrap_or("").trim().to_ascii_lowercase();
        line.contains('=') && matches!(key.as_str(), "preup" | "postup" | "predown" | "postdown")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts_are_found_in_any_case() {
        assert!(has_scripts("[Interface]\npostup = cmd /c calc\n"));
        assert!(has_scripts("[Interface]\n  PreDown=x\n"));
        assert!(!has_scripts("[Interface]\nPrivateKey = a\n# PostUp is not set\n"));
    }

    // Пример из man wg(8): открытый ключ вычисляется из приватного, сам приватный не сохраняется.
    const SAMPLE: &str = "[Interface]\r\n\
        PrivateKey = yAnz5TF+lXXJte14tji3zlMNq+hd2rYUIgJBgB3fBmk=\r\n\
        Address = 10.0.0.2/32, fd00::2/128\r\n\
        DNS = 1.1.1.1\r\n\
        MTU = 1280\r\n\
        Jc = 4\r\n\
        H1 = 1234567890\r\n\
        # комментарий\r\n\
        [Peer]\r\n\
        PublicKey = xTIBA5rboUvnH4htodjb6e697QjLERt1NAB4mZqp8Dg=\r\n\
        PresharedKey = AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=\r\n\
        Endpoint = 203.0.113.5:51820\r\n\
        AllowedIPs = 0.0.0.0/0, ::/0\r\n\
        PersistentKeepalive = 25\r\n";

    #[test]
    fn parses_interface_peer_and_awg() {
        let info = parse(SAMPLE);
        assert_eq!(info.public_key, "HIgo9xNzJMWLKASShiTqIybxZ0U3wGLiUeJ1PKf8ykw=");
        assert_eq!(info.addresses, vec!["10.0.0.2/32", "fd00::2/128"]);
        assert_eq!((info.dns.clone(), info.mtu.as_str()), (vec!["1.1.1.1".to_string()], "1280"));
        assert_eq!(info.awg, vec![("Jc".to_string(), "4".to_string()), ("H1".to_string(), "1234567890".to_string())]);
        let p = &info.peers[0];
        assert!(p.preshared);
        assert_eq!((p.endpoint.as_str(), p.keepalive.as_str()), ("203.0.113.5:51820", "25"));
        assert_eq!(p.allowed_ips, vec!["0.0.0.0/0", "::/0"]);
        assert!(!format!("{info:?}").contains("yAnz5TF"), "приватный ключ не должен сохраняться");
    }

    #[test]
    fn base64_roundtrip() {
        let bytes: Vec<u8> = (0..32).collect();
        assert_eq!(base64_decode(&uapi::base64(&bytes)), Some(bytes));
    }

    fn conf(address: &str, allowed: &str) -> Footprint {
        Footprint::of(&parse(&format!("[Interface]\nAddress = {address}\n[Peer]\nAllowedIPs = {allowed}\n")))
    }

    #[test]
    fn same_address_or_both_full_route_conflict() {
        // Типичный случай: два варианта одного клиента — общий адрес и оба на весь трафик.
        assert!(conf("10.255.254.2/32", "0.0.0.0/0").conflicts(&conf("10.255.254.2/32", "0.0.0.0/0")));
        assert!(conf("10.0.0.2/32", "10.0.0.0/24").conflicts(&conf("10.0.0.2/24", "192.168.5.0/24")), "общий адрес");
        assert!(conf("10.0.0.2/32", "0.0.0.0/0").conflicts(&conf("10.1.0.2/32", "0.0.0.0/0")), "оба на весь IPv4");
        assert!(!conf("10.0.0.2/32", "0.0.0.0/0").conflicts(&conf("10.1.0.2/32", "::/0")), "разные семейства");
        assert!(!conf("10.0.0.2/32", "10.0.0.0/24").conflicts(&conf("10.1.0.2/32", "10.1.0.0/24")), "независимые сети");
    }
}
