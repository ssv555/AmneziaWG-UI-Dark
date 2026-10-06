//! Проверка «трафик реально проходит»: ICMP-пинг до узла из Allowed IPs раз в 10 секунд,
//! пока подключён хоть один туннель. Рукопожатие говорит только, что сервер отвечает.
//! Ведёт его агент (`daemon::agent`), в демо-режиме — само окно; ядро пинга не касается.

use std::collections::VecDeque;
use std::net::{Ipv4Addr, ToSocketAddrs};
use std::ops::ControlFlow;
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
    /// Для канала ядра: вместо моментов времени — возраст замеров в секундах.
    pub fn to_dto(&self) -> crate::daemon::proto::PingDto {
        crate::daemon::proto::PingDto {
            host: self.host.clone(),
            last: self.last.clone(),
            fails: self.fails,
            history: self.history.iter().map(|(at, ms)| (at.elapsed().as_secs_f64(), *ms)).collect(),
        }
    }

    pub fn from_dto(d: crate::daemon::proto::PingDto) -> PingState {
        let now = Instant::now();
        let history = d.history.into_iter().filter_map(|(age, ms)| now.checked_sub(Duration::from_secs_f64(age)).map(|at| (at, ms))).collect();
        PingState { host: d.host, last: d.last, fails: d.fails, history }
    }

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

/// Где живёт пинг: окно демо-режима (`Shared`, выдуманные туннели) или агент (`daemon::agent`). Поток один и тот же,
/// разные только источники настроек, туннелей и замера и место результата.
pub(crate) trait PingHome: Send + Sync {
    /// Включён ли пинг и до какого узла.
    fn settings(&self) -> (bool, String);
    /// Подключён ли хоть один туннель: без туннеля пинг ничего не говорит о VPN.
    fn any_running(&self) -> bool;
    fn measure(&self, host: &str) -> Result<u32, String>;
    fn update(&self, f: &mut dyn FnMut(&mut PingState));
    /// Паника замера: в журнал; поток продолжает после паузы `wait`.
    fn report_panic(&self, panic: &str, wait: Duration);
}

/// Поток пинга. Пинг вторичен: его паника (странный ответ ICMP, сбой разбора имени) — запись в журнал и пауза
/// (`crash::nonfatal_loop`), а не остановка процесса.
pub(crate) fn spawn(home: Arc<dyn PingHome>) {
    crate::crash::spawn_named("ping", move || {
        let mut pinger = Pinger::new(Instant::now());
        let report = |panic: &str, wait: Duration| home.report_panic(panic, wait);
        crate::crash::nonfatal_loop(STEP, &std::thread::sleep, &report, || {
            pinger.step(home.as_ref(), Instant::now());
            ControlFlow::Continue(())
        });
    });
}

/// Шаг потока пинга: так часто проверяются настройки.
const STEP: Duration = Duration::from_millis(500);

/// Расписание замеров: раз в `INTERVAL`, пока пинг включён и подключён хоть один туннель.
struct Pinger {
    next: Instant,
}

impl Pinger {
    fn new(now: Instant) -> Pinger {
        Pinger { next: now }
    }

    fn step(&mut self, home: &dyn PingHome, now: Instant) {
        let (enabled, host) = home.settings();
        if !enabled {
            home.update(&mut |p| *p = PingState { host: host.clone(), ..Default::default() });
            self.next = now;
            return;
        }
        if now < self.next {
            return;
        }
        // До замера: паника в нём не повторяет замер на каждом шаге, следующий — по расписанию. Туннели проверяются
        // тоже по расписанию: у агента это запрос к ядру, а не чтение своей памяти.
        self.next = now + INTERVAL;
        if !home.any_running() {
            home.update(&mut |p| *p = PingState { host: host.clone(), ..Default::default() });
            return;
        }
        let mut result = Some(home.measure(&host));
        home.update(&mut |p| {
            if let Some(r) = result.take() {
                p.record(&host, r);
            }
        });
    }
}

/// Окно демо-режима: туннели и замер выдуманные (`Demo`), результат — в `Shared`, откуда его рисует окно.
impl PingHome for Shared {
    fn settings(&self) -> (bool, String) {
        let o = self.options();
        (o.ping, o.ping_host)
    }
    fn any_running(&self) -> bool {
        Shared::any_running(self)
    }
    fn measure(&self, host: &str) -> Result<u32, String> {
        // Окно с ядром пингом не занимается (его ведёт агент) и туннелей своих не имеет.
        self.host().ok_or_else(|| "ping: no tunnel host in this process".to_string())?.ping_ms(host)
    }
    fn update(&self, f: &mut dyn FnMut(&mut PingState)) {
        self.update_ping(|p| f(p));
    }
    fn report_panic(&self, panic: &str, wait: Duration) {
        self.report_secondary_panic(panic, wait);
    }
}

