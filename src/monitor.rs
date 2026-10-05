//! Фоновый опрос раз в секунду: список туннелей, состояние запущенных, история для графика,
//! накопительная статистика, события, значок в трее. Работает и при скрытом окне.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::backend::TunnelHost;
use crate::crash::lock;
use crate::daemon::proto::CoreState;
use crate::events::{self, Event, EventLog, Severity};
use crate::health::{self, Health, Level};
use crate::i18n::{tr, trf};
use crate::ping::PingState;
use crate::stats::{self, Stats};
use crate::settings::Mode;
use crate::tray;
use crate::uapi::Status;

const PERIOD: Duration = Duration::from_secs(1);
/// Пауза между попытками достучаться до ядра, пока связи нет: дёргать канал каждую секунду незачем.
const RETRY_PERIOD: Duration = Duration::from_secs(3);
/// Столько неудач подряд — связь с ядром потеряна (одиночный сбой — не повод менять всё окно).
const LOST_AFTER: u32 = 2;
const STATS_SAVE_EVERY: Duration = Duration::from_secs(15);
/// Час истории при замере раз в секунду — максимум периода графика.
pub const HISTORY_SECS: usize = 3600;

#[derive(Clone, Copy)]
pub struct Sample {
    pub at: Instant,
    pub rx: u64,
    pub tx: u64,
}

#[derive(Default, Clone)]
pub struct Live {
    pub status: Option<Status>,
    pub error: Option<String>,
    pub history: VecDeque<Sample>,
}

impl Live {
    /// Средняя скорость (байт/с) приёма и передачи за последние `secs` секунд.
    pub fn rate(&self, secs: f64) -> (f64, f64) {
        let Some(last) = self.history.back() else { return (0.0, 0.0) };
        let first = self
            .history
            .iter()
            .find(|s| last.at.duration_since(s.at).as_secs_f64() <= secs)
            .unwrap_or(last);
        let dt = last.at.duration_since(first.at).as_secs_f64();
        if dt <= 0.0 {
            return (0.0, 0.0);
        }
        (last.rx.saturating_sub(first.rx) as f64 / dt, last.tx.saturating_sub(first.tx) as f64 / dt)
    }

    /// Скорости по соседним замерам за последние `secs` секунд — точки графика.
    pub fn rate_series(&self, secs: f64) -> Vec<(f64, f64)> {
        let Some(last) = self.history.back() else { return Vec::new() };
        let recent: Vec<&Sample> =
            self.history.iter().filter(|s| last.at.duration_since(s.at).as_secs_f64() <= secs).collect();
        recent
            .windows(2)
            .map(|w| {
                let dt = w[1].at.duration_since(w[0].at).as_secs_f64().max(0.001);
                (w[1].rx.saturating_sub(w[0].rx) as f64 / dt, w[1].tx.saturating_sub(w[0].tx) as f64 / dt)
            })
            .collect()
    }

    /// Сколько секунд туннель наблюдается в этом запуске программы.
    pub fn observed_secs(&self) -> f64 {
        self.history.front().map(|s| s.at.elapsed().as_secs_f64()).unwrap_or(0.0)
    }
}

#[derive(Default, Clone)]
pub struct Snapshot {
    /// Все известные туннели: конфиги менеджера плюс запущенные не из них.
    pub tunnels: BTreeSet<String>,
    pub running: BTreeMap<String, Live>,
    pub error: Option<String>,
    /// Сколько опросов прошло: 0 — данных ещё нет (пустой список ещё ничего не значит).
    pub polls: u64,
    /// Окно потеряло связь с ядром: что с туннелями — неизвестно (а не «все отключены»).
    pub core_lost: bool,
}

impl Snapshot {
    /// Состояние туннеля для окна и трея: при потере связи с ядром — «неизвестно», а не «отключён».
    pub fn health(&self, name: &str, pending: Option<&str>, ping: Option<&PingState>) -> Health {
        if self.core_lost {
            return Health { level: Level::Warn, text: tr("health.core_lost") };
        }
        health::health(self.running.get(name), pending, ping)
    }
}

/// Связь окна с ядром: считает неудачи подряд и сообщает о переходах.
#[derive(Default)]
struct CoreLink {
    failures: u32,
    lost: bool,
}

#[derive(PartialEq, Eq, Debug)]
enum LinkChange {
    Lost,
    Restored,
}

