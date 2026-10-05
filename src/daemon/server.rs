//! Работа ядра: опрос, пинг и статистика в фоне, ответы окну по каналу, смена режима на лету.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use super::helper::{self, Op, Out};
use super::pipe::Server;
use super::proto::{CoreState, NativeOp, Plan, Request, Response};
use super::retry::{self, Note, Retries, Seen};
use super::{data_dir, Config, CoreApi, DATA_SDDL};
use crate::backend::{EngineHost, Real, TunnelHost, MANAGER_SERVICE};
use crate::crash::lock;
use crate::events::Severity;
use crate::i18n::{tr, trf};
use crate::monitor::{Options, Shared};
use crate::settings::Mode;
use crate::store;
use crate::update::manager::Manager;

/// Сколько ждём, пока служба туннеля появится или исчезнет после команды.
const SWITCH_TIMEOUT: Duration = Duration::from_secs(15);
/// Одновременных соединений с окном больше этого — лишние получают отказ (память и потоки ядра не бесконечны).
const MAX_CONNECTIONS: usize = 32;

pub struct Core {
    shared: Arc<Shared>,
    config: Mutex<Config>,
    /// Смена режима, переключения туннелей и запросы, зависящие от режима, не пересекаются.
    switching: Mutex<()>,
    /// Действия в родном окне — по одному: два помощника сразу мешали бы друг другу в одном окне.
    helper_lock: Mutex<()>,
    /// Занятые места соединений; место держит `Slot`.
    connections: Arc<AtomicUsize>,
    /// Сведения о туннелях режима 1, прочитанные из родного окна: по ним видно, с кем туннель конфликтует.
    details: Mutex<HashMap<String, crate::conf::TunnelInfo>>,
    /// Обновления и откаты компонентов.
    updates: Arc<Manager>,
    /// Надзор за желаемыми туннелями — единственный, кто их переподключает. Берётся после `switching`, не наоборот.
    retries: Mutex<Retries>,
}

