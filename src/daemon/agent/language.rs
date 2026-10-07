//! Язык агента — язык ядра из `core.ini` (его пишет ядро по `SetLanguage` окна). Без этого все тексты агента
//! (ход и ошибки обновлений, записи журнала, ошибки помощника) были бы на английском при русском окне.
//!
//! Как узнать о смене: агент сверяет время изменения и размер `core.ini` при каждом ответе ядра в опросе `core_poll`
//! (раз в секунду) и перечитывает файл, только когда они изменились. Выбрано вместо отдельного запроса окна к
//! агенту: источник один — файл ядра, окну не нужно помнить второй вызов, и смена языка, пока агент перезапускался,
//! тоже не теряется. Опрос идёт, пока ядро отвечает, — а `core.ini` меняет только живое ядро. Ядро пишет файл через
//! временный (`Ini::save`), так что половину файла агент не прочтёт.
//!
//! `core.ini` агент только читает (`Config` без `load_guarded`: нечитаемый файл отодвигает и показывает ядро).
//! При запуске нечитаемый файл даёт язык по умолчанию — как у ядра; на ходу язык остаётся прежним, причина — в журнал.
//! Уже записанные тексты (строки журнала, история обновлений) остаются на языке, на котором были записаны;
//! новый язык — только у новых.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Instant, SystemTime};

use super::core_poll::{CoreFeed, Log};
use crate::daemon::proto::CoreState;
use crate::daemon::Config;
use crate::events::Severity;
use crate::ini::Ini;

/// Выбор языка процесса (`i18n::set` у настоящего, запись — в проверках).
pub(super) type Apply = dyn Fn(&str) + Send + Sync;

/// Отпечаток файла: смена языка меняет время изменения (и обычно размер). `None` — файла нет или он не читается.
type Stamp = Option<(SystemTime, u64)>;

pub(super) struct AgentLanguage {
    core_ini: PathBuf,
    apply: Box<Apply>,
    log: Box<Log>,
    /// Отпечаток последнего прочтения и выбранный код.
    seen: Mutex<(Stamp, String)>,
}

impl AgentLanguage {
    /// Прочитать язык из `core_ini` и сразу выбрать его.
    pub(super) fn new(core_ini: PathBuf, apply: Box<Apply>, log: Box<Log>) -> AgentLanguage {
        let stamp = stamp_of(&core_ini);
        let code = Config::load_from(&core_ini).language;
        apply(&code);
        AgentLanguage { core_ini, apply, log, seen: Mutex::new((stamp, code)) }
    }

    /// Настоящий: `core.ini` ядра и файлы `.lng` из той же папки, что у ядра (`daemon::lang_dir`).
    pub(super) fn real(log: Box<Log>) -> AgentLanguage {
        AgentLanguage::new(Config::path(), Box::new(|code| crate::i18n::set(&crate::daemon::lang_dir(), code)), log)
    }

    /// Файл изменился — перечитать; язык выбирается заново, только если код другой (ядро пишет `core.ini` и при
    /// смене туннелей). Недопустимый или пропавший код — как у ядра при запуске: язык по умолчанию (`Config`).
    pub(super) fn refresh(&self) {
        let stamp = stamp_of(&self.core_ini);
        let mut seen = crate::crash::lock(&self.seen);
        if seen.0 == stamp {
            return;
        }
        seen.0 = stamp;
        let ini = match Ini::read(&self.core_ini) {
            Ok(ini) => ini,
            // Тот же отпечаток не перечитывается: одна запись на одно состояние файла, а не раз в секунду.
            Err(e) => return (self.log)(Severity::Warn, &crate::fsutil::io_ctx(&self.core_ini, e)),
        };
        let code = Config::from_ini(&ini).language;
        if code != seen.1 {
            (self.apply)(&code);
            seen.1 = code;
        }
    }
}

impl CoreFeed for AgentLanguage {
    fn accept(&self, _: &CoreState, _: Instant) {
        self.refresh();
    }
}

