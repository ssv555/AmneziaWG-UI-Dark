//! Windows-специфика: права администратора, заголовок и рамка окна под тему, служба менеджера, автозапуск, один экземпляр.

use std::ffi::c_void;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE};
use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use crate::scm::{Handle, Service, ServiceControl, State};

/// Строка UTF-16 с завершающим нулём — для вызовов Win32 `*W`.
pub(crate) fn wide(s: &str) -> Vec<u16> {
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
    use windows_sys::Win32::System::Services::{SERVICE_QUERY_STATUS, SERVICE_START};
    let scm = Handle::scm_connect()?;
    let svc = Service::open(&scm, name, SERVICE_QUERY_STATUS | SERVICE_START)?;
    match svc.query()?.state {
        State::Running | State::StartPending => Ok(false),
        _ => svc.start().map(|()| true),
    }
}

/// Путь к exe службы из её настроек — так AmneziaWG находится и при установке не в Program Files.
pub fn service_binary(name: &str) -> Option<std::path::PathBuf> {
    service_command(name).as_deref().and_then(exe_from_command_line)
}

/// Командная строка службы (`lpBinaryPathName`); службы нет или она недоступна — None.
pub fn service_command(name: &str) -> Option<String> {
    use windows_sys::Win32::System::Services::SERVICE_QUERY_CONFIG;
    let scm = Handle::scm_connect().ok()?;
    Service::try_open(&scm, name, SERVICE_QUERY_CONFIG).ok().flatten()?.command_line()
}

/// `"C:\dir\app.exe" /arg` или `C:\dir\app.exe /arg` → путь к exe.
pub fn exe_from_command_line(s: &str) -> Option<std::path::PathBuf> {
    let s = s.trim();
    let path = match s.strip_prefix('"') {
        Some(rest) => &rest[..rest.find('"')?],
        None => {
            // ASCII-регистр: длина в байтах не меняется, индекс годится для исходной строки (у `İ` lowercase длиннее).
            let end = s.to_ascii_lowercase().find(".exe")? + 4;
            &s[..end]
        }
    };
    Some(std::path::PathBuf::from(path))
}

/// Стандартный диалог Windows «Открыть» / «Сохранить как» для файлов .conf. None — отмена.
pub fn pick_conf(save: bool, initial: Option<&std::path::Path>) -> Option<std::path::PathBuf> {
    pick_files(Files::Conf, save, false, initial).into_iter().next()
}

/// Какие файлы показывает диалог выбора.
#[derive(Clone, Copy)]
pub enum Files {
    Conf,
    ConfOrZip,
    Zip,
    /// Журнал событий: `.log`, можно и `.txt`.
    Log,
}

/// Стандартный диалог открытия/сохранения; `multi` — несколько файлов сразу. Отмена — пустой список.
pub fn pick_files(kind: Files, save: bool, multi: bool, initial: Option<&std::path::Path>) -> Vec<std::path::PathBuf> {
    use windows_sys::Win32::UI::Controls::Dialogs::{
        GetOpenFileNameW, GetSaveFileNameW, OFN_ALLOWMULTISELECT, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR,
        OFN_OVERWRITEPROMPT, OFN_PATHMUSTEXIST, OPENFILENAMEW,
    };
    let mut buf = vec![0u16; if multi { 65536 } else { 4096 }];
    if let Some(p) = initial {
        let w: Vec<u16> = p.as_os_str().to_string_lossy().encode_utf16().take(buf.len() - 1).collect();
        buf[..w.len()].copy_from_slice(&w);
    }
    let (filter, ext) = match kind {
        Files::Conf => ("AmneziaWG / WireGuard (*.conf)\0*.conf\0*.*\0*.*\0\0", "conf"),
        Files::ConfOrZip => ("*.conf, *.zip\0*.conf;*.zip\0*.*\0*.*\0\0", "conf"),
        Files::Zip => ("ZIP (*.zip)\0*.zip\0\0", "zip"),
        Files::Log => ("Log (*.log)\0*.log\0Text (*.txt)\0*.txt\0*.*\0*.*\0\0", "log"),
    };
    let filter: Vec<u16> = filter.encode_utf16().collect();
    let ext = wide(ext);
    let mut ofn: OPENFILENAMEW = unsafe { std::mem::zeroed() };
    ofn.lStructSize = size_of::<OPENFILENAMEW>() as u32;
    ofn.lpstrFilter = filter.as_ptr();
    ofn.lpstrFile = buf.as_mut_ptr();
    ofn.nMaxFile = buf.len() as u32;
    ofn.lpstrDefExt = ext.as_ptr();
    ofn.Flags = OFN_EXPLORER | OFN_NOCHANGEDIR | if save { OFN_OVERWRITEPROMPT } else { OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST };
    if multi {
        ofn.Flags |= OFN_ALLOWMULTISELECT;
    }
    let ok = unsafe { if save { GetSaveFileNameW(&mut ofn) } else { GetOpenFileNameW(&mut ofn) } };
    if ok == 0 {
        return Vec::new();
    }
    picked_paths(&buf)
}

