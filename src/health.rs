//! Итоговое состояние туннеля: уровень (цвет точки, значок в трее, события) и текст для человека.

use crate::fmt;
use crate::i18n::{tr, trf};
use crate::monitor::{unix_now, Live};
use crate::ping::PingState;

/// WireGuard отбрасывает сессию через 180 с без нового рукопожатия.
pub const STALE_HANDSHAKE_SECS: u64 = 180;
/// За сколько секунд считаем «пакеты идут».
const TRAFFIC_WINDOW_SECS: f64 = 10.0;
/// Столько секунд после появления туннеля отсутствие рукопожатия — ещё не ошибка.
const STARTUP_GRACE_SECS: f64 = 15.0;
/// С какого числа неудачных пингов подряд считать, что трафик не проходит.
pub const PING_FAILS_WARN: u32 = 2;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    Off,
    Busy,
    Ok,
    Warn,
    Bad,
}

#[derive(Clone, Debug)]
pub struct Health {
    pub level: Level,
    pub text: String,
}

/// `ping` — Some, только если проверка пингом включена.
pub fn health(live: Option<&Live>, pending: Option<&str>, ping: Option<&PingState>) -> Health {
    let h = |level, text: &str| Health { level, text: text.to_string() };
    if let Some(p) = pending {
        return h(Level::Busy, &tr(p));
    }
    let Some(live) = live else { return h(Level::Off, &tr("health.off")) };
    let Some(st) = &live.status else {
        return h(Level::Bad, &trf("health.no_service", &[live.error.as_deref().unwrap_or("?")]));
    };
    let hs = st.last_handshake_sec();
    if hs == 0 {
        return if live.observed_secs() < STARTUP_GRACE_SECS {
            h(Level::Busy, &tr("health.wait"))
        } else {
            h(Level::Bad, &tr("health.no_handshake"))
        };
    }
    let age = unix_now().saturating_sub(hs);
    if age > STALE_HANDSHAKE_SECS {
        return h(Level::Bad, &trf("health.stale", &[&fmt::ago(age)]));
    }
    if let Some(p) = ping.filter(|p| p.fails >= PING_FAILS_WARN) {
        return h(Level::Warn, &trf("health.ping_fail", &[&p.host]));
    }
    if live.rate(TRAFFIC_WINDOW_SECS).0 > 0.0 {
        h(Level::Ok, &tr("health.ok_traffic"))
    } else {
        h(Level::Ok, &tr("health.ok_idle"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::Sample;
    use crate::uapi::{Peer, Status};
    use std::time::{Duration, Instant};

    fn live(handshake_age: Option<u64>, rx: &[u64]) -> Live {
        let hs = handshake_age.map(|a| unix_now() - a).unwrap_or(0);
        let status = Status { peers: vec![Peer { last_handshake_sec: hs, ..Default::default() }], ..Default::default() };
        let t0 = Instant::now() - Duration::from_secs(rx.len() as u64);
        let history = rx
            .iter()
            .enumerate()
            .map(|(i, r)| Sample { at: t0 + Duration::from_secs(i as u64), rx: *r, tx: 0 })
            .collect();
        Live { status: Some(status), error: None, history }
    }

    #[test]
    fn levels() {
        assert_eq!(health(None, None, None).level, Level::Off);
        assert_eq!(health(None, Some("Подключение…"), None).level, Level::Busy);
        assert_eq!(health(Some(&live(Some(5), &[0, 100])), None, None).level, Level::Ok);
        // Тишина при свежем рукопожатии — норма, а не предупреждение (иначе уведомления на каждый простой).
        assert_eq!(health(Some(&live(Some(5), &[0, 0])), None, None).level, Level::Ok);
        assert_eq!(health(Some(&live(Some(400), &[0, 0])), None, None).level, Level::Bad);
        assert_eq!(health(Some(&live(None, &[0])), None, None).level, Level::Busy);
        assert_eq!(health(Some(&live(None, &[0; 30])), None, None).level, Level::Bad);
    }

    #[test]
    fn ping_failures_warn_only_with_fresh_handshake() {
        let ping = PingState { host: "1.1.1.1".into(), fails: 3, ..Default::default() };
        assert_eq!(health(Some(&live(Some(5), &[0, 1])), None, Some(&ping)).level, Level::Warn);
        assert_eq!(health(Some(&live(Some(400), &[0, 1])), None, Some(&ping)).level, Level::Bad);
    }
}