impl CoreLink {
    fn failed(&mut self) -> Option<LinkChange> {
        self.failures = self.failures.saturating_add(1);
        if self.lost || self.failures < LOST_AFTER {
            return None;
        }
        self.lost = true;
        Some(LinkChange::Lost)
    }

    fn answered(&mut self) -> Option<LinkChange> {
        self.failures = 0;
        std::mem::take(&mut self.lost).then_some(LinkChange::Restored)
    }
}

/// Настройки, нужные фоновым потокам; окно обновляет их при изменении.
#[derive(Clone)]
pub struct Options {
    pub ping: bool,
    pub ping_host: String,
    pub notify: bool,
    pub tray: bool,
    /// Точка состояния на значке окна.
    pub taskbar: bool,
}

/// Общее состояние окна и фоновых потоков. Поля закрыты: снаружи состояние читают и меняют методами по смыслу,
/// а порядок захвата — snapshot → pending → ping → остальное — соблюдается только здесь. Замки берутся через
/// `crash::lock`: паника в потоке запроса или окна не делает состояние недоступным для остальных.
pub struct Shared {
    /// Туннели ядра (ядро меняет хост при смене режима — поэтому под замком) или демо-режима. `None` — окно с ядром:
    /// туннели ведёт ядро, окно зеркалит его состояние.
    host: RwLock<Option<Arc<dyn TunnelHost>>>,
    snapshot: Mutex<Snapshot>,
    /// туннель → «Подключение…»/«Отключение…», пока идёт команда пользователя
    pending: Mutex<BTreeMap<String, &'static str>>,
    ping: Mutex<PingState>,
    options: Mutex<Options>,
    events: Mutex<EventLog>,
    stats: Mutex<Stats>,
    stats_path: Option<PathBuf>,
    /// Состояние службы менеджера AmneziaWG — для строки состояния.
    service: Mutex<String>,
    /// Окно: режим, о котором сообщило ядро.
    core_mode: Mutex<Option<Mode>>,
    /// Окно: до какого события дочитан журнал ядра.
    core_cursor: Mutex<events::Cursor>,
}

/// Всё, что окно рисует за кадр, снятое в одном порядке захвата. Снимок опроса удерживается, пока жив `FrameView`
/// (его копия на каждый кадр стоила бы часа истории замеров по каждому туннелю); остальное — копии.
pub struct FrameView<'a> {
    pub snap: MutexGuard<'a, Snapshot>,
    pub pending: BTreeMap<String, &'static str>,
    pub ping: PingState,
    pub stats: Stats,
    pub service: String,
}

/// Пометка туннелей «заняты» на время команды; снимается в `Drop` — и при панике посреди переключения. Иначе
/// туннель остался бы занятым навсегда: повторное нажатие по занятому туннелю ничего не делает.
pub struct PendingGuard {
    shared: Arc<Shared>,
    names: Vec<String>,
}

impl PendingGuard {
    /// Снять пометку с одного туннеля раньше остальных (переподключение по очереди).
    pub fn release(&mut self, name: &str) {
        if let Some(i) = self.names.iter().position(|n| n == name) {
            self.names.swap_remove(i);
            lock(&self.shared.pending).remove(name);
        }
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        let mut pending = lock(&self.shared.pending);
        for name in &self.names {
            pending.remove(name);
        }
    }
}

impl Shared {
    pub fn new(host: Option<Arc<dyn TunnelHost>>, options: Options, stats_path: Option<PathBuf>, events_path: Option<PathBuf>) -> Shared {
        let stats = stats_path.as_deref().map(stats::load).unwrap_or_default();
        Shared {
            host: RwLock::new(host),
            snapshot: Default::default(),
            pending: Default::default(),
            ping: Mutex::new(PingState { host: options.ping_host.clone(), ..Default::default() }),
            options: Mutex::new(options),
            events: Mutex::new(EventLog::open(events_path)),
            stats: Mutex::new(stats),
            stats_path,
            service: Default::default(),
            core_mode: Default::default(),
            core_cursor: Default::default(),
        }
    }

    /// Окно с ядром — зеркало: своих туннелей нет, всё приходит от ядра.
    pub fn mirrors_core(&self) -> bool {
        self.host.read().unwrap().is_none()
    }

    pub fn host(&self) -> Option<Arc<dyn TunnelHost>> {
        self.host.read().unwrap().clone()
    }

    /// Ядро: хост нового режима.
    pub fn set_host(&self, host: Arc<dyn TunnelHost>) {
        *self.host.write().unwrap() = Some(host);
    }

