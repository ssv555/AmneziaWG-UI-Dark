//! Фоновый опрос раз в секунду: список туннелей, состояние запущенных, история для графика,
//! события, значок в трее. Работает и при скрытом окне.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::backend::TunnelHost;
use crate::crash::{self, lock};
use crate::daemon::agent::client::AgentApi;
use crate::daemon::proto::{CoreState, RetryState};
use crate::events::{self, Event, EventLog, Severity};
use crate::health::{self, Health, Level};
use crate::i18n::{tr, trf};
use crate::ping::PingState;
use crate::stats::Stats;
use crate::settings::Mode;
use crate::tray;
use crate::uapi::Status;
use crate::update::NativeUiMark;

const PERIOD: Duration = Duration::from_secs(1);
/// Пауза между попытками достучаться до ядра, пока связи нет: дёргать канал каждую секунду незачем.
const RETRY_PERIOD: Duration = Duration::from_secs(3);
/// Столько неудач подряд — связь с ядром потеряна (одиночный сбой — не повод менять всё окно).
const LOST_AFTER: u32 = 2;
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
    /// Желаемые туннели, которые ядро переподключает (`daemon::retry`).
    pub retries: BTreeMap<String, RetryState>,
}

impl Snapshot {
    /// Состояние туннеля для окна и трея: при потере связи с ядром — «неизвестно», а не «отключён».
    pub fn health(&self, name: &str, pending: Option<&str>, ping: Option<&PingState>) -> Health {
        if self.core_lost {
            return Health::new(Level::Warn, tr("health.core_lost"));
        }
        let live = self.running.get(name);
        if let (None, None, Some(retry)) = (live, pending, self.retries.get(name)) {
            return health::retrying(retry);
        }
        health::health(live, pending, ping)
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
    /// Общий с потоком `events-writer`: он кладёт сюда сбои записи файла.
    events: Arc<Mutex<EventLog>>,
    /// Окно: статистика трафика из `State` агента (её ведёт агент, `daemon::agent::stats`); демо — выдуманная.
    stats: Mutex<Stats>,
    /// Состояние службы менеджера AmneziaWG — для строки состояния.
    service: Mutex<String>,
    /// Окно: агент не ответил на последний опрос — пинга нет, на его месте «вторичная служба недоступна».
    agent_down: AtomicBool,
    /// Окно: режим, о котором сообщило ядро.
    core_mode: Mutex<Option<Mode>>,
    /// Окно: до какого события дочитан журнал ядра.
    core_cursor: Mutex<events::Cursor>,
    /// Окно: до какого события дочитан журнал агента (история файла и события самого агента).
    agent_cursor: Mutex<events::Cursor>,
    /// Окно: с какого момента оно видит ядро в нынешнем состоянии («Скопировать диагностику»).
    core_seen: Mutex<Stamp>,
    /// Окно: то же для агента.
    agent_seen: Mutex<Stamp>,
}

/// Состояние связи («не отвечает» или нет) и с какого момента окно его видит. Окно не знает, когда процесс запущен на
/// самом деле, — только когда оно само заметило перемену; для разбора жалоб этого достаточно, и это честно названо.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stamp {
    pub down: bool,
    /// Unix-секунды; 0 — наблюдений ещё не было.
    pub since: u64,
}

impl Stamp {
    /// Очередное наблюдение: время сдвигается, только когда состояние сменилось (или это первое наблюдение).
    pub fn observe(&mut self, down: bool, now: u64) {
        if self.since == 0 || self.down != down {
            *self = Stamp { down, since: now };
        }
    }
}

