//! Windows-специфика: права администратора, тёмный заголовок, служба менеджера, автозапуск, один экземпляр.

use std::ffi::c_void;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE};
use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

pub fn is_elevated() -> bool {
    unsafe {
        let mut token: HANDLE = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut _ as *mut c_void,
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        );
        CloseHandle(token);
        ok != 0 && elevation.TokenIsElevated != 0
    }
}

/// Перезапуск себя с правами администратора (запрос UAC). true — новый процесс запущен.
pub fn relaunch_elevated(args: &str) -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    let (verb, exe, args) = (wide("runas"), wide(&exe.to_string_lossy()), wide(args));
    let r = unsafe { ShellExecuteW(null_mut(), verb.as_ptr(), exe.as_ptr(), args.as_ptr(), null(), SW_SHOWNORMAL) };
    r as usize > 32
}

/// Консольная утилита без мелькающего окна консоли (мы — GUI-программа).
pub fn hidden_command(exe: impl AsRef<std::ffi::OsStr>) -> std::process::Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut cmd = std::process::Command::new(exe);
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

/// Служба запущена? Если нет — запустить. Ok(true) — запустили сейчас.
pub fn ensure_service(name: &str) -> Result<bool, String> {
    use windows_sys::Win32::System::Services::{
        CloseServiceHandle, OpenSCManagerW, OpenServiceW, QueryServiceStatus, StartServiceW, SC_MANAGER_CONNECT,
        SERVICE_QUERY_STATUS, SERVICE_RUNNING, SERVICE_START, SERVICE_START_PENDING, SERVICE_STATUS,
    };
    let err = |what: &str| format!("{what} {name}: {}", std::io::Error::last_os_error());
    unsafe {
        let scm = OpenSCManagerW(null(), null(), SC_MANAGER_CONNECT);
        if scm == 0 as _ {
            return Err(err("service manager"));
        }
        let svc = OpenServiceW(scm, wide(name).as_ptr(), SERVICE_QUERY_STATUS | SERVICE_START);
        if svc == 0 as _ {
            let e = err("service");
            CloseServiceHandle(scm);
            return Err(e);
        }
        let mut st: SERVICE_STATUS = std::mem::zeroed();
        let result = if QueryServiceStatus(svc, &mut st) == 0 {
            Err(err("service status"))
        } else if st.dwCurrentState == SERVICE_RUNNING || st.dwCurrentState == SERVICE_START_PENDING {
            Ok(false)
        } else if StartServiceW(svc, 0, null()) != 0 {
            Ok(true)
        } else {
            Err(err("service start"))
        };
        CloseServiceHandle(svc);
        CloseServiceHandle(scm);
        result
    }
}

/// Путь к exe службы из её настроек — так AmneziaWG находится и при установке не в Program Files.
pub fn service_binary(name: &str) -> Option<std::path::PathBuf> {
    use windows_sys::Win32::System::Services::{
        CloseServiceHandle, OpenSCManagerW, OpenServiceW, QueryServiceConfigW, QUERY_SERVICE_CONFIGW,
        SC_MANAGER_CONNECT, SERVICE_QUERY_CONFIG,
    };
    unsafe {
        let scm = OpenSCManagerW(null(), null(), SC_MANAGER_CONNECT);
        if scm == 0 as _ {
            return None;
        }
        let svc = OpenServiceW(scm, wide(name).as_ptr(), SERVICE_QUERY_CONFIG);
        let mut command = None;
        if svc != 0 as _ {
            let mut needed = 0u32;
            QueryServiceConfigW(svc, null_mut(), 0, &mut needed);
            // u64 — чтобы буфер был выровнен под структуру.
            let mut buf = vec![0u64; (needed as usize).div_ceil(8).max(1)];
            if QueryServiceConfigW(svc, buf.as_mut_ptr().cast(), (buf.len() * 8) as u32, &mut needed) != 0 {
                let cfg = &*(buf.as_ptr() as *const QUERY_SERVICE_CONFIGW);
                let p = cfg.lpBinaryPathName;
                if !p.is_null() {
                    let len = (0..).take_while(|&i| *p.add(i) != 0).count();
                    command = Some(String::from_utf16_lossy(std::slice::from_raw_parts(p, len)));
                }
            }
            CloseServiceHandle(svc);
        }
        CloseServiceHandle(scm);
        command.as_deref().and_then(exe_from_command_line)
    }
}