    pub fn save_stats(&self) {
        if let Some(path) = &self.stats_path {
            // Копия — чтобы не держать stats, пока берём snapshot (poll захватывает их в обратном порядке).
            let stats = lock(&self.stats).clone();
            if let Err(e) = stats::save(path, &stats) {
                lock(&self.snapshot).error = Some(crate::fsutil::io_ctx(&path, e));
            }
        }
    }

    pub fn log(&self, tunnel: &str, severity: Severity, text: &str) {
        // Пишут и запросы окна в ядре: паника одного из них не должна лишить журнал записей.
        lock(&self.events).push(Event::new(unix_now(), tunnel, severity, text, false));
    }

    // --- снимок опроса

    /// Имена запущенных туннелей.
    pub fn running_names(&self) -> Vec<String> {
        lock(&self.snapshot).running.keys().cloned().collect()
    }

    pub fn is_running(&self, name: &str) -> bool {
        lock(&self.snapshot).running.contains_key(name)
    }

    pub fn any_running(&self) -> bool {
        !lock(&self.snapshot).running.is_empty()
    }

    /// Все известные туннели: конфиги менеджера плюс запущенные не из них.
    pub fn known_tunnels(&self) -> Vec<String> {
        lock(&self.snapshot).tunnels.iter().cloned().collect()
    }

    /// Последний опрос закончился ошибкой.
    pub fn has_poll_error(&self) -> bool {
        lock(&self.snapshot).error.is_some()
    }

    /// Каждый запущенный туннель наблюдается не меньше `secs` секунд (график периода заполнен).
    pub fn observed_for(&self, secs: f64) -> bool {
        lock(&self.snapshot).running.values().all(|l| l.observed_secs() >= secs)
    }

    /// Последнее состояние запущенного туннеля из канала службы.
    pub fn status_of(&self, name: &str) -> Option<Status> {
        lock(&self.snapshot).running.get(name).and_then(|l| l.status.clone())
    }

    /// Копия снимка опроса — для вывода без окна, где замок держать незачем.
    pub fn snapshot_clone(&self) -> Snapshot {
        lock(&self.snapshot).clone()
    }

    /// Режим сменился: показания прежнего режима недействительны, список заполнит следующий опрос.
    pub fn reset_snapshot(&self) {
        let mut snap = lock(&self.snapshot);
        snap.tunnels.clear();
        snap.running.clear();
    }

