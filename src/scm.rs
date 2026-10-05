//! Управление службами Windows через один интерфейс: `ServiceControl` (запустить, остановить, удалить, спросить
//! состояние, дождаться состояния). Настоящая служба — `Service` над дескриптором диспетчера (`Handle`); в тестах
//! вместо неё `fake::FakeService`, поэтому логика «поднять туннель», «остановить ядро», «перезапустить ядро после
//! обновления» проверяется без службы и без прав администратора.
//!
//! Создание службы (`CreateServiceW`) сюда не входит: у ядра и туннелей оно разное. Общая настройка обоих —
//! действия при сбое (`set_failure_actions`).

use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    GetLastError, TRUE, ERROR_SERVICE_ALREADY_RUNNING, ERROR_SERVICE_CANNOT_ACCEPT_CTRL, ERROR_SERVICE_DOES_NOT_EXIST,
    ERROR_SERVICE_MARKED_FOR_DELETE, ERROR_SERVICE_NOT_ACTIVE,
};
use windows_sys::Win32::System::Services::{
    ChangeServiceConfig2W, CloseServiceHandle, ControlService, DeleteService, OpenSCManagerW, OpenServiceW,
    QueryServiceStatus, StartServiceW, SC_ACTION, SC_ACTION_NONE, SC_HANDLE, SC_MANAGER_ALL_ACCESS, SC_MANAGER_CONNECT,
    SERVICE_CONFIG_FAILURE_ACTIONS, SERVICE_CONFIG_FAILURE_ACTIONS_FLAG, SERVICE_CONTROL_STOP, SERVICE_FAILURE_ACTIONSW,
    SERVICE_FAILURE_ACTIONS_FLAG, SERVICE_RUNNING, SERVICE_START_PENDING, SERVICE_STATUS, SERVICE_STOPPED,
    SERVICE_STOP_PENDING,
};

use crate::win::wide;

/// Как часто опрашивать состояние службы при ожидании.
const POLL: Duration = Duration::from_millis(200);

/// Состояние службы (`dwCurrentState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum State {
    Stopped,
    StartPending,
    StopPending,
    Running,
    /// Любое другое (приостановлена и т. п.) или неизвестное: исходное число.
    Other(u32),
}

impl State {
    fn from_raw(raw: u32) -> State {
        match raw {
            SERVICE_STOPPED => State::Stopped,
            SERVICE_START_PENDING => State::StartPending,
            SERVICE_STOP_PENDING => State::StopPending,
            SERVICE_RUNNING => State::Running,
            other => State::Other(other),
        }
    }
}

/// Состояние службы и коды, с которыми она остановилась (`SERVICE_STATUS` без остального).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Status {
    pub(crate) state: State,
    pub(crate) win32_exit: u32,
    pub(crate) specific_exit: u32,
}

impl Status {
    pub(crate) fn new(state: State) -> Status {
        Status { state, win32_exit: 0, specific_exit: 0 }
    }

    /// Состояние неизвестно: опрос ещё не удался.
    fn unknown() -> Status {
        Status::new(State::Other(0))
    }

    fn from_raw(st: &SERVICE_STATUS) -> Status {
        Status { state: State::from_raw(st.dwCurrentState), win32_exit: st.dwWin32ExitCode, specific_exit: st.dwServiceSpecificExitCode }
    }
}

/// Одна служба Windows. Все методы синхронные; ожидание — `wait_state`.
pub(crate) trait ServiceControl {
    /// Запросить запуск. Уже запущена — не ошибка. Не ждёт готовности (`wait_state`).
    fn start(&self) -> Result<(), String>;
    /// Запросить остановку. Уже остановлена (или останавливается) — не ошибка. Не ждёт.
    fn stop(&self) -> Result<(), String>;
    /// Пометить на удаление (удалится, когда закроются все дескрипторы). Уже помечена — не ошибка.
    fn delete(&self) -> Result<(), String>;
    fn query(&self) -> Result<Status, String>;
    /// Убрать действия диспетчера при сбое (перезапуск и прочее). Повторы туннелей ведёт ядро (`daemon::retry`).
    fn clear_failure_actions(&self) -> Result<(), String>;

