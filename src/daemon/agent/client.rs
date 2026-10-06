//! Клиентская сторона агента для окна — пара к `CoreApi`: трейт, канал (`AgentPipe`) и подделка в тестах
//! (`FakeAgent`). Обязательна одна `call`; разбор ответа общий для всех реализаций.

use std::sync::Arc;
use std::time::Duration;

use super::proto::{AgentEvents, AgentRequest, AgentResponse, AgentState, TunnelRequest};
use crate::daemon::pipe::Timeouts;
use crate::daemon::proto::{Request, Response};
use crate::daemon::CoreApi;
use crate::settings::Mode;
use crate::update::{UpdateOp, UpdatesState};

/// Окно спрашивает агента раз в секунду: зависший агент не должен надолго держать поток опроса.
const WINDOW_TIMEOUTS: Timeouts = Timeouts { send: Duration::from_secs(2), reply: Duration::from_secs(5) };

pub trait AgentApi: Send + Sync {
    /// Один запрос, ответ как есть. Ошибка — агент недоступен или не ответил.
    fn call(&self, req: AgentRequest) -> Result<AgentResponse, String>;

    /// То же для долгих запросов (помощник в родном окне идёт до 120 с): ответ ждём столько же, сколько от ядра.
    fn call_slow(&self, req: AgentRequest) -> Result<AgentResponse, String> {
        self.call(req)
    }

    /// Запрос без данных в ответе.
    fn ok(&self, req: AgentRequest) -> Result<(), String> {
        match self.call(req)? {
            AgentResponse::Ok => Ok(()),
            other => Err(unexpected(other)),
        }
    }

    fn state(&self) -> Result<AgentState, String> {
        match self.call(AgentRequest::State)? {
            AgentResponse::State(s) => Ok(*s),
            other => Err(unexpected(other)),
        }
    }

    /// Журнал агента новее `after`.
    fn events(&self, after: u64) -> Result<AgentEvents, String> {
        match self.call(AgentRequest::Events { after })? {
            AgentResponse::Events(e) => Ok(*e),
            other => Err(unexpected(other)),
        }
    }

    /// Версия агента: он отвечает на `Hello` без побочных действий.
    fn version(&self) -> Result<String, String> {
        match self.call(AgentRequest::Hello)? {
            AgentResponse::Hello { version } => Ok(version),
            other => Err(unexpected(other)),
        }
    }

    fn set_ping(&self, enabled: bool, host: &str) -> Result<(), String> {
        self.ok(AgentRequest::SetPing { enabled, host: host.into() })
    }

    /// Команда окна «Обновления и откаты». Ошибка различает «агента нет» (окно показывает «вторичная служба
    /// недоступна» и не засыпает журнал повторами) и отказ самого агента (текст — как есть).
    fn updates(&self, op: UpdateOp) -> Result<UpdatesState, UpdatesError> {
        match self.call(AgentRequest::Updates(op)).map_err(UpdatesError::Unreachable)? {
            AgentResponse::Updates(s) => Ok(*s),
            other => Err(UpdatesError::Failed(unexpected(other))),
        }
    }
}

/// Почему команда обновлений не выполнена.
#[derive(Debug, Clone, PartialEq)]
pub enum UpdatesError {
    /// Агент не ответил (перезапускается, ядро прежней версии без агента): причина — ошибка канала.
    Unreachable(String),
    /// Агент ответил отказом или ошибкой.
    Failed(String),
}

/// Текст для журнала и консоли: недоступность агента названа его словами, а не голой ошибкой канала.
impl std::fmt::Display for UpdatesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpdatesError::Unreachable(e) => write!(f, "{}: {e}", crate::i18n::tr("agent.unavailable")),
            UpdatesError::Failed(e) => f.write_str(e),
        }
    }
}

/// Агент за именованным каналом.
pub struct AgentPipe;

impl AgentApi for AgentPipe {
    fn call(&self, req: AgentRequest) -> Result<AgentResponse, String> {
        crate::daemon::pipe::call_with(super::PIPE_NAME, &req, WINDOW_TIMEOUTS)
    }

    fn call_slow(&self, req: AgentRequest) -> Result<AgentResponse, String> {
        crate::daemon::pipe::call_with(super::PIPE_NAME, &req, Timeouts::CORE)
    }
}

fn unexpected(r: AgentResponse) -> String {
    match r {
        AgentResponse::Err(e) | AgentResponse::Refused(e) => e,
        other => format!("agent: unexpected answer {other:?}"),
    }
}

/// Ядро и агент для окна одним `CoreApi`: конфиги туннелей и действия в родном окне AmneziaWG обслуживает агент
/// (`agent::tunnels`), остальное — ядро. Так у окна один путь к действию, а не выбор «ядро или агент» в каждом месте.
/// Агент недоступен — ошибка действия называет вторичную службу: ядро при этом живо, «связь с ядром потеряна» была бы
/// неправдой.
pub struct Routed {
    core: Arc<dyn CoreApi>,
    agent: Arc<dyn AgentApi>,
}

