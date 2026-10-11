//! Мёртвый туннель: служба работает, а связи нет — пир недоступен, рукопожатие не обновляется. Надзор повторов
//! (`retry`) видит только остановленные и пропавшие службы; такой туннель он считает подключённым, и VPN не
//! возвращается сам. Здесь его распознают и перезапускают обычным переключением ядра (`Plan::Reconnect`), с
//! нарастающей паузой и бессрочно: 1, 2, 5 минут, дальше раз в 10 минут.
//!
//! Мёртв — по одному из двух признаков:
//! 1. Молчание пира (`SILENT`): за последние 30 с передано не меньше `SILENT_MIN_TX`, а принято ничего — при любом
//!    возрасте рукопожатия. Живой пир WireGuard на принятые данные отвечает не позже чем через 10 с (своими данными или
//!    пустым keepalive), а через 15 с без ответа WireGuard сам начинает рукопожатие, на которое живой пир отвечает за
//!    доли секунды. Порог по объёму отсекает простой с постоянным keepalive (`PersistentKeepalive`): на пустые keepalive
//!    пир не отвечает, а даже раз в секунду они дают меньше 1 КБ за 30 с. Ловит обрыв сразу после рукопожатия
//!    (пропавшая запись NAT у роутера): без этого признака перезапуск ждал бы 3 минуты, а RDP и прочие сессии внутри
//!    туннеля за это время рвутся.
//! 2. Протухшее рукопожатие: его нет дольше `STALE_HANDSHAKE_SECS` (или не было вовсе), и туннель пытается слать — за
//!    `WINDOW` передано больше, а принято ничего. WireGuard обновляет рукопожатие раз в 2 минуты, пока идёт трафик, а
//!    ключи сессии старше 3 минут отбрасывает: без нового рукопожатия за 3 минуты данные по туннелю уже не ходят. Это
//!    признак для туннеля с редким трафиком, который до порога молчания не дотягивает.
//!
//! Туннель без трафика рукопожатие не обновляет вовсе — это простой, а не обрыв: без роста передачи он мёртвым не
//! считается (иначе перезапуски шли бы на каждом простое). Попытки рукопожатия WireGuard считает в переданном
//! (`tx_bytes`), поэтому недоступный пир виден ростом передачи без приёма.
//!
//! Здесь только решения; время и показания — параметрами, хост — снаружи (`server::Core::watch_dead`).

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use crate::health::STALE_HANDSHAKE_SECS;
use crate::monitor::Live;

/// За сколько последних секунд смотрим рост передачи и приёма. Пир отвечает на рукопожатие за доли секунды, WireGuard
/// повторяет его раз в 5 с: минута без единого принятого байта при отправке — не задержка, а недоступность.
pub(super) const WINDOW: Duration = Duration::from_secs(60);
/// Окно признака молчания: вдвое больше 15 с, через которые WireGuard без ответа пира сам начинает рукопожатие.
pub(super) const SILENT: Duration = Duration::from_secs(30);
/// Сколько надо передать за `SILENT` без приёма, чтобы это было молчание пира, а не простой с keepalive (32 байта на
/// пакет: раз в секунду — 960 байт за окно).
pub(super) const SILENT_MIN_TX: u64 = 1024;
/// Последний замер старше этого — опрос не идёт, по старым данным не решаем.
const SAMPLE_MAX_AGE: Duration = Duration::from_secs(10);
/// Паузы между перезапусками по их номеру; последняя — бессрочно.
const BACKOFF: [Duration; 4] =
    [Duration::from_secs(60), Duration::from_secs(2 * 60), Duration::from_secs(5 * 60), Duration::from_secs(10 * 60)];

/// Что видно по показаниям туннеля.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Verdict {
    /// Рукопожатие свежее — связь есть.
    Fresh,
    /// Шлёт, ничего не принимает: пир молчит `SILENT` или рукопожатие протухло.
    Dead,
    /// Ни то ни другое: простой, мало замеров или нет показаний.
    Unknown,
}

