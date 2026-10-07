//! Переподключение туннелей — один хозяин повторов на всё ядро. Желаемый туннель (`Config::tunnels`), который не
//! работает — не поднялся при загрузке (сеть бывает готова только через минуту-две) или упал сам, без команды
//! пользователя, — ядро поднимает снова обычным переключением, по расписанию: первые 3 минуты — каждые 10 с,
//! до 10 минут — раз в минуту, дальше — раз в 10 минут бессрочно (владелец может быть далеко от машины). Перезапуск
//! служб туннелей диспетчером Windows при сбое отключён (`engine::clear_restart_on_failure`): два механизма
//! пересоздавали бы одну службу наперегонки. Смена сети (`netwatch`) — попытка сразу, в любой фазе.
//!
//! Здесь только решения: что делать на очередном такте и что писать в журнал. Время — параметром, хост — снаружи
//! (`server::Core::supervise_tick`): расписание проверяется тестами без часов и служб.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use super::deadwatch::{DeadTick, DeadWatch, Verdict};
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
/// Дольше этого туннель не держится вне надзора одной арендой (`Retries::hold`), сколько бы ни попросили: ошибка
/// в держателе не должна оставить желаемый туннель без переподключения на часы.
pub(super) const MAX_LEASE: Duration = Duration::from_secs(60 * 60);
/// Столько после истечения аренды пропажа службы туннеля режима 1 не считается отключением в окне AmneziaWG: держатель
/// пропал или завис, и установщик мог убрать службу уже после конца аренды. Туннель, который так и не заработал,
/// остаётся «вернуть» и дольше — пока не подключится (`Retries::after_lease`).
pub(super) const AFTER_LEASE: Duration = Duration::from_secs(30 * 60);
/// Неявная аренда на время чужого установщика Windows (режим 1, служба желаемого туннеля пропала, пока идёт MSI не
/// из наших обновлений — тот берёт аренду сам): продлевается каждый такт, пока установщик работает, и истекает через
/// столько после его конца — тогда туннель возвращается, как после обычной истёкшей аренды.
pub(super) const INSTALLER_GRACE: Duration = Duration::from_secs(15);

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
    /// У AmneziaWG (режим 1) ещё есть конфиг туннеля; не узнать — «есть»: из набора выводится только то, чего нет точно.
    pub config_exists: &'a dyn Fn(&str) -> bool,
    /// Почему служба туннеля остановилась (коды завершения), для журнала; `None` — не узнать.
    pub stop_reason: &'a dyn Fn(&str) -> Option<String>,
    /// Службы туннелей принадлежат AmneziaWG (режим 1): их может удалить его родное окно.
    pub native_services: bool,
    /// Идёт установщик Windows (чужой MSI может убирать службы туннелей режима 1); спрашивается только в режиме 1.
    pub installer_running: &'a dyn Fn() -> bool,
    /// С прошлого такта менялась сеть.
    pub network_changed: bool,
}

/// Аренда: до какого момента и чья — держателя (`HoldNative`) или неявная, на время чужого установщика.
#[derive(Debug, Clone, Copy)]
struct Hold {
    until: Instant,
    installer: bool,
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
    /// Аренда истекла сама (держатель не вернул туннели): надзор над ними снова идёт.
    pub expired: Vec<String>,
    /// Аренда истекла, а у AmneziaWG туннеля больше нет (конфиг удалён): вернуть нечего, выходят из желаемого набора.
    pub gone: Vec<String>,
    /// Служба пропала, пока идёт чужой установщик: взяты в неявную аренду до его конца (на этом такте — впервые).
    pub installer_held: Vec<String>,
    /// Чужой установщик закончил (неявная аренда истекла): надзор над ними снова идёт, попытка сразу.
    pub installer_done: Vec<String>,
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
    /// Туннели, снятые с надзора на время чужой работы над ними (установщик AmneziaWG), и когда аренда истекает.
    holds: BTreeMap<String, Hold>,
    /// Работавшие на прошлом такте (список работающих читался). Пропажа службы значит «отключён в окне AmneziaWG»
    /// только для того, кого ядро само видело работающим: туннель, который не поднялся по команде или при перезапуске
    /// мёртвого, службы не имеет по нашей вине, а не по воле пользователя.
    up: BTreeSet<String>,
    /// Желаемые туннели, чья аренда истекла сама, и когда: их надо вернуть, а не считать отключёнными в окне AmneziaWG.
    /// Пометка снимается, когда туннель работает спустя `AFTER_LEASE`, выходит из набора или его берёт пользователь.
    after_lease: BTreeMap<String, Instant>,
    /// Первый такт после запуска ядра уже был.
    started: bool,
    /// Мёртвые туннели: служба работает, связи нет (`deadwatch`). Здесь же, под одним замком: аренда, команда
    /// пользователя и смена режима снимают и его счёт.
    pub(super) dead: DeadWatch,
}