/// Ответ диалога: один путь (`путь\0\0`) или папка и имена (`папка\0имя1\0имя2\0\0`).
fn picked_paths(buf: &[u16]) -> Vec<std::path::PathBuf> {
    let parts: Vec<String> = buf.split(|&c| c == 0).take_while(|p| !p.is_empty()).map(String::from_utf16_lossy).collect();
    match parts.as_slice() {
        [] => Vec::new(),
        [one] => vec![std::path::PathBuf::from(one)],
        [dir, names @ ..] => names.iter().map(|n| std::path::Path::new(dir).join(n)).collect(),
    }
}

/// Создать папку (если нет) и выставить ей владельца и права из SDDL (`O:…D:P…`) — без наследования от родителя.
/// Ставится каждый раз: вдруг права поменяли. Папка с чужим владельцем (её заранее создал обычный пользователь —
/// в ProgramData это может любой) отодвигается целиком: владелец мог бы вернуть себе доступ, а внутри — что угодно.
pub fn protect_dir(dir: &std::path::Path, sddl: &str) -> Result<(), String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::{
        GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, ACL, DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    };
    if dir.exists() && !owned_by_admins(dir)? {
        let aside = dir.with_file_name(format!("{}.foreign-{}", dir.file_name().unwrap_or_default().to_string_lossy(), crate::store::random_hex()));
        std::fs::rename(dir, &aside).map_err(|e| crate::fsutil::io_ctx(&dir, e))?;
    }
    std::fs::create_dir_all(dir).map_err(|e| crate::fsutil::io_ctx(&dir, e))?;
    unsafe {
        let mut sd: PSECURITY_DESCRIPTOR = null_mut();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(wide(sddl).as_ptr(), SDDL_REVISION_1, &mut sd, null_mut()) == 0 {
            return Err(format!("SDDL: {}", std::io::Error::last_os_error()));
        }
        let (mut present, mut defaulted, mut dacl) = (0, 0, null_mut::<ACL>());
        GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted);
        let mut owner: PSID = null_mut();
        GetSecurityDescriptorOwner(sd, &mut owner, &mut defaulted);
        let what = DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION | if owner.is_null() { 0 } else { OWNER_SECURITY_INFORMATION };
        let r = SetNamedSecurityInfoW(wide(&dir.to_string_lossy()).as_ptr(), SE_FILE_OBJECT, what, owner, null_mut(), dacl, null());
        LocalFree(sd);
        if r != 0 {
            return Err(format!("{}: {}", dir.display(), std::io::Error::from_raw_os_error(r as i32)));
        }
    }
    Ok(())
}

