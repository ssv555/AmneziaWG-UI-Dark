//! Переподключение туннелей — один хозяин повторов на всё ядро. Желаемый туннель (`Config::tunnels`), который не
//! работает — не поднялся при загрузке (сеть бывает готова только через минуту-две) или упал сам, без команды
//! пользователя, — ядро поднимает снова обычным переключением, по расписанию: первые 3 минуты — каждые 10 с,
//! до 10 минут — раз в минуту, дальше — раз в 10 минут бессрочно (владелец может быть далеко от машины). Перезапуск
//! служб туннелей диспетчером Windows при сбое отключён (`engine::clear_restart_on_failure`): два механизма
//! пересоздавали бы одну службу наперегонки. Смена сети (`netwatch`) — попытка сразу, в любой фазе.
//!
//! Здесь только решения: что делать на очередном такте и что писать в журнал. Время — параметром, хост — снаружи
//! (`server::Core::supervise_tick`): расписание проверяется тестами без часов и служб.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use super::proto::RetryState;
use crate::i18n::tr;

/// Первая фаза: попытка каждые `FAST_EVERY`, пока с начала не прошло `FAST_FOR`.
pub(super) const FAST_EVERY: Duration = Duration::from_secs(10);
pub(super) const FAST_FOR: Duration = Duration::from_secs(3 * 60);
/// Вторая фаза: раз в минуту до `SLOW_FROM` с начала.
pub(super) const MID_EVERY: Duration = Duration::from_secs(60);
pub(super) const SLOW_FROM: Duration = Duration::from_secs(10 * 60);
/// Третья фаза — бессрочно, без записей в журнал о каждой попытке (только вход в неё и успех).
pub(super) const SLOW_EVERY: Duration = Duration::from_secs(10 * 60);
/// Внеочередная попытка по смене сети — не чаще этого после предыдущей: Windows шлёт уведомления пачкой.
pub(super) const NETWORK_MIN_GAP: Duration = Duration::from_secs(3);
/// Такт надзора ядра.
pub(super) const TICK: Duration = Duration::from_secs(1);
/// Столько туннель должен проработать, чтобы считаться подключённым. `tunnel.dll` сообщает «работает» до настройки
/// адресов интерфейса, а она ещё может сорваться («Element not found», код 1168) — служба тогда встаёт через долю
/// секунды: «подключён» по одному удачному запуску был бы неправдой.
pub(super) const CONFIRM_FOR: Duration = Duration::from_secs(5);

/// Один туннель под надзором.
#[derive(Debug, Clone)]
struct Track {
    /// Начало расписания: от него считаются фазы.
    since: Instant,
    /// Сделано попыток.
    attempt: u32,
    next_at: Instant,
    last_try: Option<Instant>,
    last_error: String,
    /// Идёт третья фаза (раз в 10 минут).
    slow: bool,
    /// С какого момента туннель виден работающим (ждёт подтверждения `CONFIRM_FOR`).
    up_since: Option<Instant>,
    /// Поднят нашей попыткой: встал до подтверждения — это неудачная попытка, а не тишина.
    started: bool,
}

impl Track {
    fn new(now: Instant, first_in: Duration) -> Track {
        Track {
            since: now,
            attempt: 0,
            next_at: now + first_in,
            last_try: None,
            last_error: String::new(),
            slow: false,
            up_since: None,
            started: false,
        }
    }

    /// Попытка сделана сейчас.
    fn attempted(&mut self, now: Instant) {
        self.attempt += 1;
        self.last_try = Some(now);
    }

    /// Последняя попытка не удалась (сразу или туннель встал до подтверждения): следующая — по расписанию от того,
    /// когда это стало известно; что записать в журнал.
    fn failed(&mut self, now: Instant, error: String) -> Option<Note> {
        self.last_error = error.clone();
        let elapsed = now.saturating_duration_since(self.since);
        let was_slow = self.slow;
        self.slow = elapsed >= SLOW_FROM;
        self.next_at = now + interval(elapsed);
        match (was_slow, self.slow) {
            (false, false) => Some(Note::Failed { attempt: self.attempt, error }),
            (false, true) => Some(Note::Slow { error }),
            // Третья фаза молчит: попытка раз в 10 минут сутками иначе забила бы журнал.
            (true, _) => None,
        }
    }
}