impl Retries {
    /// Очередной такт: кого взять под надзор, кого отпустить и кого подключать сейчас.
    pub(super) fn tick(&mut self, now: Instant, seen: &Seen) -> Tick {
        let (expired, installer_done) = self.expire(now, seen);
        let mut tick = Tick { expired, installer_done, ..Tick::default() };
        if let Some(running) = seen.running {
            // Вышел из набора (отключён пользователем, удалён) — надзор не нужен; заработавший снимается с надзора,
            // только проработав `CONFIRM_FOR` (`watch`).
            self.tracks.retain(|t, _| seen.desired.contains(t));
            self.after_lease.retain(|t, at| seen.desired.contains(t) && (now < *at + AFTER_LEASE || !running.contains(t)));
            tick.gone = self.gone_after_lease(running, seen);
            tick.notes = self.watch(now, running, seen.stop_reason);
            tick.installer_held = self.hold_for_installer(now, running, seen);
            let fresh: Vec<String> = seen
                .desired
                .iter()
                .filter(|t| !running.contains(*t) && !self.tracks.contains_key(*t) && !self.is_held(t) && !(seen.pending)(t) && !tick.gone.contains(*t))
                .cloned()
                .collect();
            for t in fresh {
                if self.after_lease.contains_key(&t) {
                    // Аренда кончилась, а установщик убрал службу уже после неё (держатель завис или пропал): это не
                    // воля пользователя, туннель подключается заново — `connect` ставит службу из конфига сам.
                    self.tracks.insert(t, Track::new(now, Duration::ZERO));
                    continue;
                }
                let exists = (seen.service_exists)(&t);
                if self.started && seen.native_services && !exists && self.up.contains(&t) {
                    // Работал и пропал вместе со службой — его отключили в окне AmneziaWG: поднимать обратно значило
                    // бы спорить с пользователем. Только о том, кого ядро само видело работающим: не поднявшийся по
                    // команде или при перезапуске мёртвого (`supervise_failed`) без службы по нашей вине.
                    tick.outside.push(t);
                    continue;
                }
                // Первый такт — восстановление после запуска ядра: служба есть — Windows как раз может поднимать её
                // сама, подождать; службы нет — ждать нечего. Позже — упал сам: первая попытка через обычный шаг.
                let first_in = if !self.started && !exists { Duration::ZERO } else { FAST_EVERY };
                self.tracks.insert(t, Track::new(now, first_in));
            }
            self.up = running.iter().cloned().collect();
        }
        self.started = true;
        if seen.network_changed {
            // Чужой установщик между «служба остановлена» и «служба удалена»: туннель без попыток, чья служба ещё
            // есть, в аренду не взят (`hold_for_installer`), а смена сети от пропавшего адаптера подключала бы его
            // прямо внутри транзакции MSI — та самая гонка. Ему остаётся обычный первый шаг; попытка уже была —
            // установщик не при чём.
            let installer = seen.native_services && self.tracks.values().any(|t| t.attempt == 0) && (seen.installer_running)();
            for track in self.tracks.values_mut().filter(|t| !(installer && t.attempt == 0)) {
                let earliest = track.last_try.map_or(now, |at| at + NETWORK_MIN_GAP);
                track.next_at = track.next_at.min(earliest.max(now));
            }
        }
        tick.due = self
            .tracks
            .iter()
            .filter(|(t, tr)| tr.next_at <= now && tr.up_since.is_none() && !self.is_held(t) && !(seen.pending)(t))
            .map(|(t, _)| t.clone())
            .collect();
        tick
    }

    /// Истёкшие аренды снимаются. Желаемый неработающий туннель сразу получает попытку, а все желаемые из истёкших
    /// помечаются «вернуть» (`after_lease`): иначе в режиме 1 туннель, чью службу убрал установщик (до конца аренды
    /// или, зависнув, после), такт счёл бы отключённым в окне AmneziaWG и вывел бы из набора — VPN не вернулся бы.
    /// Возвращает истёкшие аренды держателей и истёкшие неявные (чужой установщик закончил) — отдельно, для журнала.
    fn expire(&mut self, now: Instant, seen: &Seen) -> (Vec<String>, Vec<String>) {
        let expired: Vec<(String, bool)> = self.holds.iter().filter(|(_, h)| h.until <= now).map(|(t, h)| (t.clone(), h.installer)).collect();
        for (t, _) in &expired {
            self.holds.remove(t);
            if !seen.desired.contains(t) {
                continue;
            }
            self.after_lease.insert(t.clone(), now);
            if !seen.running.is_some_and(|r| r.contains(t)) {
                self.restart(t, now);
            }
        }
        let (installer, holder): (Vec<_>, Vec<_>) = expired.into_iter().partition(|(_, installer)| *installer);
        (holder.into_iter().map(|(t, _)| t).collect(), installer.into_iter().map(|(t, _)| t).collect())
    }