/// `"C:\dir\app.exe" /arg` или `C:\dir\app.exe /arg` → путь к exe.
fn exe_from_command_line(s: &str) -> Option<std::path::PathBuf> {
    let s = s.trim();
    let path = match s.strip_prefix('"') {
        Some(rest) => &rest[..rest.find('"')?],
        None => {
            let end = s.to_lowercase().find(".exe")? + 4;
            &s[..end]
        }
    };
    Some(std::path::PathBuf::from(path))
}

/// Стандартный диалог Windows «Открыть» / «Сохранить как» для файлов .conf. None — отмена.
pub fn pick_conf(save: bool, initial: Option<&std::path::Path>) -> Option<std::path::PathBuf> {
    use windows_sys::Win32::UI::Controls::Dialogs::{
        GetOpenFileNameW, GetSaveFileNameW, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR, OFN_OVERWRITEPROMPT,
        OFN_PATHMUSTEXIST, OPENFILENAMEW,
    };
    let mut buf = vec![0u16; 4096];
    if let Some(p) = initial {
        let w: Vec<u16> = p.as_os_str().to_string_lossy().encode_utf16().take(buf.len() - 1).collect();
        buf[..w.len()].copy_from_slice(&w);
    }
    let filter: Vec<u16> = "AmneziaWG / WireGuard (*.conf)\0*.conf\0*.*\0*.*\0\0".encode_utf16().collect();
    let ext = wide("conf");
    let mut ofn: OPENFILENAMEW = unsafe { std::mem::zeroed() };
    ofn.lStructSize = size_of::<OPENFILENAMEW>() as u32;
    ofn.lpstrFilter = filter.as_ptr();
    ofn.lpstrFile = buf.as_mut_ptr();
    ofn.nMaxFile = buf.len() as u32;
    ofn.lpstrDefExt = ext.as_ptr();
    ofn.Flags = OFN_EXPLORER | OFN_NOCHANGEDIR | if save { OFN_OVERWRITEPROMPT } else { OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST };
    let ok = unsafe { if save { GetSaveFileNameW(&mut ofn) } else { GetOpenFileNameW(&mut ofn) } };
    if ok == 0 {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(std::path::PathBuf::from(String::from_utf16_lossy(&buf[..len])))
}

/// Открыть файл программой, связанной с ним в Windows (ShellExecute «open»).
/// Для .conf связи часто нет — тогда блокнот.
pub fn shell_open(path: &std::path::Path) -> Result<(), String> {
    const SE_ERR_NOASSOC: usize = 31;
    let (verb, file) = (wide("open"), wide(&path.to_string_lossy()));
    let r = unsafe { ShellExecuteW(null_mut(), verb.as_ptr(), file.as_ptr(), null(), null(), SW_SHOWNORMAL) } as usize;
    match r {
        r if r > 32 => Ok(()),
        SE_ERR_NOASSOC => std::process::Command::new("notepad.exe").arg(path).spawn().map(drop).map_err(|e| e.to_string()),
        r => Err(format!("ShellExecute: {r}")),
    }
}

/// Завершить процессы `exe_name` в сессии этого пользователя (не службы в сессии 0). Возвращает их число.
pub fn close_session_processes(exe_name: &str) -> usize {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
    let mut closed = 0;
    unsafe {
        let mut own_session = 0u32;
        if ProcessIdToSessionId(std::process::id(), &mut own_session) == 0 {
            return 0;
        }
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return 0;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        let mut ok = Process32FirstW(snap, &mut entry);
        while ok != 0 {
            let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
            let mut session = u32::MAX;
            ProcessIdToSessionId(entry.th32ProcessID, &mut session);
            if name.eq_ignore_ascii_case(exe_name) && session == own_session {
                let h = OpenProcess(PROCESS_TERMINATE, 0, entry.th32ProcessID);
                if h != 0 as _ {
                    if TerminateProcess(h, 0) != 0 {
                        closed += 1;
                    }
                    CloseHandle(h);
                }
            }
            ok = Process32NextW(snap, &mut entry);
        }
        CloseHandle(snap);
    }
    closed
}

const TASK_NAME: &str = "awg-ui";

/// Автозапуск при входе в Windows: задача планировщика с повышенными правами (без запроса UAC).
pub fn autostart_enabled() -> bool {
    hidden_command("schtasks").args(["/Query", "/TN", TASK_NAME]).output().map(|o| o.status.success()).unwrap_or(false)
}

pub fn set_autostart(enable: bool) -> Result<(), String> {
    if enable {
        register_task(TASK_NAME, true, "--tray")
    } else {
        schtasks(&["/Delete", "/TN", TASK_NAME, "/F"])
    }
}

/// Задача запуска по требованию: ярлык и обычный запуск без прав администратора поднимают программу
/// через неё — с повышенными правами, без запроса UAC.
const LAUNCH_TASK: &str = "awg-ui-launch";

pub fn ensure_launch_task() -> Result<(), String> {
    register_task(LAUNCH_TASK, false, "")
}

/// Запустить программу через задачу (если она есть). true — запущена.
pub fn run_launch_task() -> bool {
    hidden_command("schtasks").args(["/Run", "/TN", LAUNCH_TASK]).output().map(|o| o.status.success()).unwrap_or(false)
}

/// Зарегистрировать задачу для этого exe и текущего пользователя (с перезаписью).
fn register_task(name: &str, at_logon: bool, args: &str) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    // SID, а не DOMAIN\имя: в ssh- и служебных сессиях USERDOMAIN бывает чужим (WORKGROUP).
    let user = current_user_sid()?;
    // XML, а не флаги schtasks: только так снимается лимит «остановить через 3 дня».
    let xml = task_xml(&exe.to_string_lossy(), &user, at_logon, args);
    let path = std::env::temp_dir().join(format!("{name}-task.xml"));
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend(xml.encode_utf16().flat_map(u16::to_le_bytes));
    std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
    let result = schtasks(&["/Create", "/TN", name, "/F", "/XML", &path.to_string_lossy()]);
    let _ = std::fs::remove_file(&path);
    result
}