    /// Кадр окна: см. `FrameView`.
    pub fn frame_view(&self) -> FrameView<'_> {
        let snap = lock(&self.snapshot);
        let pending = lock(&self.pending).clone();
        let ping = lock(&self.ping).clone();
        let stats = lock(&self.stats).clone();
        let service = lock(&self.service).clone();
        FrameView { snap, pending, ping, stats, service }
    }

    /// Состояние для окна по каналу; окно-зеркало разбирает его в `mirror_core`.
    pub fn core_state(&self, mode: Mode, events_after: u64) -> CoreState {
        let snap = lock(&self.snapshot);
        let busy = lock(&self.pending).keys().cloned().collect();
        let ping = lock(&self.ping).to_dto();
        // События и код отсчёта — из одного захвата журнала: пара должна быть согласованной.
        let (events, events_instance, events_loaded) = {
            let log = lock(&self.events);
            (log.since(events_after), log.instance(), log.loaded())
        };
        CoreState {
            mode: Some(mode),
            tunnels: snap.tunnels.iter().cloned().collect(),
            running: snap
                .running
                .iter()
                .map(|(n, l)| (n.clone(), l.status.clone().ok_or_else(|| l.error.clone().unwrap_or_default())))
                .collect(),
            error: snap.error.clone(),
            stats: lock(&self.stats).clone(),
            ping,
            events,
            events_instance,
            events_loaded,
            service: lock(&self.service).clone(),
            busy,
        }
    }

    // --- «занят»

    pub fn is_pending(&self, name: &str) -> bool {
        lock(&self.pending).contains_key(name)
    }

    /// Пометить туннели занятыми; пометка снимается, когда охранник уничтожен (в том числе при панике).
    pub fn pending_guard(self: &Arc<Self>, items: impl IntoIterator<Item = (String, &'static str)>) -> PendingGuard {
        let mut pending = lock(&self.pending);
        let names = items
            .into_iter()
            .map(|(name, label)| {
                pending.insert(name.clone(), label);
                name
            })
            .collect();
        PendingGuard { shared: self.clone(), names }
    }

    /// Занять туннель, если он не занят: проверка и пометка — под одним замком, два нажатия подряд не пройдут оба.
    pub fn try_pending_guard(self: &Arc<Self>, name: &str, label: &'static str) -> Option<PendingGuard> {
        let mut pending = lock(&self.pending);
        if pending.contains_key(name) {
            return None;
        }
        pending.insert(name.to_string(), label);
        Some(PendingGuard { shared: self.clone(), names: vec![name.to_string()] })
    }

    // --- настройки фоновых потоков

    pub fn options(&self) -> Options {
        lock(&self.options).clone()
    }

    /// Подменить настройки; возвращает прежние — чтобы вызвавший увидел, что изменилось.
    pub fn replace_options(&self, new: Options) -> Options {
        std::mem::replace(&mut *lock(&self.options), new)
    }

    pub fn set_ping_options(&self, enabled: bool, host: String) {
        let mut o = lock(&self.options);
        o.ping = enabled;
        o.ping_host = host;
    }

    // --- пинг

    pub fn update_ping<R>(&self, f: impl FnOnce(&mut PingState) -> R) -> R {
        f(&mut lock(&self.ping))
    }

    // --- статистика

    pub fn update_stats<R>(&self, f: impl FnOnce(&mut Stats) -> R) -> R {
        f(&mut lock(&self.stats))
    }

    // --- журнал, служба, режим ядра

    pub fn push_event(&self, event: Event) {
        lock(&self.events).push(event);
    }

    // Ð¢Ð¾Ð»ÑÐºÐ¾ Ð´Ð»Ñ Ð¿ÑÐ¾Ð²ÐµÑÐ¾Ðº: Ð¾ÐºÐ½Ð¾ Ð¸ ÑÐ´ÑÐ¾ ÑÐ¸ÑÐ°ÑÑ Ð¶ÑÑÐ½Ð°Ð» ÑÐµÐ»Ð¸ÐºÐ¾Ð¼ (`with_events`) Ð¸Ð»Ð¸ Ð¿Ð¾ÑÑÐ¸ÑÐ¼Ð¸ (`core_state`).
    #[cfg(test)]
    pub fn events_since(&self, after: u64) -> Vec<(u64, Event)> {
        lock(&self.events).since(after)
    }

    /// Прочитать журнал под замком (окно рисует его строки, не копируя).
    pub fn with_events<R>(&self, f: impl FnOnce(&EventLog) -> R) -> R {
        f(&lock(&self.events))
    }

    pub fn set_service(&self, text: String) {
        *lock(&self.service) = text;
    }

    /// Режим, о котором сообщило ядро (окно-зеркало).
    pub fn core_mode(&self) -> Option<Mode> {
        *lock(&self.core_mode)
    }

    /// Состояние каждого известного туннеля.
    fn levels(&self) -> (BTreeMap<String, Health>, BTreeSet<String>) {
        let snap = lock(&self.snapshot);
        let pending = lock(&self.pending).clone();
        // Настройка читается отдельно: `options` — «остальное», его не берут, пока удержан `ping`.
        let want_ping = lock(&self.options).ping;
        let ping = want_ping.then(|| lock(&self.ping).clone());
        let map = snap.tunnels.iter().map(|t| (t.clone(), snap.health(t, pending.get(t).copied(), ping.as_ref()))).collect();
        (map, pending.into_keys().collect())
    }
}

/// Опрос раз в секунду. Окно с ядром — зеркало: состояние, статистика, пинг и события приходят от ядра,
/// здесь только история для графика и трей. Ядро и демо-режим опрашивают сами и сами пишут события.
pub fn spawn(shared: Arc<Shared>, on_update: Box<dyn Fn() + Send>) {
    crate::crash::spawn_named("monitor", move || {
        let mut prev: Option<BTreeMap<String, Level>> = None;
        let mut last_save = Instant::now();
        let mut link = CoreLink::default();
        loop {
            let host = shared.host();
            match &host {
                Some(host) => poll(&shared, host.as_ref()),
                None => mirror_core(&shared, &mut link),
            }
            let (levels, user) = shared.levels();
            react(&shared, &mut prev, &levels, &user, host.is_some());
            if last_save.elapsed() >= STATS_SAVE_EVERY {
                shared.save_stats();
                last_save = Instant::now();
            }
            on_update();
            std::thread::sleep(if link.lost { RETRY_PERIOD } else { PERIOD });
        }
    });
}

