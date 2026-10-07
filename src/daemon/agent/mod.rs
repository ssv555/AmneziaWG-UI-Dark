//! Агент — второй процесс (`awg-ui.exe --agent`, SYSTEM): всё, что не должно мешать VPN-ядру (обновления, пинг,
//! статистика, журнал на диске — по мере переноса, см. план core-split). Сейчас здесь канал агента, `Hello`, пинг
//! (`agent.ini`), язык ядра (`language`), обновления (`updates`), статистика (`stats`) и история скорости
//! (`history`), журнал событий (`journal`, единственный писатель `events.log`) и конфиги туннелей с помощником
//! родного окна (`tunnels`). Запускает и сторожит его ядро (`daemon::agent_watch`). VPN-кода здесь нет и быть не
//! должно: туннели, переключение и переподключение — только в ядре; тест `agent_does_not_reach_into_vpn_code`
//! сторожит это по тексту модуля.

pub mod client;
mod core_poll;
mod history;
mod journal;
mod language;
mod ping;
pub mod proto;
mod settings;
mod stats;
mod tunnels;
mod updates;

pub(crate) use settings::AgentConfig;
pub(crate) use stats::AgentStats;

use std::sync::Arc;
use std::time::{Duration, Instant};

use history::AgentHistory;
use journal::AgentJournal;
use language::AgentLanguage;
use ping::AgentPing;
use tunnels::{Caller, Tunnels};
use proto::{AgentRequest, AgentResponse, AgentState};

use super::budget::{Budgets, Limits};
use super::pipe::Server;
use crate::events::Severity;
use crate::i18n::{tr, trf};
use crate::update::manager::Manager;

/// Ключ запуска агента.
pub const AGENT_FLAG: &str = "--agent";
/// Канал агента; права те же, что у канала ядра (SYSTEM, администраторы и владелец окна).
pub const PIPE_NAME: &str = r"\\.\pipe\AmneziaWG-UI-Dark-Agent";
/// Код выхода: фоновый поток упал, процесс остановлен (сторож ядра запустит агента заново).
const FAILED: i32 = 2;
/// Сколько запросов агент обслуживает одновременно; остальным — отказ, а не новый поток.
const MAX_CONNECTIONS: usize = 16;
/// Места SYSTEM-клиентов отдельно от `MAX_CONNECTIONS`: сторож ядра (`agent_watch`) спрашивает `Hello`, и молчащие
/// подключения программ владельца не должны давать ему отказ — три отказа подряд убивают живого агента. Места
/// SYSTEM малы: ядро ходит к агенту только этим `Hello`. Запас один — тоже только для `Hello`.
const MAX_SYSTEM_CONNECTIONS: usize = 4;
const AGENT_LIMITS: Limits = Limits { user: MAX_CONNECTIONS, system: MAX_SYSTEM_CONNECTIONS, self_check: 1 };
/// Одна и та же ошибка ожидания клиента попадает в журнал не чаще этого.
const ACCEPT_LOG_EVERY: Duration = Duration::from_secs(60);

