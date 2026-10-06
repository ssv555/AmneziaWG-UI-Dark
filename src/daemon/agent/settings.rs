//! Настройки агента: `agent.ini` в папке данных ядра. Сейчас там пинг; ядро этих ключей больше не читает.

use std::path::{Path, PathBuf};

use crate::ini::Ini;

const SECTION: &str = "agent";

#[derive(Clone, Debug, PartialEq)]
pub struct AgentConfig {
    pub ping: bool,
    pub ping_host: String,
}

impl AgentConfig {
    pub fn path() -> PathBuf {
        super::super::data_dir().join("agent.ini")
    }

    /// Настройки при запуске агента. Первый запуск после обновления (в `agent.ini` ключей ещё нет): пинг берётся из
    /// `core.ini`, где его хранило ядро прежней версии, и сразу записывается в `agent.ini`. Дальше `core.ini` агенту
    /// не нужен: ключи там могут остаться (ядро сохраняет чужие ключи нетронутыми), но не читаются.
    /// `core.ini` агент не пишет никогда — его пишет ядро, две записи вперемешку потеряли бы чьё-то изменение.
    /// Второе значение — что записать в журнал (нечитаемый файл, неудавшаяся запись); работе это не мешает.
    pub fn load_or_migrate(agent_ini: &Path, core_ini: &Path) -> (AgentConfig, Vec<String>) {
        let mut notes = Vec::new();
        let (ini, problem) = Ini::load_guarded(agent_ini);
        if let Some(problem) = problem {
            notes.push(problem.text());
        }
        if ini.get(SECTION, "ping").is_some() {
            return (Self::from_section(&ini, SECTION), notes);
        }
        let config = Self::from_section(&Ini::load(core_ini), "core");
        if let Err(e) = config.save_to(agent_ini) {
            // Пинг работает с этими значениями и так; не записалось — следующий запуск перенесёт ключи снова.
            notes.push(e);
        }
        (config, notes)
    }

    fn from_section(ini: &Ini, section: &str) -> AgentConfig {
        AgentConfig {
            ping: ini.get_bool(section, "ping", true),
            ping_host: ini.get(section, "ping_host").filter(|h| !h.is_empty()).unwrap_or(crate::settings::DEFAULT_PING_HOST).to_string(),
        }
    }

    pub fn save_to(&self, path: &Path) -> Result<(), String> {
        let mut ini = Ini::default();
        ini.set_bool(SECTION, "ping", self.ping);
        ini.set(SECTION, "ping_host", &self.ping_host);
        ini.save(path).map_err(|e| crate::fsutil::io_ctx(path, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("awg-agent-cfg-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn ping_keys_move_from_core_ini_once() {
        let dir = dir("migrate");
        let (agent_ini, core_ini) = (dir.join("agent.ini"), dir.join("core.ini"));
        let core_text = "[core]\r\nmode=engine\r\nping=0\r\nping_host=9.9.9.9\r\nowner_sid=S-1-5-21-1\r\n";
        std::fs::write(&core_ini, core_text).unwrap();

        let (first, notes) = AgentConfig::load_or_migrate(&agent_ini, &core_ini);
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(first, AgentConfig { ping: false, ping_host: "9.9.9.9".into() });
        assert_eq!(std::fs::read_to_string(&core_ini).unwrap(), core_text, "core.ini агент не трогает");
        let written = Ini::load(&agent_ini);
        assert_eq!((written.get(SECTION, "ping"), written.get(SECTION, "ping_host")), (Some("0"), Some("9.9.9.9")));

        // Второй запуск: ключи уже в agent.ini; изменение core.ini (ядро прежней версии после отката) не переносится.
        std::fs::write(&core_ini, "[core]\r\nping=1\r\nping_host=8.8.8.8\r\n").unwrap();
        let (second, notes) = AgentConfig::load_or_migrate(&agent_ini, &core_ini);
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(second, first);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Обновление с 0.4.0 в том порядке, в каком оно идёт на машине: `core.ini` записан ядром 0.4.0 (ключи и их порядок —
    /// как в его `Config::save_to`), установка и запуск нового ядра сохраняют свои настройки поверх, и только потом
    /// впервые стартует агент. Ловит: новое ядро пишет `core.ini` с чистого листа (как 0.4.0) — пинг «выкл.» и свой
    /// адрес молча сменились бы умолчаниями; или новое ядро не дочитало настройки 0.4.0 (режим, владелец, набор).
    #[test]
    fn core_ini_of_0_4_0_survives_the_new_core_save_and_its_ping_reaches_the_agent() {
        let dir = dir("upgrade");
        let (agent_ini, core_ini) = (dir.join("agent.ini"), dir.join("core.ini"));
        let v040 = "[core]\r\nmode=engine\r\nping=0\r\nping_host=9.9.9.9\r\nowner_sid=S-1-5-21-1-2-3-1001\r\nlanguage=rus\r\ntunnels=office,home\r\nmultiple=1\r\n";
        std::fs::write(&core_ini, v040).unwrap();

        let (config, problem) = crate::daemon::Config::load_guarded_from(&core_ini);
        assert!(problem.is_none());
        assert_eq!((config.mode, config.owner_sid.as_str(), config.language.as_str()), (crate::settings::Mode::Engine, "S-1-5-21-1-2-3-1001", "rus"));
        assert_eq!(config.tunnels.as_deref(), Some(&["office".to_string(), "home".to_string()][..]));
        assert!(config.multiple);
        config.save_to(&core_ini).unwrap();

        let (agent, notes) = AgentConfig::load_or_migrate(&agent_ini, &core_ini);
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(agent, AgentConfig { ping: false, ping_host: "9.9.9.9".into() });
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn without_ping_keys_anywhere_defaults_are_written() {
        let dir = dir("defaults");
        let (agent_ini, core_ini) = (dir.join("agent.ini"), dir.join("core.ini"));
        let (config, notes) = AgentConfig::load_or_migrate(&agent_ini, &core_ini);
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(config, AgentConfig { ping: true, ping_host: crate::settings::DEFAULT_PING_HOST.into() });
        assert!(agent_ini.exists(), "перенос сделан: следующий запуск core.ini не читает");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unreadable_agent_ini_is_kept_reported_and_rebuilt_from_core_ini() {
        let dir = dir("corrupt");
        let (agent_ini, core_ini) = (dir.join("agent.ini"), dir.join("core.ini"));
        std::fs::write(&agent_ini, [b'[', 0xff, 0xfe, b'\n']).unwrap();
        std::fs::write(&core_ini, "[core]\r\nping=0\r\n").unwrap();
        let (config, notes) = AgentConfig::load_or_migrate(&agent_ini, &core_ini);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("agent.ini"), "{}", notes[0]);
        assert!(!config.ping);
        assert_eq!(Ini::load(&agent_ini).get(SECTION, "ping"), Some("0"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