/// Настоящий замер: имя узла → IPv4 → эхо-запрос.
pub(crate) fn measure(host: &str) -> Result<u32, String> {
    resolve(host).and_then(|ip| echo(ip, TIMEOUT_MS))
}

/// Узел для пинга, который вообще можно разрешить: адрес IPv4 или имя DNS (RFC 1123: метки 1–63 знаков из букв,
/// цифр и дефиса, не с дефиса по краям, всё имя до 253 знаков, точка в конце допустима). IPv6 нет: `resolve`
/// ищет только IPv4. Имя из одних цифровых меток (`999.1.1.1`) — опечатка в адресе, а не имя.
/// Окно не даёт сохранить другое — мусор не уходит агенту.
pub(crate) fn valid_host(host: &str) -> bool {
    if host.parse::<Ipv4Addr>().is_ok() {
        return true;
    }
    let name = host.strip_suffix('.').unwrap_or(host);
    let label_ok = |l: &str| {
        (1..=63).contains(&l.len()) && !l.starts_with('-') && !l.ends_with('-') && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    };
    let last_alpha = name.rsplit('.').next().is_some_and(|l| !l.bytes().all(|b| b.is_ascii_digit()));
    !name.is_empty() && name.len() <= 253 && name.split('.').all(label_ok) && last_alpha
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
    // u64, а не u8: ответ читается как ICMP_ECHO_REPLY (в нём указатель), буфер из u8 выровнен только на 1.
    let mut reply = vec![0u64; (size_of::<ICMP_ECHO_REPLY>() + payload.len() + 64).div_ceil(8)];
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
            size_of_val(reply.as_slice()) as u32,
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
    fn ping_host_validation() {
        let long_label = "a".repeat(63);
        let long_name = format!("{0}.{0}.{0}.{0}", "b".repeat(63)); // 255 знаков
        let good = ["1.1.1.1", "8.8.8.8", "localhost", "one.one.one.one", "dns.google.", "my-host.example.com", "xn--80ak6aa92e.com", &long_label];
        for h in good {
            assert!(valid_host(h), "rejected good host {h:?}");
        }
        let too_long_label = "a".repeat(64);
        let bad = [
            "", " ", ".", "1.1.1.1 ", "http://1.1.1.1", "host name", "-host.com", "host-.com", "a..b", "999.1.1.1", "1.2.3",
            "::1", "2606:4700::1111", "host_name.com", "хост.рф", "1.1.1.1:80", &too_long_label, &long_name,
        ];
        for h in bad {
            assert!(!valid_host(h), "accepted bad host {h:?}");
        }
    }

    #[test]
    fn loopback_answers() {
        assert!(echo(Ipv4Addr::LOCALHOST, 1000).is_ok());
    }

    /// Один подключённый туннель; первый замер паникует (как на странном ответе ICMP), дальше — 7 мс.
    #[derive(Default)]
    struct PanickyHost {
        pings: std::sync::atomic::AtomicU32,
    }

    impl crate::backend::TunnelHost for PanickyHost {
        fn configs(&self) -> std::io::Result<Vec<String>> {
            Ok(vec!["t".into()])
        }
        fn running(&self) -> std::io::Result<Vec<String>> {
            Ok(vec!["t".into()])
        }
        fn query(&self, _: &str) -> std::io::Result<crate::uapi::Status> {
            Ok(crate::uapi::Status::default())
        }
        fn connect(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn disconnect(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn ping_ms(&self, _: &str) -> Result<u32, String> {
            if self.pings.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                assert!(crate::crash::isolated_now(), "паника пинга не станет сбоем ядра");
                panic!("odd ICMP reply");
            }
            Ok(7)
        }
    }

    #[test]
    fn ping_panic_is_logged_and_the_next_measure_runs() {
        use crate::monitor::{Options, Shared};
        let host = Arc::new(PanickyHost::default());
        let options = Options { ping: true, ping_host: "h".into(), notify: false, tray: false, taskbar: false };
        let shared = Shared::new(Some(host.clone()), options, None);
        crate::monitor::poll(&shared, host.as_ref());
        assert!(shared.any_running());

        let start = Instant::now();
        let now = std::cell::Cell::new(start);
        let mut pinger = Pinger::new(start);
        let report = |panic: &str, wait: Duration| shared.report_secondary_panic(panic, wait);
        crate::crash::nonfatal_loop(STEP, &|d| now.set(now.get() + d), &report, || {
            pinger.step(&shared, now.get());            if host.pings.load(std::sync::atomic::Ordering::SeqCst) >= 2 {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        });
        assert!(crate::crash::core_failure().is_none());
        assert_eq!(shared.update_ping(|p| p.last.clone()), Some(Ok(7)), "после паники замер идёт по расписанию");
        let events = shared.events_since(0);
        assert!(events.iter().any(|(_, e)| e.severity == crate::events::Severity::Bad && e.text.contains("odd ICMP reply")), "паника — в журнале");
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