/// Вердикт по показаниям `live`. `since` — последний перезапуск: замеры до него принадлежат прежней службе (её
/// счётчики не сравнимы с новыми), и окно после перезапуска набирается заново.
pub(super) fn verdict(live: &Live, now: Instant, now_unix: u64, since: Option<Instant>) -> Verdict {
    let Some(status) = &live.status else { return Verdict::Unknown };
    if growth(live, now, since, SILENT).is_some_and(|(tx, rx)| tx >= SILENT_MIN_TX && rx == 0) {
        return Verdict::Dead;
    }
    let handshake = status.last_handshake_sec();
    if handshake != 0 && now_unix.saturating_sub(handshake) <= STALE_HANDSHAKE_SECS {
        return Verdict::Fresh;
    }
    match growth(live, now, since, WINDOW) {
        Some((tx, 0)) if tx > 0 => Verdict::Dead,
        _ => Verdict::Unknown,
    }
}

/// Прирост (передано, принято) за последние `len`; `None` — опрос стоит или замеров новой службы на окно ещё не набралось.
fn growth(live: &Live, now: Instant, since: Option<Instant>, len: Duration) -> Option<(u64, u64)> {
    let last = live.history.back()?;
    if now.saturating_duration_since(last.at) > SAMPLE_MAX_AGE {
        return None;
    }
    let from = now.checked_sub(len)?;
    let first = live.history.iter().rev().find(|s| s.at <= from).filter(|s| since.is_none_or(|at| s.at > at))?;
    Some((last.tx.saturating_sub(first.tx), last.rx.saturating_sub(first.rx)))
}

/// Пауза после перезапуска номер `restart` (с 1).
fn backoff(restart: u32) -> Duration {
    BACKOFF[(restart.max(1) as usize - 1).min(BACKOFF.len() - 1)]
}

/// Что записать в журнал.
#[derive(Debug, PartialEq)]
pub(super) enum DeadNote {
    /// Туннель признан мёртвым — один раз на обрыв.
    Detected,
    /// Перезапуск сделан (первые фазы, паузы 1, 2, 5 минут); `error` — не удался.
    Restarted { restart: u32, error: Option<String> },
    /// Вход в фазу «раз в 10 минут» — один раз, с уведомлением; дальше перезапуски молчат.
    Slow { restarts: u32, error: Option<String> },
    /// Связь вернулась (свежее рукопожатие) после стольких перезапусков.
    Recovered { restarts: u32 },
}

/// Один мёртвый туннель.
#[derive(Debug)]
struct Track {
    restarts: u32,
    next_at: Instant,
    /// Последний перезапуск: от него набирается окно замеров.
    restarted_at: Option<Instant>,
    slow: bool,
}

/// Состояние сторожа. Хозяин — надзор ядра (`Retries::dead`), меняется под его замком.
#[derive(Default)]
pub(super) struct DeadWatch {
    tracks: BTreeMap<String, Track>,
}

/// Итог такта сторожа.
#[derive(Debug, Default, PartialEq)]
pub(super) struct DeadTick {
    /// Перезапустить сейчас.
    pub due: Vec<String>,
    pub notes: Vec<(String, DeadNote)>,
}

impl DeadWatch {
    /// Когда туннели перезапускались — для `verdict`.
    pub(super) fn restarted_at(&self) -> BTreeMap<String, Instant> {
        self.tracks.iter().filter_map(|(t, tr)| tr.restarted_at.map(|at| (t.clone(), at))).collect()
    }

    /// Такт: `verdicts` — по работающим туннелям; `skip` — туннель сейчас не наш (аренда, команда пользователя,
    /// надзор повторов): не трогаем, его счётчики не меняются.
    pub(super) fn tick(&mut self, now: Instant, desired: &[String], verdicts: &[(String, Verdict)], skip: &dyn Fn(&str) -> bool) -> DeadTick {
        self.tracks.retain(|t, _| desired.contains(t));
        let mut tick = DeadTick::default();
        for (t, verdict) in verdicts {
            if !desired.contains(t) || skip(t) {
                continue;
            }
            match verdict {
                Verdict::Fresh => {
                    if let Some(track) = self.tracks.remove(t) {
                        tick.notes.push((t.clone(), DeadNote::Recovered { restarts: track.restarts }));
                    }
                }
                Verdict::Dead => {
                    let track = self.tracks.entry(t.clone()).or_insert_with(|| {
                        tick.notes.push((t.clone(), DeadNote::Detected));
                        Track { restarts: 0, next_at: now, restarted_at: None, slow: false }
                    });
                    if track.next_at <= now {
                        tick.due.push(t.clone());
                    }
                }
                // Простой или мало замеров: ждём, счётчики не сбрасываем — обрыв может продолжиться.
                Verdict::Unknown => {}
            }
        }
        tick
    }

