//! Ядро — служба Windows «AmneziaWG UI Dark Core» (SYSTEM, запускается вместе с Windows): режим работы,
//! туннели обоих режимов, опрос раз в секунду, статистика, журнал событий (пинг ведёт агент, `agent`). Окно — только
//! интерфейс: говорит с ядром по именованному каналу (`pipe`, протокол `proto`); закрыли окно — всё продолжает работать.

pub mod agent;
pub(crate) mod agent_watch;
pub(crate) mod budget;
mod deadwatch;
pub mod helper;
pub mod install;
pub mod pipe;
pub mod proto;
mod netwatch;
pub(crate) mod phase_trace;
pub(crate) mod restore;
pub(crate) mod retry;
pub mod server;
pub mod service;
pub mod session;
#[cfg(test)]
pub mod fake;

use std::path::PathBuf;

use proto::{NativeOp, Request, Response};

use crate::events::{Event, Severity};
use crate::ini::Ini;
use crate::settings::Mode;

/// Имя службы ядра и ключ, с которым её запускает Windows.
pub const SERVICE: &str = "AwgUiCore";
pub const SERVICE_FLAG: &str = "--core";
/// Папка данных ядра (настройки, статистика, журнал, временные файлы помощника): только SYSTEM и администраторы.
pub const DATA_SDDL: &str = "O:BAD:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";

/// `C:\ProgramData\AmneziaWG UI Dark`.
pub fn data_dir() -> PathBuf {
    crate::win::program_data().join("AmneziaWG UI Dark")
}

/// Папка файлов языков (`.lng`) ядра и агента: только папка программы в Program Files (туда пишут администраторы),
/// не папка окна. Нет файла — встроенный перевод или английский.
fn lang_dir() -> PathBuf {
    crate::engine::install_dir().join("lang")
}

/// Язык процессов без окна, которые запускаются отдельно от ядра и агента (помощник родного окна `--native-op`,
/// перезапуск ядра `--restart-core`): язык ядра из `core.ini`, как у агента (`agent::language`). Без этого их
/// тексты (ошибки окна AmneziaWG, записи об откате обновления) были бы на английском при русском окне.
/// Нечитаемый `core.ini` — язык по умолчанию, как у ядра; файл только читается.
pub fn use_core_language() {
    crate::i18n::set(&lang_dir(), &core_language(&Config::path()));
}

fn core_language(core_ini: &std::path::Path) -> String {
    Config::load_from(core_ini).language
}

/// Журнал событий ядра.
pub fn events_file() -> PathBuf {
    data_dir().join("logs").join("events.log")
}

/// Настройки ядра: `core.ini` в папке данных.
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub mode: Mode,
    /// Язык журнала событий — язык окна владельца (код ISO 639-2).
    pub language: String,
    /// SID учётной записи владельца: ей (кроме SYSTEM и администраторов) открыт канал ядра.
    pub owner_sid: String,
    /// Туннели, которые пользователь оставил подключёнными (режима `mode`): после перезапуска ядра они поднимаются
    /// снова (`restore`). Меняются только командами пользователя — не тем, что туннель упал сам или Windows выключается.
    /// `None` — `core.ini` прежней версии, где набора ещё не было: при запуске ядро берёт работающие туннели.
    pub tunnels: Option<Vec<String>>,
    /// «Несколько туннелей одновременно» последнего подключения: по нему восстановление решает, кого заменить.
    pub multiple: bool,
}

impl Config {
    pub fn path() -> PathBuf {
        data_dir().join("core.ini")
    }

    /// Настройки ядра для чтения (окно берёт из них SID владельца): сбой чтения даёт умолчания, файл не трогается.
    pub fn load() -> Config {
        Self::load_from(&Self::path())
    }

    /// То же из заданного файла: агент читает язык ядра (`agent::language`), проверки — свой файл.
    fn load_from(path: &std::path::Path) -> Config {
        Self::from_ini(&Ini::load(path))
    }

