//! Работа ядра: опрос, пинг и статистика в фоне, ответы окну по каналу, смена режима на лету.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::budget::{Budgets, Limits};
use super::phase_trace::mark;
use super::pipe::Server;
use super::proto::{CoreState, NativeOp, Plan, Request, Response};
use super::agent_watch::{self, AgentCell};
use super::deadwatch::{self, DeadNote, Verdict};
use super::retry::{self, Note, Retries, Seen};
use super::{data_dir, Config, CoreApi, DATA_SDDL};
use crate::backend::{EngineHost, Real, TunnelHost, MANAGER_SERVICE};
use crate::crash::lock;
use crate::events::{Event, Severity};
use crate::i18n::{tr, trf};
use crate::monitor::{Options, PendingGuard, Shared};
use crate::settings::Mode;
use crate::store;

/// Сколько ждём, пока служба туннеля появится или исчезнет после команды.
const SWITCH_TIMEOUT: Duration = Duration::from_secs(15);
/// Одновременных соединений с окном больше этого — лишние получают отказ (память и потоки ядра не бесконечны).
const MAX_CONNECTIONS: usize = 32;
/// Своё малое число мест для клиентов-SYSTEM (второй процесс — агент): он ходит к ядру редко и короткими
/// запросами, 8 хватает с запасом. Предела «без границ», как было, нет: цикл с ошибкой в агенте иначе плодил бы
/// потоки ядра без счёта. Места отдельные от `MAX_CONNECTIONS`: зависший агент не отрезает окно от ядра, а 32
/// подключения программ владельца не отрезают агента.
const MAX_SYSTEM_CONNECTIONS: usize = 8;
/// Запас для проверки канала самим ядром при старте (`wait_listening`): Hello от SYSTEM принимается, даже когда
/// `MAX_SYSTEM_CONNECTIONS` заняты. Запас один, и на нём отвечают только на Hello — потоки по-прежнему ограничены.
const SELF_CHECK_CONNECTIONS: usize = 1;
const CORE_LIMITS: Limits = Limits { user: MAX_CONNECTIONS, system: MAX_SYSTEM_CONNECTIONS, self_check: SELF_CHECK_CONNECTIONS };

pub struct Core {
    shared: Arc<Shared>,
    config: Mutex<Config>,
    /// Куда записывается `config` при смене желаемого набора: `core.ini` ядра (в тестах — свой файл, не живой).
    /// `None` — только в памяти: тесты изоляции ядра (`tests::isolation_*`). Запись `core.ini` надёжная (сброс на диск
    /// дважды) и под нагрузкой на диск занимает больше секунды; их срок «Switch быстрее секунды» мерит ядро, а не диск.
    config_file: Option<std::path::PathBuf>,
    /// Смена режима, переключения туннелей и запросы, зависящие от режима, не пересекаются.
    switching: Mutex<()>,
    /// Занятые места соединений; место держит `budget::Slot`.
    connections: Budgets,
    /// Сведения о туннелях режима 1, прочитанные из родного окна: по ним видно, с кем туннель конфликтует.
    details: Mutex<HashMap<String, crate::conf::TunnelInfo>>,
    /// Надзор за желаемыми туннелями — единственный, кто их переподключает. Берётся после `switching`, не наоборот.
    retries: Mutex<Retries>,
    /// Пометки «занят» туннелей под арендой (`HoldNative`); срок аренды — в `retries`, пометка снимается вместе с ней.
    /// Берётся после `retries`, не наоборот.
    held: Mutex<HashMap<String, PendingGuard>>,
    /// Движок заменён: такт надзора переподключит туннели режима 2 (`ReconnectEngine`).
    engine_replaced: AtomicBool,
    /// Состояние второго процесса (агента); пишет только его сторож (`agent_watch`).
    agent: Arc<AgentCell>,
    /// До какого номера журнал ядра забрал агент: `events_after` его последнего `State` (`agent::journal`). На
    /// остановке ядра события после него дописываются в файл напрямую (`unpersisted_tail`).
    agent_took: AtomicU64,
}

/// Кто переключает туннель: пользователь (команда окна), надзор ядра, поднимая желаемый туннель (`retry`), или сторож
/// мёртвых туннелей, перезапуская работающий без связи (`deadwatch`).
#[derive(Clone, Copy, PartialEq)]
enum Origin {
    User,
    Retry,
    Dead,
}

/// Сколько ждём после старта, пока канал ядра ответит на собственный запрос; не ответил — ядро не поднялось.
const READY_TIMEOUT: Duration = Duration::from_secs(15);

/// Почему ядро не поднялось или остановилось с ошибкой.
#[derive(Debug, PartialEq)]
pub enum RunError {
    /// Канал так и не заработал, потому что его имя занял чужой канал. Сборка ядра тут ни при чём: перезапуск
    /// после обновления не должен принимать это за сломанную сборку и возвращать прежнюю (иначе любая программа
    /// учётной записи владельца, заняв имя, откатывала бы обновление без UAC).
    PipeTaken(String),
    Failed(String),
}

impl RunError {
    pub fn text(&self) -> &str {
        match self {
            RunError::PipeTaken(t) | RunError::Failed(t) => t,
        }
    }
}

impl From<String> for RunError {
    fn from(text: String) -> RunError {
        RunError::Failed(text)
    }
}

/// Запустить ядро и работать, пока не поднят `stop`. `ready` вызывается один раз — когда канал уже отвечает;
/// ошибка до этого — ядро не поднялось. `persist_tail` — на выходе (любом, после создания ядра): события, которых агент
/// не успел забрать; файл пишет вызвавший (служба), не ядро — у ядра нет записи в журнал на живых путях.
pub fn run(stop: &AtomicBool, ready: impl FnOnce(), persist_tail: impl FnOnce(Vec<Event>)) -> Result<(), RunError> {
    let dir = data_dir();
    crate::win::protect_dir(&dir, DATA_SDDL)?;
    let (config, unreadable) = Config::load_guarded();
    crate::i18n::set(&super::lang_dir(), &config.language);
    // После выбора языка (текст на нём) и до проверки владельца: без `core.ini` ядро дальше не пойдёт, и запись
    // в журнале — единственное, что объяснит, почему.
    if let Some(problem) = &unreadable {
        super::log_unreadable(problem);
    }
    if config.owner_sid.is_empty() {
        return Err(tr("core.no_owner").into());
    }
    // Пинг ведёт агент: у ядра он выключен, `State` несёт пустой.
    let options = Options { ping: false, ping_host: String::new(), notify: false, tray: false, taskbar: false };
    // Журнал ядра — только память: файл ведёт агент, забирая события по курсору из `State` (`agent::journal`).
    // Зависший диск так не держит ни блокировку журнала, ни остановку ядра.
    let shared = Arc::new(Shared::new(Some(host_for(config.mode)), options, None));
    let core = Arc::new(Core {
        shared,
        config: Mutex::new(config),
        config_file: Some(Config::path()),
        switching: Mutex::new(()),
        connections: Budgets::new(CORE_LIMITS),
        details: Mutex::default(),
        retries: Mutex::default(),
        held: Mutex::default(),
        engine_replaced: AtomicBool::new(false),
        agent: Arc::default(),
        agent_took: AtomicU64::new(0),
    });
    core.prepare_mode();
    let result = serve_until_stopped(&core, stop, ready, Endpoints::live());
    finish(&core, persist_tail);
    result
}

/// Остановка: агент забирает журнал ядра раз в секунду и гибнет вместе со службой — события последней секунды иначе
/// не дошли бы до `events.log`.
fn finish(core: &Core, persist_tail: impl FnOnce(Vec<Event>)) {
    let tail = core.unpersisted_tail();
    if !tail.is_empty() {
        persist_tail(tail);
    }
}

/// Имена и процессы, постоянные у живого ядра: канал ядра и агент. Тест изоляции ядра (`tests::isolation_*`)
/// подставляет свои — не трогая живую службу, её каналы и файлы.
struct Endpoints {
    pipe: String,
    agent: agent_watch::AgentSpec,
}

impl Endpoints {
    fn live() -> Endpoints {
        Endpoints { pipe: super::pipe::NAME.to_string(), agent: agent_watch::AgentSpec::live() }
    }
}