    /// Ждать состояния `want`; служба остановилась раньше (при ожидании не-остановки) или вышло время — последнее
    /// известное состояние: из него видны коды завершения.
    fn wait_state(&self, want: State, timeout: Duration) -> Result<(), Status> {
        wait_for(self, want, timeout, POLL)
    }

    fn is_running(&self) -> bool {
        matches!(self.query(), Ok(st) if st.state == State::Running)
    }
}

/// Опрос `svc` каждые `pause`, пока не наступит `want`. Ошибка опроса ожидание не прерывает: она может быть
/// разовой, а на выходе остаётся последнее удачно прочитанное состояние (до первого — «неизвестно»).
pub(crate) fn wait_for<S: ServiceControl + ?Sized>(svc: &S, want: State, timeout: Duration, pause: Duration) -> Result<(), Status> {
    let until = Instant::now() + timeout;
    let mut last = Status::unknown();
    loop {
        if let Ok(st) = svc.query() {
            last = st;
        }
        if last.state == want {
            return Ok(());
        }
        if (want != State::Stopped && last.state == State::Stopped) || Instant::now() >= until {
            return Err(last);
        }
        std::thread::sleep(pause);
    }
}

/// Остановить `svc` и дождаться остановки (уже стоит — сразу да). Ошибка — текст для журнала.
pub(crate) fn stop_and_wait(svc: &dyn ServiceControl, timeout: Duration) -> Result<(), String> {
    svc.stop()?;
    svc.wait_state(State::Stopped, timeout).map_err(|st| format!("не остановилась за {} с: {:?}", timeout.as_secs(), st))
}

/// Код ошибки `ControlService(STOP)`, который не мешает остановке: службы нет в работе или она уже не принимает
/// команды, потому что останавливается. Дальше всё равно ждём `Stopped`.
fn stop_tolerated(code: u32) -> bool {
    matches!(code, ERROR_SERVICE_NOT_ACTIVE | ERROR_SERVICE_CANNOT_ACCEPT_CTRL)
}

/// Дескриптор диспетчера служб или службы; закрывается сам.
pub(crate) struct Handle(pub(crate) SC_HANDLE);

impl Handle {
    pub(crate) fn new(h: SC_HANDLE) -> Result<Handle, std::io::Error> {
        if h.is_null() {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(Handle(h))
        }
    }

    /// Диспетчер с полными правами — для создания служб.
    pub(crate) fn scm() -> Result<Handle, String> {
        Handle::new(unsafe { OpenSCManagerW(null(), null(), SC_MANAGER_ALL_ACCESS) }).map_err(|e| format!("OpenSCManager: {e}"))
    }

    /// Диспетчер только для открытия уже существующих служб.
    pub(crate) fn scm_connect() -> Result<Handle, String> {
        Handle::new(unsafe { OpenSCManagerW(null(), null(), SC_MANAGER_CONNECT) }).map_err(|e| format!("OpenSCManager: {e}"))
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { CloseServiceHandle(self.0) };
    }
}

/// Настоящая служба: адаптер `ServiceControl` над дескриптором SCM.
pub(crate) struct Service {
    handle: Handle,
    name: String,
}

impl Service {
    /// Открыть службу `name` с правами `access` (`SERVICE_*`). Нет такой службы — `Ok(None)`; любая другая ошибка
    /// (в том числе отказ в доступе) — `Err`: «нет службы» и «не могу открыть» разные вещи.
    pub(crate) fn try_open(scm: &Handle, name: &str, access: u32) -> Result<Option<Service>, String> {
        let h = unsafe { OpenServiceW(scm.0, wide(name).as_ptr(), access) };
        match Handle::new(h) {
            Ok(handle) => Ok(Some(Service { handle, name: name.to_string() })),
            Err(e) if e.raw_os_error() == Some(ERROR_SERVICE_DOES_NOT_EXIST as i32) => Ok(None),
            Err(e) => Err(format!("OpenService {name}: {e}")),
        }
    }