/// События, уведомления и значок в трее по новым уровням.
/// `make_events` — события по переходам пишет тот, кто опрашивает сам (ядро, демо); окно-зеркало получает их от ядра.
fn react(shared: &Shared, prev: &mut Option<BTreeMap<String, Level>>, levels: &BTreeMap<String, Health>, user: &BTreeSet<String>, make_events: bool) {
    // «Занят» (переключение, ожидание рукопожатия) — промежуточное состояние, событий не даёт.
    let settled: BTreeMap<String, (Level, String)> = levels
        .iter()
        .filter(|(_, h)| h.level != Level::Busy)
        .map(|(n, h)| (n.clone(), (h.level, h.text.clone())))
        .collect();
    let options = lock(&shared.options).clone();
    // Первый опрос — точка отсчёта: туннель, уже поднятый при старте, не событие.
    if let (Some(prev), true) = (prev.as_ref(), make_events) {
        let fresh = events::transitions(prev, &settled, user, unix_now());
        record_and_notify(&shared.events, fresh, options.notify && options.tray, &mut |e| toast(e));
    }
    let p = prev.get_or_insert_with(BTreeMap::new);
    for (n, (level, _)) in settled {
        p.insert(n, level);
    }

    let core_lost = lock(&shared.snapshot).core_lost;
    let mut worst = levels.values().map(|h| h.level).max_by_key(|l| severity_rank(*l)).unwrap_or(Level::Off);
    if core_lost && severity_rank(worst) < severity_rank(Level::Warn) {
        // Туннелей в списке может не быть вовсе — значок всё равно предупреждает.
        worst = Level::Warn;
    }
    if options.tray {
        let active: Vec<(&String, &Health)> = levels.iter().filter(|(_, h)| h.level != Level::Off).collect();
        let tip = if core_lost {
            tr("tray.core_lost")
        } else if active.is_empty() {
            crate::i18n::tr("tray.none")
        } else {
            active.iter().map(|(n, h)| format!("{n}: {}", h.text)).collect::<Vec<_>>().join("\n")
        };
        tray::set_state(worst, &tip);
    }
    tray::set_window_state(options.taskbar.then_some(worst));
}

fn severity_rank(level: Level) -> u8 {
    match level {
        Level::Off => 0,
        Level::Ok => 1,
        Level::Busy => 2,
        Level::Warn => 3,
        Level::Bad => 4,
    }
}

pub fn poll(shared: &Shared, backend: &dyn TunnelHost) {
    let configs = backend.configs();
    let running = backend.running();
    let queried: Vec<(String, Result<Status, String>)> = running
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|name| (name.clone(), backend.query(name).map_err(|e| e.to_string())))
        .collect();

    let now = Instant::now();
    let mut snap = lock(&shared.snapshot);
    let mut stats = lock(&shared.stats);
    snap.error = [configs.as_ref().err(), running.as_ref().err()]
        .into_iter()
        .flatten()
        .map(|e| e.to_string())
        .reduce(|a, b| format!("{a}; {b}"));
    let listed = configs.is_ok();
    if let Ok(configs) = configs {
        snap.tunnels = configs.into_iter().collect();
    }
    let mut next = BTreeMap::new();
    for (name, result) in queried {
        snap.tunnels.insert(name.clone());
        let mut live = snap.running.remove(&name).unwrap_or_default();
        match result {
            Ok(st) => {
                let dt = live.history.back().map(|s| now.duration_since(s.at).as_secs_f64());
                stats.entry(name.clone()).or_default().observe(st.rx_bytes(), st.tx_bytes(), st.listen_port, dt, unix_now());
                live.history.push_back(Sample { at: now, rx: st.rx_bytes(), tx: st.tx_bytes() });
                while live.history.len() > HISTORY_SECS + 1 {
                    live.history.pop_front();
                }
                live.status = Some(st);
                live.error = None;
            }
            Err(e) => live.error = Some(e),
        }
        next.insert(name, live);
    }
    snap.running = next;
    snap.polls += 1;
    // Только по настоящему списку: при ошибке чтения списка туннелей все выглядели бы удалёнными.
    let pruned = if listed { stats::prune(&mut stats, &snap.tunnels, unix_now()) } else { Vec::new() };
    drop((snap, stats));
    for name in pruned {
        shared.log(&name, Severity::Info, &trf("stats.pruned", &[&name, &stats::KEEP_ABSENT_DAYS.to_string()]));
    }
}

