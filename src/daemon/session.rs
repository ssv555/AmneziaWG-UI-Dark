//! Запуск помощника в сеансе пользователя. Ядро — служба в сеансе 0, рабочего стола не видит; действия в родном
//! окне AmneziaWG (оно запущено с правами администратора) выполняет копия программы в сеансе того, кто
//! прислал запрос, с полным (повышенным) токеном этого пользователя. Запрос UAC не нужен: ядро — SYSTEM.

use std::ffi::c_void;
use std::path::Path;
use std::ptr::{null, null_mut};
use std::time::Duration;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::Security::{
    DuplicateTokenEx, GetTokenInformation, SecurityIdentification, TokenImpersonation, TokenLinkedToken, TOKEN_LINKED_TOKEN, TOKEN_QUERY,
};
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

/// Чьим токеном запускать помощника.
#[derive(Debug, PartialEq)]
enum HelperToken {
    /// Полный токен администратора под UAC — связанный с токеном сеанса.
    Linked,
    /// Сам токен сеанса: администратор без UAC (встроенный Administrator, UAC выключен) — он уже полный.
    Own,
    /// Обычный пользователь: прав администратора нет ни в каком виде.
    NotAdmin,
}

/// Окно AmneziaWG запущено с правами администратора, и управлять им может только процесс с ними же. У владельца —
/// обычного пользователя (ядро для него ставил другой администратор) нет ни связанного полного токена, ни
/// «Администраторов» в своём: помощник с его токеном не прочёл бы даже своё задание (папка `ops` — только SYSTEM и
/// администраторы), и пользователь видел бы «файл не найден» вместо причины. `own_admin` — «Администраторы»
/// действуют в токене сеанса; `None` — узнать не удалось: запуск как раньше, отказ тогда покажет код выхода помощника.
fn helper_token(has_linked: bool, own_admin: Option<bool>) -> HelperToken {
    match (has_linked, own_admin) {
        (true, _) => HelperToken::Linked,
        (false, Some(false)) => HelperToken::NotAdmin,
        (false, _) => HelperToken::Own,
    }
}

/// «Администраторы» действуют в первичном токене `user`: проверка — на его копии олицетворения
/// (`pipe::token_in_admins`). `None` — копию сделать или проверить не удалось.
unsafe fn in_admins(user: HANDLE) -> Option<bool> {
    let mut copy: HANDLE = null_mut();
    if DuplicateTokenEx(user, TOKEN_QUERY, null(), SecurityIdentification, TokenImpersonation, &mut copy) == 0 {
        return None;
    }
    let member = super::pipe::token_in_admins(copy);
    CloseHandle(copy);
    member
}

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
        let mut linked = TOKEN_LINKED_TOKEN { LinkedToken: null_mut() };
        let mut len = 0u32;
        let has_linked = GetTokenInformation(user, TokenLinkedToken, (&mut linked as *mut TOKEN_LINKED_TOKEN).cast(), size_of::<TOKEN_LINKED_TOKEN>() as u32, &mut len) != 0
            && !linked.LinkedToken.is_null();
        let own_admin = if has_linked { None } else { in_admins(user) };
        let token = match helper_token(has_linked, own_admin) {
            HelperToken::Linked => {
                CloseHandle(user);
                linked.LinkedToken
            }
            HelperToken::Own => user,
            HelperToken::NotAdmin => {
                CloseHandle(user);
                return Err(crate::i18n::tr("core.helper_needs_admin"));
            }
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

    /// Владелец — обычный пользователь: помощник не запускается, причина — «нужна учётная запись администратора»,
    /// а не «файл не найден» от помощника без доступа к `ops`.
    #[test]
    fn helper_token_needs_an_administrator() {
        assert_eq!(helper_token(true, None), HelperToken::Linked);
        assert_eq!(helper_token(false, Some(true)), HelperToken::Own);
        assert_eq!(helper_token(false, Some(false)), HelperToken::NotAdmin);
        assert_eq!(helper_token(false, None), HelperToken::Own, "не узнали — запуск как раньше, отказ скажет код выхода");
    }

    /// Проверка членства работает на первичном токене (как у `WTSQueryUserToken`) — через копию олицетворения.
    #[test]
    fn admin_membership_is_read_from_a_primary_token() {
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
        unsafe {
            let mut token: HANDLE = null_mut();
            assert_ne!(OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY | windows_sys::Win32::Security::TOKEN_DUPLICATE, &mut token), 0);
            let member = in_admins(token);
            CloseHandle(token);
            assert!(member.is_some());
        }
    }
}
