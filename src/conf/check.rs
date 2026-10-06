//! Проверка текста .conf до сохранения: те же правила, что у разбора конфига движком
//! (amneziawg-windows `conf/parser.go` `FromWgQuick`, H1–H4 — amneziawg-go `UintRange.FromString` и запрет
//! пересечения диапазонов в `mergeWithDevice`). Иначе битый конфиг всплывёт только при подключении.
//! Чистая функция: текст -> список замечаний с номерами строк; текст сообщений — по ключу i18n у вызывающего.

use std::net::IpAddr;

/// Замечание к конфигу: строка (с 1; 0 — конфиг целиком), ключ сообщения i18n и подстановка `{0}`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Issue {
    pub line: usize,
    pub key: &'static str,
    pub arg: String,
}

/// Ключи без значения допустимы (движок считает их «не задано»).
const MAY_BE_EMPTY: &[&str] = &[
    "i1", "i2", "i3", "i4", "i5", "j1", "j2", "j3", "itime", "headerprotectionkey", "contentpaddingaddition", "rekeyaftertime",
    "rekeytimeout", "rejectaftertime", "keepalivetimeout", "maxhandshakeattempts", "randomtrailers", "disablecookies",
];

/// Ключи [Interface], значение которых движок передаёт дальше без проверки при разборе.
const INTERFACE_FREE: &[&str] = &[
    "i1", "i2", "i3", "i4", "i5", "contentpaddingaddition", "rekeyaftertime", "rekeytimeout", "rejectaftertime", "keepalivetimeout",
    "maxhandshakeattempts", "randomtrailers", "disablecookies", "preup", "postup", "predown", "postdown",
];

#[derive(PartialEq)]
enum Section {
    None,
    Interface,
    Peer,
}

/// Все замечания к тексту конфига по порядку строк; пустой список — движок примет конфиг.
pub fn check(text: &str) -> Vec<Issue> {
    let mut issues = Vec::new();
    let mut section = Section::None;
    let mut private_key = false;
    // Строка заголовка [Peer] и есть ли у пира PublicKey.
    let mut peers: Vec<(usize, bool)> = Vec::new();
    let mut headers: Vec<(usize, &str, (u32, u32))> = Vec::new();
    for (n, raw) in text.lines().enumerate() {
        let line_no = n + 1;
        let mut issue = |key: &'static str, arg: &str| issues.push(Issue { line: line_no, key, arg: arg.to_string() });
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        if lower == "[interface]" {
            section = Section::Interface;
            continue;
        }
        if lower == "[peer]" {
            section = Section::Peer;
            peers.push((line_no, false));
            continue;
        }
        if section == Section::None {
            issue("chk.outside", line);
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            issue("chk.no_equals", line);
            continue;
        };
        let (key, value) = (k.trim().to_ascii_lowercase(), v.trim());
        if value.is_empty() {
            if !MAY_BE_EMPTY.contains(&key.as_str()) {
                issue("chk.empty", k.trim());
            }
            continue;
        }
        let problem = if section == Section::Interface {
            if key == "privatekey" {
                private_key = true;
            }
            if let Some(range) = header_range(&key, value) {
                match range {
                    Ok(r) => headers.push((line_no, k.trim(), r)),
                    Err(()) => issue("chk.header", k.trim()),
                }
                continue;
            }
            interface_value(&key, value)
        } else {
            if key == "publickey" {
                if let Some(p) = peers.last_mut() {
                    p.1 = true;
                }
            }
            peer_value(&key, value)
        };
        if let Some((msg, arg)) = problem {
            issue(msg, &arg);
        }
    }
    if !private_key {
        issues.push(Issue { line: 0, key: "chk.no_private_key", arg: String::new() });
    }
    for &(line, _) in peers.iter().filter(|p| !p.1) {
        issues.push(Issue { line, key: "chk.no_public_key", arg: String::new() });
    }
    for (i, a) in headers.iter().enumerate() {
        if let Some(b) = headers[..i].iter().find(|b| a.2 .0 <= b.2 .1 && b.2 .0 <= a.2 .1) {
            issues.push(Issue { line: a.0, key: "chk.headers_overlap", arg: format!("{} / {}", b.1, a.1) });
        }
    }
    issues.sort_by_key(|i| if i.line == 0 { usize::MAX } else { i.line });
    issues
}