/// Окно: забрать состояние у ядра. История для графика копится здесь, по счётчикам из ответа.
fn mirror_core(shared: &Shared, link: &mut CoreLink) {
    use crate::daemon::proto::{Request, Response};
    let after = lock(&shared.core_cursor).after();
    let state = match crate::daemon::pipe::call(&Request::State { events_after: after }) {
        Ok(Response::State(s)) => s,
        Ok(other) => {
            // Ядро ответило, но состояния не дало (занято, отказ): связь есть, показываем причину и ждём следующего опроса.
            let change = link.answered();
            {
                let mut snap = lock(&shared.snapshot);
                snap.core_lost = false;
                snap.error = Some(format!("core: {other:?}"));
            }
            report_link(shared, change, "");
            return;
        }
        Err(e) => {
            core_unreachable(shared, link, &e);
            return;
        }
    };
    let restored = link.answered();
    let now = Instant::now();
    {
        let mut snap = lock(&shared.snapshot);
        snap.core_lost = false;
        snap.error = state.error;
        snap.tunnels = state.tunnels.into_iter().collect();
        let mut next = BTreeMap::new();
        for (name, result) in state.running {
            let mut live = snap.running.remove(&name).unwrap_or_default();
            match result {
                Ok(st) => {
                    live.history.push_back(Sample { at: now, rx: st.rx_bytes(), tx: st.tx_bytes() });
                    while live.history.len() > HISTORY_SECS + 1 {
                        live.history.pop_front();
                    }
                    live.status = Some(st);
                    live.error = None;
                }
                Err(e) => live.error = Some(e),
            }
            next.insert(name, live);
        }
        snap.running = next;
        snap.polls += 1;
    }
    report_link(shared, restored, "");
    *lock(&shared.ping) = crate::ping::PingState::from_dto(state.ping);
    *lock(&shared.stats) = state.stats;
    *lock(&shared.service) = state.service;
    *lock(&shared.core_mode) = state.mode;
    // Уведомления — только о новых событиях, не о тех, что пришли первой порцией при открытии окна;
    // а после перезапуска ядра курсор переставляется и новые события приходят со следующим опросом.
    let batch = lock(&shared.core_cursor).accept(state.events_instance, state.events_loaded, state.events);
    let options = lock(&shared.options).clone();
    record_and_notify(&shared.events, batch.events.into_iter().map(|(_, e)| e), !batch.quiet && options.notify && options.tray, &mut |e| toast(e));
}

/// Показать событие уведомлением Windows.
fn toast(e: &Event) {
    tray::notify(crate::APP_TITLE, &format!("{} — {}", e.tunnel, e.text), e.severity != Severity::Info);
}

/// Записать события в журнал, а уведомить о них — уже после того, как блокировка журнала отпущена: `notify` идёт
/// в Explorer и может ждать его сколько угодно, а журнал на это время читает поток окна на каждом кадре.
/// `notifier` — вызов оболочки (в тесте подменяется).
fn record_and_notify(log: &Mutex<EventLog>, events: impl IntoIterator<Item = Event>, enabled: bool, notifier: &mut dyn FnMut(&Event)) {
    let mut to_show = Vec::new();
    {
        let mut log = lock(log);
        for e in events {
            if enabled && e.notify {
                to_show.push(e.clone());
            }
            log.push(e);
        }
    }
    for e in &to_show {
        notifier(e);
    }
}

/// Канал к ядру не отвечает. Показания туннелей больше недостоверны — сбрасываются, окно и значок в трее
/// показывают «нет связи с ядром», а не «все отключены»; событие пишется один раз, без уведомления.
fn core_unreachable(shared: &Shared, link: &mut CoreLink, error: &str) {
    let change = link.failed();
    {
        let mut snap = lock(&shared.snapshot);
        snap.error = Some(trf("core.unavailable", &[error]));
        if change == Some(LinkChange::Lost) {
            snap.running.clear();
            snap.core_lost = true;
        }
        snap.polls += 1;
    }
    report_link(shared, change, error);
}

/// Событие о смене состояния связи с ядром — только в журнал окна: уведомление о ней было бы ложной тревогой
/// (ядро могли перезапускать при обновлении), а значок в трее уже предупреждает.
fn report_link(shared: &Shared, change: Option<LinkChange>, error: &str) {
    match change {
        Some(LinkChange::Lost) => shared.log("", Severity::Warn, &trf("ev.core_lost", &[error])),
        Some(LinkChange::Restored) => shared.log("", Severity::Info, &tr("ev.core_back")),
        None => {}
    }
}

