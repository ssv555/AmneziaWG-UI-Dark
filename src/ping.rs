//! Проверка «трафик реально проходит»: ICMP-пинг до узла из Allowed IPs раз в 10 секунд,
//! пока подключён хоть один туннель. Рукопожатие говорит только, что сервер отвечает.

use std::collections::VecDeque;
use std::net::{Ipv4Addr, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::monitor::Shared;

const INTERVAL: Duration = Duration::from_secs(10);
const TIMEOUT_MS: u32 = 2000;
/// Час истории при замере раз в 10 секунд.
const KEEP: usize = 360;

#[derive(Clone, Default, Debug)]
pub struct PingState {
    pub host: String,
    /// Последний результат: задержка, мс, или текст ошибки.
    pub last: Option<Result<u32, String>>,
    /// Неудач подряд.
    pub fails: u32,
    pub history: VecDeque<(Instant, Option<u32>)>,
}

impl PingState {
    fn record(&mut self, host: &str, result: Result<u32, String>) {
        if self.host != host {
            *self = PingState { host: host.to_string(), ..Default::default() };
        }
        self.fails = if result.is_ok() { 0 } else { self.fails + 1 };
        self.history.push_back((Instant::now(), result.as_ref().ok().copied()));
        while self.history.len() > KEEP {
            self.history.pop_front();
        }
        self.last = Some(result);
    }
}

pub fn spawn(shared: Arc<Shared>) {
    std::thread::spawn(move || {
        let mut next = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(500));
            let (enabled, host) = {
                let o = shared.options.lock().unwrap();
                (o.ping, o.ping_host.clone())
            };
            let active = !shared.snapshot.lock().unwrap().running.is_empty();
            if !enabled || !active {
                *shared.ping.lock().unwrap() = PingState { host, ..Default::default() };
                next = Instant::now();
                continue;
            }
            if Instant::now() < next {
                continue;
            }
            next = Instant::now() + INTERVAL;
            let result = resolve(&host).and_then(|ip| echo(ip, TIMEOUT_MS));
            shared.ping.lock().unwrap().record(&host, result);
        }
    });
}

fn resolve(host: &str) -> Result<Ipv4Addr, String> {
    if let Ok(ip) = host.parse::<Ipv4Addr>() {
        return Ok(ip);
    }
    (host, 0)
        .to_socket_addrs()
        .map_err(|e| format!("{host}: {e}"))?
        .find_map(|a| match a.ip() {
            std::net::IpAddr::V4(ip) => Some(ip),
            _ => None,
        })
        .ok_or_else(|| format!("{host}: no IPv4 address"))
}

/// Один эхо-запрос через IcmpSendEcho (права администратора не нужны).
fn echo(ip: Ipv4Addr, timeout_ms: u32) -> Result<u32, String> {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::NetworkManagement::IpHelper::{IcmpCloseHandle, IcmpCreateFile, IcmpSendEcho, ICMP_ECHO_REPLY};

    let payload = *b"awg-ui-ping-0123456789abcdef";
    let mut reply = vec![0u8; size_of::<ICMP_ECHO_REPLY>() + payload.len() + 64];
    unsafe {
        let handle = IcmpCreateFile();
        if handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let n = IcmpSendEcho(
            handle,
            u32::from_ne_bytes(ip.octets()),
            payload.as_ptr().cast(),
            payload.len() as u16,
            std::ptr::null(),
            reply.as_mut_ptr().cast(),
            reply.len() as u32,
            timeout_ms,
        );
        let err = std::io::Error::last_os_error();
        IcmpCloseHandle(handle);
        if n == 0 {
            return Err(format!("no reply ({err})"));
        }
        let r = &*(reply.as_ptr() as *const ICMP_ECHO_REPLY);
        if r.Status != 0 {
            return Err(format!("ICMP status {}", r.Status));
        }
        Ok(r.RoundTripTime)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_answers() {
        assert!(echo(Ipv4Addr::LOCALHOST, 1000).is_ok());
    }

    #[test]
    fn fails_count_and_reset() {
        let mut st = PingState::default();
        st.record("h", Err("x".into()));
        st.record("h", Err("x".into()));
        assert_eq!(st.fails, 2);
        st.record("h", Ok(10));
        assert_eq!((st.fails, st.history.len()), (0, 3));
        st.record("other", Ok(5));
        assert_eq!(st.history.len(), 1);
    }
}