/// Кто переключает туннель: пользователь (команда окна) или надзор ядра, поднимая желаемый туннель (`retry`).
#[derive(Clone, Copy, PartialEq)]
enum Origin {
    User,
    Retry,
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
/// ошибка до этого — ядро не поднялось.
pub fn run(stop: &AtomicBool, ready: impl FnOnce()) -> Result<(), RunError> {
    let dir = data_dir();
    crate::win::protect_dir(&dir, DATA_SDDL)?;
    let (config, unreadable) = Config::load_guarded();
    crate::i18n::set(&crate::engine::install_dir().join("lang"), &config.language);
    // После выбора языка (текст на нём) и до проверки владельца: без `core.ini` ядро дальше не пойдёт, и запись
    // в журнале — единственное, что объяснит, почему.
    if let Some(problem) = &unreadable {
        super::log_unreadable(problem);
    }
    if config.owner_sid.is_empty() {
        return Err(tr("core.no_owner").into());
    }
    let options = Options { ping: config.ping, ping_host: config.ping_host.clone(), notify: false, tray: false, taskbar: false };
    let shared = Arc::new(Shared::new(Some(host_for(config.mode)), options, Some(dir.join("Stats.ini")), Some(super::events_file())));
    let core = Arc::new_cyclic(|me: &Weak<Core>| {
        // Движок заменён — туннели режима 2 переподключаются уже на новом.
        let me = me.clone();
        let on_engine_changed = Box::new(move || {
            if let Some(core) = me.upgrade() {
                core.reconnect_engine();
            }
        });
        Core {
            updates: Manager::new(shared.clone(), on_engine_changed),
            shared,
            config: Mutex::new(config),
            switching: Mutex::new(()),
            helper_lock: Mutex::new(()),
            connections: Arc::default(),
            details: Mutex::default(),
            retries: Mutex::default(),
        }
    });
    core.prepare_mode();
    crate::monitor::spawn(core.shared.clone(), Box::new(|| {}));
    crate::ping::spawn(core.shared.clone());
    // Надзор за желаемыми туннелями: его первый такт — восстановление после запуска ядра.
    let net_core = core.clone();
    super::netwatch::spawn(move |severity, text| net_core.shared.log("", severity, text));
    let supervisor = core.clone();
    crate::crash::spawn_named("retry", move || supervisor.supervise());

    let server_core = core.clone();
    let taken = Arc::new(AtomicBool::new(false));
    let server_taken = taken.clone();
    crate::crash::spawn_named("pipe-accept", move || serve(&server_core, &server_taken));
    if let Err(e) = wait_listening() {
        return Err(if taken.load(Ordering::SeqCst) { RunError::PipeTaken(e) } else { RunError::Failed(e) });
    }
    ready();

    while !stop.load(Ordering::SeqCst) {
        // Фоновый поток упал (причина уже в журнале) — ядро без него полуживое: остановиться с кодом сбоя, чтобы
        // диспетчер служб перезапустил службу. Статистика не сохраняется: поток мог упасть посреди её изменения,
        // остаётся последнее периодическое сохранение.
        if crate::crash::core_failure().is_some() {
            return Err(RunError::Failed(tr("core.thread_failed")));
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    core.shared.save_stats();
    Ok(())
}

/// Цикл канала: ждать клиента и отвечать ему в своём потоке. `taken` — последняя попытка упёрлась в чужой канал с
/// тем же именем (для `run`: почему канал не заработал).
fn serve(core: &Arc<Core>, taken: &AtomicBool) {
    let owner = lock(&core.config).owner_sid.clone();
    let (mut server, bad_owner) = Server::new(&owner);
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
        let Some(slot) = Slot::take(&core.connections, connection_limit(conn.from_system())) else {
            // Не дошёл отказ — клиент сам увидит ошибку канала; ядру тут терять нечего.
            drop(conn.reply(&Response::Refused(tr("core.busy"))));
            continue;
        };
        let core = core.clone();
        crate::crash::spawn_named("pipe-request", move || {
            // Место возвращается при выходе из потока при любом исходе.
            let _slot = slot;
            let response = match conn.read() {
                Ok(req) => core.handle_isolated(req, conn.session, conn.client_sid.as_deref(), conn.elevated),
                Err(e) => Response::Refused(e),
            };
            // Не дошёл ответ (клиент ушёл или не забирает его в срок) — клиент сам увидит ошибку канала.
            drop(conn.reply(&response));
        });
    }
}

/// Сколько соединений клиента обслуживать одновременно. Проверку канала при старте (запрос от самой системы)
/// предел не касается: иначе 32 подключения любой программы владельца сорвали бы запуск, и перезапуск после
/// обновления счёл бы новую сборку сломанной и вернул прежнюю.
fn connection_limit(from_system: bool) -> usize {
    if from_system { usize::MAX } else { MAX_CONNECTIONS }
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
fn wait_listening() -> Result<(), String> {
    let until = Instant::now() + READY_TIMEOUT;
    loop {
        let last = match super::PipeClient.hello() {
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

fn host_for(mode: Mode) -> Arc<dyn TunnelHost> {
    match mode {
        Mode::Overlay => Arc::new(Real::new()),
        Mode::Engine => Arc::new(EngineHost),
    }
}

/// Возврат компонента к версии из копии — только по запросу с правами администратора (подтверждение UAC): иначе
/// любая программа учётной записи владельца молча откатила бы компонент к старой версии с известными дырами.
/// Правило — для любого возврата, а не только к более старой версии: версии сравнимы не всегда (AmneziaWG может
/// быть не установлен, номер сборки — неизвестен), а простое правило нечем обойти.
fn updates_allowed(op: &crate::update::UpdateOp, elevated: bool) -> Result<(), String> {
    match op {
        crate::update::UpdateOp::Restore(_) if !elevated => Err(tr("core.restore_needs_admin")),
        _ => Ok(()),
    }
}

fn done(r: Result<(), String>) -> Response {
    r.map_or_else(Response::Err, |()| Response::Ok)
}

/// Место в счётчике соединений: возвращается в `Drop` — и при панике обработчика запроса. Иначе каждая паника
/// навсегда занимала бы место, и через `MAX_CONNECTIONS` паник ядро отвечало бы «занято» на всё.
struct Slot(Arc<AtomicUsize>);

impl Slot {
    fn take(counter: &Arc<AtomicUsize>, max: usize) -> Option<Slot> {
        if counter.fetch_add(1, Ordering::SeqCst) >= max {
            counter.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(Slot(counter.clone()))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
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
    fn handle_isolated(&self, req: Request, session: u32, caller: Option<&str>, elevated: bool) -> Response {
        crate::crash::isolate(|| self.handle(req, session, caller, elevated)).unwrap_or_else(|panic| {
            self.shared.log("", Severity::Bad, &trf("core.request_failed", &[&panic]));
            Response::Err(trf("core.request_failed", &[&panic]))
        })
    }

    fn handle(&self, req: Request, session: u32, caller: Option<&str>, elevated: bool) -> Response {
        // Имя туннеля из запроса становится частью путей и командных строк — только допустимые имена.
        if let Some(bad) = names_in(&req).into_iter().find(|n| !crate::engine::valid_name(n)) {
            return Response::Refused(trf("eng.bad_name", &[bad]));
        }
        // Команды PreUp/PostUp/… служба туннеля выполнила бы от SYSTEM — из окна их не принимаем.
        if texts_in(&req).into_iter().any(has_scripts) {
            return Response::Refused(tr("core.no_scripts"));
        }
        match req {
            Request::Hello => Response::Hello { version: env!("CARGO_PKG_VERSION").into(), mode: self.mode() },
            Request::State { events_after } => Response::State(Box::new(self.state(events_after))),
            Request::Switch { tunnel, plan, multiple } => done(self.switch(&tunnel, plan, multiple)),
            Request::SetMode(mode) => done(self.set_mode(mode)),
            Request::SetPing { enabled, host } => done(self.set_ping(enabled, host)),
            Request::SetLanguage(code) => done(self.set_language(code)),
            Request::Retry(tunnel) => done(self.retry(&tunnel)),
            // Обновления — вне блокировки режима: загрузка идёт минутами.
            Request::Updates(op) => {
                if let Err(e) = updates_allowed(&op, elevated) {
                    self.shared.log("", Severity::Warn, &trf("core.restore_refused", &[caller.unwrap_or("?")]));
                    return Response::Err(e);
                }
                match self.updates.handle(op) {
                    Ok(s) => Response::Updates(Box::new(s)),
                    Err(e) => Response::Err(e),
                }
            }
            // Остальное зависит от режима: пока оно выполняется, режим не меняется.
            other => {
                let _guard = (!native_helper_request(self.mode(), &other)).then(|| lock(&self.switching));
                self.handle_in_mode(other, session, caller)
            }
        }
    }

    fn handle_in_mode(&self, req: Request, session: u32, caller: Option<&str>) -> Response {
        let engine = self.mode() == Mode::Engine;
        let need_engine = || if engine { Ok(()) } else { Err(tr("core.only_engine")) };
        match req {
            Request::Read(t) if engine => store::read(&store::path(&t)).map_or_else(Response::Err, Response::Text),
            Request::Read(t) => self.helper(session, caller, Op::ReadConfig(t)),
            Request::Write { tunnel, text } if engine => done(store::write(&tunnel, &text).map(drop)),
            Request::Write { tunnel, text } => {
                lock(&self.details).remove(&tunnel);
                self.helper(session, caller, Op::WriteConfig(tunnel, text))
            }
            Request::Delete(t) if engine => done(EngineHost.delete_tunnel(&t).inspect(|()| self.forget(&t))),
            Request::Delete(t) => {
                lock(&self.details).remove(&t);
                let r = self.helper(session, caller, Op::Delete(t.clone()));
                if matches!(r, Response::Ok) {
                    // Иначе `[имя]` остаётся в Stats.ini навсегда и искажает доли времени.
                    self.forget(&t);
                }
                r
            }
            Request::Details(t) if engine => {
                store::read(&store::path(&t)).map_or_else(Response::Err, |text| Response::Info(crate::conf::parse(&text)))
            }
            Request::Details(t) => {
                let r = self.helper(session, caller, Op::Details(t.clone()));
                if let Response::Info(info) = &r {
                    lock(&self.details).insert(t, info.clone());
                }
                r
            }
            Request::Import(entries) => match need_engine().and_then(|()| store::import(&entries)) {
                Ok(r) => {
                    self.log_imported(r.added.len());
                    Response::Report(r)
                }
                Err(e) => Response::Err(e),
            },
            Request::ExportAll => need_engine().and_then(|()| store::export_all()).map_or_else(Response::Err, Response::Entries),
            Request::Rename { old, new } => done(need_engine().and_then(|()| self.rename(&old, &new))),
            Request::NewTunnel(name) => need_engine().and_then(|()| new_tunnel(&name)).map_or_else(Response::Err, Response::Text),
            Request::TakeNative => match need_engine().and_then(|()| self.take_native(session, caller)) {
                Ok(r) => Response::Report(r),
                Err(e) => Response::Err(e),
            },
            Request::Native(_) if engine => Response::Err(tr("core.only_overlay")),
            Request::Native(op) => self.helper(
                session,
                caller,
                match op {
                    NativeOp::Open => Op::Open,
                    NativeOp::Edit(t) => Op::Edit(t),
                    NativeOp::Import(f) => Op::Import(f),
                    NativeOp::Close => Op::Close,
                },
            ),
            // Разобраны в `handle`.
            Request::Hello | Request::State { .. } | Request::Switch { .. } | Request::SetMode(_) | Request::SetPing { .. }
            | Request::SetLanguage(_)
            | Request::Retry(_)
            | Request::Updates(_) => {
                Response::Err("core: unexpected".into())
            }
        }
    }

    /// Действие в родном окне — через помощника в сеансе того, кто прислал запрос, и от его имени.
    fn helper(&self, session: u32, caller: Option<&str>, op: Op) -> Response {
        match self.run_helper(session, caller, &op) {
            Ok(Out::Ok) => Response::Ok,
            Ok(Out::Text(t)) => Response::Text(t),
            Ok(Out::Info(i)) => Response::Info(i),
            Ok(Out::Err(e)) | Err(e) => Response::Err(e),
        }
    }

    fn run_helper(&self, session: u32, caller: Option<&str>, op: &Op) -> Result<Out, String> {
        let caller = caller.ok_or_else(|| tr("core.other_session"))?;
        let _one = lock(&self.helper_lock);
        helper::run(session, caller, op)
    }

    fn state(&self, events_after: u64) -> CoreState {
        self.shared.core_state(self.mode(), events_after)
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
        if self.shared.is_pending(name) {
            return Ok(false);
        }
        let _guard = lock(&self.switching);
        // Список запущенных — у служб, а не из снимка опроса: только что подключённый туннель в снимок ещё не попал.
        let found = self.host().and_then(|b| b.running().map(|r| (b, r)).map_err(|e| e.to_string()));
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
        // Под той же блокировкой, что и команды пользователя: отключённый им за время ожидания не поднимается обратно.
        if origin == Origin::Retry && (running.iter().any(|r| r == name) || !self.is_desired(name)) {
            return Ok(false);
        }
        let others = to_replace(name, plan, multiple, &running, |n| self.footprint(n));
        if origin == Origin::User {
            self.update_desired(name, |config| {
                config.tunnels = Some(super::restore::after_switch(config.tunnels.as_deref().unwrap_or_default(), name, plan, &others));
                if plan != Plan::Disconnect {
                    config.multiple = multiple;
                }
            });
            let mut retries = lock(&self.retries);
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
        let _busy = self.shared.pending_guard(std::iter::once((name.to_string(), label)).chain(others.iter().map(|o| (o.clone(), "busy.disconnect"))));
        let result = run_switch(b.as_ref(), name, plan, &others);
        if let (Origin::User, Err(e)) = (origin, &result) {
            self.shared.log(name, Severity::Bad, e);
        }
        result.map(|()| true)
    }

    /// Изменить желаемый набор туннелей и сохранить его. Не записался — набор в памяти всё равно новый (до перезапуска
    /// ядра он верен), а в журнале предупреждение: после перезагрузки поднимется прежний набор.
    fn update_desired(&self, tunnel: &str, change: impl FnOnce(&mut Config)) {
        let mut config = lock(&self.config);
        let mut next = config.clone();
        change(&mut next);
        if next == *config {
            return;
        }
        let saved = next.save();
        *config = next;
        drop(config);
        if let Err(e) = saved {
            self.shared.log(tunnel, Severity::Warn, &trf("core.desired_unsaved", &[&e]));
        }
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
                pending: &|t| self.shared.is_pending(t),
                service_exists: &|t| host.service_exists(t),
                stop_reason: &|t| host.stop_reason(t),
                native_services: host.native_services(),
                network_changed,
            },
        );
        for t in &tick.outside {
            self.update_desired(t, |config| config.tunnels.iter_mut().for_each(|tunnels| tunnels.retain(|d| d != t)));
            self.shared.log(t, Severity::Info, &trf("core.retry_outside", &[t]));
        }
        for (t, note) in tick.notes {
            self.log_note(&t, note);
        }
        for t in &tick.due {
            let result = self.switch_from(Origin::Retry, t, Plan::Connect, multiple);
            let note = lock(&self.retries).outcome(t, Instant::now(), result);
            if let Some(note) = note {
                self.log_note(t, note);
            }
        }
        self.shared.set_retries(lock(&self.retries).view(Instant::now()));
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

    fn set_ping(&self, enabled: bool, host: String) -> Result<(), String> {
        self.shared.set_ping_options(enabled, host.clone());
        let mut config = lock(&self.config);
        if config.ping != enabled || config.ping_host != host {
            config.ping = enabled;
            config.ping_host = host;
            config.save()?;
        }
        Ok(())
    }

    /// Язык журнала — как у окна. Файл .lng ядро берёт только из своей папки в Program Files (туда пишут
    /// администраторы); нет файла — встроенный перевод или английский.
    fn set_language(&self, code: String) -> Result<(), String> {
        if !crate::i18n::is_code(&code) {
            return Err(format!("language: {code}"));
        }
        crate::i18n::set(&crate::engine::install_dir().join("lang"), &code);
        let mut config = lock(&self.config);
        if config.language != code {
            config.language = code;
            config.save()?;
        }
        Ok(())
    }

    /// Переименование туннеля хранилища (отключённого); статистика переходит к новому имени.
    fn rename(&self, old: &str, new: &str) -> Result<(), String> {
        if !crate::engine::valid_name(new) {
            return Err(trf("eng.bad_name", &[new]));
        }
        if self.shared.is_running(old) {
            return Err(trf("eng.rename_running", &[old]));
        }
        store::rename(old, new)?;
        self.shared.update_stats(|stats| {
            if let Some(st) = stats.remove(old) {
                stats.insert(new.to_string(), st);
            }
        });
        // Желаемый, но упавший сам туннель после перезапуска поднимается под новым именем.
        self.update_desired(new, |config| config.tunnels.iter_mut().flatten().filter(|t| *t == old).for_each(|t| *t = new.to_string()));
        Ok(())
    }

    /// Удалённый туннель — и его статистика, и место в желаемом наборе.
    fn forget(&self, tunnel: &str) {
        self.shared.update_stats(|stats| {
            stats.remove(tunnel);
        });
        self.update_desired(tunnel, |config| config.tunnels.iter_mut().for_each(|tunnels| tunnels.retain(|t| t != tunnel)));
    }

    /// «Забрать всё из AmneziaWG»: родной экспорт (помощник в сеансе пользователя) во временный архив в защищённой
    /// папке хранилища, импорт, архив удаляется сразу (остаток после сбоя уберёт `store::startup`).
    fn take_native(&self, session: u32, caller: Option<&str>) -> Result<store::ImportReport, String> {
        let zip = store::export_temp()?;
        let exported = match self.run_helper(session, caller, &Op::Export(zip.display().to_string())) {
            Ok(Out::Ok) => crate::archive::read(&zip, None).map_err(|e| format!("{}: {e:?}", zip.display())),
            Ok(Out::Err(e)) | Err(e) => Err(e),
            Ok(other) => Err(format!("helper: {other:?}")),
        };
        let removed = match std::fs::remove_file(&zip) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(crate::fsutil::io_ctx(&zip, e)),
            _ => Ok(()),
        };
        let report = store::import(&removed.and(exported)?)?;
        self.log_imported(report.added.len());
        Ok(report)
    }

    fn log_imported(&self, n: usize) {
        if n > 0 {
            self.shared.log("", Severity::Info, &trf("eng.imported", &[&n.to_string()]));
        }
    }
}

/// Новый туннель хранилища со свежим ключом; возвращает его текст.
fn new_tunnel(name: &str) -> Result<String, String> {
    if !crate::engine::valid_name(name) {
        return Err(trf("eng.bad_name", &[name]));
    }
    let names = store::list().map_err(|e| e.to_string())?;
    if store::find(&names, name).is_some() {
        return Err(tr("dlg.err_exists"));
    }
    let text = store::template();
    store::write(name, &text)?;
    Ok(text)
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
    if plan != Plan::Connect {
        host.disconnect(name)?;
        wait(false);
    }
    if plan != Plan::Disconnect {
        host.connect(name)?;
        wait(true);
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

/// Запросы режима 1, которые целиком идут помощнику в родное окно. Они не трогают службы туннелей, поэтому
/// `switching` не держат: иначе медленный шаг UI Automation (до 120 с) стопорит подключение и отключение.
/// Одновременность с самим помощником задаёт `helper_lock`.
fn native_helper_request(mode: Mode, req: &Request) -> bool {
    mode == Mode::Overlay
        && matches!(req, Request::Read(_) | Request::Write { .. } | Request::Delete(_) | Request::Details(_) | Request::Native(_))
}

/// Имена туннелей из запроса.
fn names_in(req: &Request) -> Vec<&str> {
    match req {
        Request::Switch { tunnel, .. } | Request::Write { tunnel, .. } => vec![tunnel],
        Request::Read(t) | Request::Delete(t) | Request::Details(t) | Request::NewTunnel(t) | Request::Retry(t) => vec![t],
        Request::Rename { old, new } => vec![old, new],
        Request::Native(NativeOp::Edit(t)) => vec![t],
        _ => vec![],
    }
}

/// Тексты конфигов из запроса.
fn texts_in(req: &Request) -> Vec<&str> {
    match req {
        Request::Write { text, .. } => vec![text],
        Request::Import(entries) => entries.iter().map(|e| e.text.as_str()).collect(),
        _ => vec![],
    }
}

/// В конфиге есть команды, которые служба туннеля выполнила бы (PreUp, PostUp, PreDown, PostDown).
fn has_scripts(text: &str) -> bool {
    text.lines().any(|line| {
        let key = line.split('=').next().unwrap_or("").trim().to_ascii_lowercase();
        line.contains('=') && matches!(key.as_str(), "preup" | "postup" | "predown" | "postdown")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panicking_request_returns_its_connection_slot() {
        let counter: Arc<AtomicUsize> = Arc::default();
        let slot = Slot::take(&counter, 1).expect("свободное место");
        assert!(Slot::take(&counter, 1).is_none(), "мест больше нет");
        assert_eq!(counter.load(Ordering::SeqCst), 1, "отказ не занимает место");
        let r = crate::crash::isolate(move || {
            let _slot = slot;
            panic!("handler bug");
        });
        assert!(r.is_err());
        assert_eq!(counter.load(Ordering::SeqCst), 0, "место вернулось после паники");
        assert!(Slot::take(&counter, 1).is_some());
    }

    #[test]
    fn panic_mid_switch_leaves_no_tunnel_busy_and_lock_usable() {
        use crate::backend::Demo;
        let options = Options { ping: false, ping_host: String::new(), notify: false, tray: false, taskbar: false };
        let shared = Arc::new(Shared::new(Some(Arc::new(Demo::new())), options, None, None));
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

    #[test]
    fn scripts_are_found_in_any_case() {
        assert!(has_scripts("[Interface]\npostup = cmd /c calc\n"));
        assert!(has_scripts("[Interface]\n  PreDown=x\n"));
        assert!(!has_scripts("[Interface]\nPrivateKey = a\n# PostUp is not set\n"));
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
    fn full_core_still_answers_its_own_startup_check() {
        assert_eq!(connection_limit(false), MAX_CONNECTIONS, "окну и остальным — общий предел");
        assert_eq!(connection_limit(true), usize::MAX, "проверка от системы проходит всегда");
    }

    #[test]
    fn restore_needs_elevated_caller() {
        use crate::update::UpdateOp;
        assert_eq!(updates_allowed(&UpdateOp::Restore(3), false), Err(tr("core.restore_needs_admin")), "без UAC — отказ");
        assert_eq!(updates_allowed(&UpdateOp::Restore(3), true), Ok(()));
        for op in [UpdateOp::State, UpdateOp::Check, UpdateOp::Apply(vec![])] {
            assert_eq!(updates_allowed(&op, false), Ok(()), "{op:?} — без прав администратора, как раньше");
        }
    }

    #[test]
    fn native_helper_requests_skip_the_switch_lock_only_in_overlay_mode() {
        let slow = Request::Details("t".into());
        assert!(native_helper_request(Mode::Overlay, &slow));
        assert!(native_helper_request(Mode::Overlay, &Request::Native(NativeOp::Open)));
        assert!(!native_helper_request(Mode::Engine, &slow), "режим 2: хранилище меняется под блокировкой");
        assert!(!native_helper_request(Mode::Overlay, &Request::Rename { old: "a".into(), new: "b".into() }));
        assert!(!native_helper_request(Mode::Overlay, &Request::SetMode(Mode::Engine)));
    }

    #[test]
    fn names_in_requests_are_collected_for_checks() {
        let r = Request::Switch { tunnel: r"C:\Users\Public\x".into(), plan: Plan::Connect, multiple: true };
        assert!(names_in(&r).into_iter().any(|n| !crate::engine::valid_name(n)), "путь вместо имени отвергается");
        assert_eq!(names_in(&Request::Rename { old: "a".into(), new: "b".into() }), vec!["a", "b"]);
    }

    /// Хост в памяти, который записывает команды; `fail` — на этой команде ошибка.
    #[derive(Default)]
    struct Recorder {
        running: Mutex<Vec<String>>,
        calls: Mutex<Vec<String>>,
        fail: Option<&'static str>,
    }

    impl Recorder {
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
            Ok(lock(&self.running).clone())
        }
        fn running(&self) -> std::io::Result<Vec<String>> {
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
}