/// Что ядро видит на такте.
pub(super) struct Seen<'a> {
    /// Желаемый набор.
    pub desired: &'a [String],
    /// Работающие; `None` — список не прочитался: новых решений не принимаем, назначенные попытки идут (их
    /// ошибка будет видна в журнале).
    pub running: Option<&'a [String]>,
    /// Туннель сейчас переключается по команде — не трогать.
    pub pending: &'a dyn Fn(&str) -> bool,
    pub service_exists: &'a dyn Fn(&str) -> bool,
    /// Почему служба туннеля остановилась (коды завершения), для журнала; `None` — не узнать.
    pub stop_reason: &'a dyn Fn(&str) -> Option<String>,
    /// Службы туннелей принадлежат AmneziaWG (режим 1): их может удалить его родное окно.
    pub native_services: bool,
    /// С прошлого такта менялась сеть.
    pub network_changed: bool,
}

/// Итог такта.
#[derive(Debug, Default, PartialEq)]
pub(super) struct Tick {
    /// Подключать сейчас.
    pub due: Vec<String>,
    /// Отключены вне программы (служба AmneziaWG удалена из его окна): выходят из желаемого набора.
    pub outside: Vec<String>,
    /// Итоги наблюдения: подключение подтверждено, поднятый попыткой туннель встал сразу после запуска.
    pub notes: Vec<(String, Note)>,
}

/// Что записать в журнал после попытки.
#[derive(Debug, PartialEq)]
pub(super) enum Note {
    /// Попытка не удалась (первые две фазы): номер, ошибка.
    Failed { attempt: u32, error: String },
    /// Вход в третью фазу — один раз, с уведомлением.
    Slow { error: String },
    /// Подключён (проработал `CONFIRM_FOR`) после стольких попыток; 0 — поднялся сам, без попыток ядра.
    Connected { attempts: u32 },
}

/// Состояние надзора. Хозяин — ядро (`Core::retries`), меняется только под его замком.
#[derive(Default)]
pub(super) struct Retries {
    tracks: BTreeMap<String, Track>,
    /// Первый такт после запуска ядра уже был.
    started: bool,
}

impl Retries {
    /// Очередной такт: кого взять под надзор, кого отпустить и кого подключать сейчас.
    pub(super) fn tick(&mut self, now: Instant, seen: &Seen) -> Tick {
        let mut tick = Tick::default();
        if let Some(running) = seen.running {
            // Вышел из набора (отключён пользователем, удалён) — надзор не нужен; заработавший снимается с надзора,
            // только проработав `CONFIRM_FOR` (`watch`).
            self.tracks.retain(|t, _| seen.desired.contains(t));
            tick.notes = self.watch(now, running, seen.stop_reason);
            let fresh: Vec<String> =
                seen.desired.iter().filter(|t| !running.contains(*t) && !self.tracks.contains_key(*t) && !(seen.pending)(t)).cloned().collect();
            for t in fresh {
                let exists = (seen.service_exists)(&t);
                if self.started && seen.native_services && !exists {
                    // Работал и пропал вместе со службой — его отключили в окне AmneziaWG: поднимать обратно значило
                    // бы спорить с пользователем.
                    tick.outside.push(t);
                    continue;
                }
                // Первый такт — восстановление после запуска ядра: служба есть — Windows как раз может поднимать её
                // сама, подождать; службы нет — ждать нечего. Позже — упал сам: первая попытка через обычный шаг.
                let first_in = if !self.started && !exists { Duration::ZERO } else { FAST_EVERY };
                self.tracks.insert(t, Track::new(now, first_in));
            }
        }
        self.started = true;
        if seen.network_changed {
            for track in self.tracks.values_mut() {
                let earliest = track.last_try.map_or(now, |at| at + NETWORK_MIN_GAP);
                track.next_at = track.next_at.min(earliest.max(now));
            }
        }
        tick.due = self
            .tracks
            .iter()
            .filter(|(t, tr)| tr.next_at <= now && tr.up_since.is_none() && !(seen.pending)(t))
            .map(|(t, _)| t.clone())
            .collect();
        tick
    }