/// Владелец папки — SYSTEM или администраторы.
fn owned_by_admins(dir: &std::path::Path) -> Result<bool, String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID};
    unsafe {
        let (mut owner, mut sd): (PSID, PSECURITY_DESCRIPTOR) = (null_mut(), null_mut());
        let r = GetNamedSecurityInfoW(
            wide(&dir.to_string_lossy()).as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            null_mut(),
            null_mut(),
            &mut sd,
        );
        if r != 0 {
            return Err(format!("{}: {}", dir.display(), std::io::Error::from_raw_os_error(r as i32)));
        }
        let sid = sid_string(owner);
        LocalFree(sd);
        Ok(matches!(sid.as_deref(), Some(LOCAL_SYSTEM_SID | BUILTIN_ADMINS_SID)))
    }
}

/// SID строкой `S-1-…`.
pub unsafe fn sid_string(sid: windows_sys::Win32::Security::PSID) -> Option<String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    let mut text: *mut u16 = null_mut();
    if sid.is_null() || ConvertSidToStringSidW(sid, &mut text) == 0 {
        return None;
    }
    let len = (0..).take_while(|&i| *text.add(i) != 0).count();
    let s = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
    LocalFree(text.cast());
    Some(s)
}

/// Известная папка Windows (Program Files, ProgramData …) — из оболочки, не из переменных окружения:
/// их пользователь может подменить у себя.
pub fn known_folder(id: &windows_sys::core::GUID) -> Option<std::path::PathBuf> {
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::SHGetKnownFolderPath;
    unsafe {
        let mut p: *mut u16 = null_mut();
        let ok = SHGetKnownFolderPath(id, 0, null_mut(), &mut p) == 0 && !p.is_null();
        let dir = ok.then(|| {
            let len = (0..).take_while(|&i| *p.add(i) != 0).count();
            std::path::PathBuf::from(String::from_utf16_lossy(std::slice::from_raw_parts(p, len)))
        });
        if !p.is_null() {
            CoTaskMemFree(p.cast());
        }
        dir
    }
}

/// Program Files — известная папка оболочки, а не переменная окружения: переменную пользователь может подменить
/// у себя, и программа с правами администратора положила бы движок службы SYSTEM (или запустила бы exe от
/// администратора) в его папке. Оболочка не ответила — стандартный путь.
pub fn program_files() -> std::path::PathBuf {
    use windows_sys::Win32::UI::Shell::FOLDERID_ProgramFiles;
    known_folder(&FOLDERID_ProgramFiles).unwrap_or_else(|| std::path::PathBuf::from(r"C:\Program Files"))
}

/// ProgramData — по тем же причинам, что и `program_files`, не из переменной окружения.
pub fn program_data() -> std::path::PathBuf {
    use windows_sys::Win32::UI::Shell::FOLDERID_ProgramData;
    known_folder(&FOLDERID_ProgramData).unwrap_or_else(|| std::path::PathBuf::from(r"C:\ProgramData"))
}

/// SID учётной записи SYSTEM.
pub const LOCAL_SYSTEM_SID: &str = "S-1-5-18";
/// SID группы «Администраторы».
pub const BUILTIN_ADMINS_SID: &str = "S-1-5-32-544";

/// Строка годится как SID в SDDL (`(A;;FR;;;<sid>)`): `S-1-`, дальше только цифры и дефисы, не пусто. Всё, что
/// попало в SDDL, меняет права папки, поэтому чужую строку (из настроек, аргумента командной строки) сначала
/// проверяют здесь.
pub fn is_sid(s: &str) -> bool {
    s.strip_prefix("S-1-").is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit() || b == b'-'))
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

/// Автозапуск окна при входе в Windows: значение в `HKCU\…\Run`. Права администратора окну больше не нужны —
/// всё, что их требует, делает ядро.
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "AmneziaWG UI Dark";

pub fn autostart_enabled() -> bool {
    hidden_command("reg").args(["query", RUN_KEY, "/v", RUN_VALUE]).output().map(|o| o.status.success()).unwrap_or(false)
}