pub fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(text: &str, notify: bool) -> Event {
        Event::new(1, "a", Severity::Warn, text, notify)
    }

    #[test]
    fn toast_runs_after_the_log_lock_is_released() {
        let log = Mutex::new(EventLog::open(None));
        let mut shown = Vec::new();
        record_and_notify(&log, vec![event("один", true), event("тихо", false), event("два", true)], true, &mut |e| {
            // Explorer может отвечать долго: в это время журнал должен быть доступен окну.
            let free = log.try_lock().map(|l| l.items.len());
            shown.push((e.text.clone(), free.ok()));
        });
        assert_eq!(shown, vec![("один".to_string(), Some(3)), ("два".to_string(), Some(3))], "журнал свободен и уже полон");
    }

    #[test]
    fn no_toasts_when_disabled_but_events_are_logged() {
        let log = Mutex::new(EventLog::open(None));
        let mut calls = 0;
        record_and_notify(&log, vec![event("один", true)], false, &mut |_| calls += 1);
        assert_eq!(calls, 0);
        assert_eq!(lock(&log).items.len(), 1);
    }

    #[test]
    fn rate_over_window() {
        let t0 = Instant::now();
        let mut live = Live::default();
        for i in 0..5u64 {
            live.history.push_back(Sample { at: t0 + Duration::from_secs(i), rx: i * 1000, tx: i * 10 });
        }
        let (rx, tx) = live.rate(2.0);
        assert!((rx - 1000.0).abs() < 1e-6 && (tx - 10.0).abs() < 1e-6);
        assert_eq!(live.rate_series(120.0).len(), 4);
        assert_eq!(live.rate_series(2.0).len(), 2);
    }

    #[test]
    fn rate_empty_is_zero() {
        assert_eq!(Live::default().rate(5.0), (0.0, 0.0));
    }

    #[test]
    fn link_is_lost_after_repeated_failures_and_restored_once() {
        let mut link = CoreLink::default();
        assert_eq!(link.failed(), None, "одиночный сбой — не потеря связи");
        assert_eq!(link.answered(), None);
        assert_eq!(link.failed(), None, "счётчик начат заново после ответа");
        assert_eq!(link.failed(), Some(LinkChange::Lost));
        assert_eq!(link.failed(), None, "потерю объявляют один раз");
        assert!(link.lost);
        assert_eq!(link.answered(), Some(LinkChange::Restored));
        assert_eq!(link.answered(), None);
    }

    fn snapshot_with_running() -> Snapshot {
        let mut snap = Snapshot { tunnels: ["a".to_string()].into(), ..Default::default() };
        snap.running.insert("a".into(), Live::default());
        snap
    }

    #[test]
    fn lost_core_is_unknown_not_disconnected() {
        let mut snap = snapshot_with_running();
        snap.running.clear();
        assert_eq!(snap.health("a", None, None).level, Level::Off, "ядро на связи, туннель не запущен — отключён");
        snap.core_lost = true;
        assert_eq!(snap.health("a", None, None).level, Level::Warn, "связи нет — неизвестно, а не «отключён»");
    }

    #[test]
    fn unreachable_core_marks_state_and_logs_one_quiet_event() {
        let shared = Shared::new(None, Options { ping: false, ping_host: String::new(), notify: true, tray: true, taskbar: false }, None, None);
        *lock(&shared.snapshot) = snapshot_with_running();
        let mut link = CoreLink::default();
        core_unreachable(&shared, &mut link, "pipe: gone");
        assert!(!lock(&shared.snapshot).core_lost, "первая неудача — ещё не потеря");
        core_unreachable(&shared, &mut link, "pipe: gone");
        core_unreachable(&shared, &mut link, "pipe: gone");
        {
            let snap = lock(&shared.snapshot);
            assert!(snap.core_lost && snap.running.is_empty());
            assert!(snap.error.as_deref().is_some_and(|e| e.contains("pipe: gone")));
        }
        let log = lock(&shared.events);
        assert_eq!(log.items.len(), 1, "событие о потере — одно, не по одному на каждый повтор");
        assert!(!log.items[0].notify, "без всплывающего уведомления");
        assert_eq!(log.items[0].severity, Severity::Warn);
    }

    fn shared() -> Arc<Shared> {
        let options = Options { ping: false, ping_host: String::new(), notify: false, tray: false, taskbar: false };
        Arc::new(Shared::new(None, options, None, None))
    }

    #[test]
    fn pending_guard_marks_and_releases_on_drop() {
        let shared = shared();
        let guard = shared.pending_guard([("a".to_string(), "busy.connect"), ("b".to_string(), "busy.disconnect")]);
        assert!(shared.is_pending("a") && shared.is_pending("b"));
        assert!(!shared.is_pending("c"));
        drop(guard);
        assert!(!shared.is_pending("a") && !shared.is_pending("b"), "пометки сняты вместе с охранником");
    }

    #[test]
    fn pending_guard_is_released_when_the_holder_panics() {
        let shared = shared();
        let held = shared.clone();
        let r = crate::crash::isolate(move || {
            let _busy = held.pending_guard([("a".to_string(), "busy.connect")]);
            panic!("backend bug");
        });
        assert!(r.is_err());
        assert!(!shared.is_pending("a"), "паника посреди команды не оставляет туннель занятым");
        // Замок пережил панику: следующая пометка проходит, а не паникует на отравленном замке.
        assert!(shared.try_pending_guard("a", "busy.connect").is_some());
    }

    #[test]
    fn busy_tunnel_cannot_be_claimed_twice() {
        let shared = shared();
        let first = shared.try_pending_guard("a", "busy.connect").expect("свободный туннель");
        assert!(shared.try_pending_guard("a", "busy.disconnect").is_none(), "второе нажатие по занятому — отказ");
        assert!(shared.try_pending_guard("b", "busy.connect").is_some(), "другой туннель свободен");
        drop(first);
        assert!(shared.try_pending_guard("a", "busy.disconnect").is_some(), "после снятия — снова можно");
    }

    #[test]
    fn released_name_is_freed_early_and_not_removed_twice() {
        let shared = shared();
        let mut guard = shared.pending_guard([("a".to_string(), "busy.reconnect"), ("b".to_string(), "busy.reconnect")]);
        guard.release("a");
        assert!(!shared.is_pending("a") && shared.is_pending("b"));
        // Туннель, занятый другой командой после раннего снятия, чужой охранник не трогает.
        let other = shared.try_pending_guard("a", "busy.connect").expect("освобождённый туннель");
        drop(guard);
        assert!(shared.is_pending("a"), "охранник не снимает то, что уже отпустил");
        assert!(!shared.is_pending("b"));
        drop(other);
    }

    #[test]
    fn intent_methods_read_the_snapshot() {
        let shared = shared();
        *lock(&shared.snapshot) = snapshot_with_running();
        assert_eq!(shared.running_names(), ["a"]);
        assert_eq!(shared.known_tunnels(), ["a"]);
        assert!(shared.is_running("a") && !shared.is_running("b") && shared.any_running());
        assert!(!shared.has_poll_error());
        assert_eq!(shared.snapshot_clone().polls, 0);
        shared.reset_snapshot();
        assert!(shared.running_names().is_empty() && shared.known_tunnels().is_empty() && !shared.any_running());
    }

    #[test]
    fn frame_view_holds_snapshot_and_copies_the_rest() {
        let shared = shared();
        *lock(&shared.snapshot) = snapshot_with_running();
        let _busy = shared.try_pending_guard("a", "busy.connect");
        shared.set_service("running".into());
        let view = shared.frame_view();
        assert!(view.snap.tunnels.contains("a"));
        assert_eq!(view.pending.get("a"), Some(&"busy.connect"));
        assert_eq!(view.service, "running");
        // Остальное под замком не держится: пометки и журнал доступны, пока кадр открыт.
        assert!(shared.is_pending("a"));
        shared.log("", Severity::Info, "x");
        assert_eq!(shared.events_since(0).len(), 1);
    }

    #[test]
    fn core_state_mirrors_the_shared_state() {
        let shared = shared();
        *lock(&shared.snapshot) = snapshot_with_running();
        let _busy = shared.try_pending_guard("a", "busy.connect");
        shared.log("a", Severity::Info, "hello");
        let state = shared.core_state(Mode::Engine, 0);
        assert_eq!(state.mode, Some(Mode::Engine));
        assert_eq!(state.tunnels, ["a"]);
        assert_eq!(state.busy, ["a"]);
        assert_eq!(state.events.len(), 1);
        assert!(state.running.contains_key("a"));
    }
}