    /// Настройки ядра, которые затем будут перезаписаны (запуск и установка ядра): нечитаемый `core.ini` отодвигается
    /// в сторону (см. `Ini::load_guarded`), а не пропадает под записью. Вызывающий обязан показать `Unreadable`
    /// (`log_unreadable`) — ядро с настройками по умолчанию иначе молча забыло бы режим и владельца.
    pub fn load_guarded() -> (Config, Option<crate::ini::Unreadable>) {
        Self::load_guarded_from(&Self::path())
    }

    fn load_guarded_from(path: &std::path::Path) -> (Config, Option<crate::ini::Unreadable>) {
        let (ini, problem) = Ini::load_guarded(path);
        (Self::from_ini(&ini), problem)
    }

    fn from_ini(ini: &Ini) -> Config {
        Config {
            mode: if ini.get("core", "mode") == Some(Mode::Engine.as_str()) { Mode::Engine } else { Mode::Overlay },
            owner_sid: ini.get("core", "owner_sid").unwrap_or_default().to_string(),
            language: ini.get("core", "language").filter(|c| crate::i18n::is_code(c)).unwrap_or(crate::i18n::DEFAULT).to_string(),
            // Имя туннеля уходит в пути и командные строки — только допустимые (запятой в них нет).
            tunnels: ini.get("core", "tunnels").map(|v| v.split(',').filter(|t| crate::engine::valid_name(t)).map(str::to_string).collect()),
            multiple: ini.get_bool("core", "multiple", false),
        }
    }

    pub fn save(&self) -> Result<(), String> {
        self.save_to(&Self::path())
    }

    fn save_to(&self, path: &std::path::Path) -> Result<(), String> {
        // Поверх файла, а не с чистого листа: чужие ключи остаются. Так `ping`/`ping_host` ядра прежней версии
        // доживают до первого запуска агента, который переносит их в `agent.ini` (`agent::AgentConfig`).
        let mut ini = Ini::load(path);
        ini.set("core", "mode", self.mode.as_str());
        ini.set("core", "owner_sid", &self.owner_sid);
        ini.set("core", "language", &self.language);
        if let Some(tunnels) = &self.tunnels {
            ini.set("core", "tunnels", tunnels.join(","));
        }
        ini.set_bool("core", "multiple", self.multiple);
        ini.save(path).map_err(|e| crate::fsutil::io_ctx(path, e))
    }
}

/// Записать в журнал событий ядра сообщение о нечитаемом `core.ini`.
pub fn log_unreadable(problem: &crate::ini::Unreadable) {
    log_notice(events_file(), Severity::Bad, &problem.text());
}

/// Запись в файл журнала напрямую, а не через `Shared`: установка и ранний отказ запуска (нет владельца)
/// заканчиваются раньше, чем он появится. Только дописать строку: файл ведёт и ротирует агент, он и покажет запись.
pub(crate) fn log_notice(file: PathBuf, severity: Severity, text: &str) {
    let event = Event::new(crate::monitor::unix_now(), "", severity, text, false);
    if let Err(e) = crate::events::append_event(&file, &event) {
        // Служба без консоли: это всё, что остаётся, когда не пишется сам журнал.
        eprintln!("core: cannot write {}: {e}", file.display());
    }
}

/// Клиентская сторона ядра: всё, что окно и ключи командной строки просят у ядра. Реализации — канал
/// (`PipeClient`) и подделка в тестах (`fake::FakeCore`): так потоки окна проверяются без службы.
/// Обязательна одна `call`; разбор ответа и соответствие «операция -> запрос» общие для всех реализаций.
pub trait CoreApi: Send + Sync {
    /// Один запрос, ответ как есть. Ошибка — ядро недоступно или не ответило.
    fn call(&self, req: Request) -> Result<Response, String>;