pub fn set_autostart(enable: bool) -> Result<(), String> {
    let out = if enable {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let command = format!("\"{}\" --tray", exe.display());
        hidden_command("reg").args(["add", RUN_KEY, "/v", RUN_VALUE, "/t", "REG_SZ", "/d", &command, "/f"]).output()
    } else {
        hidden_command("reg").args(["delete", RUN_KEY, "/v", RUN_VALUE, "/f"]).output()
    }
    .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("reg: {}", String::from_utf8_lossy(&out.stderr).trim()))
    }
}

/// Задачи планировщика прежних версий (автозапуск и запуск без UAC). Была задача автозапуска — вернуть `true`,
/// чтобы установка ядра включила автозапуск по-новому. Нужны права администратора.
pub fn remove_legacy_tasks() -> bool {
    let exists = |name: &str| hidden_command("schtasks").args(["/Query", "/TN", name]).output().map(|o| o.status.success()).unwrap_or(false);
    let had_autostart = exists("awg-ui");
    for name in ["awg-ui", "awg-ui-launch"] {
        if exists(name) {
            // Прежние задачи убираются по возможности: оставшаяся находится при следующей установке этой же проверкой `exists`.
            let _ = hidden_command("schtasks").args(["/Delete", "/TN", name, "/F"]).output();
        }
    }
    had_autostart
}

/// Один аргумент командной строки так, как его разберёт обратно CommandLineToArgvW (правила MSVC): кавычки внутри
/// и обратные косые перед ними экранируются, иначе аргумент с `"` или `\` в конце развалится на части.
pub fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_string();
    }
    let mut out = String::from("\"");
    let mut slashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => slashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(slashes * 2 + 1));
                slashes = 0;
            }
            _ => {
                out.push_str(&"\\".repeat(slashes));
                slashes = 0;
            }
        }
        if c != '\\' {
            out.push(c);
        }
    }
    out.push_str(&"\\".repeat(slashes * 2));
    out.push('"');
    out
}

/// Запустить этот exe с `args` с правами администратора (запрос UAC) и дождаться. Код выхода; отказ — ошибка.
pub fn run_elevated_wait(args: &str) -> Result<u32, String> {
    match run_elevated(args)? {
        Elevated::Exited(code) => Ok(code),
        Elevated::Cancelled => Err(crate::i18n::tr("core.uac_declined")),
    }
}

/// Чем кончился запуск с правами администратора.
pub enum Elevated {
    Exited(u32),
    /// Пользователь отменил запрос UAC — это его выбор, а не сбой.
    Cancelled,
}

/// Как `run_elevated_wait`, но отмену UAC отличает от сбоя запуска.
pub fn run_elevated(args: &str) -> Result<Elevated, String> {
    use windows_sys::Win32::Foundation::ERROR_CANCELLED;
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
    use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let (verb, file, params) = (wide("runas"), wide(&exe.to_string_lossy()), wide(args));
    unsafe {
        let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
        info.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOCLOSEPROCESS;
        info.lpVerb = verb.as_ptr();
        info.lpFile = file.as_ptr();
        info.lpParameters = params.as_ptr();
        info.nShow = SW_HIDE;
        if ShellExecuteExW(&mut info) == 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(ERROR_CANCELLED as i32) {
                return Ok(Elevated::Cancelled);
            }
            return Err(format!("{}: {e}", crate::i18n::tr("core.uac_declined")));
        }
        if info.hProcess.is_null() {
            return Err(crate::i18n::tr("core.uac_declined"));
        }
        WaitForSingleObject(info.hProcess, INFINITE);
        let mut code = 1u32;
        GetExitCodeProcess(info.hProcess, &mut code);
        CloseHandle(info.hProcess);
        Ok(Elevated::Exited(code))
    }
}

/// SID пользователя, от которого работает процесс, строкой `S-1-5-…`.
pub fn current_user_sid() -> Result<String, String> {
    use windows_sys::Win32::Security::{TokenUser, TOKEN_USER};
    unsafe {
        let mut token: HANDLE = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        // u64, а не u8: буфер читается как TOKEN_USER (в нём указатель), буфер из u8 выровнен только на 1.
        let mut buf = vec![0u64; 32];
        let mut len = 0u32;
        let ok = GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), size_of_val(buf.as_slice()) as u32, &mut len);
        CloseHandle(token);
        if ok == 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        sid_string(user.User.Sid).ok_or_else(|| std::io::Error::last_os_error().to_string())
    }
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

