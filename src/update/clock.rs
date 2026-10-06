//! Часы планировщика ежедневной проверки: время суток, время работы ядра и сон между шагами за интерфейсом, чтобы
//! решение «пора проверять» и сам шаг проверялись без ожидания.

use std::time::{Duration, Instant};

use crate::monitor::unix_now;

/// Первая фоновая проверка — через столько после запуска ядра.
pub(super) const FIRST_CHECK: Duration = Duration::from_secs(120);
/// Дальше — не чаще раза в сутки, сек.
pub(super) const CHECK_EVERY: u64 = 24 * 3600;
/// Шаг сна фонового потока.
pub(super) const TICK: Duration = Duration::from_secs(30);

pub(super) trait Clock: Send + Sync {
    /// Время сейчас (unix, сек).
    fn now(&self) -> u64;
    /// Сколько прошло с запуска планировщика.
    fn uptime(&self) -> Duration;
    fn sleep(&self, d: Duration);
}

/// Настоящие часы; отсчёт времени работы — с создания.
pub(super) struct SystemClock {
    start: Instant,
}

impl SystemClock {
    pub(super) fn new() -> SystemClock {
        SystemClock { start: Instant::now() }
    }
}

impl Clock for SystemClock {
    fn now(&self) -> u64 {
        unix_now()
    }

    fn uptime(&self) -> Duration {
        self.start.elapsed()
    }

    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// Пора ли проверять: первая проверка — после `FIRST_CHECK` работы, дальше — когда с последней (`checked_at`)
/// прошли сутки или проверки ещё не было. Часы ушли назад (`checked_at` в будущем) — не пора, а не «прошло много».
pub(super) fn check_due(first: bool, uptime: Duration, checked_at: Option<u64>, now: u64) -> bool {
    if first {
        return uptime >= FIRST_CHECK;
    }
    checked_at.map_or(true, |t| now.saturating_sub(t) >= CHECK_EVERY)
}

#[cfg(test)]
pub(super) mod fake {
    use std::sync::Mutex;

    use super::*;

    /// Подделка: время стоит, пока его не сдвинут; сон двигает время вперёд и не ждёт.
    pub(in super::super) struct FakeClock {
        pub at: Mutex<u64>,
        pub up: Mutex<Duration>,
        /// Столько следующих вызовов `uptime` паникуют (как сбой часов) — проверка изоляции цикла планировщика.
        pub panics: Mutex<u32>,
    }

    impl FakeClock {
        pub fn new(at: u64) -> FakeClock {
            FakeClock { at: Mutex::new(at), up: Mutex::new(Duration::ZERO), panics: Mutex::new(0) }
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> u64 {
            *self.at.lock().unwrap()
        }

        fn uptime(&self) -> Duration {
            let mut panics = self.panics.lock().unwrap();
            if *panics > 0 {
                *panics -= 1;
                drop(panics);
                assert!(crate::crash::isolated_now(), "паника планировщика не станет сбоем ядра");
                panic!("clock edge");
            }
            drop(panics);
            *self.up.lock().unwrap()
        }

        fn sleep(&self, d: Duration) {
            *self.at.lock().unwrap() += d.as_secs();
            *self.up.lock().unwrap() += d;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_check_waits_for_the_startup_delay() {
        let before = FIRST_CHECK - Duration::from_secs(1);
        assert!(!check_due(true, before, None, 1_000_000), "рано после запуска, даже если проверок не было");
        assert!(check_due(true, FIRST_CHECK, None, 1_000_000));
        assert!(check_due(true, FIRST_CHECK, Some(999_999), 1_000_000), "первая — независимо от прошлой проверки");
    }

    #[test]
    fn later_checks_run_once_a_day() {
        let now = 10_000_000;
        assert!(check_due(false, Duration::ZERO, None, now), "проверок ещё не было");
        assert!(!check_due(false, Duration::ZERO, Some(now - CHECK_EVERY + 1), now), "нет суток");
        assert!(check_due(false, Duration::ZERO, Some(now - CHECK_EVERY), now), "ровно сутки");
        assert!(!check_due(false, Duration::ZERO, Some(now + 5), now), "часы ушли назад — не пора");
    }
}