    /// Запрос к ядру без данных в ответе.
    fn ok(&self, req: Request) -> Result<(), String> {
        match self.call(req)? {
            Response::Ok => Ok(()),
            other => Err(unexpected(other)),
        }
    }

    fn text(&self, req: Request) -> Result<String, String> {
        match self.call(req)? {
            Response::Text(t) => Ok(t),
            other => Err(unexpected(other)),
        }
    }

    fn info(&self, req: Request) -> Result<crate::conf::TunnelInfo, String> {
        match self.call(req)? {
            Response::Info(i) => Ok(i),
            other => Err(unexpected(other)),
        }
    }

    fn entries(&self, req: Request) -> Result<Vec<crate::archive::Entry>, String> {
        match self.call(req)? {
            Response::Entries(e) => Ok(e),
            other => Err(unexpected(other)),
        }
    }

    fn report(&self, req: Request) -> Result<crate::store::ImportReport, String> {
        match self.call(req)? {
            Response::Report(r) => Ok(r),
            other => Err(unexpected(other)),
        }
    }

    /// Версия ядра и режим; ядро не отвечает — ошибка.
    fn hello(&self) -> Result<(String, Mode), String> {
        match self.call(Request::Hello)? {
            Response::Hello { version, mode } => Ok((version, mode)),
            other => Err(unexpected(other)),
        }
    }

    // Операции над туннелями, которые окно делает через ядро; запрос для каждой — только здесь.

    fn delete_tunnel(&self, tunnel: &str) -> Result<(), String> {
        self.ok(Request::Delete(tunnel.into()))
    }

    /// Сведения о неподключённом туннеле.
    fn details(&self, tunnel: &str) -> Result<crate::conf::TunnelInfo, String> {
        self.info(Request::Details(tunnel.into()))
    }

    fn read_config(&self, tunnel: &str) -> Result<String, String> {
        self.text(Request::Read(tunnel.into()))
    }

    fn write_config(&self, tunnel: &str, text: &str) -> Result<(), String> {
        self.ok(Request::Write { tunnel: tunnel.into(), text: text.into() })
    }

    /// Действие в родном окне AmneziaWG.
    fn native(&self, op: NativeOp) -> Result<(), String> {
        self.ok(Request::Native(op))
    }

    /// «Повторить»: переподключение туннеля по расписанию с начала.
    fn retry_tunnel(&self, tunnel: &str) -> Result<(), String> {
        self.ok(Request::Retry(tunnel.into()))
    }

    /// Родное окно: импорт туннеля из файла (с `file` — диалог сразу на нём).
    fn import_in_native(&self, file: Option<&std::path::Path>) -> Result<(), String> {
        self.native(NativeOp::Import(file.map(|f| f.display().to_string())))
    }
}

/// Ядро за именованным каналом.
pub struct PipeClient;

impl CoreApi for PipeClient {
    fn call(&self, req: Request) -> Result<Response, String> {
        pipe::call(&req)
    }
}

