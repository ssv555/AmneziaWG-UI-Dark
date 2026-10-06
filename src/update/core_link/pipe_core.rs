//! Связь обновлений с ядром по его каналу — для менеджера во втором процессе (агенте): внутренние запросы
//! `HoldNative`/`Release`/`ReconnectEngine`. Ядро берёт их только от SYSTEM; агент работает от SYSTEM.

use std::time::Duration;

use super::CoreLink;
use crate::daemon::pipe::{self, Timeouts};
use crate::daemon::proto::{Request, Response};

/// Ответ на внутренний запрос без переключений (`ReconnectEngine`): ядро отвечает сразу, а долгое ожидание держало бы
/// работу обновления на зависшем ядре.
const REPLY: Duration = Duration::from_secs(15);
/// Одно переключение туннеля в ядре: процесс `amneziawg.exe` до минуты (`backend::RUN_TIMEOUT`) и ожидание службы.
/// `HoldNative` ждёт идущее переключение (берёт `switching`), `Release` подключает возвращённые туннели по одному. Без
/// этого запаса ответ ядра мог бы прийти после срока: агент счёл бы аренду не взятой, а ядро держало бы её до истечения.
const PER_SWITCH: Duration = Duration::from_secs(75);

pub struct PipeCore {
    pipe: String,
}

impl PipeCore {
    pub fn new() -> PipeCore {
        PipeCore { pipe: pipe::NAME.to_string() }
    }

    /// Запрос `request` (в тексте ошибки — `what`) со сроком ответа `reply`. Не дошёл, ядро отказало или ответило не
    /// тем — ошибка с именем запроса: в журнале обновлений видно, какая просьба к ядру не прошла.
    fn ask(&self, what: &str, request: Request, reply: Duration) -> Result<(), String> {
        let answer = pipe::call_to(&self.pipe, &request, Timeouts::with_reply(reply));
        expect_ok(answer).map_err(|e| format!("core {what}: {e}"))
    }
}

impl CoreLink for PipeCore {
    fn hold_native(&self, lease: Duration) -> Result<Vec<String>, String> {
        let request = Request::HoldNative { lease_s: lease.as_secs() };
        let answer = pipe::call_to(&self.pipe, &request, Timeouts::with_reply(reply_timeout(1)));
        expect_held(answer).map_err(|e| format!("core HoldNative: {e}"))
    }
    fn release(&self, tunnels: &[String]) -> Result<(), String> {
        self.ask("Release", Request::Release { tunnels: tunnels.to_vec() }, reply_timeout(tunnels.len()))
    }
    fn reconnect_engine(&self) -> Result<(), String> {
        self.ask("ReconnectEngine", Request::ReconnectEngine, reply_timeout(0))
    }
}

/// Срок ответа на запрос, перед ответом на который ядро может сделать до `switches` переключений.
fn reply_timeout(switches: usize) -> Duration {
    REPLY.saturating_add(PER_SWITCH.saturating_mul(u32::try_from(switches).unwrap_or(u32::MAX)))
}

/// Ответ ядра на внутренний запрос: успех — только `Ok`.
fn expect_ok(answer: Result<Response, String>) -> Result<(), String> {
    match answer? {
        Response::Ok => Ok(()),
        Response::Err(e) | Response::Refused(e) => Err(e),
        other => Err(format!("unexpected reply {}", reply_kind(&other))),
    }
}

/// Ответ ядра на `HoldNative`: успех — только `Held` со взятыми туннелями.
fn expect_held(answer: Result<Response, String>) -> Result<Vec<String>, String> {
    match answer? {
        Response::Held(tunnels) => Ok(tunnels),
        Response::Err(e) | Response::Refused(e) => Err(e),
        other => Err(format!("unexpected reply {}", reply_kind(&other))),
    }
}

/// Имя варианта ответа без содержимого: состояние ядра целиком в журнал не нужно.
fn reply_kind(r: &Response) -> String {
    let text = format!("{r:?}");
    text.split(|c: char| !c.is_alphanumeric()).next().unwrap_or_default().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// Ядра нет (канала с таким именем нет): ошибка сразу, с именем запроса и причиной «ядро недоступно».
    #[test]
    fn absent_core_pipe_is_an_error_naming_the_request() {
        let core = PipeCore { pipe: format!(r"\\.\pipe\awg-ui-test-no-core-{}", std::process::id()) };
        let unavailable = crate::i18n::trf("core.unavailable", &[&std::io::Error::from_raw_os_error(2).to_string()]);
        let started = Instant::now();
        let tunnels = vec!["a".to_string()];
        assert_eq!(core.hold_native(Duration::from_secs(900)), Err(format!("core HoldNative: {unavailable}")));
        assert_eq!(core.release(&tunnels), Err(format!("core Release: {unavailable}")));
        assert_eq!(core.reconnect_engine(), Err(format!("core ReconnectEngine: {unavailable}")));
        assert!(started.elapsed() < Duration::from_secs(5), "нет канала — не ждать сроков ответа: {:?}", started.elapsed());
    }

    /// Внутренние запросы идут в канал ядра, а не в собственный канал агента.
    #[test]
    fn talks_to_the_core_pipe() {
        assert_eq!(PipeCore::new().pipe, pipe::NAME);
        assert_ne!(PipeCore::new().pipe, crate::daemon::agent::PIPE_NAME);
    }

    /// Аренда — только ответ `Held`: «Ok» от ядра прежней версии или отказ — ошибка, и установщик не запустится.
    #[test]
    fn only_held_is_a_taken_hold() {
        assert_eq!(expect_held(Ok(Response::Held(vec!["a".into()]))), Ok(vec!["a".to_string()]));
        assert_eq!(expect_held(Ok(Response::Held(vec![]))), Ok(vec![]), "режим 2 — брать нечего");
        assert_eq!(expect_held(Ok(Response::Ok)), Err("unexpected reply Ok".into()));
        assert_eq!(expect_held(Ok(Response::Refused("not SYSTEM".into()))), Err("not SYSTEM".into()));
        assert_eq!(expect_held(Err("pipe: closed".into())), Err("pipe: closed".into()));
    }

    #[test]
    fn only_ok_is_success() {
        assert_eq!(expect_ok(Ok(Response::Ok)), Ok(()));
        assert_eq!(expect_ok(Ok(Response::Err("busy".into()))), Err("busy".into()));
        assert_eq!(expect_ok(Ok(Response::Refused("not SYSTEM".into()))), Err("not SYSTEM".into()));
        assert_eq!(expect_ok(Ok(Response::Text("x".into()))), Err("unexpected reply Text".into()));
        assert_eq!(expect_ok(Err("pipe: closed".into())), Err("pipe: closed".into()));
    }

    #[test]
    fn reply_waits_for_the_switches_the_core_may_do_first() {
        assert_eq!(reply_timeout(0), REPLY, "ReconnectEngine — без переключений");
        assert_eq!(reply_timeout(1), REPLY + PER_SWITCH);
        assert_eq!(reply_timeout(3), REPLY + PER_SWITCH * 3, "Release — по переключению на туннель");
        assert_eq!(reply_timeout(usize::MAX), REPLY + PER_SWITCH * u32::MAX, "огромное число — без паники");
    }
}