/// Всё, что окно рисует за кадр, снятое в одном порядке захвата. Снимок опроса удерживается, пока жив `FrameView`
/// (его копия на каждый кадр стоила бы часа истории замеров по каждому туннелю); остальное — копии.
pub struct FrameView<'a> {
    pub snap: MutexGuard<'a, Snapshot>,
    pub pending: BTreeMap<String, &'static str>,
    pub ping: PingState,
    pub stats: Stats,
    pub service: String,
    /// Агент не ответил на последний опрос окна (`mirror_agent`).
    pub agent_down: bool,
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
    pub fn new(host: Option<Arc<dyn TunnelHost>>, options: Options, events_path: Option<PathBuf>) -> Shared {
        let events = Arc::new(Mutex::new(EventLog::open(events_path)));
        // Запись под блокировкой журнала не должна ждать диск: её ждали бы переключение туннелей и надзор. Ядро и окно
        // файла не дают (его ведёт агент), тогда это ничего не делает.
        EventLog::write_in_background(&events);
        Shared {
            host: RwLock::new(host),
            snapshot: Default::default(),
            pending: Default::default(),
            ping: Mutex::new(PingState { host: options.ping_host.clone(), ..Default::default() }),
            options: Mutex::new(options),
            events,
            stats: Default::default(),
            service: Default::default(),
            agent_down: AtomicBool::new(false),
            core_mode: Default::default(),
            core_cursor: Default::default(),
            agent_cursor: Default::default(),
            core_seen: Default::default(),
            agent_seen: Default::default(),
        }
    }

    /// Ядро и агент глазами окна: с каких пор в нынешнем состоянии.
    pub fn link_stamps(&self) -> (Stamp, Stamp) {
        (*lock(&self.core_seen), *lock(&self.agent_seen))
    }

    /// Окно с ядром — зеркало: своих туннелей нет, всё приходит от ядра.
    pub fn mirrors_core(&self) -> bool {
        crash::read(&self.host).is_none()
    }

    pub fn host(&self) -> Option<Arc<dyn TunnelHost>> {
        crash::read(&self.host).clone()
    }

    /// Ядро: хост нового режима.
    pub fn set_host(&self, host: Arc<dyn TunnelHost>) {
        *crash::write(&self.host) = Some(host);
    }

    /// Паника шага вторичного потока (`crash::nonfatal_loop`): в журнал — с паузой до следующего шага.
    pub fn report_secondary_panic(&self, panic: &str, wait: Duration) {
        self.log("", Severity::Bad, &trf("core.secondary_failed", &[panic, &wait.as_secs().to_string()]));
    }

    pub fn log(&self, tunnel: &str, severity: Severity, text: &str) {
        // Пишут и запросы окна в ядре: паника одного из них не должна лишить журнал записей.
        lock(&self.events).push(Event::new(unix_now(), tunnel, severity, text, false));
    }

    /// Событие, которое окно покажет и уведомлением Windows.
    pub fn notify(&self, tunnel: &str, severity: Severity, text: &str) {
        lock(&self.events).push(Event::new(unix_now(), tunnel, severity, text, true));
    }

    /// Ядро: состояние надзора за туннелями для окна.
    pub fn set_retries(&self, retries: BTreeMap<String, RetryState>) {
        lock(&self.snapshot).retries = retries;
    }

    // --- снимок опроса

    /// Имена запущенных туннелей.
    pub fn running_names(&self) -> Vec<String> {
        lock(&self.snapshot).running.keys().cloned().collect()
    }

    /// Туннели, которые ядро переподключает.
    pub fn retrying_names(&self) -> Vec<String> {
        lock(&self.snapshot).retries.keys().cloned().collect()
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

    /// Прочитать снимок под замком, без копии часа истории каждого туннеля (сторож ядра смотрит его раз в секунду).
    pub fn with_snapshot<R>(&self, f: impl FnOnce(&Snapshot) -> R) -> R {
        f(&lock(&self.snapshot))
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
        snap.retries.clear();
    }

    /// Кадр окна: см. `FrameView`.
    pub fn frame_view(&self) -> FrameView<'_> {
        let snap = lock(&self.snapshot);
        let pending = lock(&self.pending).clone();
        let ping = lock(&self.ping).clone();
        let stats = lock(&self.stats).clone();
        let service = lock(&self.service).clone();
        let agent_down = self.agent_down.load(Ordering::SeqCst);
        FrameView { snap, pending, ping, stats, service, agent_down }
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
            // Статистику ведёт агент; пустая — для окна прежней версии, которое без поля ответа не разберёт.
            stats: Stats::default(),
            ping,
            events,
            events_instance,
            events_loaded,
            service: lock(&self.service).clone(),
            busy,
            retries: snap.retries.clone(),
            // Агента знает сторож ядра, а не `Shared`: заполняет `server::Core::state`.
            agent: None,
        }
    }

    // --- «занят»

    pub fn is_pending(&self, name: &str) -> bool {
        lock(&self.pending).contains_key(name)
    }

    /// Что сейчас делается с туннелем («подключение…»); `None` — не занят.
    pub fn pending_label(&self, name: &str) -> Option<&'static str> {
        lock(&self.pending).get(name).copied()
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

    // Только для проверок: окно и ядро читают журнал целиком (`with_events`) или порциями (`core_state`).
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

    /// Состояние каждого известного туннеля, переключаемые командой и туннели под надзором ядра (работает ли).
    fn levels(&self) -> (BTreeMap<String, Health>, BTreeSet<String>, BTreeMap<String, bool>) {
        let snap = lock(&self.snapshot);
        let supervised = snap.retries.keys().map(|t| (t.clone(), snap.running.contains_key(t))).collect();
        let pending = lock(&self.pending).clone();
        // Настройка читается отдельно: `options` — «остальное», его не берут, пока удержан `ping`.
        let want_ping = lock(&self.options).ping;
        let ping = want_ping.then(|| lock(&self.ping).clone());
        let map = snap.tunnels.iter().map(|t| (t.clone(), snap.health(t, pending.get(t).copied(), ping.as_ref()))).collect();
        (map, pending.into_keys().collect(), supervised)
    }
}