    /// Открыть, не различая «нет службы» и прочие ошибки.
    pub(crate) fn open(scm: &Handle, name: &str, access: u32) -> Result<Service, String> {
        Service::try_open(scm, name, access)?.ok_or_else(|| format!("OpenService {name}: {}", std::io::Error::from_raw_os_error(ERROR_SERVICE_DOES_NOT_EXIST as i32)))
    }

    /// Только что созданная служба (`CreateServiceW`).
    pub(crate) fn from_handle(handle: Handle, name: &str) -> Service {
        Service { handle, name: name.to_string() }
    }

    /// Сырой дескриптор для вызовов, которых нет в интерфейсе (настройка службы).
    pub(crate) fn raw(&self) -> SC_HANDLE {
        self.handle.0
    }

    /// Командная строка службы (`lpBinaryPathName`); не удалось прочитать настройки — `None`.
    pub(crate) fn command_line(&self) -> Option<String> {
        use windows_sys::Win32::System::Services::{QueryServiceConfigW, QUERY_SERVICE_CONFIGW};
        unsafe {
            let mut needed = 0u32;
            QueryServiceConfigW(self.handle.0, std::ptr::null_mut(), 0, &mut needed);
            // u64 — чтобы буфер был выровнен под структуру.
            let mut buf = vec![0u64; (needed as usize).div_ceil(8).max(1)];
            if QueryServiceConfigW(self.handle.0, buf.as_mut_ptr().cast(), (buf.len() * 8) as u32, &mut needed) == 0 {
                return None;
            }
            let p = (*(buf.as_ptr() as *const QUERY_SERVICE_CONFIGW)).lpBinaryPathName;
            if p.is_null() {
                return None;
            }
            let len = (0..).take_while(|&i| *p.add(i) != 0).count();
            Some(String::from_utf16_lossy(std::slice::from_raw_parts(p, len)))
        }
    }

    fn fail(&self, what: &str) -> String {
        format!("{what} {}: {}", self.name, std::io::Error::last_os_error())
    }
}

impl ServiceControl for Service {
    fn start(&self) -> Result<(), String> {
        unsafe {
            if StartServiceW(self.handle.0, 0, null()) == 0 {
                let code = GetLastError();
                if code != ERROR_SERVICE_ALREADY_RUNNING {
                    return Err(format!("StartService {}: {}", self.name, std::io::Error::from_raw_os_error(code as i32)));
                }
            }
        }
        Ok(())
    }

    fn stop(&self) -> Result<(), String> {
        unsafe {
            let mut st: SERVICE_STATUS = std::mem::zeroed();
            if ControlService(self.handle.0, SERVICE_CONTROL_STOP, &mut st) == 0 {
                let code = GetLastError();
                if !stop_tolerated(code) {
                    return Err(format!("ControlService(stop) {}: {}", self.name, std::io::Error::from_raw_os_error(code as i32)));
                }
            }
        }
        Ok(())
    }

    fn delete(&self) -> Result<(), String> {
        unsafe {
            if DeleteService(self.handle.0) == 0 {
                let code = GetLastError();
                if code != ERROR_SERVICE_MARKED_FOR_DELETE {
                    return Err(format!("DeleteService {}: {}", self.name, std::io::Error::from_raw_os_error(code as i32)));
                }
            }
        }
        Ok(())
    }

    fn query(&self) -> Result<Status, String> {
        unsafe {
            let mut st: SERVICE_STATUS = std::mem::zeroed();
            if QueryServiceStatus(self.handle.0, &mut st) == 0 {
                return Err(self.fail("QueryServiceStatus"));
            }
            Ok(Status::from_raw(&st))
        }
    }