/// Куда уходит запрос окна.
enum Route {
    Core(Request),
    Agent(TunnelRequest),
}

impl Routed {
    pub fn new(core: Arc<dyn CoreApi>, agent: Arc<dyn AgentApi>) -> Routed {
        Routed { core, agent }
    }

    /// Удаление туннеля режима 2 убирает его службу — его делает ядро; удаление в родном окне (режим 1) — агент.
    /// Режим знает ядро: окно спрашивает его, а не полагается на свои настройки, которые могли отстать.
    fn route(&self, req: Request) -> Result<Route, String> {
        Ok(match req {
            Request::Read(t) => Route::Agent(TunnelRequest::Read(t)),
            Request::Write { tunnel, text } => Route::Agent(TunnelRequest::Write { tunnel, text }),
            Request::Details(t) => Route::Agent(TunnelRequest::Details(t)),
            Request::Import(entries) => Route::Agent(TunnelRequest::Import(entries)),
            Request::ExportAll => Route::Agent(TunnelRequest::ExportAll),
            Request::NewTunnel(name) => Route::Agent(TunnelRequest::NewTunnel(name)),
            Request::TakeNative => Route::Agent(TunnelRequest::TakeNative),
            Request::Native(op) => Route::Agent(TunnelRequest::Native(op)),
            Request::Delete(t) => match self.core.hello()?.1 {
                Mode::Engine => Route::Core(Request::Delete(t)),
                Mode::Overlay => Route::Agent(TunnelRequest::Delete(t)),
            },
            other => Route::Core(other),
        })
    }

    fn via_agent(&self, req: TunnelRequest) -> Result<Response, String> {
        let answer = self.agent.call_slow(AgentRequest::Tunnel(req)).map_err(|e| format!("{}: {e}", crate::i18n::tr("agent.unavailable")))?;
        Ok(match answer {
            AgentResponse::Ok => Response::Ok,
            AgentResponse::Err(e) => Response::Err(e),
            AgentResponse::Refused(e) => Response::Refused(e),
            AgentResponse::Text(t) => Response::Text(t),
            AgentResponse::Info(i) => Response::Info(i),
            AgentResponse::Entries(e) => Response::Entries(e),
            AgentResponse::Report(r) => Response::Report(r),
            AgentResponse::Hello { .. } | AgentResponse::Updates(_) | AgentResponse::State(_) | AgentResponse::Events(_) => {
                return Err("agent: unexpected answer to a tunnel request".into());
            }
        })
    }
}

impl CoreApi for Routed {
    fn call(&self, req: Request) -> Result<Response, String> {
        match self.route(req)? {
            Route::Core(req) => self.core.call(req),
            Route::Agent(req) => self.via_agent(req),
        }
    }
}

/// Подделка агента для тестов окна: ответ задаёт тест.
#[cfg(test)]
pub struct FakeAgent(pub Box<dyn Fn(&AgentRequest) -> Result<AgentResponse, String> + Send + Sync>);

#[cfg(test)]
impl FakeAgent {
    /// Агент не запущен (ядро прежней версии, перезапуск): каждый запрос — ошибка канала.
    pub fn unreachable(error: &str) -> FakeAgent {
        let error = error.to_string();
        FakeAgent(Box::new(move |_| Err(error.clone())))
    }
}