    /// Работающие под надзором: проработал `CONFIRM_FOR` — подключён, надзор снят; поднятый нашей попыткой встал до
    /// подтверждения — неудачная попытка (с причиной остановки), расписание продолжается.
    fn watch(&mut self, now: Instant, running: &[String], stop_reason: &dyn Fn(&str) -> Option<String>) -> Vec<(String, Note)> {
        let mut notes = Vec::new();
        let mut confirmed = Vec::new();
        for (t, track) in self.tracks.iter_mut() {
            if running.contains(t) {
                let since = *track.up_since.get_or_insert(now);
                if now.saturating_duration_since(since) >= CONFIRM_FOR {
                    confirmed.push(t.clone());
                }
            } else if track.up_since.take().is_some() && std::mem::take(&mut track.started) {
                let error = stop_reason(t).unwrap_or_else(|| tr("core.retry_died"));
                notes.extend(track.failed(now, error).map(|n| (t.clone(), n)));
            }
        }
        for t in confirmed {
            let attempts = self.tracks.remove(&t).map_or(0, |tr| tr.attempt);
            notes.push((t, Note::Connected { attempts }));
        }
        notes
    }

    /// Исход попытки: `Ok(true)` — служба поднялась, `Ok(false)` — делать было нечего (уже работает или больше не
    /// желаемый — это решит следующий такт), `Err` — не удалось. Поднявшийся ещё не «подключён»: это запишет такт,
    /// когда туннель проработает `CONFIRM_FOR` (`watch`). Туннель, с которого пользователь за это время снял надзор
    /// своей командой, не трогается.
    pub(super) fn outcome(&mut self, name: &str, now: Instant, result: Result<bool, String>) -> Option<Note> {
        let track = self.tracks.get_mut(name)?;
        match result {
            Ok(true) => {
                track.attempted(now);
                track.up_since = Some(now);
                track.started = true;
                track.next_at = now + interval(now.saturating_duration_since(track.since));
                None
            }
            Ok(false) => None,
            Err(e) => {
                track.attempted(now);
                track.failed(now, e)
            }
        }
    }

    /// Команда пользователя над туннелем: надзор с него снимается (подключение он сделал сам, отключение — его воля).
    pub(super) fn forget(&mut self, name: &str) {
        self.tracks.remove(name);
    }

    /// Смена режима или отключение всего: надзор снимается целиком.
    pub(super) fn clear(&mut self) {
        self.tracks.clear();
    }

    /// «Повторить»: расписание с начала, попытка сразу.
    pub(super) fn restart(&mut self, name: &str, now: Instant) {
        self.tracks.insert(name.to_string(), Track::new(now, Duration::ZERO));
    }

    /// Состояние для окна.
    pub(super) fn view(&self, now: Instant) -> BTreeMap<String, RetryState> {
        self.tracks
            .iter()
            .map(|(t, tr)| {
                let left = tr.next_at.saturating_duration_since(now);
                // Округление вверх: «через 0 с» до самой попытки не показывается.
                let next_in_s = left.as_secs() + u64::from(left.subsec_nanos() > 0);
                (t.clone(), RetryState { attempt: tr.attempt, next_in_s, last_error: tr.last_error.clone(), slow: tr.slow })
            })
            .collect()
    }
}