    /// Идёт чужой установщик Windows (наш берёт аренду сам — `hold`), а служба желаемого туннеля режима 1 пропала:
    /// это не воля пользователя, но и ставить службу заново посреди установки нельзя — та же гонка с MSI, ради которой
    /// есть аренда. Такой туннель берётся в неявную аренду на `INSTALLER_GRACE`; она продлевается каждый такт, пока
    /// установщик работает, и истекает после его конца — тогда туннель возвращается (`expire`: пометка «вернуть»,
    /// попытка сразу). Туннель, упавший сам при живой службе, идёт по обычному расписанию: чужая установка не повод
    /// задерживать его подъём. Возвращает взятых впервые.
    fn hold_for_installer(&mut self, now: Instant, running: &[String], seen: &Seen) -> Vec<String> {
        if !seen.native_services || !(seen.installer_running)() {
            return Vec::new();
        }
        let until = now + INSTALLER_GRACE;
        for hold in self.holds.values_mut().filter(|h| h.installer) {
            hold.until = until;
        }
        let mut taken = Vec::new();
        for t in seen.desired {
            if running.contains(t) || self.is_held(t) || (seen.pending)(t) || (seen.service_exists)(t) {
                continue;
            }
            self.tracks.remove(t);
            self.after_lease.remove(t);
            self.dead.forget(t);
            self.holds.insert(t.clone(), Hold { until, installer: true });
            taken.push(t.clone());
        }
        taken
    }

    /// Помеченные «вернуть», которых у AmneziaWG больше нет (режим 1, туннель не работает, конфига нет): подключать
    /// нечего, надзор и пометка с них снимаются, ядро выводит их из набора с записью в журнал.
    fn gone_after_lease(&mut self, running: &[String], seen: &Seen) -> Vec<String> {
        if !seen.native_services {
            return Vec::new();
        }
        let gone: Vec<String> =
            self.after_lease.keys().filter(|t| !running.contains(*t) && !(seen.pending)(t) && !(seen.config_exists)(t)).cloned().collect();
        for t in &gone {
            self.after_lease.remove(t);
            self.tracks.remove(t);
        }
        gone
    }

    /// Снять туннели с надзора на `lease` (не дольше `MAX_LEASE`): их не подключают и не выводят из набора, пока
    /// аренду не вернут (`release`) или она не истечёт сама — тогда надзор идёт снова, даже если держатель пропал.
    /// Повторная аренда продлевает срок.
    pub(super) fn hold(&mut self, tunnels: &[String], now: Instant, lease: Duration) {
        let until = now + lease.min(MAX_LEASE);
        for t in tunnels {
            self.tracks.remove(t);
            self.after_lease.remove(t);
            self.dead.forget(t);
            // Поверх неявной аренды установщика: держатель объявился — срок и журнал теперь его.
            self.holds.insert(t.clone(), Hold { until, installer: false });
        }
    }

    /// Вернуть аренду; возвращает туннели, которые действительно были сняты с надзора.
    pub(super) fn release(&mut self, tunnels: &[String]) -> Vec<String> {
        tunnels.iter().filter(|t| self.holds.remove(*t).is_some()).cloned().collect()
    }

    pub(super) fn is_held(&self, name: &str) -> bool {
        self.holds.contains_key(name)
    }

    /// Такт сторожа мёртвых туннелей. Не трогает туннели под арендой, под надзором повторов (он их и поднимает) и
    /// переключаемые по команде (`pending`).
    pub(super) fn dead_tick(&mut self, now: Instant, desired: &[String], verdicts: &[(String, Verdict)], pending: &dyn Fn(&str) -> bool) -> DeadTick {
        let (holds, tracks) = (&self.holds, &self.tracks);
        self.dead.tick(now, desired, verdicts, &|t| holds.contains_key(t) || tracks.contains_key(t) || pending(t))
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
        self.after_lease.remove(name);
        self.dead.forget(name);
    }