#[cfg(test)]
impl AgentApi for FakeAgent {
    fn call(&self, req: AgentRequest) -> Result<AgentResponse, String> {
        (self.0)(&req)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::daemon::fake::FakeCore;
    use crate::daemon::proto::NativeOp;

    /// Ядро в режиме `mode`, которое на всё остальное отвечает `Ok`.
    fn core_in(mode: Mode) -> Arc<FakeCore> {
        Arc::new(FakeCore::new(move |req| match req {
            Request::Hello => Ok(Response::Hello { version: "1".into(), mode }),
            _ => Ok(Response::Ok),
        }))
    }

    /// Агент, который записывает запросы и отвечает `reply`.
    fn recording_agent(reply: AgentResponse) -> (Arc<FakeAgent>, Arc<Mutex<Vec<String>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let reply = serde_json::to_string(&reply).unwrap();
        let fake = FakeAgent(Box::new(move |req| {
            log.lock().unwrap().push(format!("{req:?}"));
            Ok(serde_json::from_str(&reply).unwrap())
        }));
        (Arc::new(fake), seen)
    }

    #[test]
    fn tunnel_configs_and_native_actions_go_to_the_agent() {
        let core = core_in(Mode::Overlay);
        let (agent, seen) = recording_agent(AgentResponse::Text("[Interface]\n".into()));
        let routed = Routed::new(core.clone(), agent);
        assert_eq!(routed.read_config("a").unwrap(), "[Interface]\n");
        let (agent, seen_native) = recording_agent(AgentResponse::Ok);
        let routed_ok = Routed::new(core.clone(), agent);
        routed_ok.native(NativeOp::Open).unwrap();
        routed_ok.write_config("a", "[Interface]\n").unwrap();
        assert_eq!(*seen.lock().unwrap(), [r#"Tunnel(Read("a"))"#]);
        assert_eq!(*seen_native.lock().unwrap(), ["Tunnel(Native(Open))", r#"Tunnel(Write { tunnel: "a", text: "[Interface]\n" })"#]);
        assert!(core.requests().is_empty(), "ядро этих запросов не видит: {:?}", core.requests());
    }

    /// Удаление: режим 1 — агенту (родное окно), режим 2 — ядру (служба туннеля); режим спрашивается у ядра.
    #[test]
    fn delete_goes_by_the_core_mode() {
        let core = core_in(Mode::Overlay);
        let (agent, seen) = recording_agent(AgentResponse::Ok);
        Routed::new(core.clone(), agent).delete_tunnel("a").unwrap();
        assert_eq!(*seen.lock().unwrap(), [r#"Tunnel(Delete("a"))"#]);
        assert_eq!(core.requests(), ["Hello"]);

        let core = core_in(Mode::Engine);
        let (agent, seen) = recording_agent(AgentResponse::Ok);
        Routed::new(core.clone(), agent).delete_tunnel("a").unwrap();
        assert!(seen.lock().unwrap().is_empty());
        assert_eq!(core.requests(), ["Hello", r#"Delete("a")"#]);
    }

    #[test]
    fn vpn_requests_stay_with_the_core() {
        let core = core_in(Mode::Engine);
        let (agent, seen) = recording_agent(AgentResponse::Ok);
        let routed = Routed::new(core.clone(), agent);
        routed.ok(Request::Rename { old: "a".into(), new: "b".into() }).unwrap();
        routed.retry_tunnel("a").unwrap();
        assert!(seen.lock().unwrap().is_empty());
        assert_eq!(core.requests(), [r#"Rename { old: "a", new: "b" }"#, r#"Retry("a")"#]);
    }

    /// Агент не отвечает: ошибка действия называет вторичную службу, а не ядро.
    #[test]
    fn unreachable_agent_is_named_as_the_secondary_service() {
        let routed = Routed::new(core_in(Mode::Engine), Arc::new(FakeAgent::unreachable("pipe: not found")));
        let error = routed.report(Request::TakeNative).unwrap_err();
        assert_eq!(error, format!("{}: pipe: not found", crate::i18n::tr("agent.unavailable")));
        assert!(routed.details("a").unwrap_err().starts_with(&crate::i18n::tr("agent.unavailable")));
    }

    /// Новое окно над ядром 0.4.0 (агента нет — его канал не открывается): управление VPN идёт к ядру и работает,
    /// агент для него ни разу не спрашивается, а действия агента получают ошибку «вторичная служба недоступна».
    /// Ловит: запрос VPN, ушедший через агента (или ждущий его), — без агента окно не могло бы подключить туннель.
    #[test]
    fn old_core_without_agent_keeps_vpn_controls_working() {
        let core = core_in(Mode::Engine);
        let asked = Arc::new(Mutex::new(Vec::new()));
        let log = asked.clone();
        let agent = Arc::new(FakeAgent(Box::new(move |req| {
            log.lock().unwrap().push(format!("{req:?}"));
            Err("pipe: not found".into())
        })));
        let routed = Routed::new(core.clone(), agent);
        routed.ok(Request::Switch { tunnel: "a".into(), plan: crate::daemon::proto::Plan::Connect, multiple: false }).unwrap();
        routed.ok(Request::SetMode(Mode::Engine)).unwrap();
        routed.call(Request::State { events_after: 0 }).unwrap();
        routed.delete_tunnel("a").unwrap();
        routed.retry_tunnel("a").unwrap();
        assert!(asked.lock().unwrap().is_empty(), "VPN без агента: {:?}", asked.lock().unwrap());
        assert_eq!(core.requests().len(), 6, "всё у ядра (удаление спрашивает режим): {:?}", core.requests());

        let error = routed.read_config("a").unwrap_err();
        assert!(error.starts_with(&crate::i18n::tr("agent.unavailable")) && error.contains("pipe: not found"), "{error}");
    }

    /// Отказ и ошибка агента доходят до окна его текстом.
    #[test]
    fn agent_refusal_reaches_the_window_as_is() {
        let (agent, _) = recording_agent(AgentResponse::Refused("bad name".into()));
        let routed = Routed::new(core_in(Mode::Overlay), agent);
        assert_eq!(routed.details("a").unwrap_err(), "bad name");
    }
}