    fn clear_failure_actions(&self) -> Result<(), String> {
        let mut placeholder = [SC_ACTION { Type: SC_ACTION_NONE, Delay: 0 }];
        let none = no_failure_actions(&mut placeholder);
        unsafe {
            if ChangeServiceConfig2W(self.handle.0, SERVICE_CONFIG_FAILURE_ACTIONS, (&none as *const SERVICE_FAILURE_ACTIONSW).cast()) == 0 {
                return Err(self.fail("clear failure actions"));
            }
        }
        Ok(())
    }
}

/// Действия диспетчера при сбое службы: `actions` по очереди (после последнего повторяется последнее), счётчик сбоев
/// сбрасывается через `reset_period` секунд без сбоев. Сбой — и когда служба сама остановилась с ненулевым кодом, а не
/// только когда процесс упал. Общее для ядра и служб туннелей.
pub(crate) fn set_failure_actions(svc: &Service, reset_period: u32, actions: &mut [SC_ACTION]) -> Result<(), String> {
    let failure = SERVICE_FAILURE_ACTIONSW {
        dwResetPeriod: reset_period,
        lpRebootMsg: null_mut(),
        lpCommand: null_mut(),
        cActions: actions.len() as u32,
        lpsaActions: actions.as_mut_ptr(),
    };
    unsafe {
        if ChangeServiceConfig2W(svc.raw(), SERVICE_CONFIG_FAILURE_ACTIONS, (&failure as *const SERVICE_FAILURE_ACTIONSW).cast()) == 0 {
            return Err(svc.fail("failure actions"));
        }
        let flag = non_crash_failures();
        if ChangeServiceConfig2W(svc.raw(), SERVICE_CONFIG_FAILURE_ACTIONS_FLAG, (&flag as *const SERVICE_FAILURE_ACTIONS_FLAG).cast()) == 0 {
            return Err(svc.fail("failure actions flag"));
        }
    }
    Ok(())
}

/// Описание «действий при сбое нет»: `cActions = 0` при ненулевом `lpsaActions` — так `ChangeServiceConfig2` удаляет и
/// действия, и период сброса; с нулевым указателем он оставил бы прежние как есть.
fn no_failure_actions(placeholder: &mut [SC_ACTION; 1]) -> SERVICE_FAILURE_ACTIONSW {
    SERVICE_FAILURE_ACTIONSW { dwResetPeriod: 0, lpRebootMsg: null_mut(), lpCommand: null_mut(), cActions: 0, lpsaActions: placeholder.as_mut_ptr() }
}

/// Действия при сбое — и когда служба сама остановилась с ненулевым кодом, а не только когда процесс упал.
fn non_crash_failures() -> SERVICE_FAILURE_ACTIONS_FLAG {
    SERVICE_FAILURE_ACTIONS_FLAG { fFailureActionsOnNonCrashFailures: TRUE }
}

#[cfg(test)]
pub(crate) mod fake {
    //! Подмена службы для тестов: состояние меняется по сценарию, вызовы записываются.
    use std::cell::{Cell, RefCell};

    use super::*;

    /// Что станет со службой после запроса запуска/остановки.
    #[derive(Clone)]
    pub(crate) enum Reaction {
        /// Через `lag` опросов состояния она придёт в `Status` (до тех пор — «в процессе»).
        Goes(Status, u32),
        /// Запрос принят, служба застряла в состоянии `State` и не выходит из него.
        Stuck(State),
        /// Запрос отвергнут.
        Fails(&'static str),
    }

