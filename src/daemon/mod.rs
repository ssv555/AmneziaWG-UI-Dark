//! Ядро — служба Windows «AmneziaWG UI Dark Core» (SYSTEM, запускается вместе с Windows): режим работы,
//! туннели обоих режимов, опрос раз в секунду, статистика, журнал событий, пинг. Окно — только интерфейс:
//! говорит с ядром по именованному каналу (`pipe`, протокол `proto`); закрыли окно — всё продолжает работать.

pub mod helper;
pub mod install;
pub mod pipe;
pub mod proto;
pub mod server;
pub mod service;
pub mod session;
#[cfg(test)]
pub mod fake;

use std::path::PathBuf;

use proto::{NativeOp, Request, Response};

use crate::events::{Event, EventLog, Severity};
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

/// Журнал событий ядра.
pub fn events_file() -> PathBuf {
    data_dir().join("logs").join("events.log")
}

/// Настройки ядра: `core.ini` в папке данных.
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub mode: Mode,
    pub ping: bool,
    pub ping_host: String,
    /// Язык журнала событий — язык окна владельца (код ISO 639-2).
    pub language: String,
    /// SID учётной записи владельца: ей (кроме SYSTEM и администраторов) открыт канал ядра.
    pub owner_sid: String,
}

impl Config {
    pub fn path() -> PathBuf {
        data_dir().join("core.ini")
    }

    /// Настройки ядра для чтения (окно берёт из них SID владельца): сбой чтения даёт умолчания, файл не трогается.
    pub fn load() -> Config {
        Self::from_ini(&Ini::load(&Self::path()))
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
            ping: ini.get_bool("core", "ping", true),
            ping_host: ini.get("core", "ping_host").filter(|h| !h.is_empty()).unwrap_or(crate::settings::DEFAULT_PING_HOST).to_string(),
            owner_sid: ini.get("core", "owner_sid").unwrap_or_default().to_string(),
            language: ini.get("core", "language").filter(|c| crate::i18n::is_code(c)).unwrap_or(crate::i18n::DEFAULT).to_string(),
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let mut ini = Ini::default();
        ini.set("core", "mode", self.mode.as_str());
        ini.set_bool("core", "ping", self.ping);
        ini.set("core", "ping_host", &self.ping_host);
        ini.set("core", "owner_sid", &self.owner_sid);
        ini.set("core", "language", &self.language);
        ini.save(&Self::path()).map_err(|e| crate::fsutil::io_ctx(Self::path(), e))
    }
}

/// Записать в журнал событий ядра сообщение о нечитаемом `core.ini`.
pub fn log_unreadable(problem: &crate::ini::Unreadable) {
    log_notice(events_file(), Severity::Bad, &problem.text());
}

/// Запись в файл журнала ядра напрямую, а не через `Shared`: установка и ранний отказ запуска (нет владельца)
/// заканчиваются раньше, чем он появится. Журнал, открытый позже, подхватывает запись из файла.
fn log_notice(file: PathBuf, severity: Severity, text: &str) {
    EventLog::open(Some(file)).push(Event::new(crate::monitor::unix_now(), "", severity, text, false));
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

    fn updates(&self, op: crate::update::UpdateOp) -> Result<crate::update::UpdatesState, String> {
        match self.call(Request::Updates(op))? {
            Response::Updates(s) => Ok(*s),
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
        other => format!("core: unexpected answer {other:?}"),
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

    #[test]
    fn normal_core_ini_is_read() {
        let dir = dir("normal");
        let path = dir.join("core.ini");
        std::fs::write(&path, "[core]\r\nmode=engine\r\nping=0\r\nowner_sid=S-1-5-21-1\r\n").unwrap();
        let (config, problem) = Config::load_guarded_from(&path);
        assert!(problem.is_none());
        assert_eq!((config.mode, config.ping, config.owner_sid.as_str()), (Mode::Engine, false, "S-1-5-21-1"));
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
        let reopened = EventLog::open(Some(log));
        let texts: Vec<_> = reopened.items.iter().map(|e| (e.severity, e.text.clone())).collect();
        assert_eq!(texts.len(), 1);
        assert_eq!(texts[0].0, Severity::Bad);
        assert!(texts[0].1.contains("core.ini"), "{}", texts[0].1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