/// Опрос раз в секунду. Окно с ядром — зеркало: состояние и события приходят от ядра (пинг и статистика — от агента,
/// `spawn_agent`), здесь только история для графика и трей. Ядро и демо-режим опрашивают сами и сами пишут события.
/// Статистики трафика у ядра нет: её ведёт агент по `State` ядра (`daemon::agent::stats`).
pub fn spawn(shared: Arc<Shared>, on_update: Box<dyn Fn() + Send>) {
    crate::crash::spawn_named("monitor", move || {
        let mut watch = Watch::default();
        let mut link = CoreLink::default();
        loop {
            let host = shared.host();
            match &host {
                Some(host) => poll(&shared, host.as_ref()),
                None => mirror_core(&shared, &mut link),
            }
            let core_lost = lock(&shared.snapshot).core_lost;
            lock(&shared.core_seen).observe(core_lost, unix_now());
            let (levels, user, supervised) = shared.levels();
            react(&shared, &mut watch, &levels, &user, &supervised, host.is_some());
            on_update();
            std::thread::sleep(if link.lost { RETRY_PERIOD } else { PERIOD });
        }
    });
}

/// Окно: опрос агента раз в секунду, в своём потоке — зависший агент не задерживает состояние ядра. `on_native_ui`
/// получает отметку последней работы с MSI AmneziaWG из каждого опроса (`mirror_agent`); по ней окно открывает снова
/// окно AmneziaWG, закрытое MSI, — и тогда, когда скрыто в трее и кадры не рисуются.
pub fn spawn_agent(
    shared: Arc<Shared>,
    agent: Arc<dyn AgentApi>,
    on_update: Box<dyn Fn() + Send>,
    mut on_native_ui: Box<dyn FnMut(Option<NativeUiMark>) + Send>,
) {
    crate::crash::spawn_named("agent-monitor", move || loop {
        on_native_ui(mirror_agent(&shared, agent.as_ref()));
        mirror_agent_events(&shared, agent.as_ref());
        on_update();
        std::thread::sleep(PERIOD);
    });
}

/// Окно: забрать состояние у агента. Агент недоступен (перезапускается, ядро прежней версии без агента) — пинга и
/// статистики нет (пустые, а не прежние: устаревшие числа выглядели бы живыми), на месте пинга окно показывает
/// «вторичная служба недоступна». Ни «нет связи с ядром», ни записи в журнал: туннели
/// от агента не зависят, а его остановки пишет в журнал сторож ядра.
/// Возвращает отметку последней работы с MSI AmneziaWG; `None` — агент не ответил или прежней версии.
fn mirror_agent(shared: &Shared, agent: &dyn AgentApi) -> Option<NativeUiMark> {
    let (ping, stats, native_ui, down) = match agent.state() {
        Ok(state) => (crate::ping::PingState::from_dto(state.ping), state.stats, state.native_ui, false),
        Err(_) => (PingState { host: lock(&shared.options).ping_host.clone(), ..Default::default() }, Stats::default(), None, true),
    };
    *lock(&shared.ping) = ping;
    *lock(&shared.stats) = stats;
    shared.agent_down.store(down, Ordering::SeqCst);
    lock(&shared.agent_seen).observe(down, unix_now());
    native_ui
}