/// Работа ядра с подготовленным режимом: фоновые потоки, канал, агент — до `stop`.
fn serve_until_stopped(core: &Arc<Core>, stop: &AtomicBool, ready: impl FnOnce(), ends: Endpoints) -> Result<(), RunError> {
    crate::monitor::spawn(core.shared.clone(), Box::new(|| {}));
    // Надзор за желаемыми туннелями: его первый такт — восстановление после запуска ядра.
    let net_core = core.clone();
    super::netwatch::spawn(move |severity, text| net_core.shared.log("", severity, text));
    let supervisor = core.clone();
    // Запись хронометража теста (в сборке без тестов — ничего) идёт за потоками, через которые проходит Switch.
    let carry = super::phase_trace::Carry::here();
    let supervisor_carry = carry.clone();
    crate::crash::spawn_named("retry", move || {
        supervisor_carry.adopt();
        supervisor.supervise()
    });

    let server_core = core.clone();
    let taken = Arc::new(AtomicBool::new(false));
    let server_taken = taken.clone();
    let pipe = ends.pipe.clone();
    crate::crash::spawn_named("pipe-accept", move || {
        carry.adopt();
        serve(&server_core, &server_taken, &pipe)
    });
    if let Err(e) = wait_listening(&ends.pipe) {
        return Err(if taken.load(Ordering::SeqCst) { RunError::PipeTaken(e) } else { RunError::Failed(e) });
    }
    ready();
    // Агент — после того как ядро поднялось: туннели и канал ядра от него не зависят. Поток сторожа вторичный
    // (паника не останавливает ядро) и не берёт блокировок ядра — только журнал и свою ячейку состояния.
    let (agent, agent_log, spec) = (core.agent.clone(), core.shared.clone(), ends.agent);
    crate::crash::spawn_named("agent-watch", move || agent_watch::run(&agent, &|severity, text| agent_log.log("", severity, text), &spec));

    while !stop.load(Ordering::SeqCst) {
        // Фоновый поток упал (причина уже в журнале) — ядро без него полуживое: остановиться с кодом сбоя, чтобы
        // диспетчер служб перезапустил службу.
        if crate::crash::core_failure().is_some() {
            return Err(RunError::Failed(tr("core.thread_failed")));
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    Ok(())
}

/// Цикл канала: ждать клиента и отвечать ему в своём потоке. `taken` — последняя попытка упёрлась в чужой канал с
/// тем же именем (для `run`: почему канал не заработал).
fn serve(core: &Arc<Core>, taken: &AtomicBool, name: &str) {
    let owner = lock(&core.config).owner_sid.clone();
    let (mut server, bad_owner) = Server::new(name, &owner);
    if let Some(bad) = bad_owner {
        core.shared.log("", Severity::Bad, &trf("core.bad_owner", &[&bad]));
    }
    let mut errors = AcceptErrors::default();
    loop {
        let mut conn = match server.accept() {
            Ok(conn) => conn,
            Err(e) => {
                taken.store(e.taken, Ordering::SeqCst);
                let (log, pause) = errors.failed(Instant::now(), &e.text);
                if let Some(text) = log {
                    core.shared.log("", Severity::Bad, &text);
                }
                std::thread::sleep(pause);
                continue;
            }
        };
        taken.store(false, Ordering::SeqCst);
        if errors.recovered() {
            core.shared.log("", Severity::Info, &tr("core.pipe_recovered"));
        }
        let Some((slot, hello_only)) = core.connections.admit(conn.from_system()) else {
            mark("accept: no connection slot, refused");
            // Не дошёл отказ — клиент сам увидит ошибку канала; ядру тут терять нечего.
            drop(conn.reply(&Response::Refused(tr("core.busy"))));
            continue;
        };
        mark("accept: slot admitted, starting request thread");
        let core = core.clone();
        let carry = super::phase_trace::Carry::here();
        crate::crash::spawn_named("pipe-request", move || {
            carry.adopt();
            mark("request: thread started");
            // Место возвращается при выходе из потока при любом исходе.
            let _slot = slot;
            let response = match conn.read() {
                Ok(req) if hello_only && !matches!(req, Request::Hello) => Response::Refused(tr("core.busy")),
                Ok(req) => {
                    mark("request: read");
                    core.handle_isolated(req, conn.client_sid.as_deref(), conn.from_system())
                }
                Err(e) => Response::Refused(e),
            };
            mark("request: handled");
            // Не дошёл ответ (клиент ушёл или не забирает его в срок) — клиент сам увидит ошибку канала.
            drop(conn.reply(&response));
            mark("request: replied");
        });
    }
}

/// Пауза перед новой попыткой после первой ошибки ожидания клиента; дальше она удваивается до `ACCEPT_PAUSE_MAX`.
const ACCEPT_PAUSE_MIN: Duration = Duration::from_secs(1);
const ACCEPT_PAUSE_MAX: Duration = Duration::from_secs(60);
/// Одна и та же ошибка подряд пишется в журнал не чаще этого.
const ACCEPT_REPEAT_LOG: Duration = Duration::from_secs(3600);

/// Ошибки ожидания клиента подряд. Стойкая ошибка (имя канала заняла другая программа, кончились ресурсы)
/// повторялась бы каждую секунду: 86 400 строк в сутки в журнале, и из памяти ядра за минуты ушли бы все настоящие
/// события. Поэтому пауза между попытками растёт, а в журнал идёт новая ошибка и раз в час — что она повторяется.
#[derive(Default)]
struct AcceptErrors {
    pause: Duration,
    /// Последняя записанная ошибка и когда.
    logged: Option<(String, Instant)>,
    /// Повторов той же ошибки после её последней записи.
    repeats: u32,
}

impl AcceptErrors {
    /// Очередная ошибка: что записать в журнал (None — ничего) и сколько ждать до следующей попытки.
    fn failed(&mut self, now: Instant, text: &str) -> (Option<String>, Duration) {
        self.pause = if self.pause.is_zero() { ACCEPT_PAUSE_MIN } else { (self.pause * 2).min(ACCEPT_PAUSE_MAX) };
        let log = match &self.logged {
            Some((last, at)) if last == text && now.duration_since(*at) < ACCEPT_REPEAT_LOG => {
                self.repeats += 1;
                None
            }
            Some((last, _)) if last == text => Some(trf("core.accept_repeats", &[&(self.repeats + 1).to_string(), text])),
            _ => Some(text.to_string()),
        };
        if log.is_some() {
            self.logged = Some((text.to_string(), now));
            self.repeats = 0;
        }
        (log, self.pause)
    }

    /// Клиент принят. Да — перед этим были ошибки (стоит записать, что канал снова работает).
    fn recovered(&mut self) -> bool {
        let was = self.logged.is_some();
        *self = AcceptErrors::default();
        was
    }
}

/// Ядро слушает канал: запрос Hello к самому себе прошёл весь путь (клиент заодно проверяет, что владелец канала —
/// SYSTEM) и ответила эта же версия, а не чужой канал с тем же именем.
fn wait_listening(name: &str) -> Result<(), String> {
    let until = Instant::now() + READY_TIMEOUT;
    loop {
        let last = match PipeAt(name).hello() {
            Ok((version, _)) if version == env!("CARGO_PKG_VERSION") => return Ok(()),
            Ok((version, _)) => format!("core {version}"),
            Err(e) => e,
        };
        if Instant::now() >= until {
            return Err(format!("core pipe not ready: {last}"));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Клиент канала ядра с этим именем (у живого ядра — `pipe::NAME`, как у `PipeClient`).
struct PipeAt<'a>(&'a str);

impl CoreApi for PipeAt<'_> {
    fn call(&self, req: Request) -> Result<Response, String> {
        super::pipe::call_to(self.0, &req, super::pipe::Timeouts::CORE)
    }
}

fn host_for(mode: Mode) -> Arc<dyn TunnelHost> {
    match mode {
        Mode::Overlay => Arc::new(Real::new()),
        Mode::Engine => Arc::new(EngineHost),
    }
}


fn done(r: Result<(), String>) -> Response {
    r.map_or_else(Response::Err, |()| Response::Ok)
}

impl Core {
    fn mode(&self) -> Mode {
        lock(&self.config).mode
    }

    /// Хост туннелей текущего режима. `run` создаёт `Shared` ядра с хостом, `set_mode` только заменяет его — `None`
    /// значит ошибку в программе; она уходит ответом и в журнал вызывающего, а не паникой службы.
    fn host(&self) -> Result<Arc<dyn TunnelHost>, String> {
        self.shared.host().ok_or_else(|| "core: no tunnel host".to_string())
    }

    /// Подготовка режима: режим 2 — права хранилища и уборка; режим 1 — служба менеджера AmneziaWG запущена.
    fn prepare_mode(&self) {
        match self.mode() {
            Mode::Engine => {
                if let Err(e) = store::startup() {
                    self.shared.log("", Severity::Bad, &e);
                }
                // Службы прежних версий перезапускались Windows при сбое — теперь повторы только у надзора (`retry`).
                for (tunnel, e) in crate::engine::clear_restart_on_failure() {
                    self.shared.log(&tunnel, Severity::Warn, &trf("core.failure_actions_left", &[&e]));
                }
                self.shared.set_service(String::new());
            }
            Mode::Overlay => {
                let text = match crate::win::ensure_service(MANAGER_SERVICE) {
                    Ok(false) => trf("service.running", &[MANAGER_SERVICE]),
                    Ok(true) => {
                        self.shared.log("", Severity::Info, &trf("service.started", &[MANAGER_SERVICE]));
                        trf("service.started", &[MANAGER_SERVICE])
                    }
                    Err(e) => {
                        self.shared.log("", Severity::Bad, &e);
                        e
                    }
                };
                self.shared.set_service(text);
            }
        }
    }

    /// Запрос окна: паника в нём — ошибка этого запроса (в журнал и окну), а не остановка ядра.
    fn handle_isolated(&self, req: Request, caller: Option<&str>, system: bool) -> Response {
        crate::crash::isolate(|| self.handle(req, caller, system)).unwrap_or_else(|panic| {
            self.shared.log("", Severity::Bad, &trf("core.request_failed", &[&panic]));
            Response::Err(trf("core.request_failed", &[&panic]))
        })
    }

    /// `system` — клиент канала работает от SYSTEM (`ServerConn::from_system`): только ему открыты внутренние запросы.
    fn handle(&self, req: Request, caller: Option<&str>, system: bool) -> Response {
        // Снять туннели с надзора, дёрнуть переподключение или убрать туннель из желаемого набора может только служба:
        // окно (любая программа владельца) так оставляло бы желаемые туннели без надзора.
        if internal_request(&req) && !system {
            self.shared.log("", Severity::Warn, &trf("core.internal_refused", &[caller.unwrap_or("?")]));
            return Response::Refused(tr("core.internal_only"));
        }
        // Имя туннеля из запроса становится частью путей и командных строк — только допустимые имена.
        if let Some(bad) = names_in(&req).into_iter().find(|n| !crate::engine::valid_name(n)) {
            return Response::Refused(trf("eng.bad_name", &[bad]));
        }
        match req {
            Request::Hello => Response::Hello { version: env!("CARGO_PKG_VERSION").into(), mode: self.mode() },
            Request::State { events_after } => {
                self.note_agent_position(events_after, system);
                Response::State(Box::new(self.state(events_after)))
            }
            Request::Switch { tunnel, plan, multiple } => done(self.switch(&tunnel, plan, multiple)),
            Request::SetMode(mode) => done(self.set_mode(mode)),
            // Окно прежней версии: пинг теперь у агента, а не у ядра.
            Request::SetPing { .. } => Response::Refused(tr("core.ping_moved")),
            Request::SetLanguage(code) => done(self.set_language(code)),
            Request::Retry(tunnel) => done(self.retry(&tunnel)),
            Request::HoldNative { lease_s } => self.hold_native(Duration::from_secs(lease_s)).map_or_else(Response::Err, Response::Held),
            Request::Release { tunnels } => done(self.release(&tunnels)),
            Request::ReconnectEngine => {
                self.request_engine_reconnect();
                Response::Ok
            }
            Request::Forget(tunnel) => {
                lock(&self.details).remove(&tunnel);
                self.forget(&tunnel);
                Response::Ok
            }
            Request::Footprint { tunnel, info } => {
                let mut details = lock(&self.details);
                match info {
                    Some(info) => details.insert(tunnel, info),
                    None => details.remove(&tunnel),
                };
                Response::Ok
            }
            // Окно прежней версии: обновления ведёт агент; его менеджер просит ядро только `HoldNative`/`Release`/
            // `ReconnectEngine` (выше).
            Request::Updates(_) => Response::Refused(tr("core.updates_moved")),
            // Удаление туннеля режима 2 убирает его службу — оно остаётся в ядре, под `switching`: режим не меняется
            // посреди удаления. Удаление в родном окне (режим 1) делает агент и сообщает ядру `Forget`.
            Request::Delete(t) => {
                let _guard = lock(&self.switching);
                if self.mode() != Mode::Engine {
                    return Response::Refused(tr("core.moved_to_agent"));
                }
                done(EngineHost.delete_tunnel(&t).inspect(|()| self.forget(&t)))
            }
            Request::Rename { old, new } => {
                let _guard = lock(&self.switching);
                let need_engine = if self.mode() == Mode::Engine { Ok(()) } else { Err(tr("core.only_engine")) };
                done(need_engine.and_then(|()| self.rename(&old, &new)))
            }
            // Окно прежней версии: помощник родного окна и хранилище режима 2 ведёт агент (`agent::tunnels`).
            Request::Read(_)
            | Request::Write { .. }
            | Request::Details(_)
            | Request::Import(_)
            | Request::ExportAll
            | Request::NewTunnel(_)
            | Request::TakeNative
            | Request::Native(_) => Response::Refused(tr("core.moved_to_agent")),
        }
    }

    /// Позиция агента в журнале ядра. Только SYSTEM: окно (не SYSTEM) читает журнал для себя, а пинг агента спрашивает
    /// `State` без событий (`u64::MAX`) — это не подтверждение. Позиция дальше последнего события ядра — метка
    /// прежнего экземпляра ядра (агент перезапустился со старой меткой, пока ядро запущено заново): она не говорит,
    /// что агент забрал события нынешнего, и запись стёрла бы из хвоста остановки всё, что агент ещё не получил.
    fn note_agent_position(&self, events_after: u64, system: bool) {
        if system && events_after != u64::MAX && events_after <= self.shared.with_events(|log| log.last_seq()) {
            self.agent_took.store(events_after, Ordering::SeqCst);
        }
    }

    /// События журнала ядра после последней позиции агента. Агент мог успеть записать часть из них (его последний
    /// ответ ещё не подтверждён следующим запросом): лучше строка дважды, чем ни одной.
    fn unpersisted_tail(&self) -> Vec<Event> {
        let after = self.agent_took.load(Ordering::SeqCst);
        self.shared.with_events(|log| log.since(after)).into_iter().map(|(_, event)| event).collect()
    }

    fn state(&self, events_after: u64) -> CoreState {
        let mut state = self.shared.core_state(self.mode(), events_after);
        state.agent = self.agent.get();
        state
    }

    /// Что занимает туннель: режим 2 — по конфигу из хранилища, режим 1 — по сведениям из родного окна, если они
    /// уже читались; у работающего добавляются маршруты из канала состояния.
    fn footprint(&self, name: &str) -> Option<crate::conf::Footprint> {
        let info = if self.mode() == Mode::Engine {
            // Конфиг не читается — занятые им адреса неизвестны, и отключать при подключении некого: сказать об этом.
            match store::read(&store::path(name)) {
                Ok(t) => Some(crate::conf::parse(&t)),
                Err(e) => {
                    self.shared.log(name, Severity::Warn, &trf("core.footprint_unread", &[&e.to_string()]));
                    None
                }
            }
        } else {
            lock(&self.details).get(name).cloned()
        };
        let mut fp = info.map(|i| crate::conf::Footprint::of(&i));
        if let Some(st) = self.shared.status_of(name) {
            fp.get_or_insert_with(Default::default).merge(crate::conf::Footprint::of_status(&st));
        }
        fp
    }

    /// Переключить туннель: без `multiple` сначала отключаются остальные подключённые, с `multiple` — только те,
    /// с кем он вместе работать не может. Пока идёт команда, туннель помечен занятым — отключение по команде не
    /// считается аварией.
    fn switch(&self, name: &str, plan: Plan, multiple: bool) -> Result<(), String> {
        self.switch_from(Origin::User, name, plan, multiple).map(drop)
    }

    /// Переключение по команде пользователя (запоминает желаемый набор и снимает с туннеля надзор) или попытка надзора
    /// (подключает, только если туннель всё ещё желаемый и не работает). `Ok(false)` — делать было нечего.
    fn switch_from(&self, origin: Origin, name: &str, plan: Plan, multiple: bool) -> Result<bool, String> {
        // Повторное нажатие, пока туннель ещё переключается, — ничего не делать.
        mark("switch: begin");
        if self.shared.is_pending(name) {
            return Ok(false);
        }
        mark("switch: pending checked, waiting for the switching lock");
        let _guard = lock(&self.switching);
        let _held = super::phase_trace::Span::new("switch: switching lock held", "switch: done, releasing the switching lock");
        // Список запущенных — у служб, а не из снимка опроса: только что подключённый туннель в снимок ещё не попал.
        let found = self.host().and_then(|b| b.running().map(|r| (b, r)).map_err(|e| e.to_string()));
        mark("switch: running tunnels listed");
        let (b, running) = match found {
            Ok(found) => found,
            // Ответ `Err` на `Switch` окно не показывает — оно ждёт эту ошибку в журнале ядра. Ошибку попытки надзора
            // пишет сам надзор (`supervise_tick`) по своему расписанию журнала.
            Err(e) => {
                if origin == Origin::User {
                    self.shared.log(name, Severity::Bad, &e);
                }
                return Err(e);
            }
        };
        // Под той же блокировкой, что и команды пользователя: отключённый им за время ожидания не поднимается обратно,
        // а взятый в аренду, пока попытка ждала блокировку (`hold`), не трогается.
        let is_running = running.iter().any(|r| r == name);
        let nothing_to_do = match origin {
            Origin::User => false,
            Origin::Retry => is_running,
            // Перезапуск мёртвого — только пока он работает: остановленный поднимает надзор повторов.
            Origin::Dead => !is_running,
        };
        if nothing_to_do || (origin != Origin::User && (!self.is_desired(name) || lock(&self.retries).is_held(name))) {
            return Ok(false);
        }
        // Перезапуск мёртвого трогает только его: с соседями он ужился, когда подключался.
        mark("switch: decided, finding conflicting tunnels");
        let others = if origin == Origin::Dead { Vec::new() } else { to_replace(name, plan, multiple, &running, |n| self.footprint(n)) };
        if origin == Origin::User {
            self.update_desired(name, |config| {
                config.tunnels = Some(super::restore::after_switch(config.tunnels.as_deref().unwrap_or_default(), name, plan, &others));
                if plan != Plan::Disconnect {
                    config.multiple = multiple;
                }
            });
            let mut retries = lock(&self.retries);
            mark("switch: desired set updated, retries lock held");
            std::iter::once(name).chain(others.iter().map(String::as_str)).for_each(|t| retries.forget(t));
            drop(retries);
            // Отключение того, что не работает и службы не имеет (переподключался, выход с отключением): снять
            // желание — и всё, отключать нечего (`/uninstalltunnelservice` на несуществующую службу дал бы ошибку).
            if plan == Plan::Disconnect && !running.iter().any(|r| r == name) && !b.service_exists(name) {
                return Ok(true);
            }
        }
        if multiple {
            for o in &others {
                self.shared.log(o, Severity::Info, &trf("core.replaced", &[name]));
            }
        }
        let label = match plan {
            Plan::Connect => "busy.connect",
            Plan::Disconnect => "busy.disconnect",
            Plan::Reconnect => "busy.reconnect",
        };
        // Отключаемые попутно — тоже «по команде», а не авария.
        mark("switch: marking tunnels busy");
        let _busy = self.shared.pending_guard(std::iter::once((name.to_string(), label)).chain(others.iter().map(|o| (o.clone(), "busy.disconnect"))));
        mark("switch: engine calls begin");
        let result = run_switch(b.as_ref(), name, plan, &others);
        mark("switch: engine calls done");
        if let (Origin::User, Err(e)) = (origin, &result) {
            self.shared.log(name, Severity::Bad, e);
        }
        result.map(|()| true)
    }

    /// Изменить желаемый набор туннелей и сохранить его. Не записался — набор в памяти всё равно новый (до перезапуска
    /// ядра он верен), а в журнале предупреждение: после перезагрузки поднимется прежний набор.
    fn update_desired(&self, tunnel: &str, change: impl FnOnce(&mut Config)) {
        mark("desired: waiting for the config lock");
        let mut config = lock(&self.config);
        mark("desired: config lock held");
        let mut next = config.clone();
        change(&mut next);
        if next == *config {
            return;
        }
        let saved = self.config_file.as_deref().map_or(Ok(()), |path| {
            let _write = super::phase_trace::Span::new("desired: config write and flush begin", "desired: config write and flush done");
            next.save_to(path)
        });
        *config = next;
        drop(config);
        if let Err(e) = saved {
            self.shared.log(tunnel, Severity::Warn, &trf("core.desired_unsaved", &[&e]));
        }
    }

    /// Вывести туннель из желаемого набора по решению надзора.
    fn drop_desired(&self, tunnel: &str) {
        self.update_desired(tunnel, |config| config.tunnels.iter_mut().for_each(|tunnels| tunnels.retain(|d| d != tunnel)));
    }

    fn is_desired(&self, name: &str) -> bool {
        lock(&self.config).tunnels.iter().flatten().any(|t| t == name)
    }

    /// Надзор за желаемыми туннелями (`retry`), такт раз в секунду, пока живёт ядро. Первый такт — восстановление после
    /// запуска. Набор неизвестен (обновление с версии без него) — сначала он берётся из работающих.
    fn supervise(&self) {
        if lock(&self.config).tunnels.is_none() {
            match self.host() {
                Ok(host) => self.adopt_running(host.as_ref()),
                Err(e) => self.shared.log("", Severity::Bad, &e),
            }
        }
        let mut net_seen = super::netwatch::changes();
        loop {
            let net = super::netwatch::changes();
            self.supervise_tick(Instant::now(), net != net_seen);
            net_seen = net;
            std::thread::sleep(retry::TICK);
        }
    }

    /// Один такт надзора: решения `Retries::tick`, попытки — обычным переключением, исходы — в журнал и окну.
    fn supervise_tick(&self, now: Instant, network_changed: bool) {
        // Замена движка — здесь, в потоке надзора: работа обновления переподключения не ждёт и блокировку туннелей
        // не держит, а такты не идут вперемешку с переподключением.
        if self.engine_replaced.swap(false, Ordering::SeqCst) {
            self.reconnect_engine();
        }
        let (desired, multiple) = {
            let config = lock(&self.config);
            (config.tunnels.clone().unwrap_or_default(), config.multiple)
        };
        // Хоста нет только при ошибке в программе: её уже пишет любой запрос окна (`host`), такт просто ждёт.
        let Ok(host) = self.host() else { return };
        let running = host.running().ok();
        let tick = lock(&self.retries).tick(
            now,
            &Seen {
                desired: &desired,
                running: running.as_deref(),
                // Пометку аренды надзор решает сам (`Retries::hold`): истёкшая аренда на этом же такте даёт попытку,
                // её пометка снимается после такта.
                pending: &|t| self.shared.is_pending(t) && !lock(&self.held).contains_key(t),
                service_exists: &|t| host.service_exists(t),
                // Список конфигов не прочитался — «есть»: туннель остаётся под надзором, ошибку его попытки журнал
                // покажет по расписанию; вывести из набора можно только то, чего нет точно.
                config_exists: &|t| host.configs().map_or(true, |c| c.iter().any(|n| n == t)),
                stop_reason: &|t| host.stop_reason(t),
                native_services: host.native_services(),
                network_changed,
            },
        );
        for t in &tick.outside {
            self.drop_desired(t);
            self.shared.log(t, Severity::Info, &trf("core.retry_outside", &[t]));
        }
        for t in &tick.gone {
            self.drop_desired(t);
            self.shared.log(t, Severity::Warn, &trf("core.lease_gone", &[t]));
        }
        for (t, note) in tick.notes {
            self.log_note(&t, note);
        }
        self.unmark_held(&tick.expired);
        for t in &tick.expired {
            self.shared.log(t, Severity::Warn, &trf("core.hold_expired", &[t]));
        }
        self.attempt(&tick.due, multiple);
        self.watch_dead(now, &desired, multiple);
        self.shared.set_retries(lock(&self.retries).view(Instant::now()));
    }

    /// Сторож мёртвых туннелей (`deadwatch`): по показаниям опроса — кто работает без связи; таких перезапускает
    /// обычным переключением (`Plan::Reconnect`), исходы — в счёт и журнал.
    fn watch_dead(&self, now: Instant, desired: &[String], multiple: bool) {
        let restarted = lock(&self.retries).dead.restarted_at();
        let now_unix = crate::monitor::unix_now();
        let verdicts: Vec<(String, Verdict)> = self.shared.with_snapshot(|snap| {
            snap.running
                .iter()
                .filter(|(t, _)| desired.contains(*t))
                .map(|(t, live)| (t.clone(), deadwatch::verdict(live, now, now_unix, restarted.get(t).copied())))
                .collect()
        });
        self.dead_round(now, desired, &verdicts, multiple);
    }

    /// Решения сторожа по вердиктам и перезапуски; отдельно от чтения снимка — проверяется тестами без опроса.
    fn dead_round(&self, now: Instant, desired: &[String], verdicts: &[(String, Verdict)], multiple: bool) {
        let tick = lock(&self.retries).dead_tick(now, desired, verdicts, &|t| self.shared.is_pending(t));
        for (t, note) in tick.notes {
            self.log_dead(&t, note);
        }
        for t in &tick.due {
            let result = self.switch_from(Origin::Dead, t, Plan::Reconnect, multiple);
            let note = lock(&self.retries).dead.restarted(t, Instant::now(), result);
            if let Some(note) = note {
                self.log_dead(t, note);
            }
        }
    }

    /// Запись сторожа мёртвых туннелей в журнал: обнаружение, перезапуски первых фаз, вход в 10-минутную фазу — с
    /// уведомлением, возврат связи.
    fn log_dead(&self, t: &str, note: DeadNote) {
        let failed = |error: Option<String>| error.map_or_else(String::new, |e| trf("core.dead_restart_failed", &[&e]));
        match note {
            DeadNote::Detected => self.shared.log(t, Severity::Warn, &trf("core.dead_detected", &[t])),
            DeadNote::Restarted { restart, error } => {
                self.shared.log(t, Severity::Warn, &(trf("core.dead_restarted", &[t, &restart.to_string()]) + &failed(error)))
            }
            DeadNote::Slow { restarts, error } => {
                self.shared.notify(t, Severity::Bad, &(trf("core.dead_slow", &[t, &restarts.to_string()]) + &failed(error)))
            }
            DeadNote::Recovered { restarts: 0 } => self.shared.log(t, Severity::Info, &trf("core.dead_back_alone", &[t])),
            DeadNote::Recovered { restarts } => self.shared.log(t, Severity::Info, &trf("core.dead_recovered", &[t, &restarts.to_string()])),
        }
    }

    /// Попытки надзора — обычным переключением ядра; исходы — в расписание и журнал.
    fn attempt(&self, tunnels: &[String], multiple: bool) {
        for t in tunnels {
            let result = self.switch_from(Origin::Retry, t, Plan::Connect, multiple);
            let note = lock(&self.retries).outcome(t, Instant::now(), result);
            if let Some(note) = note {
                self.log_note(t, note);
            }
        }
    }

    /// Аренда туннелей режима 1 на время установщика AmneziaWG (`HoldNative`), который убирает их службы. Набор решает
    /// ядро, а не держатель: у менеджера обновлений в агенте своих сведений о туннелях нет, и пустой набор от него
    /// оставлял MSI без аренды — надзор выводил туннели из желаемого набора, VPN не возвращался. В аренду идут
    /// работающие и желаемые: желаемый неработающий надзор иначе поднимал бы наперегонки с MSI. Режим 2 — пусто, его
    /// службы установщик не трогает. Список работающих не прочитался — ошибка без аренды: установщик не запускается.
    /// Под `switching`: идущее переключение доходит до конца, и набор не меняется между чтением и арендой.
    fn hold_native(&self, lease: Duration) -> Result<Vec<String>, String> {
        let _guard = lock(&self.switching);
        let host = self.host()?;
        if !host.native_services() {
            return Ok(Vec::new());
        }
        let mut tunnels = host.running().map_err(|e| trf("core.hold_no_running", &[&e.to_string()]))?;
        tunnels.extend(lock(&self.config).tunnels.clone().unwrap_or_default());
        tunnels.sort();
        tunnels.dedup();
        self.hold(&tunnels, lease);
        Ok(tunnels)
    }

    /// Аренда туннелей: на `lease` (не дольше `retry::MAX_LEASE`) надзор их не подключает и из набора не выводит, окно
    /// видит их занятыми. Только под `switching` (берёт вызывающий): пометку «занят» идущего переключения аренда не
    /// перекрывает.
    fn hold(&self, tunnels: &[String], lease: Duration) {
        lock(&self.retries).hold(tunnels, Instant::now(), lease);
        let mut held = lock(&self.held);
        for t in tunnels {
            held.entry(t.clone()).or_insert_with(|| self.shared.pending_guard([(t.clone(), "busy.update")]));
        }
        drop(held);
        let secs = lease.min(retry::MAX_LEASE).as_secs().to_string();
        for t in tunnels {
            self.shared.log(t, Severity::Info, &trf("core.hold", &[t, &secs]));
        }
        self.shared.set_retries(lock(&self.retries).view(Instant::now()));
    }

    /// Возврат аренды (`Release`): желаемые неработающие из возвращённых подключаются сразу, обычной попыткой надзора
    /// (под `switching`, с желаемым набором); не вышло — дальше по расписанию надзора. Сразу, а не на следующем такте:
    /// в режиме 1 такт счёл бы туннель без службы (её убрал установщик) отключённым в окне AmneziaWG.
    fn release(&self, tunnels: &[String]) -> Result<(), String> {
        let released = lock(&self.retries).release(tunnels);
        self.unmark_held(&released);
        if released.is_empty() {
            return Ok(());
        }
        let (desired, multiple) = {
            let config = lock(&self.config);
            (config.tunnels.clone().unwrap_or_default(), config.multiple)
        };
        // Список не прочитался — попытка всем желаемым из возвращённых: работающему она ничего не сделает
        // (`switch_from` проверит сам), а без расписания неработающий выпал бы из набора на ближайшем такте.
        let running = self.host().and_then(|h| h.running().map_err(|e| e.to_string())).unwrap_or_else(|e| {
            self.shared.log("", Severity::Warn, &e);
            Vec::new()
        });
        let due: Vec<String> = released.into_iter().filter(|t| desired.contains(t) && !running.contains(t)).collect();
        {
            let now = Instant::now();
            let mut retries = lock(&self.retries);
            due.iter().for_each(|t| retries.restart(t, now));
        }
        self.attempt(&due, multiple);
        self.shared.set_retries(lock(&self.retries).view(Instant::now()));
        Ok(())
    }

    /// Снять пометки «занят» аренды.
    fn unmark_held(&self, tunnels: &[String]) {
        let mut held = lock(&self.held);
        for t in tunnels {
            held.remove(t);
        }
    }

    /// `ReconnectEngine`: переподключение — на ближайшем такте надзора (`supervise_tick`), ответ — сразу.
    fn request_engine_reconnect(&self) {
        self.engine_replaced.store(true, Ordering::SeqCst);
    }

    /// Запись надзора в журнал. «Подключён» о туннеле под надзором пишет только он (`monitor` о нём молчит).
    fn log_note(&self, t: &str, note: Note) {
        match note {
            Note::Failed { attempt, error } => self.shared.log(t, Severity::Warn, &trf("core.retry_failed", &[t, &attempt.to_string(), &error])),
            Note::Slow { error } => self.shared.notify(t, Severity::Bad, &trf("core.retry_slow", &[t, &error])),
            Note::Connected { attempts: 0 } => self.shared.log(t, Severity::Info, &tr("ev.connected")),
            Note::Connected { attempts } => self.shared.log(t, Severity::Info, &trf("core.retry_connected", &[t, &attempts.to_string()])),
        }
    }

    /// «Повторить» из окна: расписание туннеля с начала, первая попытка — на ближайшем такте.
    fn retry(&self, name: &str) -> Result<(), String> {
        if !self.is_desired(name) {
            return Err(trf("core.retry_not_desired", &[name]));
        }
        lock(&self.retries).restart(name, Instant::now());
        self.shared.log(name, Severity::Info, &trf("core.retry_restarted", &[name]));
        Ok(())
    }

    /// Первый запуск после обновления с версии, которая набор не помнила (ключа `tunnels` в `core.ini` нет): желаемыми
    /// становятся туннели, работающие по истечении `ADOPT_AFTER` (службы успели подняться), иначе они не вернулись бы
    /// после первого же пропадания питания. Явно пустой набор сюда не попадает: он остаётся пустым.
    fn adopt_running(&self, host: &dyn TunnelHost) {
        std::thread::sleep(super::restore::ADOPT_AFTER);
        let _guard = lock(&self.switching);
        let running = match host.running() {
            Ok(running) => running,
            Err(e) => return self.shared.log("", Severity::Bad, &trf("core.desired_unsaved", &[&e.to_string()])),
        };
        let mut adopted = None;
        self.update_desired("", |config| {
            // Пользователь успел переключить туннель за время ожидания — набор уже его.
            adopted = super::restore::adopt(config, running);
        });
        if let Some(list) = adopted {
            self.shared.log("", Severity::Info, &trf("core.desired_adopted", &[&list.join(", ")]));
        }
    }

    /// Сменить режим: туннели прежнего режима отключаются, ядро берёт другой бэкенд; окно не перезапускается.
    fn set_mode(&self, mode: Mode) -> Result<(), String> {
        let _guard = lock(&self.switching);
        if mode == self.mode() {
            return Ok(());
        }
        if mode == Mode::Engine {
            crate::engine::installed_files_ok()?;
        }
        let old = self.host()?;
        let running = old.running().map_err(|e| e.to_string())?;
        // Отключение по смене режима — по команде, не авария (без тревожных событий и уведомлений).
        let disconnected = {
            let _busy = self.shared.pending_guard(running.iter().map(|t| (t.clone(), "busy.disconnect")));
            running.iter().try_for_each(|t| old.disconnect(t))
        };
        disconnected?;
        let mut config = lock(&self.config).clone();
        config.mode = mode;
        // Туннели прежнего режима отключены по команде — восстанавливать после перезапуска нечего.
        config.tunnels = Some(Vec::new());
        config.save()?;
        *lock(&self.config) = config;
        lock(&self.retries).clear();
        self.shared.set_host(host_for(mode));
        self.shared.reset_snapshot();
        self.prepare_mode();
        let name = tr(if mode == Mode::Engine { "mode.engine" } else { "mode.overlay" });
        self.shared.log("", Severity::Info, &trf("core.mode_switched", &[&name]));
        Ok(())
    }

    /// После замены движка: подключённые туннели режима 2 переподключаются (только в режиме 2). Помечены
    /// занятыми — отключение по команде, не авария.
    fn reconnect_engine(&self) {
        let _guard = lock(&self.switching);
        if self.mode() != Mode::Engine {
            return;
        }
        let found = self.host().and_then(|b| b.running().map(|r| (b, r)).map_err(|e| e.to_string()));
        let (b, running) = match found {
            Ok(found) => found,
            Err(e) => return self.shared.log("", Severity::Bad, &e),
        };
        let mut busy = self.shared.pending_guard(running.iter().map(|t| (t.clone(), "busy.reconnect")));
        for t in &running {
            if let Err(e) = run_switch(b.as_ref(), t, Plan::Reconnect, &[]) {
                self.shared.log(t, Severity::Bad, &e);
            }
            busy.release(t);
        }
    }

    /// Язык журнала — как у окна (файл .lng — из `lang_dir`). Агент подхватывает его из `core.ini` (`agent::language`).
    fn set_language(&self, code: String) -> Result<(), String> {
        if !crate::i18n::is_code(&code) {
            return Err(format!("language: {code}"));
        }
        crate::i18n::set(&super::lang_dir(), &code);
        let mut config = lock(&self.config);
        if config.language != code {
            config.language = code;
            config.save()?;
        }
        Ok(())
    }

    /// Переименование туннеля хранилища (отключённого). Статистику ведёт агент: ей о переименовании сообщает окно.
    fn rename(&self, old: &str, new: &str) -> Result<(), String> {
        if !crate::engine::valid_name(new) {
            return Err(trf("eng.bad_name", &[new]));
        }
        if self.shared.is_running(old) {
            return Err(trf("eng.rename_running", &[old]));
        }
        store::rename(old, new)?;
        // Желаемый, но упавший сам туннель после перезапуска поднимается под новым именем.
        self.update_desired(new, |config| config.tunnels.iter_mut().flatten().filter(|t| *t == old).for_each(|t| *t = new.to_string()));
        Ok(())
    }

    /// Удалённый туннель уходит из желаемого набора. Его статистику агенту велит забыть окно.
    fn forget(&self, tunnel: &str) {
        self.update_desired(tunnel, |config| config.tunnels.iter_mut().for_each(|tunnels| tunnels.retain(|t| t != tunnel)));
    }

}

/// Шаги переключения — одни для ядра и демо-ядра окна: отключить `others`, затем туннель (кроме `Connect`) и дождаться,
/// пока его служба исчезнет, затем подключить (кроме `Disconnect`) и дождаться, пока появится. Первая ошибка обрывает
/// шаги. Не дождались — не ошибка: команда прошла, а итог покажет опрос.
pub(crate) fn run_switch(host: &dyn TunnelHost, name: &str, plan: Plan, others: &[String]) -> Result<(), String> {
    // Ошибка опроса посреди ожидания — не итог: ждём дальше до срока, а исход покажет следующий опрос окна.
    let wait = |up: bool| {
        crate::fsutil::wait_until(SWITCH_TIMEOUT, Duration::from_millis(200), || host.running().map(|r| r.iter().any(|n| n == name) == up).unwrap_or(false));
    };
    others.iter().try_for_each(|o| host.disconnect(o))?;
    mark("engine: others disconnected");
    if plan != Plan::Connect {
        host.disconnect(name)?;
        mark("engine: disconnected, waiting for the service to go");
        wait(false);
        mark("engine: service gone (or wait timed out)");
    }
    if plan != Plan::Disconnect {
        host.connect(name)?;
        mark("engine: connected, waiting for the service to appear");
        wait(true);
        mark("engine: service up (or wait timed out)");
    }
    Ok(())
}

/// Какие из подключённых отключить перед подключением `name`: все остальные — если туннель один; с `multiple` —
/// только конфликтующие с ним (общий адрес или оба на весь трафик: Windows не даст им работать вместе).
pub(crate) fn to_replace(
    name: &str,
    plan: Plan,
    multiple: bool,
    running: &[String],
    footprint: impl Fn(&str) -> Option<crate::conf::Footprint>,
) -> Vec<String> {
    if plan == Plan::Disconnect {
        return vec![];
    }
    let others = running.iter().filter(|n| *n != name);
    if !multiple {
        return others.cloned().collect();
    }
    let Some(target) = footprint(name) else { return vec![] };
    others.filter(|n| footprint(n).is_some_and(|f| f.conflicts(&target))).cloned().collect()
}

/// Внутренние запросы: только от SYSTEM (второй процесс ядра), окну — отказ.
fn internal_request(req: &Request) -> bool {
    matches!(req, Request::HoldNative { .. } | Request::Release { .. } | Request::ReconnectEngine | Request::Forget(_) | Request::Footprint { .. })
}

/// Имена туннелей из запроса.
fn names_in(req: &Request) -> Vec<&str> {
    match req {
        Request::Switch { tunnel, .. } | Request::Write { tunnel, .. } => vec![tunnel],
        Request::Read(t) | Request::Delete(t) | Request::Details(t) | Request::NewTunnel(t) | Request::Retry(t) | Request::Forget(t) => vec![t],
        Request::Footprint { tunnel, .. } => vec![tunnel],
        Request::Rename { old, new } => vec![old, new],
        Request::Release { tunnels } => tunnels.iter().map(String::as_str).collect(),
        Request::Native(NativeOp::Edit(t)) => vec![t],
        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use super::super::proto::{AgentExit, AgentStatus};

    #[test]
    fn panic_mid_switch_leaves_no_tunnel_busy_and_lock_usable() {
        use crate::backend::Demo;
        let options = Options { ping: false, ping_host: String::new(), notify: false, tray: false, taskbar: false };
        let shared = Arc::new(Shared::new(Some(Arc::new(Demo::new())), options, None));
        let switching = Mutex::new(());
        let r = crate::crash::isolate(|| {
            let _guard = lock(&switching);
            let _busy = shared.pending_guard([("a".to_string(), "busy.connect"), ("b".to_string(), "busy.disconnect")]);
            assert!(shared.is_pending("a") && shared.is_pending("b"));
            panic!("backend bug");
        });
        assert!(r.is_err());
        assert!(!shared.is_pending("a") && !shared.is_pending("b"), "пометки «занят» сняты");
        assert!(switching.is_poisoned());
        drop(lock(&switching)); // следующий запрос проходит, а не паникует на отравленной блокировке
    }

    /// Стойкая ошибка канала не пишет событие каждую секунду: пауза растёт до минуты, та же ошибка — в журнал раз в
    /// час с числом повторов, новая — сразу; после восстановления — одна запись, и всё с начала.
    #[test]
    fn persistent_accept_error_is_logged_rarely_with_backoff() {
        let t0 = Instant::now();
        let mut errors = AcceptErrors::default();
        assert_eq!(errors.failed(t0, "taken"), (Some("taken".into()), ACCEPT_PAUSE_MIN));
        let mut pauses = Vec::new();
        let mut logged = 0;
        let mut now = t0;
        for _ in 0..100 {
            now += Duration::from_secs(30);
            let (log, pause) = errors.failed(now, "taken");
            logged += usize::from(log.is_some());
            pauses.push(pause);
        }
        assert_eq!(&pauses[..7], &[2, 4, 8, 16, 32, 60, 60].map(Duration::from_secs));
        assert!(pauses.iter().all(|&p| p <= ACCEPT_PAUSE_MAX));
        assert_eq!(logged, 0, "50 минут той же ошибки — ни одной новой записи");
        now += Duration::from_secs(3600);
        assert_eq!(errors.failed(now, "taken").0, Some(trf("core.accept_repeats", &["101", "taken"])));
        assert_eq!(errors.failed(now, "other").0, Some("other".into()), "другая ошибка — сразу");
        assert!(errors.recovered(), "были ошибки — запись о восстановлении");
        assert!(!errors.recovered(), "второй раз — нечего писать");
        assert_eq!(errors.failed(now, "other"), (Some("other".into()), ACCEPT_PAUSE_MIN), "после восстановления — с начала");
    }

    #[test]
    fn user_budget_is_enforced_and_unaffected_by_system_connections() {
        let b = Budgets::new(CORE_LIMITS);
        let system: Vec<_> = (0..MAX_SYSTEM_CONNECTIONS + SELF_CHECK_CONNECTIONS).map(|_| b.admit(true).expect("место SYSTEM")).collect();
        assert!(b.admit(true).is_none(), "SYSTEM сверх своего бюджета и запаса — отказ");
        let users: Vec<_> = (0..MAX_CONNECTIONS).map(|_| b.admit(false).expect("место окна при занятом SYSTEM")).collect();
        assert!(users.iter().all(|(_, hello_only)| !hello_only));
        assert!(b.admit(false).is_none(), "окну — не больше MAX_CONNECTIONS");
        drop(users);
        assert!(b.admit(true).is_none(), "освободившиеся места окна SYSTEM не достаются");
        drop(system);
    }

    #[test]
    fn system_budget_is_small_and_separate_from_the_user_one() {
        let b = Budgets::new(CORE_LIMITS);
        let users: Vec<_> = (0..MAX_CONNECTIONS).map(|_| b.admit(false).expect("место окна")).collect();
        assert!(b.admit(false).is_none());
        let system: Vec<_> = (0..MAX_SYSTEM_CONNECTIONS).map(|_| b.admit(true).expect("SYSTEM при занятых местах окна")).collect();
        assert!(system.iter().all(|(_, hello_only)| !hello_only), "свой бюджет — полноценные места");
        assert_eq!(MAX_SYSTEM_CONNECTIONS, 8);
        drop(users);
    }

    #[test]
    fn full_core_still_answers_its_own_startup_check() {
        let b = Budgets::new(CORE_LIMITS);
        let _users: Vec<_> = (0..MAX_CONNECTIONS).map(|_| b.admit(false).unwrap()).collect();
        let _system: Vec<_> = (0..MAX_SYSTEM_CONNECTIONS).map(|_| b.admit(true).unwrap()).collect();
        let (_slot, hello_only) = b.admit(true).expect("проверка канала проходит при всех занятых бюджетах");
        assert!(hello_only, "запас — только для Hello");
        assert!(b.admit(true).is_none(), "запас один: потоки по-прежнему ограничены");
    }

    /// Остановка ядра: события, которых агент ещё не забрал, уходят тому, кто пишет файл (служба); забранные — нет.
    /// Позиция — только от SYSTEM и не `u64::MAX` (пинг агента): окно журнал ядра не сохраняет.
    #[test]
    fn events_the_agent_did_not_take_are_handed_over_on_stop() {
        let core = engine_core(&[]);
        let base = core.shared.with_events(|log| log.since(0).last().map_or(0, |(seq, _)| *seq));
        for text in ["one", "two", "three"] {
            core.shared.log("", Severity::Info, text);
        }
        core.handle(Request::State { events_after: base + 1 }, None, true);
        core.handle(Request::State { events_after: u64::MAX }, None, true);
        core.handle(Request::State { events_after: base + 3 }, Some("S-1-5-21-1"), false);
        let mut sink = Vec::new();
        finish(&core, |tail| sink.extend(tail.into_iter().map(|e| e.text)));
        assert_eq!(sink, ["two", "three"]);

        core.handle(Request::State { events_after: base + 3 }, None, true);
        finish(&core, |tail| panic!("агент забрал всё, а отдано {}", tail.len()));
    }

    /// Агент перезапустился со старой меткой (экземпляр ядра другой, номера больше нынешних): его первый `State`
    /// не подтверждение, и если служба встанет до следующего опроса, события ядра всё равно уйдут в файл.
    #[test]
    fn position_of_a_previous_core_instance_does_not_hide_the_tail() {
        let core = engine_core(&[]);
        let base = core.shared.with_events(|log| log.last_seq());
        for text in ["one", "two"] {
            core.shared.log("", Severity::Info, text);
        }
        core.handle(Request::State { events_after: base + 1500 }, None, true);
        let mut sink = Vec::new();
        finish(&core, |tail| sink.extend(tail.into_iter().map(|e| e.text)));
        assert!(sink.ends_with(&["one".to_string(), "two".to_string()]), "{sink:?}");
        assert_eq!(sink.len() as u64, base + 2, "отдано всё: чужая позиция не принята");
    }

    /// Ядро режима 2 над демо-хостом: `desired` — желаемый набор (прочее — как после чистой установки).
    fn engine_core(desired: &[&str]) -> Arc<Core> {
        core_over(Arc::new(crate::backend::Demo::new()), desired)
    }

    /// Ядро режима 2 над хостом `host`.
    fn core_over(host: Arc<dyn TunnelHost>, desired: &[&str]) -> Arc<Core> {
        // Свой файл на каждое ядро теста: смена желаемого набора не пишет в `core.ini` живого ядра.
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let config_file = std::env::temp_dir().join(format!("awg-core-test-{}-{}.ini", std::process::id(), NEXT.fetch_add(1, Ordering::SeqCst)));
        core_saving_to(host, desired, Some(config_file))
    }

    /// Ядро теста; `config_file: None` — желаемый набор только в памяти.
    fn core_saving_to(host: Arc<dyn TunnelHost>, desired: &[&str], config_file: Option<std::path::PathBuf>) -> Arc<Core> {
        let options = Options { ping: false, ping_host: String::new(), notify: false, tray: false, taskbar: false };
        let shared = Arc::new(Shared::new(Some(host), options, None));
        let config = Config {
            mode: Mode::Engine,
            language: String::new(),
            owner_sid: String::new(),
            tunnels: Some(desired.iter().map(|t| t.to_string()).collect()),
            multiple: false,
        };
        Arc::new(Core {
            shared,
            config: Mutex::new(config),
            config_file,
            switching: Mutex::new(()),
            connections: Budgets::new(CORE_LIMITS),
            details: Mutex::default(),
            retries: Mutex::default(),
            held: Mutex::default(),
            engine_replaced: AtomicBool::new(false),
            agent: Arc::default(),
            agent_took: AtomicU64::new(0),
        })
    }

    fn calls(host: &Recorder) -> Vec<String> {
        lock(&host.calls).clone()
    }

    /// Туннель под арендой надзор не подключает и показывает занятым; возврат аренды подключает его сразу, обычной
    /// попыткой надзора.
    #[test]
    fn held_tunnel_is_skipped_by_supervise_tick_until_released() {
        let host = Arc::new(Recorder::native());
        let core = core_over(host.clone(), &["a"]);
        assert_eq!(core.hold_native(Duration::from_secs(900)), Ok(vec!["a".to_string()]));
        let t0 = Instant::now();
        for s in [0, 1, 11, 60, 600] {
            core.supervise_tick(t0 + Duration::from_secs(s), false);
        }
        assert!(calls(&host).is_empty(), "под арендой попыток нет: {:?}", calls(&host));
        assert!(core.shared.is_pending("a"), "туннель под арендой показан занятым");

        core.release(&["a".to_string()]).unwrap();
        assert_eq!(calls(&host), ["up a"], "возвращённый подключается сразу");
        assert!(!core.shared.is_pending("a"));
        core.release(&["a".to_string()]).unwrap();
        assert_eq!(calls(&host), ["up a"], "повторный возврат ничего не делает");
    }

    /// Мёртвый туннель перезапускается обычным переключением ядра — только он, работающий сосед не трогается; под
    /// арендой — не трогается вовсе. В журнале — обнаружение и перезапуск.
    #[test]
    fn dead_tunnel_is_restarted_alone_and_held_one_is_not() {
        let host = Arc::new(Recorder { running: Mutex::new(vec!["a".into(), "b".into()]), ..Default::default() });
        let core = core_over(host.clone(), &["a", "b"]);
        let desired = ["a".to_string(), "b".to_string()];
        let dead = [("a".to_string(), Verdict::Dead), ("b".to_string(), Verdict::Unknown)];
        let t0 = Instant::now();
        core.dead_round(t0, &desired, &dead, true);
        assert_eq!(calls(&host), ["down a", "up a"]);
        let texts: Vec<String> = core.shared.events_since(0).into_iter().map(|(_, e)| e.text).collect();
        assert_eq!(texts, [trf("core.dead_detected", &["a"]), trf("core.dead_restarted", &["a", "1"])]);
        core.dead_round(t0 + Duration::from_secs(30), &desired, &dead, true);
        assert_eq!(calls(&host).len(), 2, "следующий перезапуск — через минуту");

        let host = Arc::new(Recorder { running: Mutex::new(vec!["a".into()]), ..Recorder::native() });
        let core = core_over(host.clone(), &["a"]);
        assert_eq!(core.hold_native(Duration::from_secs(900)), Ok(vec!["a".to_string()]));
        core.dead_round(Instant::now(), &["a".to_string()], &[("a".to_string(), Verdict::Dead)], false);
        assert!(calls(&host).is_empty(), "под арендой не перезапускается");
    }

    /// Держатель пропал: аренда истекает на такте надзора, пометка снимается, попытка идёт сразу.
    #[test]
    fn expired_hold_is_unmarked_and_reconnected_by_the_tick() {
        // Конфиг у AmneziaWG есть, службы нет (её убрал установщик): истёкшая аренда проверяет конфиг.
        let host = Arc::new(Recorder { configs: Some(vec!["a".into()]), ..Recorder::native() });
        let core = core_over(host.clone(), &["a"]);
        assert_eq!(core.hold_native(Duration::from_secs(10)), Ok(vec!["a".to_string()]));
        let t0 = Instant::now();
        core.supervise_tick(t0, false);
        assert!(calls(&host).is_empty());
        core.supervise_tick(t0 + Duration::from_secs(11), false);
        assert_eq!(calls(&host), ["up a"]);
        assert!(!core.shared.is_pending("a"), "пометка аренды снята вместе с ней");
        assert!(core.is_desired("a"));
    }

    /// Аренда истекла, а у AmneziaWG туннеля больше нет: ядро выводит его из набора с предупреждением и не подключает.
    #[test]
    fn expired_hold_of_a_tunnel_without_config_drops_it_with_a_warning() {
        let host = Arc::new(Recorder { configs: Some(vec![]), ..Recorder::native() });
        let core = core_over(host.clone(), &["a"]);
        assert_eq!(core.hold_native(Duration::from_secs(10)), Ok(vec!["a".to_string()]));
        core.supervise_tick(Instant::now() + Duration::from_secs(11), false);
        assert!(calls(&host).is_empty(), "{:?}", calls(&host));
        assert!(!core.is_desired("a"));
        let gone = trf("core.lease_gone", &["a"]);
        assert!(core.shared.events_since(0).iter().any(|(_, e)| e.text == gone && e.severity == Severity::Warn));
    }

    /// Пинг ушёл к агенту: окно прежней версии получает понятный отказ, а State ядра несёт пустой пинг.
    #[test]
    fn set_ping_is_refused_with_a_clear_text() {
        let core = engine_core(&[]);
        let r = core.handle(Request::SetPing { enabled: true, host: "1.1.1.1".into() }, Some("S-1-5-21-1-2-3-1001"), false);
        assert!(matches!(&r, Response::Refused(t) if *t == tr("core.ping_moved")), "{r:?}");
        assert_eq!(core.state(0).ping, crate::daemon::proto::PingDto::default());
    }

    /// Внутренние запросы — только от SYSTEM: окно (любая программа владельца) получает отказ, и ничего не меняется.
    #[test]
    fn internal_requests_from_a_non_system_caller_are_refused() {
        let host = Arc::new(Recorder { running: Mutex::new(vec!["a".into()]), native: true, ..Default::default() });
        let core = core_over(host.clone(), &["a"]);
        let user = Some("S-1-5-21-1-2-3-1001");
        for req in [
            Request::HoldNative { lease_s: 900 },
            Request::Release { tunnels: vec!["a".into()] },
            Request::ReconnectEngine,
            Request::Forget("a".into()),
            Request::Footprint { tunnel: "a".into(), info: Some(crate::conf::TunnelInfo::default()) },
        ] {
            let r = core.handle(req, user, false);
            assert!(matches!(&r, Response::Refused(t) if *t == tr("core.internal_only")), "{r:?}");
        }
        assert!(!lock(&core.retries).is_held("a") && !core.shared.is_pending("a"));
        assert_eq!(lock(&core.config).tunnels.as_deref(), Some(&["a".to_string()][..]), "отклонённый Forget набор не меняет");
        assert!(lock(&core.details).is_empty(), "отклонённый Footprint сведений не кладёт");
        core.supervise_tick(Instant::now(), false);
        assert!(calls(&host).is_empty(), "отклонённый ReconnectEngine не переподключает");

        let r = core.handle(Request::HoldNative { lease_s: 900 }, None, true);
        assert!(matches!(&r, Response::Held(t) if *t == ["a"]), "{r:?}");
        assert!(lock(&core.retries).is_held("a"), "от SYSTEM аренда берётся");
        let r = core.handle(Request::Release { tunnels: vec![r"..\x".into()] }, None, true);
        assert!(matches!(r, Response::Refused(_)), "имена проверяются и у внутренних запросов: {r:?}");
    }

    /// Замена движка: запрос отвечает сразу, переподключение делает такт надзора, а не работа обновления.
    #[test]
    fn engine_reconnect_runs_on_the_supervisor_tick_not_in_the_caller() {
        let host = Arc::new(Recorder { running: Mutex::new(vec!["a".into()]), ..Default::default() });
        let core = core_over(host.clone(), &["a"]);
        let r = core.handle(Request::ReconnectEngine, None, true);
        assert!(matches!(r, Response::Ok), "{r:?}");
        assert!(calls(&host).is_empty(), "вызывающий переподключение не ждёт");
        core.supervise_tick(Instant::now(), false);
        assert_eq!(calls(&host), ["down a", "up a"]);
        core.supervise_tick(Instant::now(), false);
        assert_eq!(calls(&host), ["down a", "up a"], "один запрос — одно переподключение");
    }

    /// Повтор запроса агентом (отметка «переподключение не принято» пережила его смерть после ответа ядра) до такта
    /// надзора безвреден: два запроса — одно переподключение.
    #[test]
    fn repeated_engine_reconnect_before_the_tick_reconnects_once() {
        let host = Arc::new(Recorder { running: Mutex::new(vec!["a".into()]), ..Default::default() });
        let core = core_over(host.clone(), &["a"]);
        for _ in 0..2 {
            assert!(matches!(core.handle(Request::ReconnectEngine, None, true), Response::Ok));
        }
        core.supervise_tick(Instant::now(), false);
        core.supervise_tick(Instant::now(), false);
        assert_eq!(calls(&host), ["down a", "up a"]);
    }

    /// Менеджер обновлений агента (`PipeCore`) просит ядро по каналу от SYSTEM: аренда видна надзору, отпущенный
    /// туннель поднимается, замена движка ждёт такта надзора.
    #[test]
    fn agents_update_requests_reach_the_core_entry_points() {
        let host = Arc::new(Recorder::native());
        let core = core_over(host.clone(), &["a"]);
        let r = core.handle(Request::HoldNative { lease_s: 900 }, None, true);
        assert!(matches!(&r, Response::Held(t) if *t == ["a"]), "{r:?}");
        assert!(lock(&core.retries).is_held("a"));
        let r = core.handle(Request::Release { tunnels: vec!["a".into()] }, None, true);
        assert!(matches!(r, Response::Ok), "{r:?}");
        assert_eq!(calls(&host), ["up a"]);
        assert!(matches!(core.handle(Request::ReconnectEngine, None, true), Response::Ok));
        assert!(core.engine_replaced.load(Ordering::SeqCst));
    }


    /// Набор аренды на время установщика решает ядро: в режиме 1 — работающие и желаемые (желаемый упавший тоже, иначе
    /// надзор поднимал бы его наперегонки с MSI); в режиме 2 — ничего; список работающих не прочитался — ошибка, и
    /// ничего не взято (установщик не запустится).
    #[test]
    fn native_hold_set_is_decided_by_the_core() {
        let host = Arc::new(Recorder { running: Mutex::new(vec!["office".into(), "manual".into()]), native: true, ..Default::default() });
        let core = core_over(host.clone(), &["office", "home"]);
        assert_eq!(core.hold_native(Duration::from_secs(900)), Ok(vec!["home".to_string(), "manual".into(), "office".into()]));
        assert!(["home", "manual", "office"].iter().all(|t| lock(&core.retries).is_held(t) && core.shared.is_pending(t)));

        let core = core_over(Arc::new(Recorder { running: Mutex::new(vec!["a".into()]), ..Default::default() }), &["a"]);
        assert_eq!(core.hold_native(Duration::from_secs(900)), Ok(vec![]), "режим 2: службы установщик не трогает");
        assert!(!lock(&core.retries).is_held("a"));

        let core = core_over(Arc::new(Recorder { native: true, unreadable: true, ..Default::default() }), &["a"]);
        let r = core.hold_native(Duration::from_secs(900));
        assert!(r.as_ref().is_err_and(|e| e.contains("pipes unreadable")), "{r:?}");
        assert!(!lock(&core.retries).is_held("a") && !core.shared.is_pending("a"), "без списка аренды нет");
    }

    /// Агент удалил туннель в родном окне: ядро убирает его из желаемого набора (и из `core.ini`) и забывает его
    /// сведения — надзор больше не поднимает удалённый туннель.
    #[test]
    fn forget_from_the_agent_drops_the_tunnel_from_the_desired_set_and_details() {
        let core = core_over(Arc::new(Recorder::default()), &["a", "b"]);
        lock(&core.details).insert("a".into(), crate::conf::TunnelInfo::default());
        let r = core.handle(Request::Forget("a".into()), None, true);
        assert!(matches!(r, Response::Ok), "{r:?}");
        assert_eq!(lock(&core.config).tunnels.as_deref(), Some(&["b".to_string()][..]));
        assert!(!lock(&core.details).contains_key("a"));
        let file = core.config_file.as_deref().expect("core_over saves to a file");
        let saved = Config::from_ini(&crate::ini::Ini::load(file));
        assert_eq!(saved.tunnels.as_deref(), Some(&["b".to_string()][..]), "набор записан в файл ядра");
        std::fs::remove_file(file).unwrap();
    }

    /// Ядро без файла (тесты изоляции): смена желаемого набора — в памяти, без предупреждения «не записан» в журнале.
    #[test]
    fn core_without_a_file_keeps_the_desired_set_in_memory_silently() {
        let core = core_saving_to(Arc::new(Recorder::default()), &["a", "b"], None);
        let r = core.handle(Request::Forget("a".into()), None, true);
        assert!(matches!(r, Response::Ok), "{r:?}");
        assert_eq!(lock(&core.config).tunnels.as_deref(), Some(&["b".to_string()][..]));
        let unsaved = trf("core.desired_unsaved", &[""]);
        let unsaved = unsaved.trim_end_matches(|c: char| !c.is_alphanumeric());
        assert!(!journal(&core).iter().any(|t| t.contains(unsaved)), "{:?}", journal(&core));
    }

    /// Сведения из родного окна приходят от агента и решают, кого отключить при подключении с «несколько сразу»;
    /// `None` (конфиг изменён) их забывает.
    #[test]
    fn footprint_from_the_agent_feeds_conflict_detection() {
        let core = core_over(Arc::new(Recorder::default()), &[]);
        lock(&core.config).mode = Mode::Overlay;
        let full = |address: &str| crate::conf::parse(&format!("[Interface]\nAddress = {address}\n[Peer]\nAllowedIPs = 0.0.0.0/0\n"));
        for (tunnel, info) in [("a", full("10.1.0.2/32")), ("b", full("10.2.0.2/32"))] {
            let r = core.handle(Request::Footprint { tunnel: tunnel.into(), info: Some(info) }, None, true);
            assert!(matches!(r, Response::Ok), "{r:?}");
        }
        let running = vec!["a".to_string()];
        assert_eq!(to_replace("b", Plan::Connect, true, &running, |n| core.footprint(n)), ["a"], "оба на весь трафик");
        let r = core.handle(Request::Footprint { tunnel: "a".into(), info: None }, None, true);
        assert!(matches!(r, Response::Ok), "{r:?}");
        assert!(core.footprint("a").is_none());
        assert!(to_replace("b", Plan::Connect, true, &running, |n| core.footprint(n)).is_empty(), "сведений нет — не трогаем");
    }

    /// Помощник родного окна и хранилище ведёт агент: окно прежней версии получает от ядра понятный отказ, ядро
    /// помощника не запускает и хранилище не трогает. Удаление режима 2 остаётся в ядре.
    #[test]
    fn requests_moved_to_the_agent_are_refused_with_a_clear_text() {
        let core = engine_core(&[]);
        let user = Some("S-1-5-21-1-2-3-1001");
        let moved = || {
            vec![
                Request::Read("a".into()),
                Request::Write { tunnel: "a".into(), text: "[Interface]\n".into() },
                Request::Details("a".into()),
                Request::Import(vec![crate::archive::Entry { name: "a".into(), text: "[Interface]\n".into() }]),
                Request::ExportAll,
                Request::NewTunnel("a".into()),
                Request::TakeNative,
                Request::Native(NativeOp::Open),
            ]
        };
        for mode in [Mode::Engine, Mode::Overlay] {
            lock(&core.config).mode = mode;
            for req in moved() {
                let what = format!("{mode:?} {req:?}");
                let r = core.handle(req, user, false);
                assert!(matches!(&r, Response::Refused(t) if *t == tr("core.moved_to_agent")), "{what}: {r:?}");
            }
        }
        lock(&core.config).mode = Mode::Overlay;
        let r = core.handle(Request::Delete("a".into()), user, false);
        assert!(matches!(&r, Response::Refused(t) if *t == tr("core.moved_to_agent")), "удаление в родном окне: {r:?}");
    }

    /// Обновления ведёт агент: окно прежней версии получает от ядра понятный отказ, а не «ошибку ядра».
    #[test]
    fn updates_are_refused_with_a_clear_text() {
        let core = engine_core(&[]);
        for op in [crate::update::UpdateOp::State, crate::update::UpdateOp::Check, crate::update::UpdateOp::Restore(1)] {
            for system in [false, true] {
                let r = core.handle(Request::Updates(op.clone()), Some("S-1-5-21-1-2-3-1001"), system);
                assert!(matches!(&r, Response::Refused(t) if *t == tr("core.updates_moved")), "{op:?}: {r:?}");
            }
        }
    }


    #[test]
    fn names_in_requests_are_collected_for_checks() {
        let r = Request::Switch { tunnel: r"C:\Users\Public\x".into(), plan: Plan::Connect, multiple: true };
        assert!(names_in(&r).into_iter().any(|n| !crate::engine::valid_name(n)), "путь вместо имени отвергается");
        assert_eq!(names_in(&Request::Rename { old: "a".into(), new: "b".into() }), vec!["a", "b"]);
    }

    /// Хост в памяти, который записывает команды; `fail` — на этой команде ошибка; `native` — режим 1 (службы
    /// AmneziaWG); `unreadable` — список работающих не читается.
    #[derive(Default)]
    struct Recorder {
        running: Mutex<Vec<String>>,
        calls: Mutex<Vec<String>>,
        fail: Option<&'static str>,
        native: bool,
        unreadable: bool,
        /// Конфиги AmneziaWG; `None` — те же, что работают.
        configs: Option<Vec<String>>,
    }

    impl Recorder {
        fn native() -> Recorder {
            Recorder { native: true, ..Default::default() }
        }


        fn step(&self, call: String) -> Result<(), String> {
            lock(&self.calls).push(call.clone());
            if self.fail == Some(call.as_str()) {
                return Err(format!("{call}: refused"));
            }
            Ok(())
        }
    }

    impl TunnelHost for Recorder {
        fn configs(&self) -> std::io::Result<Vec<String>> {
            Ok(self.configs.clone().unwrap_or_else(|| lock(&self.running).clone()))
        }
        fn running(&self) -> std::io::Result<Vec<String>> {
            if self.unreadable {
                return Err(std::io::Error::other("pipes unreadable"));
            }
            Ok(lock(&self.running).clone())
        }
        fn query(&self, tunnel: &str) -> std::io::Result<crate::uapi::Status> {
            Err(std::io::Error::other(tunnel.to_string()))
        }
        fn connect(&self, tunnel: &str) -> Result<(), String> {
            self.step(format!("up {tunnel}"))?;
            lock(&self.running).push(tunnel.into());
            Ok(())
        }
        fn disconnect(&self, tunnel: &str) -> Result<(), String> {
            self.step(format!("down {tunnel}"))?;
            lock(&self.running).retain(|n| n != tunnel);
            Ok(())
        }
        fn native_services(&self) -> bool {
            self.native
        }
    }

    #[test]
    fn switch_steps_run_in_order_and_stop_at_the_first_error() {
        let host = Recorder { running: Mutex::new(vec!["a".into(), "b".into()]), ..Default::default() };
        run_switch(&host, "b", Plan::Reconnect, &["a".into()]).unwrap();
        assert_eq!(*lock(&host.calls), ["down a", "down b", "up b"], "сначала заменяемые, потом сам туннель");
        assert_eq!(*lock(&host.running), ["b"]);

        let host = Recorder { running: Mutex::new(vec!["a".into()]), fail: Some("down a"), ..Default::default() };
        assert_eq!(run_switch(&host, "b", Plan::Connect, &["a".into()]), Err("down a: refused".into()));
        assert_eq!(*lock(&host.calls), ["down a"], "не отключился заменяемый — новый не подключается");

        let host = Recorder { running: Mutex::new(vec!["a".into()]), ..Default::default() };
        run_switch(&host, "a", Plan::Disconnect, &[]).unwrap();
        assert_eq!(*lock(&host.calls), ["down a"], "отключение не подключает обратно");
        run_switch(&host, "c", Plan::Connect, &[]).unwrap();
        assert_eq!(*lock(&host.calls), ["down a", "up c"], "подключение не отключает сам туннель");
    }

    #[test]
    fn switch_replaces_only_conflicting_when_multiple() {
        let full = |a: &str| Some(crate::conf::Footprint::of(&crate::conf::parse(&format!("[Interface]\nAddress = {a}\n[Peer]\nAllowedIPs = 0.0.0.0/0\n"))));
        let lan = |a: &str| Some(crate::conf::Footprint::of(&crate::conf::parse(&format!("[Interface]\nAddress = {a}\n[Peer]\nAllowedIPs = 10.9.0.0/24\n"))));
        let running = vec!["opt".to_string(), "lan".to_string(), "new".to_string()];
        let fp = |n: &str| match n {
            "opt" | "new" => full("10.255.254.2/32"),
            "lan" => lan("10.9.0.2/32"),
            _ => None,
        };
        assert_eq!(to_replace("new", Plan::Connect, true, &running, fp), vec!["opt"], "конфликтующий заменяется, независимый остаётся");
        assert_eq!(to_replace("new", Plan::Connect, false, &running, fp), vec!["opt", "lan"], "один туннель — отключаются все остальные");
        assert!(to_replace("new", Plan::Disconnect, false, &running, fp).is_empty());
        assert!(to_replace("x", Plan::Connect, true, &running, |_| None).is_empty(), "неизвестный конфиг — не трогаем");
    }

    // Изоляция ядра от агента (автоматический аналог ручной проверки 13.3/13.4 плана core-split): настоящее ядро
    // (`serve_until_stopped`) на тестовом канале и хосте в памяти, настоящий процесс агента — этот же тестовый файл,
    // запущенный сторожем в объекте задания. Живые служба, каналы и файлы не трогаются. Потоки ядра под тестом живут
    // до конца процесса тестов (у ядра нет остановки потоков — его останавливает выход службы); поддельные агенты
    // гибнут вместе с ним по `KILL_ON_JOB_CLOSE`.

    /// Аргумент, с которым процесс тестов становится поддельным агентом: `<метка><serve|hang>,<пауза мс>,<канал>`.
    /// Пауза перед созданием канала — медленный запуск процесса (`isolation_slow_agent_start_*`).
    const FAKE_AGENT: &str = "awg-fake-agent=";

    /// Сколько тест ждёт запуска процесса агента (первого `Hello`, замены после перезапуска). С запасом: первый запуск
    /// только что собранного exe проверяет антивирус, и процесс может стартовать секунды (первый прогон после
    /// пересборки падал на прежних сроках, следующие проходили). Проверки ядра от этого срока не зависят и остаются
    /// строгими: Switch — быстрее секунды, надзор — по своему расписанию.
    const START_BUDGET: Duration = Duration::from_secs(30);

    /// Точка входа поддельного агента: без аргумента `FAKE_AGENT` (обычный прогон с `--ignored`) не делает ничего.
    #[test]
    #[ignore = "точка входа процесса поддельного агента, его запускает сторож в тестах изоляции ядра"]
    fn fake_agent_process() {
        let Some(arg) = std::env::args().find_map(|a| a.strip_prefix(FAKE_AGENT).map(str::to_string)) else { return };
        let mut parts = arg.splitn(3, ',');
        let (Some(behaviour), Some(delay), Some(pipe)) = (parts.next(), parts.next(), parts.next()) else {
            panic!("fake agent: bad argument {arg:?}");
        };
        let delay = delay.parse().unwrap_or_else(|e| panic!("fake agent: bad start delay {delay:?}: {e}"));
        std::thread::sleep(Duration::from_millis(delay));
        serve_fake_agent(behaviour == "hang", pipe);
    }

    /// Отвечает на `Hello` на канале `pipe`. `hang` — ответив раз, перестаёт отвечать: держит соединение и молчит,
    /// как зависший агент.
    fn serve_fake_agent(hang: bool, pipe: &str) -> ! {
        use super::super::agent::proto::{AgentRequest, AgentResponse};
        let me = crate::win::current_user_sid().unwrap_or_else(|e| panic!("fake agent SID: {e}"));
        let (mut server, _) = Server::new(pipe, &me);
        let mut answered = 0u32;
        loop {
            let mut conn = match server.accept() {
                Ok(conn) => conn,
                Err(e) => {
                    eprintln!("fake agent: accept: {}", e.text);
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
            };
            let request = conn.read_as::<AgentRequest>();
            if hang && answered > 0 {
                loop {
                    std::thread::sleep(Duration::from_secs(3600));
                }
            }
            let response = match request {
                Ok(AgentRequest::Hello) => AgentResponse::Hello { version: env!("CARGO_PKG_VERSION").into() },
                Ok(_) => AgentResponse::Refused("fake agent: Hello only".into()),
                Err(e) => AgentResponse::Refused(e),
            };
            if let Err(e) = conn.reply_with(&response) {
                eprintln!("fake agent: reply: {e}");
            }
            answered += 1;
        }
    }

    /// Хост в памяти для ядра под тестом: подключение идемпотентно и записывается со временем.
    #[derive(Default)]
    struct TimedHost {
        running: Mutex<Vec<String>>,
        connects: Mutex<Vec<(String, Instant)>>,
    }

    impl TimedHost {
        /// Туннель упал сам: служба ушла без команды ядра.
        fn crash(&self, tunnel: &str) {
            lock(&self.running).retain(|n| n != tunnel);
        }

        fn connected_after(&self, tunnel: &str, after: Instant) -> Option<Instant> {
            lock(&self.connects).iter().find(|(t, at)| t == tunnel && *at >= after).map(|(_, at)| *at)
        }
    }

    impl TunnelHost for TimedHost {
        fn configs(&self) -> std::io::Result<Vec<String>> {
            Ok(lock(&self.running).clone())
        }
        fn running(&self) -> std::io::Result<Vec<String>> {
            Ok(lock(&self.running).clone())
        }
        fn query(&self, tunnel: &str) -> std::io::Result<crate::uapi::Status> {
            Err(std::io::Error::other(tunnel.to_string()))
        }
        fn connect(&self, tunnel: &str) -> Result<(), String> {
            lock(&self.connects).push((tunnel.to_string(), Instant::now()));
            let mut running = lock(&self.running);
            if !running.iter().any(|t| t == tunnel) {
                running.push(tunnel.to_string());
            }
            Ok(())
        }
        fn disconnect(&self, tunnel: &str) -> Result<(), String> {
            lock(&self.running).retain(|n| n != tunnel);
            Ok(())
        }
    }

    /// Ядро под тестом. `Drop` останавливает его цикл.
    struct Rig {
        core: Arc<Core>,
        host: Arc<TimedHost>,
        pipe: String,
        agent_pipe: String,
        stop: Arc<AtomicBool>,
        /// Хронометраж пути Switch этого ядра; упал Switch — уходит в журнал падения (`phase_trace`).
        trace: Arc<super::super::phase_trace::Trace>,
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            RIGS_RUNNING.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// Сторож здорового агента. Пока агент не ответил, у него `start_grace` на запуск (30 с — как `START_BUDGET`): без
    /// канала `Hello` не ждёт срока, а сразу промах, и медленный первый запуск exe принимался за зависание. Замолчавший
    /// после ответа канал считается зависшим через 12 с.
    const PROBE_SERVE: agent_watch::Probe = agent_watch::Probe { every: Duration::from_secs(3), timeout: Duration::from_secs(1), start_grace: Duration::from_secs(30) };
    /// Сторож зависающего агента: зависший снимается за ~2 с, а не за 30 с живых сроков. На запуск у агента здесь 10 с;
    /// зависание отсчитывается от первого ответа — агент `hang` отвечает раз и замолкает.
    const PROBE_HANG: agent_watch::Probe = agent_watch::Probe { every: Duration::from_millis(300), timeout: Duration::from_millis(300), start_grace: Duration::from_secs(10) };

    /// Ядер под тестом запущено в этом процессе: их потоки живут до выхода процесса — для журнала падения.
    static CORES_STARTED: AtomicUsize = AtomicUsize::new(0);
    /// Тестов изоляции идёт сейчас (живых `Rig`) — для журнала падения.
    static RIGS_RUNNING: AtomicUsize = AtomicUsize::new(0);

    /// Поднять ядро на своих канале и хосте: желаемый туннель `a` работает, агент — процесс тестов в роли `behaviour`.
    fn start_core(behaviour: &str, probe: agent_watch::Probe) -> Rig {
        start_core_delayed(behaviour, Duration::ZERO, probe)
    }

    /// То же; агент создаёт свой канал через `start_delay` после запуска процесса.
    fn start_core_delayed(behaviour: &str, start_delay: Duration, probe: agent_watch::Probe) -> Rig {
        let tag = format!("{}-{}", std::process::id(), CORES_STARTED.fetch_add(1, Ordering::SeqCst));
        let pipe = format!(r"\\.\pipe\awg-ui-test-core-{tag}");
        let agent_pipe = format!(r"\\.\pipe\awg-ui-test-agent-{tag}");
        assert!(pipe != super::super::pipe::NAME && agent_pipe != super::super::agent::PIPE_NAME);
        let host = Arc::new(TimedHost::default());
        lock(&host.running).push("a".into());
        let core = core_saving_to(host.clone(), &["a"], None);
        {
            let mut config = lock(&core.config);
            // Режим 1: сведения о туннелях — из памяти ядра, а не из хранилища режима 2 на диске.
            config.mode = Mode::Overlay;
            config.owner_sid = crate::win::current_user_sid().unwrap();
        }
        let tests = module_path!().split_once("::").map_or(module_path!(), |(_, path)| path);
        let args = format!("--ignored --exact {tests}::fake_agent_process {FAKE_AGENT}{behaviour},{},{agent_pipe}", start_delay.as_millis());
        let ends = Endpoints { pipe: pipe.clone(), agent: agent_watch::AgentSpec { args, pipe: agent_pipe.clone(), probe } };
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel();
        let (serving, stopping, ready) = (core.clone(), stop.clone(), tx.clone());
        // Запись хронометража — у потока теста (клиент канала) и потока ядра; от него — у потоков приёма и запросов.
        let trace = super::super::phase_trace::Trace::new();
        trace.install();
        let core_trace = trace.clone();
        std::thread::spawn(move || {
            core_trace.install();
            let result = serve_until_stopped(&serving, &stopping, || ready.send(Ok(())).unwrap_or(()), ends);
            tx.send(result.map_err(|e| e.text().to_string())).unwrap_or(());
        });
        let ready_within = READY_TIMEOUT + Duration::from_secs(5);
        let started = rx.recv_timeout(ready_within).unwrap_or_else(|e| panic!("core under test did not start within {ready_within:?}: {e}"));
        started.unwrap_or_else(|e| panic!("core under test failed: {e}"));
        RIGS_RUNNING.fetch_add(1, Ordering::SeqCst);
        Rig { core, host, pipe, agent_pipe, stop, trace }
    }

    fn agent_pid(core: &Core) -> Option<u32> {
        match core.agent.get() {
            Some(AgentStatus::Up { pid }) => Some(pid),
            _ => None,
        }
    }

    fn agent_answers(pipe: &str) -> bool {
        use super::super::agent::proto::{AgentRequest, AgentResponse};
        let timeouts = super::super::pipe::Timeouts { send: Duration::from_secs(1), reply: Duration::from_secs(1) };
        matches!(super::super::pipe::call_with::<_, AgentResponse>(pipe, &AgentRequest::Hello, timeouts), Ok(AgentResponse::Hello { .. }))
    }

    fn journal(core: &Core) -> Vec<String> {
        core.shared.with_events(|log| log.since(0)).into_iter().map(|(_, e)| e.text).collect()
    }

    /// Ждать, пока `ready` вернёт значение.
    fn wait_for<T>(within: Duration, what: &str, mut ready: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + within;
        loop {
            if let Some(value) = ready() {
                return value;
            }
            assert!(Instant::now() < deadline, "{what}: deadline {within:?} missed");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Переключать туннель `b` через канал ядра (подключить, отключить, …), пока `done` не вернёт значение. Каждый
    /// ответ ядра — быстрее секунды: что бы ни делал агент, команды окна ядро не задерживает. Не ответил или ответил
    /// позже — в сообщении путь журнала падения с фазами этого Switch (`phase_trace`): падение редкое и не
    /// воспроизводится, разбирать его можно только по записи того прогона.
    fn switching_until<T>(rig: &Rig, within: Duration, what: &str, mut done: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + within;
        let mut answers = 0u32;
        loop {
            let plan = if answers % 2 == 0 { Plan::Connect } else { Plan::Disconnect };
            let asked = Instant::now();
            let result = PipeAt(&rig.pipe).ok(Request::Switch { tunnel: "b".into(), plan, multiple: true });
            let took = asked.elapsed();
            let failure = match &result {
                Err(_) => Some(format!("{what}: Switch {plan:?}: {result:?}")),
                Ok(()) if took >= Duration::from_secs(1) => Some(format!("{what}: Switch {plan:?} answered in {took:?}")),
                Ok(()) => None,
            };
            if let Some(failure) = failure {
                let facts = [
                    ("Switch answers before this one", answers.to_string()),
                    ("isolation tests running now", RIGS_RUNNING.load(Ordering::SeqCst).to_string()),
                    ("cores under test started in this process (threads never stop)", CORES_STARTED.load(Ordering::SeqCst).to_string()),
                ];
                let log = rig.trace.write_failure(&failure, asked, &facts);
                panic!("{failure}; {log}");
            }
            answers += 1;
            if let Some(value) = done() {
                return value;
            }
            assert!(Instant::now() < deadline, "{what}: deadline {within:?} missed ({answers} Switch answers)");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn terminate(pid: u32, code: u32) {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
        let raw = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
        assert!(!raw.is_null(), "OpenProcess {pid}: {}", std::io::Error::last_os_error());
        let process = unsafe { OwnedHandle::from_raw_handle(raw) };
        assert!(unsafe { TerminateProcess(process.as_raw_handle(), code) } != 0, "TerminateProcess {pid}: {}", std::io::Error::last_os_error());
    }

    /// Процесса больше нет (или он выходит в ближайшие 2 с).
    fn exited(pid: u32) -> bool {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
        use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
        let raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if raw.is_null() {
            return true;
        }
        let process = unsafe { OwnedHandle::from_raw_handle(raw) };
        unsafe { WaitForSingleObject(process.as_raw_handle(), 2000) == WAIT_OBJECT_0 }
    }

    /// Агента убили (`TerminateProcess`), а желаемый туннель в тот же миг упал сам: ядро отвечает на Switch быстрее
    /// секунды всё время, надзор поднимает туннель по своему расписанию, сторож запускает нового агента после первой
    /// паузы перезапуска и пишет причину в журнал.
    #[test]
    fn isolation_killed_agent_is_respawned_and_the_core_keeps_working() {
        let rig = start_core("serve", PROBE_SERVE);
        let first = wait_for(START_BUDGET, "agent up and answering (start budget)", || agent_pid(&rig.core).filter(|_| agent_answers(&rig.agent_pipe)));
        switching_until(&rig, Duration::from_secs(5), "before the kill", || Some(()));

        let killed_at = Instant::now();
        terminate(first, 77);
        rig.host.crash("a");
        // Новый pid виден, когда вернулся `CreateProcess` — после паузы сторожа и запуска процесса (срок запуска).
        let respawn_within = agent_watch::RESTART_FIRST + START_BUDGET;
        let second = switching_until(&rig, respawn_within, "agent respawned (restart pause + start budget)", || agent_pid(&rig.core).filter(|pid| *pid != first));
        let respawned_in = killed_at.elapsed();
        assert!(respawned_in >= agent_watch::RESTART_FIRST, "agent {second} respawned in {respawned_in:?}, before the restart pause");
        assert!(exited(first), "the killed agent {first} is gone");
        wait_for(START_BUDGET, "respawned agent answers (start budget)", || agent_answers(&rig.agent_pipe).then_some(()));

        let schedule = retry::FAST_EVERY + Duration::from_secs(3);
        let reconnected = switching_until(&rig, schedule + Duration::from_secs(2), "dropped tunnel reconnected", || rig.host.connected_after("a", killed_at));
        assert!(reconnected - killed_at <= schedule, "reconnected {:?} after the drop", reconnected - killed_at);
        let exit = agent_watch::describe_with(&AgentExit::Code(77), PROBE_SERVE);
        assert!(journal(&rig.core).iter().any(|t| t.contains(&exit)), "journal names the exit ({exit}): {:?}", journal(&rig.core));
    }

    /// Агент перестал отвечать на `Hello`: сторож завершает его и запускает нового; ядро отвечает на Switch быстрее
    /// секунды всё это время, работающий туннель не трогается.
    #[test]
    fn isolation_hung_agent_is_killed_and_respawned_without_slowing_the_core() {
        let rig = start_core("hang", PROBE_HANG);
        let started = Instant::now();
        let first = wait_for(START_BUDGET, "agent up (start budget)", || agent_pid(&rig.core));
        let second = switching_until(&rig, START_BUDGET, "hung agent replaced (start budget)", || agent_pid(&rig.core).filter(|pid| *pid != first));
        assert!(exited(first), "the hung agent {first} is terminated, not left behind (new {second})");
        let hung = agent_watch::describe_with(&AgentExit::Hung, PROBE_HANG);
        assert!(journal(&rig.core).iter().any(|t| t.contains(&hung)), "journal names the hang ({hung}): {:?}", journal(&rig.core));
        assert!(lock(&rig.host.running).iter().any(|t| t == "a"), "the working tunnel stays up");
        assert!(rig.host.connected_after("a", started).is_none(), "the working tunnel is not reconnected");
    }

    /// Проверка самой обвязки: агент создаёт канал через 4 с после запуска (медленный первый запуск exe под
    /// антивирусом). Тест его дожидается, сторож не принимает его за зависший — отвечает тот же процесс, о зависании
    /// в журнале ничего, ядро всё это время отвечает на Switch быстрее секунды. При прежнем сроке сторожа (на запуск
    /// 3 с) агент снимался бы как зависший.
    #[test]
    fn isolation_slow_agent_start_is_waited_for_not_taken_for_a_hang() {
        let rig = start_core_delayed("serve", Duration::from_secs(4), PROBE_SERVE);
        let first = wait_for(START_BUDGET, "agent up (start budget)", || agent_pid(&rig.core));
        switching_until(&rig, START_BUDGET, "slow agent answers (start budget)", || agent_answers(&rig.agent_pipe).then_some(()));
        assert_eq!(agent_pid(&rig.core), Some(first), "the slow agent was replaced; journal: {:?}", journal(&rig.core));
        let hung = agent_watch::describe_with(&AgentExit::Hung, PROBE_SERVE);
        assert!(!journal(&rig.core).iter().any(|t| t.contains(&hung)), "slow start taken for a hang: {:?}", journal(&rig.core));
    }
}