    pub(crate) struct FakeService {
        status: Cell<Status>,
        pending: Cell<Option<(u32, Status)>>,
        pub(crate) on_start: Reaction,
        pub(crate) on_stop: Reaction,
        pub(crate) delete_error: Option<&'static str>,
        /// Имена вызванных методов по порядку: `start`, `stop`, `delete`.
        pub(crate) calls: RefCell<Vec<&'static str>>,
        pub(crate) queries: Cell<u32>,
    }

    impl FakeService {
        pub(crate) fn in_state(state: State) -> FakeService {
            FakeService {
                status: Cell::new(Status::new(state)),
                pending: Cell::new(None),
                on_start: Reaction::Goes(Status::new(State::Running), 0),
                on_stop: Reaction::Goes(Status::new(State::Stopped), 0),
                delete_error: None,
                calls: RefCell::new(Vec::new()),
                queries: Cell::new(0),
            }
        }

        pub(crate) fn calls(&self) -> Vec<&'static str> {
            self.calls.borrow().clone()
        }

        fn react(&self, reaction: &Reaction, pending: State) -> Result<(), String> {
            match reaction {
                Reaction::Goes(target, 0) => self.status.set(*target),
                Reaction::Goes(target, lag) => {
                    self.status.set(Status::new(pending));
                    self.pending.set(Some((*lag, *target)));
                }
                Reaction::Stuck(state) => self.status.set(Status::new(*state)),
                Reaction::Fails(e) => return Err((*e).to_string()),
            }
            Ok(())
        }
    }

    impl ServiceControl for FakeService {
        fn start(&self) -> Result<(), String> {
            self.calls.borrow_mut().push("start");
            if self.status.get().state == State::Running {
                return Ok(());
            }
            self.react(&self.on_start.clone(), State::StartPending)
        }

        fn stop(&self) -> Result<(), String> {
            self.calls.borrow_mut().push("stop");
            if self.status.get().state == State::Stopped {
                return Ok(());
            }
            self.react(&self.on_stop.clone(), State::StopPending)
        }

        fn delete(&self) -> Result<(), String> {
            self.calls.borrow_mut().push("delete");
            self.delete_error.map_or(Ok(()), |e| Err(e.to_string()))
        }

        fn query(&self) -> Result<Status, String> {
            self.queries.set(self.queries.get() + 1);
            if let Some((lag, target)) = self.pending.get() {
                if lag == 0 {
                    self.status.set(target);
                    self.pending.set(None);
                } else {
                    self.pending.set(Some((lag - 1, target)));
                }
            }
            Ok(self.status.get())
        }

        fn wait_state(&self, want: State, timeout: Duration) -> Result<(), Status> {
            wait_for(self, want, timeout, Duration::from_millis(1))
        }

