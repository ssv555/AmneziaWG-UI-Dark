//! Пинг в агенте: настройки — `agent.ini`, подключённые туннели — у ядра (его `State`), замер — `ping::measure`.
//! Поток тот же, что в демо-режиме окна (`ping::spawn`); здесь только его дом (`PingHome`) и запросы окна.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use super::settings::AgentConfig;
use crate::crash::lock;
use crate::daemon::pipe::Timeouts;
use crate::daemon::proto::{PingDto, Request, Response};
use crate::i18n::trf;
use crate::ping::{PingHome, PingState};

/// Запрос к ядру «есть ли подключённые туннели»: ядро отвечает сразу, ждать дольше — только задерживать пинг.
const CORE_TIMEOUTS: Timeouts = Timeouts { send: Duration::from_secs(5), reply: Duration::from_secs(15) };

type Running = dyn Fn() -> Result<bool, String> + Send + Sync;
type Measure = dyn Fn(&str) -> Result<u32, String> + Send + Sync;
type Note = dyn Fn(&str) + Send + Sync;

/// Пинг агента. Внешний мир (ядро, ICMP, журнал) — функциями: так он проверяется без ядра и сети.
pub(super) struct AgentPing {
    settings: Mutex<AgentConfig>,
    path: PathBuf,
    state: Mutex<PingState>,
    running: Box<Running>,
    measure: Box<Measure>,
    note: Box<Note>,
    /// Последняя ошибка запроса к ядру уже в журнале: та же самая подряд не пишется (запрос раз в 10 секунд).
    core_error: Mutex<Option<String>>,
}

impl AgentPing {
    pub(super) fn new(settings: AgentConfig, path: PathBuf, running: Box<Running>, measure: Box<Measure>, note: Box<Note>) -> AgentPing {
        let state = PingState { host: settings.ping_host.clone(), ..Default::default() };
        AgentPing { settings: Mutex::new(settings), path, state: Mutex::new(state), running, measure, note, core_error: Mutex::default() }
    }

    /// Настоящий: туннели спрашиваются у ядра, замер — эхо-запрос ICMP.
    pub(super) fn real(settings: AgentConfig, path: PathBuf, note: Box<Note>) -> AgentPing {
        AgentPing::new(settings, path, Box::new(core_has_running_tunnels), Box::new(crate::ping::measure), note)
    }

    pub(super) fn dto(&self) -> PingDto {
        lock(&self.state).to_dto()
    }

    /// Окно сменило настройку. Новые значения действуют сразу; не записался `agent.ini` — ошибка окну (после
    /// перезапуска агента пинг вернулся бы к прежним значениям, пользователь должен это знать).
    pub(super) fn set(&self, enabled: bool, host: String) -> Result<(), String> {
        let mut settings = lock(&self.settings);
        let next = AgentConfig { ping: enabled, ping_host: host };
        if *settings == next {
            return Ok(());
        }
        // Новый узел виден окну сразу; замеры прежнего к нему не относятся.
        if settings.ping_host != next.ping_host {
            *lock(&self.state) = PingState { host: next.ping_host.clone(), ..Default::default() };
        }
        *settings = next;
        settings.save_to(&self.path)
    }
}

impl PingHome for AgentPing {
    fn settings(&self) -> (bool, String) {
        let s = lock(&self.settings);
        (s.ping, s.ping_host.clone())
    }

    fn any_running(&self) -> bool {
        let answer = (self.running)();
        let mut last = lock(&self.core_error);
        match answer {
            Ok(running) => {
                *last = None;
                running
            }
            Err(e) => {
                // Без ядра пинговать незачем (туннели ведёт оно), но молча пропущенный замер скрыл бы причину.
                if last.as_deref() != Some(e.as_str()) {
                    (self.note)(&trf("agent.ping_no_core", &[&e]));
                    *last = Some(e);
                }
                false
            }
        }
    }

    fn measure(&self, host: &str) -> Result<u32, String> {
        (self.measure)(host)
    }

    fn update(&self, f: &mut dyn FnMut(&mut PingState)) {
        f(&mut lock(&self.state));
    }

    fn report_panic(&self, panic: &str, wait: Duration) {
        (self.note)(&trf("core.secondary_failed", &[panic, &wait.as_secs().to_string()]));
    }
}

/// Есть ли у ядра подключённые туннели. События не нужны: `events_after` — максимум, ядро их не шлёт.
fn core_has_running_tunnels() -> Result<bool, String> {
    match crate::daemon::pipe::call_to(crate::daemon::pipe::NAME, &Request::State { events_after: u64::MAX }, CORE_TIMEOUTS)? {
        Response::State(state) => Ok(!state.running.is_empty()),
        Response::Err(e) | Response::Refused(e) => Err(e),
        other => Err(crate::i18n::trf("err.core_unexpected", &[&crate::explain::variant_name(&other)])),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn agent_ping(dir: &std::path::Path, running: Result<bool, String>, notes: Arc<Mutex<Vec<String>>>) -> AgentPing {
        let settings = AgentConfig { ping: true, ping_host: "h".into() };
        AgentPing::new(
            settings,
            dir.join("agent.ini"),
            Box::new(move || running.clone()),
            Box::new(|_| Ok(12)),
            Box::new(move |text| notes.lock().unwrap().push(text.to_string())),
        )
    }

    #[test]
    fn unreachable_core_is_logged_once_and_means_no_tunnels() {
        let notes = Arc::<Mutex<Vec<String>>>::default();
        let ping = agent_ping(&std::env::temp_dir(), Err("pipe: no core".into()), notes.clone());
        assert!(!ping.any_running());
        assert!(!ping.any_running());
        let notes = notes.lock().unwrap();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("pipe: no core"), "{}", notes[0]);
    }
}