    /// Исход перезапуска: `Ok(true)` — сделан, `Ok(false)` — делать было нечего (туннель уже не работает, не желаемый
    /// или взят в аренду — решит следующий такт), `Err` — не удался (служба могла остаться остановленной — её поднимет
    /// надзор повторов). Туннель, с которого за это время сняли надзор, не трогается.
    pub(super) fn restarted(&mut self, name: &str, now: Instant, result: Result<bool, String>) -> Option<DeadNote> {
        let track = self.tracks.get_mut(name)?;
        let error = match result {
            Ok(false) => return None,
            Ok(true) => None,
            Err(e) => Some(e),
        };
        track.restarts += 1;
        track.restarted_at = Some(now);
        let wait = backoff(track.restarts);
        track.next_at = now + wait;
        if wait < BACKOFF[BACKOFF.len() - 1] {
            Some(DeadNote::Restarted { restart: track.restarts, error })
        } else if !std::mem::replace(&mut track.slow, true) {
            Some(DeadNote::Slow { restarts: track.restarts, error })
        } else {
            // Фаза «раз в 10 минут» молчит: перезапуски сутками иначе забили бы журнал.
            None
        }
    }

    /// Команда пользователя или аренда: счёт по туннелю с начала.
    pub(super) fn forget(&mut self, name: &str) {
        self.tracks.remove(name);
    }

    pub(super) fn clear(&mut self) {
        self.tracks.clear();
    }