        fn clear_failure_actions(&self) -> Result<(), String> {
            self.calls.borrow_mut().push("clear_failure_actions");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{FakeService, Reaction};
    use super::*;

    const SHORT: Duration = Duration::from_millis(30);

    #[test]
    fn no_failure_actions_deletes_them_rather_than_keeping_the_old_ones() {
        let mut placeholder = [SC_ACTION { Type: SC_ACTION_NONE, Delay: 0 }];
        let none = no_failure_actions(&mut placeholder);
        assert_eq!(none.cActions, 0);
        assert!(!none.lpsaActions.is_null(), "нулевой указатель оставил бы прежние действия");
        assert_eq!(none.dwResetPeriod, 0);
    }

    #[test]
    fn failure_actions_on_non_crash_failures() {
        assert_eq!(non_crash_failures().fFailureActionsOnNonCrashFailures, TRUE);
    }

    #[test]
    fn wait_sees_the_state_after_a_few_polls() {
        let mut svc = FakeService::in_state(State::Stopped);
        svc.on_start = Reaction::Goes(Status::new(State::Running), 3);
        svc.start().unwrap();
        assert_eq!(svc.wait_state(State::Running, Duration::from_secs(5)), Ok(()));
        assert!(svc.queries.get() >= 3, "ждали, а не вернулись сразу: {}", svc.queries.get());
    }

    #[test]
    fn wait_times_out_with_the_last_status() {
        let mut svc = FakeService::in_state(State::Stopped);
        svc.on_start = Reaction::Stuck(State::StartPending);
        svc.start().unwrap();
        // Не дождались — состояние на выходе, из которого видно, что служба так и не поднялась.
        assert_eq!(svc.wait_state(State::Running, SHORT), Err(Status::new(State::StartPending)));
    }

    #[test]
    fn waiting_for_running_gives_up_when_the_service_stops() {
        let mut svc = FakeService::in_state(State::Stopped);
        let crashed = Status { state: State::Stopped, win32_exit: 1066, specific_exit: 7 };
        svc.on_start = Reaction::Goes(crashed, 2);
        svc.start().unwrap();
        let begun = Instant::now();
        assert_eq!(svc.wait_state(State::Running, Duration::from_secs(60)), Err(crashed), "коды завершения доходят до вызывающего");
        assert!(begun.elapsed() < Duration::from_secs(30), "не ждали весь срок");
    }

    #[test]
    fn stopping_an_already_stopped_service_is_not_an_error() {
        let svc = FakeService::in_state(State::Stopped);
        assert_eq!(stop_and_wait(&svc, SHORT), Ok(()));
        assert_eq!(svc.calls(), ["stop"]);
        assert_eq!(svc.queries.get(), 1, "стоит — состояние спрошено один раз, без ожидания");
    }

    #[test]
    fn stop_waits_for_a_slow_service() {
        let mut svc = FakeService::in_state(State::Running);
        svc.on_stop = Reaction::Goes(Status::new(State::Stopped), 3);
        assert_eq!(stop_and_wait(&svc, Duration::from_secs(5)), Ok(()));
    }

    #[test]
    fn stop_that_never_completes_times_out() {
        let mut svc = FakeService::in_state(State::Running);
        svc.on_stop = Reaction::Stuck(State::StopPending);
        let e = stop_and_wait(&svc, SHORT).unwrap_err();
        assert!(e.contains("не остановилась"), "{e}");
    }

    #[test]
    fn refused_stop_is_reported_at_once() {
        let mut svc = FakeService::in_state(State::Running);
        svc.on_stop = Reaction::Fails("access denied");
        assert_eq!(stop_and_wait(&svc, Duration::from_secs(60)), Err("access denied".to_string()));
        assert_eq!(svc.queries.get(), 0, "после отказа состояние не ждётся");
    }

    #[test]
    fn running_means_state_running_only() {
        assert!(FakeService::in_state(State::Running).is_running());
        for st in [State::Stopped, State::StartPending, State::StopPending] {
            assert!(!FakeService::in_state(st).is_running(), "{st:?}");
        }
    }

    #[test]
    fn stop_errors_that_do_not_block_stopping_are_tolerated() {
        use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_SERVICE_REQUEST_TIMEOUT};
        // Не работает / уже останавливается — ждём Stopped дальше; отказ в доступе и прочее — ошибка.
        assert!(stop_tolerated(ERROR_SERVICE_NOT_ACTIVE));
        assert!(stop_tolerated(ERROR_SERVICE_CANNOT_ACCEPT_CTRL));
        assert!(!stop_tolerated(ERROR_ACCESS_DENIED));
        assert!(!stop_tolerated(ERROR_SERVICE_REQUEST_TIMEOUT));
    }

    #[test]
    fn raw_states_map_to_the_enum() {
        assert_eq!(State::from_raw(SERVICE_STOPPED), State::Stopped);
        assert_eq!(State::from_raw(SERVICE_RUNNING), State::Running);
        assert_eq!(State::from_raw(SERVICE_START_PENDING), State::StartPending);
        assert_eq!(State::from_raw(SERVICE_STOP_PENDING), State::StopPending);
        assert_eq!(State::from_raw(7), State::Other(7));
    }
}