/// Уровни туннелей между опросами → события журнала.
#[derive(Default)]
struct Watch {
    /// Прошлые уровни; `None` — опросов ещё не было.
    prev: Option<BTreeMap<String, Level>>,
    /// Работает, но «подключён» о нём не писали: поднял надзор ядра, и о подключении пишет он сам.
    quiet: BTreeSet<String>,
}

impl Watch {
    /// События очередного опроса. `supervised` — туннели под надзором ядра (`daemon::retry`) и работает ли каждый.
    /// О них переходы молчат: жёлтое «переподключение» — не подключение, а подключение надзор подтверждает и пишет сам
    /// («подключён после N попыток», когда туннель проработал) — вторая строка «Подключён» была бы о том же. Пропажу
    /// туннеля, который работал не по надзору, пишут как обычно.
    fn step(&mut self, levels: &BTreeMap<String, Health>, supervised: &BTreeMap<String, bool>, user: &BTreeSet<String>, now: u64) -> Vec<Event> {
        let mut settled = BTreeMap::new();
        let mut silent = Vec::new();
        for (n, h) in levels {
            match supervised.get(n) {
                Some(true) => {
                    self.quiet.insert(n.clone());
                    silent.push((n.clone(), Level::Ok));
                }
                Some(false) if self.quiet.remove(n) => silent.push((n.clone(), Level::Off)),
                Some(false) => {
                    settled.insert(n.clone(), (Level::Off, h.log_text()));
                }
                None => {
                    self.quiet.remove(n);
                    // «Занят» (переключение, ожидание рукопожатия) — промежуточное состояние, событий не даёт.
                    if h.level != Level::Busy {
                        settled.insert(n.clone(), (h.level, h.log_text()));
                    }
                }
            }
        }
        // Первый опрос — точка отсчёта: туннель, уже поднятый при старте, не событие.
        let fresh = self.prev.as_ref().map(|prev| events::transitions(prev, &settled, user, now)).unwrap_or_default();
        let p = self.prev.get_or_insert_with(BTreeMap::new);
        p.extend(settled.into_iter().map(|(n, (level, _))| (n, level)));
        p.extend(silent);
        fresh
    }
}