fn unexpected(r: Response) -> String {
    match r {
        Response::Err(e) | Response::Refused(e) => e,
        other => crate::i18n::trf("err.core_unexpected", &[&crate::explain::variant_name(&other)]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("awg-core-cfg-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn absent_core_ini_is_defaults_without_a_problem() {
        let dir = dir("absent");
        let (config, problem) = Config::load_guarded_from(&dir.join("core.ini"));
        assert!(problem.is_none());
        assert_eq!(config.mode, Mode::Overlay);
        assert!(config.owner_sid.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Помощник родного окна и `--restart-core` берут язык ядра (раньше — всегда английский).
    #[test]
    fn separate_processes_take_the_core_language() {
        let dir = dir("language");
        let path = dir.join("core.ini");
        assert_eq!(core_language(&path), crate::i18n::DEFAULT, "нет файла — язык по умолчанию");
        std::fs::write(&path, "[core]\r\nmode=overlay\r\nlanguage=rus\r\n").unwrap();
        assert_eq!(core_language(&path), "rus");
        let main = include_str!("../main.rs");
        for entry in ["if has(update::ours::RESTART_FLAG) {", "if let Some(task) = value(daemon::helper::FLAG) {"] {
            let start = main.find(entry).unwrap_or_else(|| panic!("{entry} not found"));
            let body = &main[start..start + main[start..].find("std::process::exit").unwrap()];
            assert!(body.contains("daemon::use_core_language();"), "{entry}: no language before the work");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn normal_core_ini_is_read() {
        let dir = dir("normal");
        let path = dir.join("core.ini");
        std::fs::write(&path, "[core]\r\nmode=engine\r\nping=0\r\nowner_sid=S-1-5-21-1\r\n").unwrap();
        let (config, problem) = Config::load_guarded_from(&path);
        assert!(problem.is_none());
        assert_eq!((config.mode, config.owner_sid.as_str()), (Mode::Engine, "S-1-5-21-1"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Ядро пинга больше не знает, но его ключи в `core.ini` не стирает: их ещё не перенёс агент.
    #[test]
    fn save_keeps_keys_the_core_does_not_own() {
        let dir = dir("foreign");
        let path = dir.join("core.ini");
        std::fs::write(&path, "[core]\r\nmode=overlay\r\nping=0\r\nping_host=9.9.9.9\r\nowner_sid=S-1-5-21-1\r\n").unwrap();
        let (mut config, _) = Config::load_guarded_from(&path);
        config.mode = Mode::Engine;
        config.save_to(&path).unwrap();
        let saved = Ini::load(&path);
        assert_eq!(saved.get("core", "mode"), Some("engine"));
        assert_eq!((saved.get("core", "ping"), saved.get("core", "ping_host")), (Some("0"), Some("9.9.9.9")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn desired_tunnels_survive_a_save_and_load() {
        let dir = dir("desired");
        let path = dir.join("core.ini");
        std::fs::write(&path, "[core]\r\nmode=engine\r\ntunnels=a.v4,b_2,bad name,\r\nmultiple=1\r\n").unwrap();
        let (config, _) = Config::load_guarded_from(&path);
        assert_eq!(config.tunnels.as_deref(), Some(&["a.v4".to_string(), "b_2".to_string()][..]), "недопустимое имя и пустое отброшены");
        assert!(config.multiple);
        let mut ini = Ini::default();
        ini.set("core", "tunnels", "");
        assert_eq!(Config::from_ini(&ini).tunnels, Some(vec![]), "пустой набор — не «неизвестен»");
        assert_eq!(Config::from_ini(&Ini::default()).tunnels, None, "core.ini прежней версии — набор неизвестен");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unreadable_core_ini_is_kept_and_reported_in_the_event_log() {
        let dir = dir("corrupt");
        let path = dir.join("core.ini");
        let bytes = [b'[', b'c', b']', b'\n', 0xff, 0xfe, b'\n'];
        std::fs::write(&path, bytes).unwrap();
        let (config, problem) = Config::load_guarded_from(&path);
        let problem = problem.expect("нечитаемый core.ini — сообщение");
        assert_eq!(config.mode, Mode::Overlay, "настройки по умолчанию");
        assert!(!path.exists(), "config.save() создаст новый файл, а не затрёт прежний");
        assert_eq!(std::fs::read(problem.kept_as.as_ref().unwrap()).unwrap(), bytes);

        let log = dir.join("events.log");
        log_notice(log.clone(), Severity::Bad, &problem.text());
        let reopened = crate::events::EventLog::open(Some(log));
        let texts: Vec<_> = reopened.items.iter().map(|e| (e.severity, e.text.clone())).collect();
        assert_eq!(texts.len(), 1);
        assert_eq!(texts[0].0, Severity::Bad);
        assert!(texts[0].1.contains("core.ini"), "{}", texts[0].1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Ограда ядра (core-split): ядро ведёт только VPN. Обновления, пинг, статистика, помощник родного окна и запись
    /// журнала на диск — у агента (`daemon::agent`); упоминание их в коде ядра — это работа, вернувшаяся в службу
    /// SYSTEM, которую разделение из неё вынесло. Проверка по тексту, как `dialog::window_standard`: тесты и
    /// комментарии не в счёт. `proto.rs` не здесь: его типы (`Request::Updates`, `CoreState.stats`/`ping`) общие
    /// с окном прежней версии, ядро их только разбирает и отказывает.
    #[test]
    fn core_does_not_reach_into_agent_work() {
        let deps = [
            "crate::update", "update::", "crate::ping", "ping::", "crate::stats", "stats::", "TunnelStats",
            "crate::native", "native::", "helper::", "run_elevated",
        ];
        // Прямая запись в файл журнала — только на выходе службы (`service.rs`, после `server::run`), не на живых путях.
        let live_io = ["append_event", "EventLog::open", "events_file"];
        let monitor = include_str!("../monitor.rs");
        let core_path = [
            ("server.rs", code_of(include_str!("server.rs")), true),
            ("retry.rs", code_of(include_str!("retry.rs")), true),
            ("deadwatch.rs", code_of(include_str!("deadwatch.rs")), true),
            ("restore.rs", code_of(include_str!("restore.rs")), true),
            ("agent_watch.rs", code_of(include_str!("agent_watch.rs")), true),
            ("netwatch.rs", code_of(include_str!("netwatch.rs")), true),
            ("service.rs", code_of(include_str!("service.rs")), false),
            ("monitor.rs::spawn", fn_text(monitor, "pub fn spawn("), true),
            ("monitor.rs::poll", fn_text(monitor, "pub fn poll("), true),
            ("monitor.rs::core_state", fn_text(monitor, "pub fn core_state("), true),
        ];
        let mut bad = Vec::new();
        for (name, code, live) in &core_path {
            assert!(!code.trim().is_empty(), "{name}: nothing to check");
            let words = deps.iter().chain(if *live { live_io.iter() } else { [].iter() });
            bad.extend(words.filter(|w| code.contains(**w)).map(|w| format!("{name} mentions {w}")));
        }
        assert!(bad.is_empty(), "core fence broken:\n{}", bad.join("\n"));
    }

    #[test]
    fn core_fence_sees_code_not_comments_or_tests() {
        let code = code_of("use crate::ping::PingState;\n// crate::stats in a comment\n#[cfg(test)]\nmod tests { crate::update }\n");
        assert!(code.contains("crate::ping") && !code.contains("crate::stats") && !code.contains("crate::update"), "{code}");
        let f = fn_text("fn a() {\n}\npub fn poll(x: u8) {\n    ping::measure();\n}\nfn b() { stats::x }\n", "pub fn poll(");
        assert!(f.contains("ping::measure") && !f.contains("stats::"), "{f}");
    }

    /// Код без тестов и строк-комментариев.
    fn code_of(source: &str) -> String {
        let code = source.split("#[cfg(test)]").next().unwrap_or_default();
        code.lines().filter(|l| !l.trim_start().starts_with("//")).collect::<Vec<_>>().join("\n")
    }

    /// Текст одной функции: от сигнатуры до закрывающей скобки на её отступе.
    fn fn_text(source: &str, signature: &str) -> String {
        // Рабочая копия может быть с CRLF (autocrlf): закрывающую скобку ищем по `\n`.
        let source = &source.replace("\r\n", "\n");
        let start = source.find(signature).unwrap_or_else(|| panic!("{signature} not found"));
        let indent = &source[source[..start].rfind('\n').map_or(0, |i| i + 1)..start];
        let close = format!("\n{indent}}}\n");
        let end = source[start..].find(&close).map_or(source.len(), |i| start + i + close.len());
        code_of(&source[start..end])
    }
}