/// Пауза до следующей попытки по времени с начала расписания.
fn interval(elapsed: Duration) -> Duration {
    if elapsed < FAST_FOR {
        FAST_EVERY
    } else if elapsed < SLOW_FROM {
        MID_EVERY
    } else {
        SLOW_EVERY
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Машина под надзором: что работает, какие службы есть, сколько ещё попыток подключения не пройдут.
    struct World {
        desired: Vec<String>,
        running: Vec<String>,
        services: Vec<String>,
        native: bool,
        /// Подключение не проходит, пока не наступит этот момент.
        fails_until: Option<Instant>,
        /// Столько удачных запусков туннель встанет сразу (`tunnel.dll`: «работает», затем 1168 на настройке адресов).
        dies_after_start: u32,
        attempts: Vec<(String, Duration)>,
        notes: Vec<(String, Note)>,
        outside: Vec<String>,
    }

    impl World {
        fn new(desired: &[&str]) -> World {
            World {
                desired: desired.iter().map(|s| s.to_string()).collect(),
                running: vec![],
                services: vec![],
                native: false,
                fails_until: None,
                dies_after_start: 0,
                attempts: vec![],
                notes: vec![],
                outside: vec![],
            }
        }

        /// Один такт ядра: решения надзора и попытки подключения, как в `Core::supervise_tick`.
        fn step(&mut self, r: &mut Retries, t0: Instant, now: Instant, network_changed: bool) {
            let services = self.services.clone();
            let seen = Seen {
                desired: &self.desired,
                running: Some(&self.running),
                pending: &|_| false,
                service_exists: &|t| services.iter().any(|s| s.as_str() == t),
                stop_reason: &|_| Some("код 1168".to_string()),
                native_services: self.native,
                network_changed,
            };
            let tick = r.tick(now, &seen);
            self.notes.extend(tick.notes);
            for t in tick.outside {
                self.desired.retain(|d| *d != t);
                self.outside.push(t);
            }
            for t in tick.due {
                self.attempts.push((t.clone(), now - t0));
                let result = if self.running.contains(&t) || !self.desired.contains(&t) {
                    Ok(false)
                } else if self.fails_until.is_some_and(|u| now < u) {
                    Err("Element not found".to_string())
                } else if self.dies_after_start > 0 {
                    // Служба дошла до «работает» и встала до следующего такта.
                    self.dies_after_start -= 1;
                    Ok(true)
                } else {
                    self.running.push(t.clone());
                    Ok(true)
                };
                if let Some(note) = r.outcome(&t, now, result) {
                    self.notes.push((t, note));
                }
            }
        }

        /// Прогнать такты по секунде от `from` до `to` (секунды с `t0`).
        fn run(&mut self, r: &mut Retries, t0: Instant, from: u64, to: u64) {
            for s in from..to {
                self.step(r, t0, t0 + Duration::from_secs(s), false);
            }
        }

        fn attempt_secs(&self) -> Vec<u64> {
            self.attempts.iter().map(|(_, d)| d.as_secs()).collect()
        }
    }

    fn secs(from: u64, to: u64, step: u64) -> Vec<u64> {
        (from..=to).step_by(step as usize).collect()
    }

    /// Фазы: каждые 10 с до 3 минут, раз в минуту до 10 минут, дальше раз в 10 минут — бессрочно.
    #[test]
    fn schedule_goes_fast_then_every_minute_then_every_ten_minutes_forever() {
        let t0 = Instant::now();
        let mut r = Retries::default();
        let mut w = World::new(&["office"]);
        w.fails_until = Some(t0 + Duration::from_secs(1_000_000));
        w.run(&mut r, t0, 0, 3600);
        let mut expected = secs(0, 170, 10);
        expected.extend(secs(180, 540, 60));
        expected.extend(secs(600, 3000, 600));
        assert_eq!(w.attempt_secs(), expected, "граница 3 минуты — на 180 с, 10 минут — на 600 с");

        // 100 попыток в третьей фазе — расписание не кончается и в журнал ничего не пишет.
        let before = w.notes.len();
        let mut now = t0 + Duration::from_secs(3600);
        let mut slow_attempts = 0;
        for _ in 0..100 * 600 {
            now += TICK;
            let n = w.attempts.len();
            w.step(&mut r, t0, now, false);
            slow_attempts += w.attempts.len() - n;
        }
        assert!(slow_attempts >= 100, "{slow_attempts}");
        assert_eq!(w.notes.len(), before, "третья фаза молчит");
        let view = r.view(now);
        assert!(view["office"].slow && view["office"].next_in_s <= 600, "{view:?}");
    }

    /// Журнал: каждая попытка первых двух фаз, вход в третью — один раз, дальше тишина.
    #[test]
    fn only_first_phases_and_the_entry_into_the_slow_phase_are_logged() {
        let t0 = Instant::now();
        let mut r = Retries::default();
        let mut w = World::new(&["office"]);
        w.fails_until = Some(t0 + Duration::from_secs(1_000_000));
        w.run(&mut r, t0, 0, 3000);
        let failed = w.notes.iter().filter(|(_, n)| matches!(n, Note::Failed { .. })).count();
        assert_eq!(failed, 18 + 7, "18 попыток по 10 с и 7 по минуте");
        let slow: Vec<_> = w.notes.iter().filter(|(_, n)| matches!(n, Note::Slow { .. })).collect();
        assert_eq!(slow, [&("office".to_string(), Note::Slow { error: "Element not found".into() })]);
        assert!(matches!(w.notes.last(), Some((_, Note::Slow { .. }))), "после входа в третью фазу записей нет");
        assert_eq!(w.notes[0].1, Note::Failed { attempt: 1, error: "Element not found".into() });
    }

    #[test]
    fn success_ends_the_schedule_with_one_line_in_any_phase() {
        for (works_at, attempts) in [(25, 4), (400, 23), (1300, 28)] {
            let t0 = Instant::now();
            let mut r = Retries::default();
            let mut w = World::new(&["office"]);
            w.fails_until = Some(t0 + Duration::from_secs(works_at));
            w.run(&mut r, t0, 0, 4000);
            let connected: Vec<_> = w.notes.iter().filter(|(_, n)| matches!(n, Note::Connected { .. })).collect();
            assert_eq!(connected, [&("office".to_string(), Note::Connected { attempts })], "заработал на {works_at} с");
            assert!(r.view(t0).is_empty(), "расписание закончено");
            assert_eq!(w.running, ["office"]);
        }
    }

    /// Живой прогон пропадания питания: запуск дошёл до «работает», и туннель встал на настройке адресов (1168). Это
    /// неудачная попытка с причиной, а не «подключён»; подключён — одна строка, когда туннель проработал `CONFIRM_FOR`.
    #[test]
    fn start_that_dies_right_away_is_a_failed_attempt_and_success_is_logged_once_after_confirmation() {
        let t0 = Instant::now();
        let mut r = Retries::default();
        let mut w = World::new(&["office"]);
        w.dies_after_start = 1;
        w.run(&mut r, t0, 0, 9);
        assert_eq!(w.attempt_secs(), [0]);
        assert_eq!(w.notes, [("office".to_string(), Note::Failed { attempt: 1, error: "код 1168".into() })], "не «подключён»");
        assert_eq!(r.view(t0 + Duration::from_secs(9))["office"].last_error, "код 1168");

        // Вторая попытка (через 10 с после того, как туннель встал) поднимает его: до `CONFIRM_FOR` в журнале ничего,
        // затем ровно одна строка.
        w.run(&mut r, t0, 9, 11 + CONFIRM_FOR.as_secs());
        assert_eq!(w.attempt_secs(), [0, 11]);
        assert_eq!(w.notes.len(), 1, "ещё не подтверждён: {:?}", w.notes);
        assert!(r.view(t0)["office"].attempt == 2, "под надзором до подтверждения");
        w.run(&mut r, t0, 11 + CONFIRM_FOR.as_secs(), 60);
        assert_eq!(w.notes[1..], [("office".to_string(), Note::Connected { attempts: 2 })]);
        assert_eq!(w.attempt_secs(), [0, 11], "работающий не подключается повторно");
        assert!(r.view(t0).is_empty());
    }

    #[test]
    fn network_change_triggers_an_attempt_at_once_in_every_phase() {
        let t0 = Instant::now();
        let mut r = Retries::default();
        let mut w = World::new(&["office"]);
        w.fails_until = Some(t0 + Duration::from_secs(1_000_000));
        w.run(&mut r, t0, 0, 5);
        w.step(&mut r, t0, t0 + Duration::from_secs(5), true);
        assert_eq!(w.attempt_secs(), [0, 5], "попытка сразу, не через 10 с");
        w.step(&mut r, t0, t0 + Duration::from_secs(6), true);
        assert_eq!(w.attempt_secs(), [0, 5], "пачка уведомлений — не чаще раза в 3 с");

        // Третья фаза: смена сети — попытка сразу и без записи в журнал; подключился — одна строка.
        w.run(&mut r, t0, 6, 1500);
        let notes = w.notes.len();
        let n = w.attempts.len();
        w.step(&mut r, t0, t0 + Duration::from_secs(1500), true);
        assert_eq!(w.attempts.len(), n + 1);
        assert_eq!(w.notes.len(), notes, "в третьей фазе не пишется");
        w.fails_until = None;
        w.step(&mut r, t0, t0 + Duration::from_secs(1504), true);
        w.run(&mut r, t0, 1505, 1510);
        assert!(matches!(w.notes.last(), Some((_, Note::Connected { .. }))));
    }

    #[test]
    fn user_disconnect_stops_retries_and_retry_restarts_from_the_fast_phase() {
        let t0 = Instant::now();
        let mut r = Retries::default();
        let mut w = World::new(&["office", "home"]);
        w.fails_until = Some(t0 + Duration::from_secs(1_000_000));
        w.run(&mut r, t0, 0, 700);
        assert!(r.view(t0 + Duration::from_secs(700))["office"].slow);

        // Отключил пользователь: набор без туннеля, надзор снят.
        w.desired.retain(|t| t != "home");
        r.forget("home");
        let n = w.attempts.iter().filter(|(t, _)| t == "home").count();
        w.run(&mut r, t0, 700, 2000);
        assert_eq!(w.attempts.iter().filter(|(t, _)| t == "home").count(), n, "отключённый пользователем не поднимается");
        assert!(!r.view(t0).contains_key("home"));

        // «Повторить»: попытка сразу, затем снова каждые 10 с и с записями в журнал.
        r.restart("office", t0 + Duration::from_secs(2000));
        let from = w.attempts.len();
        let notes = w.notes.len();
        w.run(&mut r, t0, 2000, 2031);
        let again: Vec<u64> = w.attempts[from..].iter().map(|(_, d)| d.as_secs()).collect();
        assert_eq!(again, [2000, 2010, 2020, 2030]);
        assert_eq!(w.notes.len(), notes + 4, "первая фаза снова пишется");
        assert!(!r.view(t0 + Duration::from_secs(2031))["office"].slow);
    }

    /// Восстановление после запуска ядра — первый такт того же надзора.
    #[test]
    fn first_round_waits_for_existing_services_and_brings_missing_ones_at_once() {
        let t0 = Instant::now();
        let mut r = Retries::default();
        let mut w = World::new(&["has-service", "no-service", "up"]);
        w.services = vec!["has-service".into(), "up".into()];
        w.running = vec!["up".into()];
        w.run(&mut r, t0, 0, 11);
        assert_eq!(w.attempts, [("no-service".to_string(), Duration::ZERO), ("has-service".to_string(), Duration::from_secs(10))]);
        assert!(!w.attempts.iter().any(|(t, _)| t == "up"), "работающий не трогается");

        // Служба поднялась сама до попытки — подключать нечего; в журнале одна строка «подключён», когда проработал.
        let mut r = Retries::default();
        let mut w = World::new(&["office"]);
        w.services = vec!["office".into()];
        w.step(&mut r, t0, t0, false);
        w.running.push("office".into());
        w.run(&mut r, t0, 1, 30);
        assert!(w.attempts.is_empty() && r.view(t0).is_empty());
        assert_eq!(w.notes, [("office".to_string(), Note::Connected { attempts: 0 })]);
    }

    #[test]
    fn tunnel_that_dropped_by_itself_is_retried_but_one_removed_in_native_window_is_not() {
        let t0 = Instant::now();
        let mut r = Retries::default();
        let mut w = World::new(&["office", "home"]);
        w.native = true;
        w.services = vec!["office".into(), "home".into()];
        w.running = vec!["office".into(), "home".into()];
        w.run(&mut r, t0, 0, 5);
        // «office» упал (служба осталась), «home» отключили в окне AmneziaWG (службы нет).
        w.running.clear();
        w.services.retain(|s| s != "home");
        w.run(&mut r, t0, 5, 20);
        assert_eq!(w.attempts, [("office".to_string(), Duration::from_secs(15))], "упавший — через 10 с");
        assert_eq!(w.outside, ["home"]);
        assert!(!w.desired.contains(&"home".to_string()));
    }

    #[test]
    fn unreadable_running_list_takes_no_new_decisions() {
        let t0 = Instant::now();
        let mut r = Retries::default();
        let desired = vec!["a".to_string()];
        let seen = Seen {
            desired: &desired,
            running: None,
            pending: &|_| false,
            service_exists: &|_| panic!("вслепую не решаем"),
            stop_reason: &|_| panic!("вслепую не решаем"),
            native_services: true,
            network_changed: false,
        };
        assert_eq!(r.tick(t0, &seen), Tick::default());
        assert!(r.view(t0).is_empty());
    }

    #[test]
    fn tunnel_switching_by_user_command_is_not_touched() {
        let t0 = Instant::now();
        let mut r = Retries::default();
        let desired = vec!["a".to_string()];
        let seen = Seen { desired: &desired, running: Some(&[]), pending: &|_| true, service_exists: &|_| false, stop_reason: &|_| None, native_services: false, network_changed: false };
        assert!(r.tick(t0, &seen).due.is_empty());
        assert!(r.view(t0).is_empty());
        // Снятый пользователем надзор: исход попытки, начатой до его команды, ничего не записывает.
        r.restart("a", t0);
        r.forget("a");
        assert_eq!(r.outcome("a", t0, Err("x".into())), None);
    }
}