    #[cfg(test)]
    fn is_tracked(&self, name: &str) -> bool {
        self.tracks.contains_key(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::Sample;
    use crate::uapi::{Peer, Status};

    /// Туннель под наблюдением: замер раз в секунду, как опрос ядра.
    struct Sim {
        t0: Instant,
        unix0: u64,
        desired: Vec<String>,
        live: Live,
        /// Что-то пытается слать (приложения или keepalive).
        sending: bool,
        /// Пир отвечает.
        peer_up: bool,
        handshake: u64,
        tx: u64,
        rx: u64,
        held: bool,
        restarts: Vec<u64>,
        notes: Vec<DeadNote>,
    }

    impl Sim {
        fn new(handshake_age: Option<u64>) -> Sim {
            let unix0 = 1_000_000;
            Sim {
                t0: Instant::now(),
                unix0,
                desired: vec!["office".into()],
                live: Live::default(),
                sending: false,
                peer_up: true,
                handshake: handshake_age.map_or(0, |a| unix0 - a),
                tx: 0,
                rx: 0,
                held: false,
                restarts: vec![],
                notes: vec![],
            }
        }

        /// Секунда `s`: замер, такт сторожа, перезапуск как `Core::watch_dead` (новая служба — счётчики с нуля).
        fn step(&mut self, w: &mut DeadWatch, s: u64) {
            let now = self.t0 + Duration::from_secs(s);
            let unix = self.unix0 + s;
            if self.sending {
                self.tx += 148;
                if self.peer_up {
                    self.rx += 92;
                    // Пока идёт трафик, рукопожатие обновляется раз в 2 минуты.
                    if self.handshake == 0 || unix - self.handshake >= 120 {
                        self.handshake = unix;
                    }
                }
            }
            let peer = Peer { last_handshake_sec: self.handshake, rx_bytes: self.rx, tx_bytes: self.tx, ..Default::default() };
            self.live.status = Some(Status { peers: vec![peer], ..Default::default() });
            self.live.history.push_back(Sample { at: now, rx: self.rx, tx: self.tx });
            let since = w.restarted_at().get("office").copied();
            let verdicts = [("office".to_string(), verdict(&self.live, now, unix, since))];
            let held = self.held;
            let tick = w.tick(now, &self.desired, &verdicts, &|_| held);
            self.notes.extend(tick.notes.into_iter().map(|(_, n)| n));
            for t in tick.due {
                self.restarts.push(s);
                (self.tx, self.rx, self.handshake) = (0, 0, 0);
                // Перезапуск занимает время: новая служба отвечает уже после замера этой секунды.
                if let Some(n) = w.restarted(&t, now + Duration::from_millis(500), Ok(true)) {
                    self.notes.push(n);
                }
            }
        }

        fn run(&mut self, w: &mut DeadWatch, from: u64, to: u64) {
            (from..to).for_each(|s| self.step(w, s));
        }
    }

    /// Простой: рукопожатие давно протухло (или его не было), но туннель ничего не шлёт — не обрыв, перезапусков нет.
    #[test]
    fn idle_tunnel_with_stale_handshake_is_not_restarted() {
        for age in [Some(3600), None] {
            let mut w = DeadWatch::default();
            let mut sim = Sim::new(age);
            sim.run(&mut w, 0, 7200);
            assert!(sim.restarts.is_empty() && sim.notes.is_empty(), "{age:?}: {:?} {:?}", sim.restarts, sim.notes);
        }
    }

    /// Пир недоступен, туннель шлёт: признан мёртвым по молчанию за 30 с, перезапуск сразу, дальше паузы 1, 2, 5 минут
    /// и раз в 10 минут бессрочно. В журнале: обнаружение, перезапуски первых фаз, вход в 10-минутную — один раз.
    #[test]
    fn dead_tunnel_is_restarted_with_backoff_forever() {
        let mut w = DeadWatch::default();
        let mut sim = Sim::new(Some(600));
        sim.sending = true;
        sim.peer_up = false;
        sim.run(&mut w, 0, 4000);
        // Окно после перезапуска набирается заново: первый замер новой службы — на следующей секунде, отсюда +1 с к паузе.
        assert_eq!(sim.restarts, [30, 91, 212, 513, 1114, 1715, 2316, 2917, 3518]);
        assert_eq!(
            sim.notes,
            [
                DeadNote::Detected,
                DeadNote::Restarted { restart: 1, error: None },
                DeadNote::Restarted { restart: 2, error: None },
                DeadNote::Restarted { restart: 3, error: None },
                DeadNote::Slow { restarts: 4, error: None },
            ]
        );
    }

    /// Рукопожатие свежее и передача мала (до порога молчания не дотягивает) — не обрыв, даже если ответов за минуту нет;
    /// протухло, но приём идёт — тоже нет. Протухло и приёма нет — мёртв по второму признаку.
    #[test]
    fn fresh_handshake_or_incoming_traffic_is_not_dead() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let live = |handshake: u64, rx_grows: bool| Live {
            status: Some(Status { peers: vec![Peer { last_handshake_sec: handshake, ..Default::default() }], ..Default::default() }),
            error: None,
            history: (0..=70).map(|s| Sample { at: at(s), rx: if rx_grows { s } else { 0 }, tx: s * 10 }).collect(),
        };
        let unix = 10_000;
        assert_eq!(verdict(&live(unix - 170, false), at(70), unix, None), Verdict::Fresh);
        assert_eq!(verdict(&live(unix - 400, true), at(70), unix, None), Verdict::Unknown);
        assert_eq!(verdict(&live(unix - 400, false), at(70), unix, None), Verdict::Dead);
        assert_eq!(verdict(&live(0, false), at(70), unix, None), Verdict::Dead, "рукопожатия не было ни разу");
        assert_eq!(verdict(&live(unix - 400, false), at(85), unix, None), Verdict::Unknown, "опрос стоит — не решаем");
        assert_eq!(verdict(&live(unix - 400, false), at(70), unix, Some(at(20))), Verdict::Unknown, "после перезапуска мало замеров");
    }

    /// Обрыв сразу после рукопожатия (pc-dev, 2026.10.11: рукопожатие в 13:55:46, пир замолчал, перезапуск только в
    /// 13:58:47 — RDP внутри туннеля за это время порвался): перезапуск через полминуты молчания, а не через 3 минуты
    /// протухания рукопожатия.
    #[test]
    fn silent_peer_right_after_a_handshake_is_restarted_within_half_a_minute() {
        let mut w = DeadWatch::default();
        let mut sim = Sim::new(Some(24));
        sim.sending = true;
        sim.peer_up = false;
        sim.run(&mut w, 0, 100);
        assert_eq!(sim.restarts, [30, 91]);
        assert_eq!(sim.notes, [DeadNote::Detected, DeadNote::Restarted { restart: 1, error: None }, DeadNote::Restarted { restart: 2, error: None }]);
    }

    /// Признак молчания: свежее рукопожатие не спасает, если за 30 с передан 1 КБ и не принято ничего; не срабатывает на
    /// простой с keepalive раз в секунду (меньше 1 КБ), на ответы пира и на окно короче 30 с.
    #[test]
    fn silence_needs_half_a_minute_and_a_kilobyte_sent_without_a_byte_received() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let unix = 10_000;
        let live = |tx_per_s: u64, rx_per_s: u64, secs: u64| Live {
            status: Some(Status { peers: vec![Peer { last_handshake_sec: unix - 20, ..Default::default() }], ..Default::default() }),
            error: None,
            history: (0..=secs).map(|s| Sample { at: at(s), rx: s * rx_per_s, tx: s * tx_per_s }).collect(),
        };
        assert_eq!(verdict(&live(100, 0, 30), at(30), unix, None), Verdict::Dead);
        assert_eq!(verdict(&live(32, 0, 30), at(30), unix, None), Verdict::Fresh, "keepalive раз в секунду — простой");
        assert_eq!(verdict(&live(100, 1, 30), at(30), unix, None), Verdict::Fresh, "пир отвечает");
        assert_eq!(verdict(&live(100, 0, 29), at(29), unix, None), Verdict::Fresh, "молчит меньше 30 с");
        assert_eq!(verdict(&live(100, 0, 30), at(30), unix, Some(at(10))), Verdict::Fresh, "после перезапуска мало замеров");
    }

