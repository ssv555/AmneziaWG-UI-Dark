//! Запуск помощника в сеансе пользователя. Ядро — служба в сеансе 0, рабочего стола не видит; действия в родном
//! окне AmneziaWG (оно запущено с правами администратора) выполняет копия программы в сеансе того, кто
//! прислал запрос, с полным (повышенным) токеном этого пользователя. Запрос UAC не нужен: ядро — SYSTEM.

use std::ffi::c_void;
use std::path::Path;
use std::ptr::{null, null_mut};
use std::time::Duration;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::Security::{GetTokenInformation, TokenLinkedToken, TOKEN_LINKED_TOKEN};
use windows_sys::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows_sys::Win32::System::RemoteDesktop::WTSQueryUserToken;
use windows_sys::Win32::System::Threading::{
    CreateProcessAsUserW, GetExitCodeProcess, TerminateProcess, WaitForSingleObject, CREATE_BREAKAWAY_FROM_JOB,
    CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION, STARTUPINFOW,
};

/// Флаги запуска помощника. Агент работает в объекте задания ядра (`agent_watch`), а задание не может держать
/// процессы разных сеансов: без выхода из задания `CreateProcessAsUser` в сеанс пользователя падает с «Access is
/// denied» (так было с 0.5.0 по 0.5.2: действия в окне AmneziaWG из агента не работали). Задание создано с
/// `JOB_OBJECT_LIMIT_BREAKAWAY_OK`; процессу вне задания (ядру) флаг ничего не меняет.
const LAUNCH_FLAGS: u32 = CREATE_UNICODE_ENVIRONMENT | CREATE_BREAKAWAY_FROM_JOB;

/// Запустить `exe args` в сеансе `session` с повышенным токеном вошедшего пользователя и дождаться выхода.
/// Только если в сеансе вошёл тот же пользователь, что прислал запрос (`caller_sid`): иначе чужой запрос
/// из чужого сеанса получил бы права вошедшего там администратора. Окружение — только системное: свои
/// переменные пользователь может подменить (например, путь к программам), а помощник работает с его
/// правами администратора. Возвращает код выхода; не уложился в `timeout` — процесс завершается, ошибка.
pub fn run_elevated(session: u32, caller_sid: &str, exe: &Path, args: &str, timeout: Duration) -> Result<u32, String> {
    unsafe {
        let mut user: HANDLE = null_mut();
        if WTSQueryUserToken(session, &mut user) == 0 {
            return Err(format!("WTSQueryUserToken({session}): {}", std::io::Error::last_os_error()));
        }
        if super::pipe::token_user_sid(user).as_deref() != Some(caller_sid) {
            CloseHandle(user);
            return Err(crate::i18n::tr("core.other_session"));
        }
        // У администратора с UAC полный токен — связанный; без UAC (встроенный Administrator) — сам токен.
        let mut linked = TOKEN_LINKED_TOKEN { LinkedToken: null_mut() };
        let mut len = 0u32;
        let token = if GetTokenInformation(user, TokenLinkedToken, (&mut linked as *mut TOKEN_LINKED_TOKEN).cast(), size_of::<TOKEN_LINKED_TOKEN>() as u32, &mut len) != 0
            && !linked.LinkedToken.is_null()
        {
            CloseHandle(user);
            linked.LinkedToken
        } else {
            user
        };
        let mut env: *mut c_void = null_mut();
        let have_env = CreateEnvironmentBlock(&mut env, null_mut(), 0) != 0;
        let mut desktop: Vec<u16> = crate::win::wide("winsta0\\default");
        let mut si: STARTUPINFOW = std::mem::zeroed();
        si.cb = size_of::<STARTUPINFOW>() as u32;
        si.lpDesktop = desktop.as_mut_ptr();
        let mut cmd: Vec<u16> = crate::win::wide(&format!("\"{}\" {args}", exe.display()));
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        let ok = CreateProcessAsUserW(
            token,
            null(),
            cmd.as_mut_ptr(),
            null(),
            null(),
            0,
            LAUNCH_FLAGS,
            if have_env { env } else { null() },
            null(),
            &si,
            &mut pi,
        );
        let error = std::io::Error::last_os_error();
        if have_env {
            DestroyEnvironmentBlock(env);
        }
        CloseHandle(token);
        if ok == 0 {
            return Err(format!("CreateProcessAsUser: {error}"));
        }
        CloseHandle(pi.hThread);
        let waited = WaitForSingleObject(pi.hProcess, timeout.as_millis() as u32);
        let mut code = 1u32;
        let result = if waited == WAIT_OBJECT_0 {
            GetExitCodeProcess(pi.hProcess, &mut code);
            Ok(code)
        } else {
            TerminateProcess(pi.hProcess, 1);
            Err(crate::i18n::tr("core.helper_timeout"))
        };
        CloseHandle(pi.hProcess);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Помощник из агента запускается в чужом сеансе, а агент сидит в объекте задания ядра: без выхода из задания
    /// запуск падает с «Access is denied» (0.5.0–0.5.2). Живой запуск в сеанс пользователя в тестах невозможен.
    #[test]
    fn helper_leaves_the_agent_job() {
        assert_ne!(LAUNCH_FLAGS & CREATE_BREAKAWAY_FROM_JOB, 0);
        assert_ne!(LAUNCH_FLAGS & CREATE_UNICODE_ENVIRONMENT, 0);
    }
}
