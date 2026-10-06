//! Точка входа службы ядра: диспетчер служб Windows, состояние службы, остановка по команде и при выключении.
//! RUNNING сообщается только когда ядро уже отвечает по каналу; не поднялось или завершилось с ошибкой —
//! STOPPED с ненулевым кодом, и диспетчер перезапускает службу (действия при сбое ставит `install`).

use std::cell::Cell;
use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

use windows_sys::Win32::Foundation::{ERROR_CALL_NOT_IMPLEMENTED, ERROR_SERVICE_SPECIFIC_ERROR, NO_ERROR};
use windows_sys::Win32::System::Services::{
    RegisterServiceCtrlHandlerExW, SetServiceStatus, StartServiceCtrlDispatcherW, SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP,
    SERVICE_CONTROL_INTERROGATE, SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP, SERVICE_RUNNING, SERVICE_START_PENDING,
    SERVICE_STATUS, SERVICE_STATUS_HANDLE, SERVICE_STOPPED, SERVICE_STOP_PENDING, SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS,
};

use super::server::RunError;

/// Код службы: ядро не поднялось (ошибка до готовности).
const NOT_STARTED: u32 = 1;
/// Код службы: ядро работало и завершилось с ошибкой.
const FAILED: u32 = 2;
/// Код службы: ядро не поднялось, потому что имя его канала заняла другая программа (сборка ни при чём).
pub const PIPE_TAKEN: u32 = 3;
/// Подсказка диспетчеру, сколько ждать запуска (миллисекунды): ожидание канала в `server::run` — до 15 с.
const START_HINT: u32 = 20_000;
/// То же для остановки.
const STOP_HINT: u32 = 10_000;

static STOP: AtomicBool = AtomicBool::new(false);
static HANDLE: AtomicPtr<c_void> = AtomicPtr::new(null_mut());

/// Процесс службы: отдать управление диспетчеру служб. Возвращает код выхода процесса.
pub fn main() -> i32 {
    // stderr службы никто не читает: паника любого потока — в журнал событий ядра (что дальше — см. `crash`).
    crate::crash::install_core(super::events_file());
    let mut name: Vec<u16> = crate::win::wide(super::SERVICE);
    let table = [
        SERVICE_TABLE_ENTRYW { lpServiceName: name.as_mut_ptr(), lpServiceProc: Some(service_main) },
        SERVICE_TABLE_ENTRYW { lpServiceName: null_mut(), lpServiceProc: None },
    ];
    if unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } == 0 {
        return 1;
    }
    0
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut *mut u16) {
    let name: Vec<u16> = crate::win::wide(super::SERVICE);
    let handle = RegisterServiceCtrlHandlerExW(name.as_ptr(), Some(control), null_mut());
    if handle.is_null() {
        return;
    }
    HANDLE.store(handle, Ordering::SeqCst);
    report(SERVICE_START_PENDING, 0);
    let started = Cell::new(false);
    let result = super::server::run(&STOP, || {
        started.set(true);
        report(SERVICE_RUNNING, 0);
        // Новое ядро отвечает — отодвинутые обновлением файлы (exe, DLL) прошлой версии больше не нужны. Удаляются
        // только теперь: если новая версия не поднимется, прежняя остаётся рядом. Занятые ещё кем-то — до следующего раза.
        super::install::remove_old_copies(&crate::engine::install_dir());
    }, |tail| {
        // События последних мгновений ядра, которых агент не успел забрать (он гибнет вместе со службой), — прямо в
        // файл, как и причина сбоя ниже: служба уже не работает, живых путей здесь нет.
        let file = super::events_file();
        if let Err(err) = crate::events::append_events(&file, &tail) {
            eprintln!("service: cannot write {} ({} events): {err}", file.display(), tail.len());
        }
    });
    if let Err(e) = &result {
        // Причина — в журнал событий ядра, код — диспетчеру.
        let file = super::events_file();
        let event = crate::events::Event::new(crate::monitor::unix_now(), "", crate::events::Severity::Bad, e.text(), false);
        if let Err(err) = crate::events::append_event(&file, &event) {
            eprintln!("service: cannot write {}: {err}", file.display());
        }
    }
    report(SERVICE_STOPPED, exit_code(&result, started.get()));
}