    /// Смена режима или отключение всего: надзор снимается целиком.
    pub(super) fn clear(&mut self) {
        self.tracks.clear();
        self.after_lease.clear();
        self.dead.clear();
    }

    /// «Повторить»: расписание с начала, попытка сразу.
    pub(super) fn restart(&mut self, name: &str, now: Instant) {
        self.tracks.insert(name.to_string(), Track::new(now, Duration::ZERO));
    }

    /// Наше переключение желаемого туннеля не удалось (команда пользователя или перезапуск мёртвого): дальше его
    /// поднимает надзор по обычному расписанию, первая попытка через `FAST_EVERY`. Без записи в журнал — ошибку уже
    /// записал вызвавший. Иначе в режиме 1 туннель, оставшийся без службы по нашей вине (`/uninstalltunnelservice`
    /// прошёл, `/installtunnelservice` — нет), следующий такт счёл бы отключённым в окне AmneziaWG и вывел из набора.
    pub(super) fn supervise_failed(&mut self, name: &str, now: Instant, error: String) {
        let mut track = Track::new(now, FAST_EVERY);
        track.last_error = error;
        self.tracks.insert(name.to_string(), track);
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
        /// Конфиги туннелей у AmneziaWG (режим 1); без конфига подключить нечем.
        configs: Vec<String>,
        native: bool,
        /// Идёт чужой установщик Windows.
        installer: bool,
        /// Подключение не проходит, пока не наступит этот момент.
        fails_until: Option<Instant>,
        /// Столько удачных запусков туннель встанет сразу (`tunnel.dll`: «работает», затем 1168 на настройке адресов).
        dies_after_start: u32,
        attempts: Vec<(String, Duration)>,
        notes: Vec<(String, Note)>,
        outside: Vec<String>,
        gone: Vec<String>,
        installer_held: Vec<String>,
        installer_done: Vec<String>,
    }

    impl World {
        fn new(desired: &[&str]) -> World {
            World {
                desired: desired.iter().map(|s| s.to_string()).collect(),
                running: vec![],
                services: vec![],
                configs: desired.iter().map(|s| s.to_string()).collect(),
                native: false,
                installer: false,
                fails_until: None,
                dies_after_start: 0,
                attempts: vec![],
                notes: vec![],
                outside: vec![],
                gone: vec![],
                installer_held: vec![],
                installer_done: vec![],
            }
        }