/// Тёмный или светлый заголовок окна, перерисованный сразу.
pub fn title_bar_dark(hwnd: isize, dark: bool) -> Result<(), String> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
    };
    title_bar_dark_mark(hwnd, dark)?;
    // Windows 10 перерисовывает заголовок только при следующей активации окна; смена рамки — сразу.
    let flags = SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE;
    if unsafe { SetWindowPos(hwnd as _, null_mut(), 0, 0, 0, 0, flags) } == 0 {
        return Err(format!("title bar redraw: SetWindowPos: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

/// Только отметка тёмного или светлого заголовка у DWM, без перерисовки рамки: дёшево, можно звать на каждую
/// активацию окна. Windows 11 перерисовывает заголовок сам, Windows 10 — при следующей активации.
pub fn title_bar_dark_mark(hwnd: isize, dark: bool) -> Result<(), String> {
    let on = i32::from(dark);
    let hr = unsafe {
        DwmSetWindowAttribute(
            hwnd as *mut c_void,
            DWMWA_USE_IMMERSIVE_DARK_MODE as _,
            &on as *const i32 as *const c_void,
            size_of::<i32>() as u32,
        )
    };
    if hr < 0 {
        return Err(format!("title bar dark={dark}: DwmSetWindowAttribute HRESULT 0x{hr:08X}"));
    }
    Ok(())
}

/// Цвет рамки окна (Windows 11; Windows 10 такой настройки не знает — не ошибка); `None` — системный.
pub fn border_color(hwnd: isize, rgb: Option<[u8; 3]>) -> Result<(), String> {
    // DWMWA_BORDER_COLOR и DWMWA_COLOR_DEFAULT из dwmapi.h; цвет — COLORREF 0x00BBGGRR.
    const DWMWA_BORDER_COLOR: u32 = 34;
    // Ответ Windows 10 на незнакомый атрибут.
    const E_INVALIDARG: i32 = 0x8007_0057_u32 as i32;
    let color: u32 = rgb.map_or(0xFFFF_FFFF, |[r, g, b]| u32::from(r) | u32::from(g) << 8 | u32::from(b) << 16);
    let hr = unsafe {
        DwmSetWindowAttribute(hwnd as *mut c_void, DWMWA_BORDER_COLOR as _, &color as *const u32 as *const c_void, size_of::<u32>() as u32)
    };
    if hr >= 0 || hr == E_INVALIDARG {
        return Ok(());
    }
    Err(format!("window border colour {color:08X}: DwmSetWindowAttribute HRESULT 0x{hr:08X}"))
}

/// Светлая ли тема приложений в Windows (`AppsUseLightTheme`) — запасной путь, когда egui не сообщил тему.
/// Значения нет (Windows до 1809, тема не менялась) — светлая, как и считает Windows.
pub fn apps_use_light_theme() -> Result<bool, String> {
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
    const KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize";
    let mut value: u32 = 0;
    let mut size = size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            wide(KEY).as_ptr(),
            wide("AppsUseLightTheme").as_ptr(),
            RRF_RT_REG_DWORD,
            null_mut(),
            &mut value as *mut u32 as *mut c_void,
            &mut size,
        )
    };
    match status {
        ERROR_SUCCESS => Ok(value != 0),
        ERROR_FILE_NOT_FOUND => Ok(true),
        e => Err(format!(r"HKCU\{KEY}\AppsUseLightTheme: {}", std::io::Error::from_raw_os_error(e as i32))),
    }
}

/// Секунды без клавиатуры и мыши в этом сеансе (признак «пользователь за компьютером» для `app::reminder`);
/// `None` — Windows не ответила.
pub fn idle_seconds() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::GetTickCount;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    // dwTime — значение `GetTickCount` в момент последнего ввода.
    let mut info = LASTINPUTINFO { cbSize: size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    if unsafe { GetLastInputInfo(&mut info) } == 0 {
        return None;
    }
    Some(idle_ms(unsafe { GetTickCount() }, info.dwTime) / 1000)
}

/// Миллисекунды между двумя отсчётами `GetTickCount`: счётчик 32-битный и раз в 49 суток обнуляется.
fn idle_ms(now_tick: u32, last_input_tick: u32) -> u64 {
    now_tick.wrapping_sub(last_input_tick) as u64
}

/// `QUNS_ACCEPTS_NOTIFICATIONS` — единственное состояние, в котором всплывающее уведомление будет показано. В
/// остальных (нет в сеансе или экран заблокирован, полноэкранная программа, игра, презентация, «не беспокоить»,
/// полноэкранное приложение Store) Windows его прячет.
const QUNS_ACCEPTS_NOTIFICATIONS: i32 = 5;

fn accepts_notifications(state: i32) -> bool {
    state == QUNS_ACCEPTS_NOTIFICATIONS
}

/// Покажет ли Windows уведомление сейчас; `Err` — текст ошибки вызова.
pub fn notifications_accepted() -> Result<bool, String> {
    let mut state = 0;
    let hr = unsafe { windows_sys::Win32::UI::Shell::SHQueryUserNotificationState(&mut state) };
    if hr < 0 {
        return Err(format!("SHQueryUserNotificationState: 0x{hr:08X}"));
    }
    Ok(accepts_notifications(state))
}

#[cfg(test)]
mod tests {
    #[test]
    fn idle_time_survives_the_tick_counter_wrapping() {
        assert_eq!(super::idle_ms(10_000, 4_000), 6_000);
        assert_eq!(super::idle_ms(5, u32::MAX - 994), 1_000, "счётчик обнулился между вводом и опросом");
        assert_eq!(super::idle_ms(7, 7), 0);
    }

    #[test]
    fn only_the_accepting_state_lets_a_toast_through() {
        for (state, accepted) in [(1, false), (2, false), (3, false), (4, false), (5, true), (6, false), (7, false)] {
            assert_eq!(super::accepts_notifications(state), accepted, "состояние {state}");
        }
    }

    #[test]
    fn probes_answer_without_panicking() {
        // Сеанс сборки может быть без ввода (служба): важно, что вызовы не падают и число правдоподобно.
        if let Some(secs) = super::idle_seconds() {
            assert!(secs < 50 * 24 * 3600, "{secs}");
        }
        let _ = super::notifications_accepted();
    }

    /// `quote_arg` -> CommandLineToArgvW возвращает исходные аргументы.
    #[test]
    fn quote_arg_round_trip() {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::UI::Shell::CommandLineToArgvW;
        let originals = ["plain", "", "a b", r"C:\Program Files\", r#"say "hi""#, r#"tail\"#, r#"x\\"y"#, r"\\server\share"];
        let line = std::iter::once("exe".to_string()).chain(originals.iter().map(|a| super::quote_arg(a))).collect::<Vec<_>>().join(" ");
        let wide = super::wide(&line);
        let mut argc = 0;
        let argv = unsafe { CommandLineToArgvW(wide.as_ptr(), &mut argc) };
        assert!(!argv.is_null());
        let parsed: Vec<String> = (1..argc as usize)
            .map(|i| unsafe {
                let p = *argv.add(i);
                let len = (0..).take_while(|&k| *p.add(k) != 0).count();
                String::from_utf16_lossy(std::slice::from_raw_parts(p, len))
            })
            .collect();
        unsafe { LocalFree(argv.cast()) };
        assert_eq!(parsed, originals, "{line}");
    }

    #[test]
    fn exe_from_command_line_forms() {
        let p = |s: &str| std::path::PathBuf::from(s);
        assert_eq!(super::exe_from_command_line(r#""C:\a b\x.exe" /svc"#), Some(p(r"C:\a b\x.exe")));
        assert_eq!(super::exe_from_command_line(r"C:\a\X.EXE /svc"), Some(p(r"C:\a\X.EXE")));
        // Не-ASCII до «.exe», чей lowercase длиннее (İ: 2 -> 3 байта): раньше срез по чужому индексу.
        assert_eq!(super::exe_from_command_line(r"C:\İİ\x.exe /svc"), Some(p(r"C:\İİ\x.exe")));
        assert_eq!(super::exe_from_command_line(r"C:\İ\x.bat"), None);
    }

    #[test]
    fn picked_paths_single_and_multi() {
        let w = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
        let p = |s: &str| std::path::PathBuf::from(s);
        assert_eq!(super::picked_paths(&w("C:\\t\\a.conf\0\0")), vec![p(r"C:\t\a.conf")]);
        assert_eq!(super::picked_paths(&w("C:\\t\0a.conf\0b.zip\0\0")), vec![p(r"C:\t\a.conf"), p(r"C:\t\b.zip")]);
        assert!(super::picked_paths(&w("\0\0")).is_empty());
    }

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
    fn wide_is_nul_terminated_utf16() {
        assert_eq!(super::wide(""), vec![0]);
        assert_eq!(super::wide("aЖ"), vec![0x61, 0x416, 0]);
        assert_eq!(super::wide("😀"), vec![0xD83D, 0xDE00, 0], "суррогатная пара");
    }

    #[test]
    fn is_sid_accepts_only_sids_that_are_safe_in_sddl() {
        for ok in [super::LOCAL_SYSTEM_SID, super::BUILTIN_ADMINS_SID, "S-1-5-21-1004336348-1177238915-682003330-1000"] {
            assert!(super::is_sid(ok), "{ok}");
        }
        for bad in ["", "S-1-", "S-2-5-18", "s-1-5-18", " S-1-5-18", "S-1-5-18 ", "S-1-5-18)(A;;GA;;;WD", "S-1-5-18;", "S-1-5-1x", "SY", "S-1-5-١٨"] {
            assert!(!super::is_sid(bad), "{bad:?}");
        }
    }

    #[test]
    fn system_sid_constant_is_what_windows_reports_for_a_system_owned_object() {
        // Администраторы и SYSTEM — единственные владельцы, которым доверяет `owned_by_admins`: константы не расходятся
        // с тем, что возвращает `sid_string` для настоящих SID.
        use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
        for sid in [super::LOCAL_SYSTEM_SID, super::BUILTIN_ADMINS_SID] {
            let mut p = std::ptr::null_mut();
            assert_ne!(unsafe { ConvertStringSidToSidW(super::wide(sid).as_ptr(), &mut p) }, 0, "{sid}");
            assert_eq!(unsafe { super::sid_string(p) }.as_deref(), Some(sid));
            unsafe { windows_sys::Win32::Foundation::LocalFree(p.cast()) };
        }
    }

    #[test]
    fn program_folders_come_from_the_shell_not_the_environment() {
        let (files, data) = (super::program_files(), super::program_data());
        assert!(files.is_absolute() && files.is_dir(), "{files:?}");
        assert!(data.is_absolute() && data.is_dir(), "{data:?}");
        assert_ne!(files, data);
    }

    #[test]
    fn missing_service_is_reported_not_started() {
        let err = super::ensure_service("AwgUiNoSuchService").unwrap_err();
        assert!(err.contains("AwgUiNoSuchService"), "{err}");
        assert_eq!(super::service_command("AwgUiNoSuchService"), None);
    }

    #[test]
    fn service_command_reads_the_binary_path_of_a_system_service() {
        // EventLog есть в любой Windows; права на чтение настроек у обычного пользователя есть.
        let command = super::service_command("EventLog").expect("EventLog");
        assert!(command.to_ascii_lowercase().contains("svchost"), "{command}");
    }
}