/// Значение ключа [Interface]: `None` — годится, иначе ключ сообщения и что подставить.
fn interface_value(key: &str, value: &str) -> Option<(&'static str, String)> {
    let bad = |msg: &'static str| (msg, value.to_string());
    match key {
        // Сам ключ в сообщение не попадает: приватный ключ не показывается нигде.
        "privatekey" => (!is_key(value)).then(|| ("chk.private_key", String::new())),
        "headerprotectionkey" => (!is_key(value)).then(|| ("chk.key", key.to_string())),
        "listenport" => value.parse::<u16>().is_err().then(|| bad("chk.port")),
        // Jc … S4 движок Windows разбирает как uint16.
        "jc" | "jmin" | "jmax" | "s1" | "s2" | "s3" | "s4" => value.parse::<u16>().is_err().then(|| ("chk.awg_u16", format!("{key} = {value}"))),
        "mtu" => (!value.parse::<u32>().is_ok_and(|m| (576..=65535).contains(&m))).then(|| bad("chk.mtu")),
        "address" => cidr_list(value),
        // DNS: адрес сервера или домен поиска — движок принимает любой непустой элемент.
        "dns" => list(value).is_none().then(|| bad("chk.list")),
        "table" => (!matches!(value, "off" | "auto" | "main") && value.parse::<u32>().is_err()).then(|| bad("chk.table")),
        k if INTERFACE_FREE.contains(&k) => None,
        _ => Some(("chk.unknown_interface", key.to_string())),
    }
}

/// Значение ключа [Peer]: `None` — годится.
fn peer_value(key: &str, value: &str) -> Option<(&'static str, String)> {
    let bad = |msg: &'static str| (msg, value.to_string());
    match key {
        "publickey" => (!is_key(value)).then(|| bad("chk.public_key")),
        // Общий ключ тоже секрет: в сообщении только то, что он неверный.
        "presharedkey" => (!is_key(value)).then(|| ("chk.preshared_key", String::new())),
        "allowedips" => cidr_list(value),
        "persistentkeepalive" => (!matches!(value, "off" | "(off)") && value.parse::<u16>().is_err()).then(|| bad("chk.keepalive")),
        "endpoint" => (!is_endpoint(value)).then(|| bad("chk.endpoint")),
        _ => Some(("chk.unknown_peer", key.to_string())),
    }
}

/// Address, AllowedIPs: список адресов с длиной префикса; первый неверный — в сообщение.
fn cidr_list(value: &str) -> Option<(&'static str, String)> {
    match list(value) {
        None => Some(("chk.list", value.to_string())),
        Some(items) => items.into_iter().find(|a| !is_cidr(a)).map(|a| ("chk.cidr", a.to_string())),
    }
}

/// H1–H4: число uint32 или диапазон `от-до` (`от` ≤ `до`). `None` — ключ не H1–H4.
fn header_range(key: &str, value: &str) -> Option<Result<(u32, u32), ()>> {
    if !matches!(key, "h1" | "h2" | "h3" | "h4") {
        return None;
    }
    let parse = |s: &str| s.parse::<u32>().map_err(|_| ());
    Some(match value.split_once('-') {
        None => parse(value).map(|v| (v, v)),
        Some((lo, hi)) => match (parse(lo), parse(hi)) {
            (Ok(lo), Ok(hi)) if lo <= hi => Ok((lo, hi)),
            _ => Err(()),
        },
    })
}

/// Список через запятую; две запятые подряд движок не принимает.
fn list(value: &str) -> Option<Vec<&str>> {
    let items: Vec<&str> = value.split(',').map(str::trim).collect();
    (!items.iter().any(|s| s.is_empty())).then_some(items)
}

/// Ключ WireGuard: base64 с выравниванием, ровно 32 байта.
fn is_key(value: &str) -> bool {
    value.len() == 44 && value.ends_with('=') && !value[..43].contains('=') && super::base64_decode(value).is_some_and(|b| b.len() == 32)
}

/// Адрес с необязательной длиной префикса: до 32 у IPv4, до 128 у IPv6.
fn is_cidr(value: &str) -> bool {
    let (addr, prefix) = match value.split_once('/') {
        Some((a, p)) => (a, Some(p)),
        None => (value, None),
    };
    let Ok(ip) = addr.parse::<IpAddr>() else { return false };
    // Как `net.IP.To4` в движке: IPv4 и IPv4, отображённый в IPv6, — до /32.
    let max = if ip.to_canonical().is_ipv4() { 32 } else { 128 };
    match prefix {
        None => true,
        Some(p) => p.parse::<u8>().is_ok_and(|p| p <= max),
    }
}