fn stamp_of(path: &Path) -> Stamp {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("awg-agent-lang-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Язык в `core.ini` так, как его пишет ядро; время изменения ставится явно — запись в ту же единицу времени
    /// файловой системы иначе могла бы его не сдвинуть.
    fn write_core_ini(path: &Path, language: &str, tick: u64) {
        std::fs::write(path, format!("[core]\r\nmode=engine\r\nowner_sid=S-1-5-21-1\r\nlanguage={language}\r\n")).unwrap();
        let at = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000 + tick);
        std::fs::File::options().write(true).open(path).unwrap().set_modified(at).unwrap();
    }

    fn recorder() -> (Arc<Mutex<Vec<String>>>, Box<Apply>) {
        let applied = Arc::new(Mutex::new(Vec::new()));
        let a = applied.clone();
        (applied, Box::new(move |code: &str| a.lock().unwrap().push(code.to_string())))
    }

    fn no_log() -> Box<Log> {
        Box::new(|_, text| panic!("неожиданная запись: {text}"))
    }

    #[test]
    fn start_takes_the_core_ini_language() {
        let dir = dir("start");
        let core_ini = dir.join("core.ini");
        write_core_ini(&core_ini, "rus", 0);
        let before = std::fs::read(&core_ini).unwrap();
        let (applied, apply) = recorder();
        let _language = AgentLanguage::new(core_ini.clone(), apply, no_log());
        assert_eq!(*applied.lock().unwrap(), vec!["rus".to_string()]);
        assert_eq!(std::fs::read(&core_ini).unwrap(), before, "core.ini агент не трогает");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn language_change_in_core_ini_is_followed_once() {
        let dir = dir("change");
        let core_ini = dir.join("core.ini");
        write_core_ini(&core_ini, "eng", 0);
        let (applied, apply) = recorder();
        let language = AgentLanguage::new(core_ini.clone(), apply, no_log());
        language.refresh();
        write_core_ini(&core_ini, "rus", 1);
        language.refresh();
        language.refresh();
        // Ядро переписало файл (туннели), язык тот же: выбирать его заново незачем.
        write_core_ini(&core_ini, "rus", 2);
        language.refresh();
        assert_eq!(*applied.lock().unwrap(), vec!["eng".to_string(), "rus".to_string()]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn invalid_code_falls_back_like_the_core() {
        let dir = dir("invalid");
        let core_ini = dir.join("core.ini");
        write_core_ini(&core_ini, "../x", 0);
        let (applied, apply) = recorder();
        let language = AgentLanguage::new(core_ini.clone(), apply, no_log());
        assert_eq!(*applied.lock().unwrap(), vec![crate::i18n::DEFAULT.to_string()], "как у ядра при запуске");
        write_core_ini(&core_ini, "rus", 1);
        language.refresh();
        write_core_ini(&core_ini, "ru s", 2);
        language.refresh();
        let expected = vec![crate::i18n::DEFAULT.to_string(), "rus".to_string(), crate::i18n::DEFAULT.to_string()];
        assert_eq!(*applied.lock().unwrap(), expected);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Файл не читается на ходу (здесь на его месте папка): язык прежний, одна запись в журнал на это состояние.
    #[test]
    fn unreadable_core_ini_keeps_the_language_and_is_logged_once() {
        let dir = dir("unreadable");
        let core_ini = dir.join("core.ini");
        write_core_ini(&core_ini, "rus", 0);
        let (applied, apply) = recorder();
        let logged = Arc::new(Mutex::new(Vec::new()));
        let l = logged.clone();
        let log: Box<Log> = Box::new(move |severity, text| l.lock().unwrap().push((severity, text.to_string())));
        let language = AgentLanguage::new(core_ini.clone(), apply, log);
        std::fs::remove_file(&core_ini).unwrap();
        std::fs::create_dir(&core_ini).unwrap();
        language.refresh();
        language.refresh();
        assert_eq!(*applied.lock().unwrap(), vec!["rus".to_string()]);
        let logged = logged.lock().unwrap();
        assert_eq!(logged.len(), 1, "{logged:?}");
        assert!(logged[0].0 == Severity::Warn && logged[0].1.contains("core.ini"), "{logged:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