unsafe extern "system" fn control(code: u32, _event: u32, _data: *mut c_void, _context: *mut c_void) -> u32 {
    match code {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            STOP.store(true, Ordering::SeqCst);
            report(SERVICE_STOP_PENDING, 0);
            NO_ERROR
        }
        SERVICE_CONTROL_INTERROGATE => NO_ERROR,
        _ => ERROR_CALL_NOT_IMPLEMENTED,
    }
}

/// Код службы при остановке: 0 — штатно, иначе `PIPE_TAKEN`, `NOT_STARTED` или `FAILED`.
fn exit_code(result: &Result<(), RunError>, started: bool) -> u32 {
    match (result, started) {
        (Ok(()), _) => 0,
        (Err(RunError::PipeTaken(_)), false) => PIPE_TAKEN,
        (Err(_), false) => NOT_STARTED,
        (Err(_), true) => FAILED,
    }
}

/// Состояние для диспетчера. Ненулевой `code` уходит как код службы (ERROR_SERVICE_SPECIFIC_ERROR) — для диспетчера
/// это сбой, и при включённом FailureActionsOnNonCrashFailures он выполняет действия при сбое.
fn status(state: u32, code: u32) -> SERVICE_STATUS {
    SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: if state == SERVICE_RUNNING { SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN } else { 0 },
        dwWin32ExitCode: if code == 0 { NO_ERROR } else { ERROR_SERVICE_SPECIFIC_ERROR },
        dwServiceSpecificExitCode: code,
        dwCheckPoint: 0,
        dwWaitHint: match state {
            SERVICE_START_PENDING => START_HINT,
            SERVICE_STOP_PENDING => STOP_HINT,
            _ => 0,
        },
    }
}

fn report(state: u32, code: u32) {
    let handle: SERVICE_STATUS_HANDLE = HANDLE.load(Ordering::SeqCst);
    unsafe { SetServiceStatus(handle, &status(state, code)) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes() {
        assert_eq!(exit_code(&Ok(()), true), 0);
        assert_eq!(exit_code(&Ok(()), false), 0);
        let failed = || Err(RunError::Failed("x".into()));
        assert_eq!(exit_code(&failed(), false), NOT_STARTED);
        assert_eq!(exit_code(&failed(), true), FAILED);
        // Имя канала занято — отдельный код: по нему перезапуск после обновления не возвращает прежнюю сборку.
        assert_eq!(exit_code(&Err(RunError::PipeTaken("x".into())), false), PIPE_TAKEN);
        assert_eq!(exit_code(&Err(RunError::PipeTaken("x".into())), true), FAILED);
        assert_ne!(PIPE_TAKEN, NOT_STARTED);
        assert_ne!(PIPE_TAKEN, FAILED);
        assert_ne!(NOT_STARTED, 0);
        assert_ne!(FAILED, 0);
    }

    #[test]
    fn start_pending_waits_and_refuses_stop() {
        let s = status(SERVICE_START_PENDING, 0);
        assert_eq!(s.dwCurrentState, SERVICE_START_PENDING);
        assert_eq!(s.dwWaitHint, START_HINT);
        assert_eq!(s.dwControlsAccepted, 0);
        assert_eq!(s.dwWin32ExitCode, NO_ERROR);
    }

    #[test]
    fn running_accepts_stop() {
        let s = status(SERVICE_RUNNING, 0);
        assert_eq!(s.dwControlsAccepted, SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN);
        assert_eq!(s.dwWaitHint, 0);
    }

    #[test]
    fn stopped_clean_is_not_failure() {
        let s = status(SERVICE_STOPPED, 0);
        assert_eq!((s.dwWin32ExitCode, s.dwServiceSpecificExitCode), (NO_ERROR, 0));
    }

    #[test]
    fn stopped_with_error_is_service_specific_failure() {
        for code in [NOT_STARTED, FAILED, PIPE_TAKEN] {
            let s = status(SERVICE_STOPPED, code);
            assert_eq!(s.dwWin32ExitCode, ERROR_SERVICE_SPECIFIC_ERROR);
            assert_eq!(s.dwServiceSpecificExitCode, code);
            assert_eq!(s.dwControlsAccepted, 0);
        }
    }

    #[test]
    fn stop_pending_hint() {
        assert_eq!(status(SERVICE_STOP_PENDING, 0).dwWaitHint, STOP_HINT);
    }
}