        /// Один такт ядра: решения надзора и попытки подключения, как в `Core::supervise_tick`.
        fn step(&mut self, r: &mut Retries, t0: Instant, now: Instant, network_changed: bool) {
            let services = self.services.clone();
            let configs = self.configs.clone();
            let installer = self.installer;
            let seen = Seen {
                desired: &self.desired,
                running: Some(&self.running),
                pending: &|_| false,
                service_exists: &|t| services.iter().any(|s| s.as_str() == t),
                config_exists: &|t| configs.iter().any(|s| s.as_str() == t),
                stop_reason: &|_| Some("код 1168".to_string()),
                native_services: self.native,
                installer_running: &|| installer,
                network_changed,
            };
            let tick = r.tick(now, &seen);
            self.notes.extend(tick.notes);
            self.installer_held.extend(tick.installer_held);
            self.installer_done.extend(tick.installer_done);
            for t in tick.gone {
                self.desired.retain(|d| *d != t);
                self.gone.push(t);
            }
            for t in tick.outside {
                self.desired.retain(|d| *d != t);
                self.outside.push(t);
            }
            for t in tick.due {
                self.attempts.push((t.clone(), now - t0));
                let result = if self.running.contains(&t) || !self.desired.contains(&t) {
                    Ok(false)
                } else if !self.configs.contains(&t) {
                    Err("no config".to_string())
                } else if self.fails_until.is_some_and(|u| now < u) {
                    Err("Element not found".to_string())
                } else if self.dies_after_start > 0 {
                    // Служба дошла до «работает» и встала до следующего такта.
                    self.dies_after_start -= 1;
                    Ok(true)
                } else {
                    self.running.push(t.clone());
                    // `connect` режима 1 ставит службу из конфига.
                    if !self.services.contains(&t) {
                        self.services.push(t.clone());
                    }
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
            config_exists: &|_| panic!("вслепую не решаем"),
            stop_reason: &|_| panic!("вслепую не решаем"),
            native_services: true,
            installer_running: &|| false,
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
        let seen = Seen { desired: &desired, running: Some(&[]), pending: &|_| true, service_exists: &|_| false, config_exists: &|_| true, stop_reason: &|_| None, native_services: false, installer_running: &|| false, network_changed: false };
        assert!(r.tick(t0, &seen).due.is_empty());
        assert!(r.view(t0).is_empty());
        // Снятый пользователем надзор: исход попытки, начатой до его команды, ничего не записывает.
        r.restart("a", t0);
        r.forget("a");
        assert_eq!(r.outcome("a", t0, Err("x".into())), None);
    }

    /// Установщик AmneziaWG держит туннель (режим 1) и убирает его службу. Пока аренда идёт, надзор туннель не
    /// подключает и из набора не выводит; держатель пропал — аренда истекает сама, и попытка идёт сразу.
    #[test]
    fn lease_expiry_resumes_supervision_even_if_the_holder_is_gone() {
        let t0 = Instant::now();
        let mut r = Retries::default();
        let mut w = World::new(&["office"]);
        w.native = true;
        w.running = vec!["office".into()];
        w.services = vec!["office".into()];
        w.run(&mut r, t0, 0, 10);
        r.hold(&["office".to_string()], t0 + Duration::from_secs(10), Duration::from_secs(900));
        assert!(r.is_held("office"));
        // MSI удалил службу и остановил туннель.
        w.running.clear();
        w.services.clear();
        w.run(&mut r, t0, 10, 910);
        assert!(w.attempts.is_empty(), "под арендой попыток нет: {:?}", w.attempts);
        assert!(w.outside.is_empty(), "под арендой туннель не выводится из набора");
        assert!(r.view(t0 + Duration::from_secs(909)).is_empty());

        let seen = Seen {
            desired: &w.desired,
            running: Some(&[]),
            pending: &|_| false,
            service_exists: &|_| false,
            config_exists: &|_| true,
            stop_reason: &|_| None,
            native_services: true,
            installer_running: &|| false,
            network_changed: false,
        };
        let tick = r.tick(t0 + Duration::from_secs(910), &seen);
        assert_eq!(tick.expired, ["office"]);
        assert_eq!(tick.due, ["office"], "аренда истекла — попытка сразу, а не вывод из набора");
        assert!(tick.outside.is_empty());
        assert!(!r.is_held("office"));
    }

    #[test]
    fn released_tunnels_are_reported_once_and_lease_is_capped() {
        let t0 = Instant::now();
        let mut r = Retries::default();
        let names = ["a".to_string(), "b".to_string()];
        r.hold(&names, t0, Duration::from_secs(u32::MAX.into()));
        assert_eq!(r.release(&["a".to_string(), "c".to_string()]), ["a"], "не взятый в аренду не возвращается");
        assert!(r.release(&["a".to_string()]).is_empty(), "вторая отдача той же аренды — пусто");
        let desired = vec!["b".to_string()];
        let seen = Seen { desired: &desired, running: Some(&[]), pending: &|_| false, service_exists: &|_| true, config_exists: &|_| true, stop_reason: &|_| None, native_services: false, installer_running: &|| false, network_changed: false };
        assert!(r.tick(t0 + MAX_LEASE - TICK, &seen).due.is_empty(), "аренда ещё идёт");
        let tick = r.tick(t0 + MAX_LEASE, &seen);
        assert_eq!((tick.expired, tick.due), (vec!["b".to_string()], vec!["b".to_string()]), "аренда не длиннее MAX_LEASE");
    }

    /// «Повторить» по туннелю под арендой не прорывает её: попытка ждёт возврата.
    #[test]
    fn held_tunnel_is_not_due_even_after_restart() {
        let t0 = Instant::now();
        let mut r = Retries::default();
        r.hold(&["a".to_string()], t0, Duration::from_secs(60));
        r.restart("a", t0);
        let desired = vec!["a".to_string()];
        let seen = Seen { desired: &desired, running: Some(&[]), pending: &|_| false, service_exists: &|_| true, config_exists: &|_| true, stop_reason: &|_| None, native_services: false, installer_running: &|| false, network_changed: false };
        assert!(r.tick(t0, &seen).due.is_empty());
    }

    /// Держатель аренды пропал посреди MSI (режим 1): аренда истекла, службы туннеля нет, AmneziaWG ещё ставится.
    /// Туннель остаётся желаемым и переподключается по обычному расписанию, а не выводится из набора как отключённый
    /// в окне AmneziaWG; когда установка закончилась — подключён.
    #[test]
    fn expired_lease_with_the_service_gone_keeps_the_tunnel_desired_and_reconnects_it() {
        let t0 = Instant::now();
        let mut r = Retries::default();
        let mut w = World::new(&["office"]);
        w.native = true;
        w.running = vec!["office".into()];
        w.services = vec!["office".into()];
        w.run(&mut r, t0, 0, 10);
        r.hold(&["office".to_string()], t0 + Duration::from_secs(10), Duration::from_secs(60));
        w.running.clear();
        w.services.clear();
        // AmneziaWG ещё не встал: подключение не проходит до 100 с.
        w.fails_until = Some(t0 + Duration::from_secs(100));
        w.run(&mut r, t0, 10, 95);
        assert!(w.outside.is_empty() && w.gone.is_empty(), "истёкшая аренда — не отключение в окне AmneziaWG");
        assert_eq!(w.desired, ["office"]);
        assert_eq!(w.attempt_secs(), [70, 80, 90], "попытка сразу по истечении, дальше каждые 10 с");
        w.run(&mut r, t0, 95, 120);
        assert_eq!(w.attempt_secs(), [70, 80, 90, 100]);
        assert_eq!(w.running, ["office"]);
        assert!(matches!(w.notes.last(), Some((t, Note::Connected { attempts: 4 })) if t == "office"), "{:?}", w.notes);
        assert!(r.view(t0).is_empty());
    }

    /// MSI завис дольше аренды: туннель ещё работал, когда она истекла, а службу установщик убрал позже. Это тоже не
    /// воля пользователя — попытка сразу. Спустя `AFTER_LEASE` работы пропажа службы снова значит «отключён в окне».
    #[test]
    fn service_removed_after_the_lease_expired_is_reconnected_not_dropped() {
        let t0 = Instant::now();
        let mut r = Retries::default();
        let mut w = World::new(&["office"]);
        w.native = true;
        w.running = vec!["office".into()];
        w.services = vec!["office".into()];
        w.run(&mut r, t0, 0, 10);
        r.hold(&["office".to_string()], t0 + Duration::from_secs(10), Duration::from_secs(60));
        w.run(&mut r, t0, 10, 200);
        assert!(w.attempts.is_empty(), "работающий не трогается");
        w.running.clear();
        w.services.clear();
        w.run(&mut r, t0, 200, 210);
        assert!(w.outside.is_empty());
        assert_eq!(w.attempt_secs(), [200]);
        assert_eq!(w.running, ["office"]);

        // Проработал `AFTER_LEASE` после истечения — пометка снята, обычное правило режима 1 снова в силе.
        let later = 70 + AFTER_LEASE.as_secs();
        w.run(&mut r, t0, 210, later + 1);
        w.running.clear();
        w.services.clear();
        w.run(&mut r, t0, later + 1, later + 3);
        assert_eq!(w.outside, ["office"]);
        assert_eq!(w.attempt_secs(), [200]);
    }

    /// Аренда истекла, а у AmneziaWG туннеля больше нет (конфиг удалён): подключать нечего — туннель выходит из набора
    /// (ядро пишет это в журнал), попыток больше нет.
    #[test]
    fn expired_lease_of_a_tunnel_amneziawg_no_longer_has_drops_it() {
        let t0 = Instant::now();
        let mut r = Retries::default();
        let mut w = World::new(&["office", "home"]);
        w.native = true;
        w.running = vec!["office".into(), "home".into()];
        w.services = w.running.clone();
        w.run(&mut r, t0, 0, 10);
        r.hold(&w.desired.clone(), t0 + Duration::from_secs(10), Duration::from_secs(60));
        w.running.clear();
        w.services.clear();
        w.configs.retain(|c| c != "home");
        w.run(&mut r, t0, 10, 200);
        assert_eq!(w.gone, ["home"]);
        assert!(w.outside.is_empty());
        assert_eq!(w.desired, ["office"]);
        assert!(!w.attempts.iter().any(|(t, _)| t == "home"), "{:?}", w.attempts);
        assert_eq!(w.running, ["office"]);
        assert!(r.view(t0).is_empty());
    }

    /// Пометка «вернуть» не переживает команду пользователя и новую аренду: отключение в окне AmneziaWG после них —
    /// снова отключение.
    #[test]
    fn user_command_ends_the_after_lease_mark() {
        let t0 = Instant::now();
        let mut r = Retries::default();
        let mut w = World::new(&["office"]);
        w.native = true;
        w.running = vec!["office".into()];
        w.services = vec!["office".into()];
        w.run(&mut r, t0, 0, 10);
        r.hold(&["office".to_string()], t0 + Duration::from_secs(10), Duration::from_secs(60));
        w.run(&mut r, t0, 10, 80);
        r.forget("office");
        w.running.clear();
        w.services.clear();
        w.run(&mut r, t0, 80, 82);
        assert_eq!(w.outside, ["office"]);
        assert!(w.attempts.is_empty());
    }

    /// Режим 1: наше переключение сорвалось и оставило туннель без службы (`/uninstalltunnelservice` прошёл,
    /// `/installtunnelservice` — нет) — перезапуск мёртвого или команда пользователя. Это не отключение в окне
    /// AmneziaWG: туннель остаётся желаемым и поднимается по расписанию, а не выходит из набора.
    #[test]
    fn failed_switch_of_our_own_keeps_the_tunnel_desired_and_supervised() {
        let t0 = Instant::now();
        let mut r = Retries::default();
        let mut w = World::new(&["office"]);
        w.native = true;
        w.running = vec!["office".into()];
        w.services = vec!["office".into()];
        w.run(&mut r, t0, 0, 5);
        // Перезапуск мёртвого на 5-й секунде: служба снята, новая не встала; AmneziaWG не даёт поставить её до 20 с.
        w.running.clear();
        w.services.clear();
        r.supervise_failed("office", t0 + Duration::from_secs(5), "install failed".into());
        w.fails_until = Some(t0 + Duration::from_secs(20));
        w.run(&mut r, t0, 5, 40);
        assert!(w.outside.is_empty(), "не отключение в окне AmneziaWG: {:?}", w.outside);
        assert_eq!(w.desired, ["office"]);
        assert_eq!(w.attempt_secs(), [15, 25], "первая попытка через 10 с после срыва, дальше по расписанию");
        assert_eq!(w.notes[0], ("office".to_string(), Note::Failed { attempt: 1, error: "Element not found".into() }), "сам срыв в журнал не пишется — его записал вызвавший");
        assert_eq!(w.running, ["office"]);
        assert!(matches!(w.notes.last(), Some((_, Note::Connected { attempts: 2 }))), "{:?}", w.notes);

        // Команда пользователя: подключить новый туннель; `/installtunnelservice` не прошёл, службы нет.
        let mut r = Retries::default();
        let mut w = World::new(&[]);
        w.native = true;
        w.run(&mut r, t0, 0, 3);
        w.desired.push("lab".into());
        w.configs.push("lab".into());
        r.forget("lab");
        r.supervise_failed("lab", t0 + Duration::from_secs(3), "no config".into());
        w.run(&mut r, t0, 3, 14);
        assert!(w.outside.is_empty(), "{:?}", w.outside);
        assert_eq!(w.attempt_secs(), [13]);
        assert_eq!(w.running, ["lab"]);
    }

    /// Пропажа службы значит «отключён в окне AmneziaWG» только для туннеля, который ядро само видело работающим:
    /// желаемый без службы, которого оно работающим не видело (команда над ним снялась, а след не оставила), берётся
    /// под надзор, а не выводится из набора.
    #[test]
    fn tunnel_the_core_never_saw_running_is_not_taken_for_removed_in_the_native_window() {
        let t0 = Instant::now();
        let mut r = Retries::default();
        let mut w = World::new(&["office"]);
        w.native = true;
        w.fails_until = Some(t0 + Duration::from_secs(1_000_000));
        w.run(&mut r, t0, 0, 3);
        assert_eq!(w.attempt_secs(), [0]);
        r.forget("office");
        w.run(&mut r, t0, 3, 20);
        assert!(w.outside.is_empty(), "{:?}", w.outside);
        assert_eq!(w.attempt_secs(), [0, 13], "снова под надзором: попытка через обычный шаг");
    }

    /// Чужой установщик Windows (MSI AmneziaWG не из наших обновлений) убирает службы туннелей режима 1. Пока он
    /// идёт, туннели без службы не выводятся из набора и не ставятся заново (гонка с MSI); упавший сам при живой службе
    /// поднимается по обычному расписанию. Установщик закончил — спустя `INSTALLER_GRACE` оба возвращаются сразу.
    #[test]
    fn services_removed_by_a_foreign_installer_are_held_until_it_finishes_then_reconnected() {
        let t0 = Instant::now();
        let mut r = Retries::default();
        let mut w = World::new(&["office", "home", "lab"]);
        w.native = true;
        w.running = vec!["office".into(), "home".into(), "lab".into()];
        w.services = w.running.clone();
        w.run(&mut r, t0, 0, 5);
        // MSI работает 2 минуты и убирает службы «office» и «home»; «lab» упал сам, его служба на месте.
        w.installer = true;
        w.running.clear();
        w.services.retain(|s| s == "lab");
        w.run(&mut r, t0, 5, 125);
        assert!(w.outside.is_empty(), "{:?}", w.outside);
        assert_eq!(w.installer_held, ["office", "home"]);
        assert_eq!(w.attempts, [("lab".to_string(), Duration::from_secs(15))], "упавший сам при живой службе чужой установки не ждёт");
        assert_eq!(w.running, ["lab"]);
        assert!(r.is_held("office") && r.is_held("home") && !r.is_held("lab"));
        assert_eq!(w.desired, ["office", "home", "lab"]);

        let done_at = 124 + INSTALLER_GRACE.as_secs();
        w.installer = false;
        w.run(&mut r, t0, 125, done_at);
        assert_eq!(w.attempts.len(), 1, "пауза после установщика ещё идёт: {:?}", w.attempts);
        assert!(w.installer_done.is_empty());
        w.run(&mut r, t0, done_at, done_at + 1 + CONFIRM_FOR.as_secs());
        assert_eq!(w.installer_done, ["home", "office"]);
        assert_eq!(w.attempt_secs()[1..], [done_at, done_at], "оба — сразу по истечении");
        assert_eq!(w.running, ["lab", "home", "office"]);
        assert_eq!(w.services, ["lab", "home", "office"], "`connect` ставит службы из конфигов");
        let connected = w.notes.iter().filter(|(_, n)| matches!(n, Note::Connected { attempts: 1 })).count();
        assert_eq!(connected, 3, "{:?}", w.notes);
        assert!(r.view(t0).is_empty());
    }

    /// Чужой MSI между «служба остановлена» и «служба удалена»: служба ещё есть, в аренду туннель не взят, а пропавший
    /// адаптер даёт смену сети. Внеочередной попытки нет (она шла бы внутри транзакции установщика) — только обычный
    /// первый шаг; туннель с попытками за спиной смена сети поднимает как всегда, установщик не при чём.
    #[test]
    fn network_change_during_a_foreign_installer_does_not_rush_a_fresh_track() {
        let t0 = Instant::now();
        let mut r = Retries::default();
        let mut w = World::new(&["office", "lab"]);
        w.native = true;
        w.running = vec!["office".into(), "lab".into()];
        w.services = w.running.clone();
        w.run(&mut r, t0, 0, 5);
        // «lab» упал сам раньше и уже пробовался; «office» останавливает MSI, его служба пока на месте.
        w.running.retain(|t| t != "lab");
        w.run(&mut r, t0, 5, 20);
        assert_eq!(w.attempts.len(), 1, "{:?}", w.attempts);
        w.installer = true;
        w.running.clear();
        w.step(&mut r, t0, t0 + Duration::from_secs(20), false);
        w.step(&mut r, t0, t0 + Duration::from_secs(21), true);
        assert!(w.attempts.iter().all(|(t, _)| t == "lab"), "по смене сети под установщиком — только «lab»: {:?}", w.attempts);
        assert_eq!(w.attempts.len(), 2, "{:?}", w.attempts);
        assert!(w.installer_held.is_empty(), "служба есть — в аренду не берётся");
        // Без установщика та же смена сети подключает свежий туннель сразу.
        let mut r = Retries::default();
        let mut w = World::new(&["office"]);
        w.native = true;
        w.running = vec!["office".into()];
        w.services = w.running.clone();
        w.run(&mut r, t0, 0, 5);
        w.running.clear();
        w.step(&mut r, t0, t0 + Duration::from_secs(5), false);
        w.step(&mut r, t0, t0 + Duration::from_secs(6), true);
        assert_eq!(w.attempt_secs(), [6]);
    }

    /// Наш установщик берёт аренду сам (`HoldNative`): неявная аренда поверх неё не ставится, срок — держателя.
    #[test]
    fn own_installer_lease_is_not_replaced_by_the_implicit_one() {
        let t0 = Instant::now();
        let mut r = Retries::default();
        let mut w = World::new(&["office"]);
        w.native = true;
        w.running = vec!["office".into()];
        w.services = vec!["office".into()];
        w.run(&mut r, t0, 0, 5);
        r.hold(&["office".to_string()], t0 + Duration::from_secs(5), Duration::from_secs(900));
        w.installer = true;
        w.running.clear();
        w.services.clear();
        w.run(&mut r, t0, 5, 60);
        assert!(w.installer_held.is_empty());
        w.installer = false;
        w.run(&mut r, t0, 60, 200);
        assert!(w.attempts.is_empty() && w.installer_done.is_empty(), "срок аренды держателя — 900 с, не INSTALLER_GRACE");
        assert_eq!(r.release(&["office".to_string()]), ["office"]);
    }
}