/// `хост:порт`; IPv6 — в квадратных скобках (`[fd00::1]:51820`), как у движка.
fn is_endpoint(value: &str) -> bool {
    let Some((host, port)) = value.rsplit_once(':') else { return false };
    if host.is_empty() || port.parse::<u16>().is_err() {
        return false;
    }
    if host.starts_with('[') || host.ends_with(']') || host.contains(':') {
        let inner = host.strip_prefix('[').and_then(|h| h.strip_suffix(']'));
        let inner = inner.map(|h| h.split('%').next().unwrap_or(h));
        return inner.is_some_and(|h| h.parse::<std::net::Ipv6Addr>().is_ok());
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRIVATE: &str = "yAnz5TF+lXXJte14tji3zlMNq+hd2rYUIgJBgB3fBmk=";
    const PUBLIC: &str = "xTIBA5rboUvnH4htodjb6e697QjLERt1NAB4mZqp8Dg=";

    fn good() -> String {
        format!(
            "[Interface]\nPrivateKey = {PRIVATE}\nAddress = 10.0.0.2/32, fd00::2/128\nDNS = 1.1.1.1, corp.example\nMTU = 1280\n\
             ListenPort = 51820\nJc = 4\nJmin = 50\nJmax = 1000\nS1 = 30\nS2 = 90\nH1 = 1234567890\nH2 = 100-200\nH3 = 201\nH4 = 5\nI1 =\n\
             # comment\n[Peer]\nPublicKey = {PUBLIC}\nPresharedKey = {PRIVATE}\nEndpoint = vpn.example.com:51820\n\
             AllowedIPs = 0.0.0.0/0, ::/0\nPersistentKeepalive = 25\n[Peer]\nPublicKey = {PUBLIC}\nEndpoint = [fd00::1]:443\n"
        )
    }

    fn keys(text: &str) -> Vec<(usize, &'static str)> {
        check(text).into_iter().map(|i| (i.line, i.key)).collect()
    }

    #[test]
    fn good_config_has_no_issues() {
        assert_eq!(check(&good()), vec![]);
        assert_eq!(check(&good().replace('\n', "\r\n")), vec![], "CRLF");
    }

    #[test]
    fn each_bad_value_is_reported_on_its_line() {
        let text = format!(
            "[Interface]\nPrivateKey = {PRIVATE}\nAddress = 10.0.0.2/33\nMTU = 500\nJc = 70000\nH1 = 9-3\nListenPort = x\nFwMark = 1\n\
             [Peer]\nPublicKey = short=\nAllowedIPs = 10.0.0.0/8,,10.1.0.0/16\nEndpoint = fd00::1:51820\nPersistentKeepalive = never\nFoo = 1\n"
        );
        assert_eq!(
            keys(&text),
            vec![
                (3, "chk.cidr"),
                (4, "chk.mtu"),
                (5, "chk.awg_u16"),
                (6, "chk.header"),
                (7, "chk.port"),
                (8, "chk.unknown_interface"),
                (10, "chk.public_key"),
                (11, "chk.list"),
                (12, "chk.endpoint"),
                (13, "chk.keepalive"),
                (14, "chk.unknown_peer"),
            ]
        );
    }

    #[test]
    fn missing_keys_and_structure() {
        // Ни [Interface], ни приватного ключа; у пира нет PublicKey; строка без «=».
        let text = "Address = 10.0.0.2/32\n[Peer]\nEndpoint\nAllowedIPs =\n";
        assert_eq!(keys(text), vec![(1, "chk.outside"), (2, "chk.no_public_key"), (3, "chk.no_equals"), (4, "chk.empty"), (0, "chk.no_private_key")]);
    }

    #[test]
    fn overlapping_headers_are_reported() {
        let text = format!("[Interface]\nPrivateKey = {PRIVATE}\nH1 = 100-200\nH2 = 150\nH3 = 201-300\n");
        let issues = check(&text);
        assert_eq!(issues, vec![Issue { line: 4, key: "chk.headers_overlap", arg: "H1 / H2".into() }]);
    }

    #[test]
    fn secrets_never_appear_in_messages() {
        let bad_private = "yAnz5TF+lXXJte14tji3zlMNq+hd2rYUIgJBgB3fBm==";
        let text = format!("[Interface]\nPrivateKey = {bad_private}\n[Peer]\nPublicKey = {PUBLIC}\nPresharedKey = {bad_private}x\n");
        let issues = check(&text);
        assert_eq!(issues.iter().map(|i| i.key).collect::<Vec<_>>(), vec!["chk.private_key", "chk.preshared_key"]);
        assert!(issues.iter().all(|i| !i.arg.contains("yAnz5TF")), "{issues:?}");
    }

    #[test]
    fn value_helpers() {
        assert!(is_key(PUBLIC));
        assert!(!is_key(&PUBLIC[..43]), "no padding");
        assert!(!is_key("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=="), "31 bytes");
        assert!(is_cidr("10.0.0.1") && is_cidr("::/0") && is_cidr("0.0.0.0/0"));
        assert!(!is_cidr("10.0.0.1/40") && !is_cidr("fd00::/129") && !is_cidr("10.0.0") && !is_cidr("10.0.0.1/"));
        assert!(is_endpoint("203.0.113.5:51820") && is_endpoint("[fe80::1%eth0]:1"));
        assert!(!is_endpoint("203.0.113.5") && !is_endpoint(":51820") && !is_endpoint("host:70000") && !is_endpoint("[1.2.3.4]:1"));
    }
}