fn schtasks(args: &[&str]) -> Result<(), String> {
    let out = hidden_command("schtasks").args(args).output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("schtasks: {}", String::from_utf8_lossy(&out.stderr).trim()))
    }
}

/// SID пользователя, от которого работает процесс, строкой `S-1-5-…`.
fn current_user_sid() -> Result<String, String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{TokenUser, TOKEN_USER};
    unsafe {
        let mut token: HANDLE = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let mut buf = vec![0u8; 256];
        let mut len = 0u32;
        let ok = GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), buf.len() as u32, &mut len);
        CloseHandle(token);
        if ok == 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut text: *mut u16 = null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut text) == 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let len = (0..).take_while(|&i| *text.add(i) != 0).count();
        let sid = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
        LocalFree(text.cast());
        Ok(sid)
    }
}

/// XML задачи: с `at_logon` — при входе пользователя, иначе только по требованию.
/// Параллельные экземпляры разрешены: второй запуск сам покажет окно уже работающего и выйдет.
fn task_xml(exe: &str, user: &str, at_logon: bool, args: &str) -> String {
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let (exe, user, args) = (esc(exe), esc(user), esc(args));
    let (description, triggers) = if at_logon {
        (
            "awg-ui: окно туннелей AmneziaWG при входе в Windows",
            format!("<Triggers><LogonTrigger><Enabled>true</Enabled><UserId>{user}</UserId><Delay>PT10S</Delay></LogonTrigger></Triggers>"),
        )
    } else {
        ("awg-ui: запуск с правами администратора без запроса UAC", String::new())
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>{description}</Description></RegistrationInfo>
  {triggers}
  <Principals><Principal id="Author"><UserId>{user}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>HighestAvailable</RunLevel></Principal></Principals>
  <Settings>
    <MultipleInstancesPolicy>Parallel</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
  </Settings>
  <Actions Context="Author"><Exec><Command>{exe}</Command><Arguments>{args}</Arguments></Exec></Actions>
</Task>
"#
    )
}