    /// Связь вернулась: одна строка с числом перезапусков, счёт с нуля — следующий обрыв снова с обнаружения и паузы 1 мин.
    #[test]
    fn recovery_is_logged_once_and_resets_the_counters() {
        let mut w = DeadWatch::default();
        let mut sim = Sim::new(Some(600));
        sim.sending = true;
        sim.peer_up = false;
        sim.run(&mut w, 0, 200);
        assert_eq!(sim.restarts, [30, 91]);
        sim.peer_up = true;
        sim.run(&mut w, 200, 1000);
        assert_eq!(sim.restarts, [30, 91]);
        assert_eq!(sim.notes.last(), Some(&DeadNote::Recovered { restarts: 2 }));
        assert_eq!(sim.notes.iter().filter(|n| matches!(n, DeadNote::Recovered { .. })).count(), 1);
        assert!(!w.is_tracked("office"));

        sim.peer_up = false;
        let n = sim.notes.len();
        sim.run(&mut w, 1000, 1200);
        // Последний приём — на 999 с: молчание 30 с, перезапуск на 1029 (рукопожатие на 920 ещё свежее), затем через минуту.
        assert_eq!(sim.restarts[2..], [1029, 1090]);
        assert_eq!(sim.notes[n..], [DeadNote::Detected, DeadNote::Restarted { restart: 1, error: None }, DeadNote::Restarted { restart: 2, error: None }]);
    }

    /// Туннель под арендой (или в руках пользователя, или у надзора повторов) не трогается, его счёт не меняется.
    #[test]
    fn held_tunnel_is_untouched() {
        let mut w = DeadWatch::default();
        let mut sim = Sim::new(Some(600));
        sim.sending = true;
        sim.peer_up = false;
        sim.held = true;
        sim.run(&mut w, 0, 3000);
        assert!(sim.restarts.is_empty() && sim.notes.is_empty());
        assert!(!w.is_tracked("office"));
    }

    /// Неудачный перезапуск тоже считается и пишется с ошибкой; туннель вне желаемого набора — счёт снят.
    #[test]
    fn failed_restart_counts_and_undesired_tunnel_is_dropped() {
        let t0 = Instant::now();
        let mut w = DeadWatch::default();
        let desired = vec!["office".to_string()];
        let dead = [("office".to_string(), Verdict::Dead)];
        assert_eq!(w.tick(t0, &desired, &dead, &|_| false).due, ["office"]);
        assert_eq!(w.restarted("office", t0, Err("refused".into())), Some(DeadNote::Restarted { restart: 1, error: Some("refused".into()) }));
        assert_eq!(w.restarted("office", t0, Ok(false)), None, "нечего делать — не перезапуск");
        assert!(w.tick(t0 + Duration::from_secs(59), &desired, &dead, &|_| false).due.is_empty());
        assert_eq!(w.tick(t0 + Duration::from_secs(60), &desired, &dead, &|_| false).due, ["office"]);
        w.tick(t0 + Duration::from_secs(61), &[], &dead, &|_| false);
        assert!(!w.is_tracked("office"));
        assert_eq!(w.restarted("office", t0, Ok(true)), None);
    }
}