/// Процесс агента: отвечает на канале, пока не упадёт фоновый поток. Возвращает код выхода.
pub fn main() -> i32 {
    // stderr агента никто не читает: паника любого потока — в журнал событий, как у ядра.
    crate::crash::install_core(super::events_file());
    let agent = Arc::new(Agent::start());
    let (journal, history) = (agent.journal.clone(), agent.history.clone());
    crate::crash::spawn_named("agent-pipe-accept", move || serve(agent));
    loop {
        if crate::crash::core_failure().is_some() {
            // История скорости — тоже на диск: снаружи агента завершают без предупреждения, а этот выход ещё свой.
            history.tick();
            // Причина уже в журнале: дописать его очередь в файл до выхода, иначе последние события пропали бы.
            journal.flush_before_exit(super::events_file());
            return FAILED;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// Всё, что ведёт агент; запросы окна обслуживаются отсюда.
struct Agent {
    ping: Arc<AgentPing>,
    /// Статистика трафика по `State` ядра (`core_poll`), файл `Stats.ini`.
    stats: Arc<AgentStats>,
    /// История скорости туннелей для графика окна (`history`), файл `history.bin`.
    history: Arc<AgentHistory>,
    /// Журнал событий: единственный писатель `events.log` (события ядра по курсору и свои).
    journal: Arc<AgentJournal>,
    /// Обновления и откаты; `None` — только у агента в проверках других запросов (отказ с объяснением).
    /// С ядром менеджер говорит по его каналу (`update::core_link::pipe_core`).
    updates: Option<Arc<Manager>>,
    /// Конфиги туннелей: помощник в родном окне AmneziaWG и хранилище режима 2 (`tunnels`).
    tunnels: Tunnels,
}

impl Agent {
    /// Журнал событий (файл ведёт агент), язык ядра (`language`), настройки (`agent.ini`, при первом запуске —
    /// перенос из `core.ini`) и фоновые потоки.
    fn start() -> Agent {
        let journal = Arc::new(AgentJournal::open(super::events_file()));
        // Язык — до первого текста агента: иначе записи ниже и всё, что пишут потоки, были бы на английском.
        let language = Arc::new(AgentLanguage::real(log_to(&journal)));
        let path = AgentConfig::path();
        let (config, notes) = AgentConfig::load_or_migrate(&path, &super::Config::path());
        for text in &notes {
            journal.log(Severity::Bad, text);
        }
        let ping = Arc::new(AgentPing::real(config, path, note_to(&journal)));
        crate::ping::spawn(ping.clone());
        let stats = Arc::new(AgentStats::real(log_to(&journal)));
        let history = Arc::new(AgentHistory::real(log_to(&journal)));
        spawn_core_poll(stats.clone(), history.clone(), journal.clone(), language);
        // Обновления ведёт агент; ядро на `Updates` отвечает окну прежней версии отказом.
        let updates = updates::start(journal.clone());
        let tunnels = Tunnels::real(log_to(&journal));
        Agent { ping, stats, history, journal, updates: Some(updates), tunnels }
    }

    fn state(&self) -> AgentState {
        AgentState { ping: self.ping.dto(), stats: self.stats.snapshot(), native_ui: self.updates.as_ref().map(|m| m.native_ui()) }
    }
}

/// Опрос ядра раз в секунду (`core_poll`): язык (смена `core.ini`), статистика, история скорости и журнал событий
/// (события ядра по курсору). Свои потоки записи `Stats.ini` и `history.bin` — медленный диск не задерживает опрос
/// (файл журнала пишет его `events-writer`). Все вторичные: паника — запись в журнал и пауза.
fn spawn_core_poll(traffic: Arc<AgentStats>, speed: Arc<AgentHistory>, journal: Arc<AgentJournal>, language: Arc<AgentLanguage>) {
    // Язык первым: строки журнала с этого же ответа ядра уже на новом языке.
    let feeds: Vec<Arc<dyn core_poll::CoreFeed>> = vec![language, traffic.clone(), speed.clone(), journal.clone()];
    core_poll::spawn(core_poll::CorePoll::real(log_to(&journal)), feeds, Box::new(report_panic(&journal)));
    let report = report_panic(&journal);
    let pruned_history = speed.clone();
    crate::crash::spawn_named("agent-stats-save", move || {
        crate::crash::nonfatal_loop(stats::SAVE_EVERY, &std::thread::sleep, &report, || {
            // История туннеля, чья статистика убрана по сроку, уходит вместе с ней.
            pruned_history.drop_pruned(&traffic.tick(crate::monitor::unix_now()));
            std::ops::ControlFlow::Continue(())
        });
    });
    let report = report_panic(&journal);
    crate::crash::spawn_named("agent-history-save", move || {
        crate::crash::nonfatal_loop(history::SAVE_EVERY, &std::thread::sleep, &report, || {
            speed.tick();
            std::ops::ControlFlow::Continue(())
        });
    });
}

/// Паника шага вторичного потока агента: в журнал — с паузой до следующего шага.
fn report_panic(journal: &Arc<AgentJournal>) -> impl Fn(&str, Duration) + Send + 'static {
    let journal = journal.clone();
    move |panic, wait| journal.log(Severity::Bad, &trf("core.secondary_failed", &[panic, &wait.as_secs().to_string()]))
}

/// Запись в журнал агента — для частей, которым журнал передаётся функцией.
fn log_to(journal: &Arc<AgentJournal>) -> Box<dyn Fn(Severity, &str) + Send + Sync> {
    let journal = journal.clone();
    Box::new(move |severity, text| journal.log(severity, text))
}

/// Запись об ошибке в журнал агента.
fn note_to(journal: &Arc<AgentJournal>) -> Box<dyn Fn(&str) + Send + Sync> {
    let journal = journal.clone();
    Box::new(move |text| journal.log(Severity::Bad, text))
}

/// Ответ на один запрос; сам канал ему не нужен — поэтому проверяется без канала.
/// `caller` — кто прислал: сеанс и SID (для помощника в родном окне), права администратора (подтверждение UAC).
fn handle(agent: &Agent, request: Result<AgentRequest, String>, caller: &Caller) -> AgentResponse {
    match request {
        Ok(AgentRequest::Hello) => AgentResponse::Hello { version: env!("CARGO_PKG_VERSION").to_string() },
        Ok(AgentRequest::Updates(op)) => updates(agent, op, caller),
        Ok(AgentRequest::State) => AgentResponse::State(Box::new(agent.state())),
        Ok(AgentRequest::SetPing { enabled, host }) => match agent.ping.set(enabled, host) {
            Ok(()) => AgentResponse::Ok,
            Err(e) => AgentResponse::Err(e),
        },
        // История скорости идёт за статистикой: то же новое имя, то же удаление. Выполняются обе, ошибка — первая.
        Ok(AgentRequest::StatsRename { old, new }) => {
            let stats = agent.stats.rename(&old, &new);
            done(stats.and(agent.history.rename(&old, &new)))
        }
        Ok(AgentRequest::StatsForget(tunnel)) => {
            let stats = agent.stats.forget(&tunnel);
            done(stats.and(agent.history.forget(&tunnel)))
        }
        // Только чтение, как `State`: место — из бюджета программ владельца (`AGENT_LIMITS`), ответ ограничен длиной ряда.
        Ok(AgentRequest::History { tunnel, range }) => AgentResponse::History(Box::new(agent.history.history(&tunnel, range))),
        Ok(AgentRequest::Events { after }) => AgentResponse::Events(Box::new(agent.journal.since(after))),
        Ok(AgentRequest::Tunnel(req)) => agent.tunnels.handle(req, caller),
        // Нечитаемая строка или неизвестный вариант запроса: причина (serde называет вариант) уходит клиенту.
        Err(e) => AgentResponse::Refused(e),
    }
}

/// Ответ на запрос без данных.
fn done(result: Result<(), String>) -> AgentResponse {
    result.map_or_else(AgentResponse::Err, |()| AgentResponse::Ok)
}

/// Команда окна «Обновления и откаты» — менеджеру агента, по тем же правилам доступа, что у ядра. Отказ в возврате
/// версии — ещё и в журнал с учётной записью того, кто просил: попытка отката без UAC — событие для аудита.
fn updates(agent: &Agent, op: crate::update::UpdateOp, caller: &Caller) -> AgentResponse {
    let Some(manager) = &agent.updates else {
        return AgentResponse::Refused(tr("agent.updates_in_core"));
    };
    if let Err(e) = crate::update::updates_allowed(&op, caller.elevated) {
        agent.journal.log(Severity::Warn, &trf("core.restore_refused", &[caller.sid.as_deref().unwrap_or("?")]));
        return AgentResponse::Err(e);
    }
    match manager.handle(op) {
        Ok(state) => AgentResponse::Updates(Box::new(state)),
        Err(e) => AgentResponse::Err(e),
    }
}

/// То же, но паника в обработчике не роняет процесс: она записана хуком, клиент получает отказ.
fn handle_isolated(agent: &Agent, request: Result<AgentRequest, String>, caller: &Caller) -> AgentResponse {
    crate::crash::isolate(|| handle(agent, request, caller)).unwrap_or_else(|panic| AgentResponse::Refused(panic))
}

/// Цикл канала: ждать клиента и отвечать ему в своём потоке.
fn serve(agent: Arc<Agent>) {
    let owner = super::Config::load().owner_sid;
    let (mut server, bad_owner) = Server::new(PIPE_NAME, &owner);
    if let Some(bad) = bad_owner {
        agent.journal.log(Severity::Bad, &trf("core.bad_owner", &[&bad]));
    }
    let connections = Budgets::new(AGENT_LIMITS);
    let mut last_logged: Option<(Instant, String)> = None;
    loop {
        let mut conn = match server.accept() {
            Ok(conn) => conn,
            Err(e) => {
                let repeated = last_logged.as_ref().is_some_and(|(at, text)| *text == e.text && at.elapsed() < ACCEPT_LOG_EVERY);
                if !repeated {
                    agent.journal.log(Severity::Bad, &trf("agent.pipe_failed", &[&e.text]));
                    last_logged = Some((Instant::now(), e.text));
                }
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        let Some((slot, hello_only)) = connections.admit(conn.from_system()) else {
            // Не дошёл отказ — клиент сам увидит ошибку канала.
            drop(conn.reply_with(&AgentResponse::Refused("agent busy".into())));
            continue;
        };
        let agent = agent.clone();
        crate::crash::spawn_named("agent-request", move || {
            // Место возвращается при выходе из потока при любом исходе.
            let _slot = slot;
            let caller = Caller { session: conn.session, sid: conn.client_sid.clone(), elevated: conn.elevated };
            let response = answer(&agent, conn.read_as(), &caller, hello_only);
            drop(conn.reply_with(&response));
        });
    }
}

/// Ответ на прочитанный запрос; на месте из запаса проверки (`hello_only`) отвечают только на `Hello`.
fn answer(agent: &Agent, request: Result<AgentRequest, String>, caller: &Caller, hello_only: bool) -> AgentResponse {
    match request {
        Ok(req) if hello_only && !matches!(req, AgentRequest::Hello) => AgentResponse::Refused("agent busy".into()),
        request => handle_isolated(agent, request, caller),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(line: &str) -> Result<AgentRequest, String> {
        serde_json::from_str(line).map_err(|e| format!("pipe: {e}"))
    }

    /// Агент без потоков и без ядра: `agent.ini` во временной папке теста.
    fn agent_in(dir: &std::path::Path) -> Agent {
        let config = AgentConfig { ping: true, ping_host: "1.1.1.1".into() };
        let ping = AgentPing::new(config, dir.join("agent.ini"), Box::new(|| Ok(false)), Box::new(|_| Ok(1)), Box::new(|_| {}));
        let traffic = AgentStats::new(dir.join("Stats.ini"), Box::new(crate::stats::save), Box::new(|_, _| {}));
        let tunnels = Tunnels::real(Box::new(|_, _| {}));
        let history = AgentHistory::new(dir.join("history.bin"), Box::new(|| HISTORY_NOW), Box::new(|_, _| {}));
        Agent { ping: Arc::new(ping), stats: Arc::new(traffic), history: Arc::new(history), journal: Arc::new(AgentJournal::memory()), updates: None, tunnels }
    }

    /// Часы истории в проверках агента: полночь UTC.
    const HISTORY_NOW: u64 = 1_789_948_800;

    /// Ответ ядра с туннелями `names`, у каждого принято `rx` байт.
    fn core_with(names: &[&str], rx: u64) -> crate::daemon::proto::CoreState {
        let status = crate::uapi::Status { listen_port: 1, peers: vec![crate::uapi::Peer { rx_bytes: rx, ..Default::default() }], ..Default::default() };
        let running = names.iter().map(|n| (n.to_string(), Ok(status.clone()))).collect();
        crate::daemon::proto::CoreState { tunnels: names.iter().map(|n| n.to_string()).collect(), running, ..Default::default() }
    }

    fn history_of(agent: &Agent, tunnel: &str, range: proto::HistoryRange) -> proto::History {
        let line = serde_json::to_string(&AgentRequest::History { tunnel: tunnel.into(), range }).unwrap();
        match handle(agent, decode(&line), &by(false)) {
            AgentResponse::History(h) => *h,
            other => panic!("{other:?}"),
        }
    }

    /// Запрос истории окна (без прав администратора) отвечается рядом агента; туннеля нет — пустой ряд, не ошибка.
    #[test]
    fn history_request_is_answered_from_the_agents_series() {
        use core_poll::CoreFeed;
        use proto::HistoryRange;
        let dir = temp_dir("history");
        let agent = agent_in(&dir);
        let t0 = Instant::now();
        agent.history.accept(&core_with(&["a"], 0), t0);
        agent.history.accept(&core_with(&["a"], 3_000), t0 + Duration::from_secs(1));
        let day = history_of(&agent, "a", HistoryRange::Day);
        assert_eq!(day.bucket_s, 60);
        assert_eq!(day.buckets.iter().map(|b| (b.start, b.rx)).collect::<Vec<_>>(), [(HISTORY_NOW, 3_000.0)]);
        assert_eq!(history_of(&agent, "b", HistoryRange::Year), proto::History { bucket_s: 86_400, buckets: vec![] });
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Клиент проверок: без сеанса и SID (помощник не запускается); `elevated` — с правами администратора.
    fn by(elevated: bool) -> Caller {
        Caller { session: 0, sid: None, elevated }
    }

    fn test_agent() -> Agent {
        agent_in(&std::env::temp_dir().join("awg-agent-unused"))
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("awg-agent-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn set_ping_is_saved_and_shown_in_state() {
        let dir = temp_dir("setping");
        let agent = agent_in(&dir);
        match handle(&agent, decode("\"State\""), &by(false)) {
            AgentResponse::State(s) => assert_eq!((s.ping.host.as_str(), s.ping.last.clone()), ("1.1.1.1", None)),
            other => panic!("{other:?}"),
        }
        let set = serde_json::to_string(&AgentRequest::SetPing { enabled: false, host: "9.9.9.9".into() }).unwrap();
        assert_eq!(handle(&agent, decode(&set), &by(false)), AgentResponse::Ok);
        let saved = crate::ini::Ini::load(&dir.join("agent.ini"));
        assert_eq!((saved.get("agent", "ping"), saved.get("agent", "ping_host")), (Some("0"), Some("9.9.9.9")));
        match handle(&agent, decode("\"State\""), &by(false)) {
            AgentResponse::State(s) => assert_eq!(s.ping.host, "9.9.9.9", "новый узел виден сразу, не после замера"),
            other => panic!("{other:?}"),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Не записался `agent.ini` (на месте файла — папка): окно получает ошибку с путём, а не «готово».
    #[test]
    fn set_ping_that_cannot_be_saved_is_an_error() {
        let dir = temp_dir("setping-fail");
        std::fs::create_dir_all(dir.join("agent.ini")).unwrap();
        let agent = agent_in(&dir);
        let set = serde_json::to_string(&AgentRequest::SetPing { enabled: true, host: "8.8.8.8".into() }).unwrap();
        match handle(&agent, decode(&set), &by(false)) {
            AgentResponse::Err(e) => assert!(e.contains("agent.ini"), "{e}"),
            other => panic!("{other:?}"),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn stats_names(agent: &Agent) -> Vec<String> {
        match handle(agent, decode("\"State\""), &by(false)) {
            AgentResponse::State(s) => s.stats.into_keys().collect(),
            other => panic!("{other:?}"),
        }
    }

    /// Окно сообщает о переименовании и удалении после ответа ядра: статистика в `State` и в `Stats.ini` меняется сразу.
    #[test]
    fn stats_rename_and_forget_go_through_the_handler() {
        use crate::daemon::proto::CoreState;
        use core_poll::CoreFeed;
        let dir = temp_dir("stats");
        let agent = agent_in(&dir);
        let mut core = CoreState { tunnels: vec!["a".into(), "b".into()], ..Default::default() };
        for name in ["a", "b"] {
            core.running.insert(name.into(), Ok(Default::default()));
        }
        agent.stats.accept(&core, Instant::now());
        assert_eq!(stats_names(&agent), ["a", "b"]);
        // История скорости тех же туннелей идёт за статистикой.
        let t0 = Instant::now();
        agent.history.accept(&core_with(&["a", "b"], 0), t0);
        agent.history.accept(&core_with(&["a", "b"], 100), t0 + Duration::from_secs(1));
        let rename = serde_json::to_string(&AgentRequest::StatsRename { old: "a".into(), new: "c".into() }).unwrap();
        assert_eq!(handle(&agent, decode(&rename), &by(false)), AgentResponse::Ok);
        let forget = serde_json::to_string(&AgentRequest::StatsForget("b".into())).unwrap();
        assert_eq!(handle(&agent, decode(&forget), &by(false)), AgentResponse::Ok);
        assert_eq!(stats_names(&agent), ["c"]);
        assert_eq!(crate::stats::load(&dir.join("Stats.ini")).into_keys().collect::<Vec<_>>(), ["c"]);
        let has_history = |name| !history_of(&agent, name, proto::HistoryRange::Day).buckets.is_empty();
        assert_eq!((has_history("a"), has_history("b"), has_history("c")), (false, false, true));
        // В файле сразу, не через минуту: перезапущенный агент видит то же.
        let again = agent_in(&dir);
        assert_eq!(history_of(&again, "c", proto::HistoryRange::Day).buckets.len(), 1);
        assert!(history_of(&again, "b", proto::HistoryRange::Day).buckets.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Не записался `Stats.ini` (на месте файла — папка): окно получает ошибку с путём, а не «готово».
    #[test]
    fn stats_forget_that_cannot_be_saved_is_an_error() {
        let dir = temp_dir("stats-fail");
        std::fs::create_dir_all(dir.join("Stats.ini")).unwrap();
        let agent = agent_in(&dir);
        match handle(&agent, Ok(AgentRequest::StatsForget("x".into())), &by(false)) {
            AgentResponse::Err(e) => assert!(e.contains("Stats.ini"), "{e}"),
            other => panic!("{other:?}"),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn hello_is_answered_with_the_version() {
        match handle(&test_agent(), decode("\"Hello\""), &by(false)) {
            AgentResponse::Hello { version } => assert_eq!(version, env!("CARGO_PKG_VERSION")),
            other => panic!("{other:?}"),
        }
    }

    /// Молчащие подключения программ владельца заняли все места окна: Hello сторожа ядра (SYSTEM) всё равно
    /// принимается и отвечается, иначе три отказа подряд убили бы живого агента.
    #[test]
    fn system_hello_is_answered_while_the_user_budget_is_full() {
        let connections = Budgets::new(AGENT_LIMITS);
        let _users: Vec<_> = (0..MAX_CONNECTIONS).map(|_| connections.admit(false).expect("место окна")).collect();
        assert!(connections.admit(false).is_none(), "окну сверх бюджета — отказ");
        let (_slot, hello_only) = connections.admit(true).expect("SYSTEM при занятых местах окна");
        assert!(!hello_only, "свой бюджет SYSTEM — полноценное место");
        match answer(&test_agent(), decode("\"Hello\""), &by(false), hello_only) {
            AgentResponse::Hello { .. } => {}
            other => panic!("{other:?}"),
        }
    }

    /// Заняты и места SYSTEM: запас отвечает на Hello и больше ни на что.
    #[test]
    fn reserve_slot_answers_only_hello() {
        let connections = Budgets::new(AGENT_LIMITS);
        let _system: Vec<_> = (0..MAX_SYSTEM_CONNECTIONS).map(|_| connections.admit(true).expect("место SYSTEM")).collect();
        let (_slot, hello_only) = connections.admit(true).expect("запас для Hello");
        assert!(hello_only);
        assert!(connections.admit(true).is_none(), "запас один");
        assert!(matches!(answer(&test_agent(), decode("\"Hello\""), &by(false), hello_only), AgentResponse::Hello { .. }));
        assert!(matches!(answer(&test_agent(), decode("\"State\""), &by(false), hello_only), AgentResponse::Refused(_)));
    }

    #[test]
    fn unknown_request_gets_a_clear_refusal() {
        // Запрос ядра, которого у агента нет, и просто мусор.
        for line in ["{\"Switch\":{\"tunnel\":\"a\"}}", "\"Nonsense\"", "not json"] {
            match handle_isolated(&test_agent(), decode(line), &by(false)) {
                AgentResponse::Refused(text) => assert!(text.starts_with("pipe: "), "{line}: {text}"),
                other => panic!("{line}: {other:?}"),
            }
        }
        match handle(&test_agent(), decode("\"Nonsense\""), &by(false)) {
            AgentResponse::Refused(text) => assert!(text.contains("unknown variant `Nonsense`"), "{text}"),
            other => panic!("{other:?}"),
        }
    }

    /// Агент с менеджером обновлений над подделкой ядра (`RecordingCore`), без сети и фоновой проверки.
    fn with_updates() -> (Agent, Arc<Manager>) {
        use crate::monitor::{Options, Shared};
        let options = Options { ping: false, ping_host: String::new(), notify: false, tray: false, taskbar: false };
        let manager = Manager::for_core_tests(Arc::new(Shared::new(None, options, None)));
        let mut agent = test_agent();
        agent.updates = Some(manager.clone());
        (agent, manager)
    }

    #[test]
    fn updates_go_to_the_agents_manager() {
        use crate::update::UpdateOp;
        let (agent, manager) = with_updates();
        let expected = AgentResponse::Updates(Box::new(manager.handle(UpdateOp::State).unwrap()));
        assert_eq!(handle_isolated(&agent, decode("{\"Updates\":\"State\"}"), &by(false)), expected);
    }

    /// Правило ядра действует и у агента: возврат версии — только от клиента с правами администратора.
    #[test]
    fn agent_refuses_restore_without_elevation() {
        use crate::update::UpdateOp;
        let (agent, manager) = with_updates();
        let before = manager.handle(UpdateOp::State).unwrap();
        let r = handle(&agent, Ok(AgentRequest::Updates(UpdateOp::Restore(1))), &by(false));
        assert_eq!(r, AgentResponse::Err(tr("core.restore_needs_admin")));
        assert_eq!(manager.handle(UpdateOp::State).unwrap(), before, "работа не начата");
    }

    /// Отказ в откате без UAC — запись аудита с учётной записью просившего (до шага 8b её писало ядро).
    #[test]
    fn refused_restore_is_logged_with_the_callers_account() {
        use crate::update::UpdateOp;
        let (agent, _manager) = with_updates();
        let caller = Caller { session: 1, sid: Some("S-1-5-21-7".into()), elevated: false };
        handle(&agent, Ok(AgentRequest::Updates(UpdateOp::Restore(1))), &caller);
        handle(&agent, Ok(AgentRequest::Updates(UpdateOp::State)), &caller);
        let logged: Vec<_> = agent.journal.since(0).events.into_iter().map(|(_, e)| (e.severity, e.text)).collect();
        assert_eq!(logged, vec![(Severity::Warn, trf("core.restore_refused", &["S-1-5-21-7"]))], "одна запись, только об отказе");
    }

    /// Пока менеджер в ядре, агент на `Updates` отвечает понятным отказом, а не молчит и не падает.
    #[test]
    fn updates_without_a_manager_are_refused_with_a_reason() {
        let r = handle(&test_agent(), decode("{\"Updates\":\"Check\"}"), &by(true));
        assert_eq!(r, AgentResponse::Refused(tr("agent.updates_in_core")));
    }

    /// Имя канала — в пространстве имён каналов, как у ядра (было `\.\pipe\…` — такой канал не создаётся вовсе).
    #[test]
    fn agent_pipe_name_is_a_local_pipe_path_distinct_from_the_core() {
        let prefix = r"\\.\pipe\";
        assert!(PIPE_NAME.starts_with(prefix) && super::super::pipe::NAME.starts_with(prefix), "{PIPE_NAME}");
        assert_ne!(PIPE_NAME, super::super::pipe::NAME);
    }

    /// Агент не тянет VPN-код: туннели, переключение и переподключение остаются в ядре.
    #[test]
    fn agent_does_not_reach_into_vpn_code() {
        let forbidden = ["daemon::server", "daemon::retry", "daemon::deadwatch", "daemon::restore", "daemon::netwatch", "crate::backend", "crate::engine", "crate::uapi", "switching"];
        for (name, source) in [
            ("mod.rs", include_str!("mod.rs")),
            ("proto.rs", include_str!("proto.rs")),
            ("ping.rs", include_str!("ping.rs")),
            ("settings.rs", include_str!("settings.rs")),
            ("client.rs", include_str!("client.rs")),
            ("core_poll.rs", include_str!("core_poll.rs")),
            ("journal.rs", include_str!("journal.rs")),
            ("stats.rs", include_str!("stats.rs")),
            ("history.rs", include_str!("history.rs")),
            ("updates.rs", include_str!("updates.rs")),
            ("tunnels.rs", include_str!("tunnels.rs")),
            ("language.rs", include_str!("language.rs")),
        ] {
            let code = source.split("#[cfg(test)]").next().unwrap();
            for word in forbidden {
                assert!(!code.contains(word), "agent/{name} mentions {word}");
            }
        }
    }
}