/// События, уведомления и значок в трее по новым уровням.
/// `make_events` — события по переходам пишет тот, кто опрашивает сам (ядро, демо); окно-зеркало получает их от ядра.
fn react(
    shared: &Shared,
    watch: &mut Watch,
    levels: &BTreeMap<String, Health>,
    user: &BTreeSet<String>,
    supervised: &BTreeMap<String, bool>,
    make_events: bool,
) {
    let options = lock(&shared.options).clone();
    let fresh = watch.step(levels, supervised, user, unix_now());
    if make_events {
        record_and_notify(&shared.events, fresh, options.notify && options.tray, &mut |e| toast(e));
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
            // Короткое состояние без подробностей; лишние туннели — числом (`fit_tip`).
            tray::fit_tip(&active.iter().map(|(n, h)| format!("{n}: {}", h.text)).collect::<Vec<_>>())
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

    let error = [configs.as_ref().err(), running.as_ref().err()]
        .into_iter()
        .flatten()
        .map(|e| e.to_string())
        .reduce(|a, b| format!("{a}; {b}"));
    set_poll_error(shared, error);

    let now = Instant::now();
    let mut snap = lock(&shared.snapshot);
    if let Ok(configs) = configs {
        snap.tunnels = configs.into_iter().collect();
    }
    let mut next = BTreeMap::new();
    for (name, result) in queried {
        snap.tunnels.insert(name.clone());
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

/// Ошибка опроса. Строка состояния показывает только ссылку на журнал, поэтому текст — в журнал событий, один раз,
/// пока он тот же (опрос идёт каждую секунду). Ошибку, пришедшую от ядра в `CoreState`, окно не пишет: её записало ядро.
fn set_poll_error(shared: &Shared, error: Option<String>) {
    let fresh = {
        let mut snap = lock(&shared.snapshot);
        let fresh = error.is_some() && snap.error != error;
        snap.error = error.clone();
        fresh
    };
    if let (true, Some(e)) = (fresh, error) {
        shared.log("", Severity::Warn, &crate::explain::log_line(&tr("ev.poll_failed"), &e));
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
            lock(&shared.snapshot).core_lost = false;
            set_poll_error(shared, Some(trf("err.core_unexpected", &[&crate::explain::variant_name(&other)])));
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
        snap.retries = state.retries;
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
    *lock(&shared.service) = state.service;
    *lock(&shared.core_mode) = state.mode;
    // Уведомления — только о новых событиях, не о тех, что пришли первой порцией при открытии окна;
    // а после перезапуска ядра курсор переставляется и новые события приходят со следующим опросом.
    // Место в журнале ядра — при каждом событии: то же событие придёт и от агента, повтор не показывается.
    let instance = state.events_instance;
    let batch = lock(&shared.core_cursor).accept(instance, state.events_loaded, state.events);
    let events = batch.events.into_iter().map(|(seq, e)| e.with_origin(events::Origin { instance, seq }));
    let options = lock(&shared.options).clone();
    record_and_notify(&shared.events, events, !batch.quiet && options.notify && options.tray, &mut |e| toast(e));
}

/// Окно: журнал агента. Первый ответ — история (файл журнала) в начало журнала окна, без уведомлений; дальше —
/// новые события агента (обновления) и события ядра, которых окно ещё не видело. Агент недоступен или прежней
/// версии (без `Events`) — журнал окна только от ядра; о недоступности агента окно уже говорит (`agent_down`).
fn mirror_agent_events(shared: &Shared, agent: &dyn AgentApi) {
    let after = lock(&shared.agent_cursor).after();
    let Ok(answer) = agent.events(after) else { return };
    let batch = lock(&shared.agent_cursor).accept(answer.instance, answer.loaded, answer.events);
    let events: Vec<Event> = batch.events.into_iter().map(|(_, e)| e).collect();
    if batch.quiet {
        // Пустой «тихий» ответ — и перезапуск агента (курсор переставлен): историю окно уже показало.
        if !events.is_empty() {
            lock(&shared.events).merge_history(events, answer.core);
        }
        return;
    }
    let options = lock(&shared.options).clone();
    record_and_notify(&shared.events, events, options.notify && options.tray, &mut |e| toast(e));
}

/// Показать событие уведомлением Windows.
fn toast(e: &Event) {
    tray::notify_tunnel(&e.tunnel, crate::APP_TITLE, &format!("{} — {}", e.tunnel, e.text), e.severity != Severity::Info);
}

/// Записать события в журнал, а уведомить о них — уже после того, как блокировка журнала отпущена: `notify` идёт
/// в Explorer и может ждать его сколько угодно, а журнал на это время читает поток окна на каждом кадре.
/// `notifier` — вызов оболочки (в тесте подменяется).
fn record_and_notify(log: &Mutex<EventLog>, events: impl IntoIterator<Item = Event>, enabled: bool, notifier: &mut dyn FnMut(&Event)) {
    let mut to_show = Vec::new();
    {
        let mut log = lock(log);
        for e in events {
            let shown = enabled && e.notify;
            // Событие ядра, уже пришедшее другим путём (от ядра или от агента), — без повтора и без второго уведомления.
            if log.push_unique(e.clone()) && shown {
                to_show.push(e);
            }
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

    #[test]
    fn stamp_moves_only_when_the_state_changes() {
        let mut s = Stamp::default();
        s.observe(false, 100);
        s.observe(false, 200);
        assert_eq!(s, Stamp { down: false, since: 100 }, "то же состояние — время прежнее");
        s.observe(true, 300);
        s.observe(true, 400);
        assert_eq!(s, Stamp { down: true, since: 300 });
        s.observe(false, 500);
        assert_eq!(s, Stamp { down: false, since: 500 });
    }

    fn event(text: &str, notify: bool) -> Event {
        Event::new(1, "a", Severity::Warn, text, notify)
    }

    /// Живой прогон пропадания питания: ядро поднялось, туннель под надзором — в журнале было «Подключён», пока служба
    /// стояла (жёлтое «переподключение» считалось подключением), и ещё раз — рядом со строкой надзора об успехе.
    #[test]
    fn supervised_reconnect_has_no_connected_line_of_its_own_but_a_real_drop_is_still_logged() {
        let one = |level| BTreeMap::from([("a".to_string(), Health::new(level, "t".into()))]);
        let none = BTreeMap::new();
        let sup = |live| BTreeMap::from([("a".to_string(), live)]);
        let user = BTreeSet::new();
        let texts = |ev: Vec<Event>| ev.into_iter().map(|e| e.text).collect::<Vec<_>>();
        let mut w = Watch::default();
        assert!(w.step(&one(Level::Off), &none, &user, 1).is_empty(), "точка отсчёта");
        // Под надзором и не работает: «переподключение», попытка, неудача — не «Подключён».
        for level in [Level::Warn, Level::Busy, Level::Warn] {
            assert_eq!(texts(w.step(&one(level), &sup(false), &user, 2)), Vec::<String>::new(), "{level:?}");
        }
        // Вторая попытка подняла, надзор подтверждает и пишет сам; после снятия надзора монитор тоже молчит.
        for (level, s) in [(Level::Busy, sup(true)), (Level::Busy, none.clone()), (Level::Ok, none.clone())] {
            assert_eq!(texts(w.step(&one(level), &s, &user, 3)), Vec::<String>::new(), "{level:?}");
        }
        // Упал сам — «Туннель отключился», даже если надзор взял его раньше этого опроса.
        assert_eq!(texts(w.step(&one(Level::Warn), &sup(false), &user, 4)), [tr("ev.dropped")]);
        // Поднят попыткой и встал до подтверждения: это неудачная попытка в строке надзора, не падение.
        assert!(w.step(&one(Level::Busy), &sup(true), &user, 5).is_empty());
        assert!(w.step(&one(Level::Warn), &sup(false), &user, 6).is_empty());
        // Без надзора — как прежде.
        assert_eq!(texts(w.step(&one(Level::Ok), &none, &user, 7)), [tr("ev.connected")]);
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

    /// Агента нет (перезапуск, ядро прежней версии): пинг пуст, отмечено «агент недоступен», и только это — ни
    /// «нет связи с ядром», ни записи в журнал, ни ошибки в строке состояния.
    #[test]
    fn absent_agent_shows_no_ping_and_no_error() {
        use crate::daemon::agent::client::FakeAgent;
        let shared = Shared::new(None, Options { ping: true, ping_host: "1.1.1.1".into(), notify: true, tray: true, taskbar: false }, None);
        *lock(&shared.snapshot) = snapshot_with_running();
        shared.update_ping(|p| p.fails = 5);
        shared.update_stats(|s| {
            s.insert("a".into(), Default::default());
        });
        mirror_agent(&shared, &FakeAgent::unreachable("pipe: not found"));
        let view = shared.frame_view();
        assert!(view.agent_down);
        assert!(view.stats.is_empty(), "без агента статистика пустая, а не прежняя");
        assert_eq!((view.ping.host.as_str(), view.ping.last.clone(), view.ping.fails, view.ping.history.len()), ("1.1.1.1", None, 0, 0));
        assert!(!view.snap.core_lost && view.snap.error.is_none());
        drop(view);
        assert!(lock(&shared.events).items.is_empty(), "в журнал окна — ничего");
        let without_ping = lock(&shared.snapshot).health("a", None, None).level;
        assert_eq!(shared.levels().0["a"].level, without_ping, "прежние неудачи пинга не портят здоровье туннеля");
    }

    #[test]
    fn agent_state_brings_the_ping_and_stats_and_clears_the_mark() {
        use crate::daemon::agent::client::FakeAgent;
        use crate::daemon::agent::proto::{AgentResponse, AgentState};
        use crate::daemon::proto::PingDto;
        let shared = Shared::new(None, Options { ping: true, ping_host: "1.1.1.1".into(), notify: false, tray: false, taskbar: false }, None);
        mirror_agent(&shared, &FakeAgent::unreachable("pipe: not found"));
        let ping = PingDto { host: "9.9.9.9".into(), last: Some(Ok(21)), fails: 0, history: vec![(1.0, Some(21))] };
        let mut office = crate::stats::TunnelStats::default();
        office.observe(4_096, 512, 1, None, 1);
        let stats: Stats = [("office".to_string(), office)].into();
        let agent = FakeAgent(Box::new(move |_| Ok(AgentResponse::State(Box::new(AgentState { ping: ping.clone(), stats: stats.clone(), ..Default::default() })))));
        mirror_agent(&shared, &agent);
        let view = shared.frame_view();
        assert!(!view.agent_down);
        assert_eq!((view.ping.host.as_str(), view.ping.last.clone(), view.ping.history.len()), ("9.9.9.9", Some(Ok(21)), 1));
        assert_eq!((view.stats["office"].rx, view.stats["office"].tx), (4_096, 512), "статистика — из состояния агента");
    }

    /// Журнал окна из двух источников: история агента — в начало, события ядра, пришедшие и от ядра, и от агента, —
    /// по одному разу; события самого агента (обновления) видны на ходу, без перезапуска ядра.
    #[test]
    fn window_journal_merges_agent_history_and_core_events_without_duplicates() {
        use crate::daemon::agent::client::FakeAgent;
        use crate::daemon::agent::proto::{AgentEvents, AgentResponse};
        use events::Origin;
        let shared = shared();
        let at = |instance, seq| Origin { instance, seq };
        let core_event = |seq: u64| Event::new(seq, "t", Severity::Info, &format!("core {seq}"), true);
        // Ядро ответило раньше агента: всё его кольцо (номера 5..=7) — первой, тихой порцией.
        let first = (5..=7).map(|n| core_event(n).with_origin(at(9, n)));
        record_and_notify(&shared.events, first, false, &mut |_| panic!("первая порция без уведомлений"));
        // История агента: старое из файла (до перезапуска ядра) и события ядра 5..=6 — 6 прочитано из файла без
        // метки (номера нет), но входит в «полно до 9/6».
        let history = vec![
            (1, Event::new(1, "t", Severity::Info, "old", false)),
            (2, core_event(5).with_origin(at(9, 5))),
            (3, core_event(6)),
        ];
        let answers = Arc::new(Mutex::new(vec![
            AgentEvents { instance: 4, loaded: 3, events: history, core: Some(at(9, 6)) },
            // Дальше: своё событие агента и событие ядра 7, которое окно уже получило от ядра.
            AgentEvents {
                instance: 4,
                loaded: 3,
                events: vec![(4, Event::new(8, "", Severity::Warn, "update failed", false)), (5, core_event(7).with_origin(at(9, 7)))],
                core: Some(at(9, 7)),
            },
        ]));
        let agent = FakeAgent(Box::new(move |_| Ok(AgentResponse::Events(Box::new(answers.lock().unwrap().remove(0))))));
        mirror_agent_events(&shared, &agent);
        mirror_agent_events(&shared, &agent);
        // Ядро повторяет 6 (окно спросило с прежнего номера) и шлёт новое 8.
        let late = [6, 8].map(|n| core_event(n).with_origin(at(9, n)));
        record_and_notify(&shared.events, late, false, &mut |_| {});
        let texts: Vec<String> = shared.events_since(0).into_iter().map(|(_, e)| e.text).collect();
        assert_eq!(texts, ["old", "core 5", "core 6", "core 7", "update failed", "core 8"]);
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
        let shared = Shared::new(None, Options { ping: false, ping_host: String::new(), notify: true, tray: true, taskbar: false }, None);
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

    /// Диск, который стоит, пока тест не отпустит (зависший диск, антивирус, занятый файл).
    struct StalledDisk(Arc<Mutex<()>>);

    impl events::Sink for StalledDisk {
        fn write(&mut self, _: &Event) -> Vec<String> {
            drop(lock(&self.0));
            Vec::new()
        }
    }

    #[test]
    fn log_does_not_wait_for_a_stalled_disk() {
        let shared = shared();
        let disk = Arc::new(Mutex::new(()));
        let stalled = lock(&disk);
        EventLog::start_writer(&shared.events, Box::new(StalledDisk(disk.clone())), 8);
        for i in 0..50 {
            let started = Instant::now();
            shared.log("a", Severity::Info, &format!("e{i}"));
            assert!(started.elapsed() < Duration::from_millis(10), "запись {i} ждала диск: {:?}", started.elapsed());
        }
        assert_eq!(shared.events_since(0).len(), 50);
        drop(stalled);
        let feed = lock(&shared.events).feed().expect("журнал с потоком записи");
        assert!(feed.flush(Duration::from_secs(10)), "диск отпущен — очередь дописана");
    }

    fn shared() -> Arc<Shared> {
        let options = Options { ping: false, ping_host: String::new(), notify: false, tray: false, taskbar: false };
        Arc::new(Shared::new(None, options, None))
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

    /// Переподключаемый туннель: строка и карточка показывают попытку, а не «отключён»; команда пользователя
    /// («Подключение…») и работающий туннель важнее; состояние уходит окну и попадает в выход с отключением.
    #[test]
    fn retrying_tunnel_is_shown_and_mirrored() {
        let shared = shared();
        *lock(&shared.snapshot) = snapshot_with_running();
        let retry = |slow| RetryState { attempt: 3, next_in_s: 10, last_error: "Element not found".into(), slow };
        shared.set_retries([("b".to_string(), retry(false)), ("c".to_string(), retry(true))].into());
        let snap = shared.snapshot_clone();
        assert_eq!(snap.health("b", None, None).text, trf("health.retrying", &["3", "10"]));
        assert_eq!(snap.health("b", None, None).level, Level::Warn);
        assert_eq!(snap.health("c", None, None).level, Level::Bad, "10 минут не помогли — ошибка с причиной");
        assert_eq!(snap.health("b", Some("busy.connect"), None).text, tr("busy.connect"), "идёт попытка — «Подключение…»");
        assert_eq!(snap.health("x", None, None).level, Level::Off);
        assert_eq!(shared.retrying_names(), ["b", "c"]);
        assert_eq!(shared.core_state(Mode::Engine, 0).retries["c"], retry(true));
        shared.reset_snapshot();
        assert!(shared.retrying_names().is_empty(), "смена режима — переподключать нечего");
    }

    /// Один туннель; список конфигов читается или нет — по флагу.
    struct OneTunnel {
        listed: bool,
    }

    impl TunnelHost for OneTunnel {
        fn configs(&self) -> std::io::Result<Vec<String>> {
            if self.listed {
                Ok(vec!["t".into()])
            } else {
                Err(std::io::Error::other("no list"))
            }
        }
        fn running(&self) -> std::io::Result<Vec<String>> {
            Ok(vec!["t".into()])
        }
        fn query(&self, _: &str) -> std::io::Result<Status> {
            Ok(Status::default())
        }
        fn connect(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn disconnect(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
    }

    /// Статистику ведёт агент: опрос ядра её не считает, а `State` ядра несёт пустую (поле — для окна прежней версии).
    #[test]
    fn core_poll_keeps_no_stats() {
        let shared = shared();
        poll(&shared, &OneTunnel { listed: true });
        poll(&shared, &OneTunnel { listed: false });
        let state = shared.core_state(Mode::Engine, 0);
        assert!(state.running.contains_key("t") && state.error.is_some());
        assert!(state.stats.is_empty());
        assert!(shared.update_stats(|s| s.is_empty()));
    }

    /// Строка состояния показывает только ссылку на журнал: текст ошибки опроса — в журнал, один раз на смену текста.
    #[test]
    fn poll_error_is_logged_once_per_change() {
        let shared = shared();
        let failures = |s: &Shared| s.events_since(0).into_iter().filter(|(_, e)| e.text.starts_with(&tr("ev.poll_failed"))).count();
        poll(&shared, &OneTunnel { listed: false });
        poll(&shared, &OneTunnel { listed: false });
        assert_eq!(failures(&shared), 1, "та же ошибка каждую секунду — одна запись");
        assert!(lock(&shared.snapshot).error.is_some(), "ссылка в строке состояния держится, пока ошибка есть");
        poll(&shared, &OneTunnel { listed: true });
        assert!(lock(&shared.snapshot).error.is_none());
        poll(&shared, &OneTunnel { listed: false });
        assert_eq!(failures(&shared), 2, "ошибка вернулась — новая запись");
        let line = shared.events_since(0).last().map(|(_, e)| e.text.clone()).unwrap();
        assert!(line.ends_with("(no list)"), "подробности в конце: {line}");
    }
}