/// Уже запущен другой экземпляр? Тогда показать его окно. Мьютекс живёт до конца процесса.
pub fn another_instance(title: &str) -> bool {
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowW, SetForegroundWindow, ShowWindow, SW_RESTORE};
    unsafe {
        CreateMutexW(null(), 0, wide("Local\\awg-ui-single-instance").as_ptr());
        if GetLastError() != ERROR_ALREADY_EXISTS {
            return false;
        }
        let hwnd = FindWindowW(null(), wide(title).as_ptr());
        if hwnd != 0 as _ {
            ShowWindow(hwnd, SW_RESTORE);
            SetForegroundWindow(hwnd);
        }
        true
    }
}

/// Точка (в пикселях экрана) внутри виртуального рабочего стола — монитор, где было окно, ещё есть.
pub fn on_screen(x: f32, y: f32) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    };
    let m = |i| unsafe { GetSystemMetrics(i) } as f32;
    let (left, top) = (m(SM_XVIRTUALSCREEN), m(SM_YVIRTUALSCREEN));
    let (right, bottom) = (left + m(SM_CXVIRTUALSCREEN), top + m(SM_CYVIRTUALSCREEN));
    x >= left - 8.0 && y >= top - 8.0 && x < right - 100.0 && y < bottom - 50.0
}

pub fn dark_title_bar(hwnd: isize) {
    let on: i32 = 1;
    unsafe {
        DwmSetWindowAttribute(
            hwnd as *mut c_void,
            DWMWA_USE_IMMERSIVE_DARK_MODE as _,
            &on as *const i32 as *const c_void,
            size_of::<i32>() as u32,
        );
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn user_sid_is_resolved() {
        assert!(super::current_user_sid().unwrap().starts_with("S-1-"));
    }

    #[test]
    fn exe_path_from_service_command() {
        use std::path::PathBuf;
        let quoted = r#""C:\Program Files\AmneziaWG\amneziawg.exe" /managerservice"#;
        assert_eq!(super::exe_from_command_line(quoted), Some(PathBuf::from(r"C:\Program Files\AmneziaWG\amneziawg.exe")));
        let bare = r"D:\Apps\AWG\amneziawg.EXE /managerservice";
        assert_eq!(super::exe_from_command_line(bare), Some(PathBuf::from(r"D:\Apps\AWG\amneziawg.EXE")));
        assert_eq!(super::exe_from_command_line("garbage"), None);
    }

    #[test]
    fn task_xml_escapes_path() {
        let xml = super::task_xml("C:/a&b/awg-ui.exe", "S-1-5-21-1", true, "--tray");
        assert!(xml.contains("<Command>C:/a&amp;b/awg-ui.exe</Command>") && xml.contains("<ExecutionTimeLimit>PT0S"));
        assert!(xml.contains("<LogonTrigger>") && xml.contains("<Arguments>--tray</Arguments>"));
    }

    #[test]
    fn launch_task_has_no_trigger() {
        let xml = super::task_xml("C:/awg-ui.exe", "S-1-5-21-1", false, "");
        assert!(!xml.contains("<Triggers>") && xml.contains("<RunLevel>HighestAvailable</RunLevel>"));
        assert!(xml.contains("<MultipleInstancesPolicy>Parallel</MultipleInstancesPolicy>"));
    }
}
