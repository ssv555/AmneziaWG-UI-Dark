//! «Запустить свой exe с правами администратора (UAC) и получить его итог» — одна реализация для окна (`run`)
//! и для помощника (`report`): установка/удаление ядра и «Вернуть» в окне обновлений.
//!
//! Итог идёт не через файл. Путь в папке пользователя (TEMP) подменяет любая программа этой учётной записи — ссылкой
//! на системный файл, и помощник с правами администратора перезаписал бы его. Поэтому окно создаёт безымянный канал
//! и передаёт помощнику только номер своего процесса и описатель записи (`--result <pid>:<описатель>`); помощник
//! копирует описатель к себе (DuplicateHandle) и пишет в него. Пути нет — подменять нечего; права у описателя те,
//! что получило окно без повышения; а описатель, который не канал, помощник отвергает, так что записью через него
//! файл не перезаписать.

use std::io::{Read, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::null_mut;

use crate::i18n::trf;
use crate::win::Elevated;

/// Ключ помощника: куда вернуть итог.
pub const RESULT_FLAG: &str = "--result";
/// Предел итога в байтах: помощник обрезает длинный текст ошибки сам, окну хватает начала.
const MAX_REPORT: usize = 16 * 1024;

/// Чем кончилась работа помощника — глазами окна.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// Помощник сделал своё; примечание — его дополнение к успеху (пустое, если нечего сказать).
    Done(String),
    /// Помощник не справился или не запустился — текст для пользователя.
    Failed(String),
    /// Пользователь отменил запрос UAC — его выбор, а не сбой.
    Cancelled,
}

/// Окно: запустить этот exe с `args` через UAC, дождаться и вернуть итог. `failed_key` — строка i18n с `{0}` для
/// кода выхода, если помощник упал, не успев ничего сообщить.
pub fn run(args: &str, failed_key: &str) -> Outcome {
    let channel = match Channel::open() {
        Ok(c) => c,
        Err(e) => return Outcome::Failed(e),
    };
    let launched = crate::win::run_elevated(&format!("{args} {RESULT_FLAG} {}", channel.target()));
    let report = channel.finish();
    match launched {
        Ok(Elevated::Exited(code)) => outcome(code, report, failed_key),
        Ok(Elevated::Cancelled) => Outcome::Cancelled,
        Err(e) => Outcome::Failed(e),
    }
}

/// Помощник: вернуть итог окну, если оно передало `--result`. Без него (запуск из консоли) — только печать.
pub fn report(target: Option<&str>, result: &Result<String, String>) {
    println!("{result:?}");
    if let Some(target) = target {
        if let Err(e) = report_to(target, result) {
            // Окно получит только код выхода; другого пути сказать ему об этом нет.
            println!("{RESULT_FLAG} {target}: {e}");
        }
    }
}

/// Итог в канале: `ok\n<примечание>` или `err\n<текст ошибки>`, не длиннее `MAX_REPORT`.
fn encode(result: &Result<String, String>) -> String {
    let (tag, text) = match result {
        Ok(note) => ("ok", note),
        Err(e) => ("err", e),
    };
    let mut end = text.len().min(MAX_REPORT);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{tag}\n{}", &text[..end])
}

/// Обратное `encode`. Нет итога или он не в этом формате — `None`.
fn parse(text: &str) -> Option<Result<String, String>> {
    match text.split_once('\n')? {
        ("ok", note) => Some(Ok(note.to_string())),
        ("err", e) => Some(Err(e.to_string())),
        _ => None,
    }
}

/// Код выхода и итог помощника -> `Outcome`. Код выхода главнее: успех с ненулевым кодом — сбой.
fn outcome(code: u32, report: Option<Result<String, String>>, failed_key: &str) -> Outcome {
    match (code, report) {
        (_, Some(Err(e))) if !e.is_empty() => Outcome::Failed(e),
        (0, Some(Ok(note))) => Outcome::Done(note),
        // Помощник сделал дело, но вернуть итог не смог (он напечатал почему); примечание потеряно.
        (0, None) => Outcome::Done(String::new()),
        (code, _) => Outcome::Failed(trf(failed_key, &[&code.to_string()])),
    }
}

/// `<pid>:<описатель>` из `--result`.
fn parse_target(target: &str) -> Result<(u32, usize), String> {
    let bad = || format!("expected <pid>:<handle>, got {target:?}");
    let (pid, handle) = target.split_once(':').ok_or_else(bad)?;
    let pid = pid.parse::<u32>().map_err(|_| bad())?;
    let handle = handle.parse::<usize>().map_err(|_| bad())?;
    if pid == 0 || handle == 0 {
        return Err(bad());
    }
    Ok((pid, handle))
}

/// Безымянный канал окна: читающий конец читает отдельный поток (иначе помощник встал бы на полном буфере, пока
/// окно ждёт его выхода), пишущий конец окно держит, пока помощник не выйдет.
struct Channel {
    write: OwnedHandle,
    reader: std::thread::JoinHandle<std::io::Result<Vec<u8>>>,
}

impl Channel {
    fn open() -> Result<Channel, String> {
        use windows_sys::Win32::System::Pipes::CreatePipe;
        let (mut read, mut write) = (null_mut(), null_mut());
        // Без SECURITY_ATTRIBUTES описатели не наследуются: процессы, которые окно запускает, пока ждёт, не унесут
        // пишущий конец и не задержат конец чтения.
        if unsafe { CreatePipe(&mut read, &mut write, std::ptr::null(), 0) } == 0 {
            return Err(format!("result channel: {}", std::io::Error::last_os_error()));
        }
        let (read, write) = unsafe { (OwnedHandle::from_raw_handle(read), OwnedHandle::from_raw_handle(write)) };
        let mut read = std::fs::File::from(read);
        let reader = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            read.read_to_end(&mut bytes).map(|_| bytes)
        });
        Ok(Channel { write, reader })
    }

    /// Значение `--result` для помощника.
    fn target(&self) -> String {
        format!("{}:{}", std::process::id(), self.write.as_raw_handle() as usize)
    }

    /// Помощник вышел: закрыть свой пишущий конец и забрать, что он прислал.
    fn finish(self) -> Option<Result<String, String>> {
        drop(self.write);
        match self.reader.join() {
            Ok(Ok(bytes)) => parse(&String::from_utf8_lossy(&bytes)),
            Ok(Err(e)) => Some(Err(format!("result channel: {e}"))),
            Err(_) => Some(Err("result channel: reader panicked".into())),
        }
    }
}

/// Записать итог в канал окна `target`.
fn report_to(target: &str, result: &Result<String, String>) -> Result<(), String> {
    let (pid, handle) = parse_target(target)?;
    let mut pipe = std::fs::File::from(take_pipe(pid, handle)?);
    pipe.write_all(encode(result).as_bytes()).map_err(|e| format!("write: {e}"))
}

/// Скопировать описатель `handle` процесса `pid` к себе. Принимается только канал: копия описателя файла
/// (подложенного вместо канала) позволила бы перезаписать файл, а запись в канал на диск не попадает.
fn take_pipe(pid: u32, handle: usize) -> Result<OwnedHandle, String> {
    use windows_sys::Win32::Foundation::{DuplicateHandle, DUPLICATE_SAME_ACCESS};
    use windows_sys::Win32::Storage::FileSystem::{GetFileType, FILE_TYPE_PIPE};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let source = open_for_dup(pid)?;
    let mut copy = null_mut();
    let ok = unsafe {
        DuplicateHandle(source.as_raw_handle(), handle as _, GetCurrentProcess(), &mut copy, 0, 0, DUPLICATE_SAME_ACCESS)
    };
    if ok == 0 {
        return Err(format!("DuplicateHandle {pid}:{handle}: {}", std::io::Error::last_os_error()));
    }
    let copy = unsafe { OwnedHandle::from_raw_handle(copy) };
    match unsafe { GetFileType(copy.as_raw_handle()) } {
        FILE_TYPE_PIPE => Ok(copy),
        kind => Err(format!("{pid}:{handle} is not a pipe (file type {kind})")),
    }
}

/// Процесс окна с правом копировать его описатели. UAC мог подтвердить другой администратор: тогда окно чужое,
/// и открыть его можно только с SeDebugPrivilege (у повышенного администратора она есть, но выключена).
fn open_for_dup(pid: u32) -> Result<OwnedHandle, String> {
    use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_DUP_HANDLE};
    let open = || {
        let h = unsafe { OpenProcess(PROCESS_DUP_HANDLE, 0, pid) };
        if h.is_null() {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(unsafe { OwnedHandle::from_raw_handle(h) })
        }
    };
    match open() {
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => {
            enable_debug_privilege().map_err(|p| format!("OpenProcess {pid}: {e}; SeDebugPrivilege: {p}"))?;
            open().map_err(|e| format!("OpenProcess {pid} (SeDebugPrivilege): {e}"))
        }
        other => other.map_err(|e| format!("OpenProcess {pid}: {e}")),
    }
}

fn enable_debug_privilege() -> Result<(), String> {
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_NOT_ALL_ASSIGNED, LUID};
    use windows_sys::Win32::Security::{
        AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES, SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES,
        TOKEN_PRIVILEGES,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    let name: Vec<u16> = "SeDebugPrivilege\0".encode_utf16().collect();
    unsafe {
        let mut luid = LUID { LowPart: 0, HighPart: 0 };
        if LookupPrivilegeValueW(std::ptr::null(), name.as_ptr(), &mut luid) == 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let mut token = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES, &mut token) == 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let token = OwnedHandle::from_raw_handle(token);
        let state = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }],
        };
        let ok = AdjustTokenPrivileges(token.as_raw_handle(), 0, &state, 0, null_mut(), null_mut());
        // Успех AdjustTokenPrivileges ещё не значит, что право было в маркере: это говорит только GetLastError.
        match (ok, GetLastError()) {
            (0, _) => Err(std::io::Error::last_os_error().to_string()),
            (_, ERROR_NOT_ALL_ASSIGNED) => Err("not held by this account".into()),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_parse_round_trip() {
        for r in [Ok(String::new()), Ok("autostart".to_string()), Err("строка 1\nстрока 2".to_string())] {
            assert_eq!(parse(&encode(&r)), Some(r));
        }
    }

    #[test]
    fn parse_rejects_foreign_text() {
        assert_eq!(parse(""), None, "помощник ничего не прислал");
        assert_eq!(parse("ok"), None, "без перевода строки — обрывок");
        assert_eq!(parse("ok autostart"), None, "прежний формат файла — не наш");
        assert_eq!(parse("done\nx"), None);
    }

    #[test]
    fn encode_truncates_on_char_boundary() {
        let long = "я".repeat(MAX_REPORT); // 2 байта на символ
        let text = encode(&Err(long));
        assert!(text.len() <= MAX_REPORT + 4);
        assert!(matches!(parse(&text), Some(Err(e)) if !e.is_empty()));
    }

    #[test]
    fn outcome_follows_exit_code() {
        let key = "core.setup_failed";
        assert_eq!(outcome(0, Some(Ok("autostart".into())), key), Outcome::Done("autostart".into()));
        assert_eq!(outcome(0, None, key), Outcome::Done(String::new()));
        assert_eq!(outcome(1, Some(Err("no service".into())), key), Outcome::Failed("no service".into()));
        assert_eq!(outcome(0, Some(Err("no service".into())), key), Outcome::Failed("no service".into()));
        let code = |c: u32| Outcome::Failed(trf(key, &[&c.to_string()]));
        assert_eq!(outcome(5, None, key), code(5), "упал молча — код выхода");
        assert_eq!(outcome(3, Some(Ok(String::new())), key), code(3), "«успех» с ненулевым кодом — сбой");
        assert_eq!(outcome(2, Some(Err(String::new())), key), code(2), "пустой текст ошибки — код выхода");
    }

    #[test]
    fn target_syntax() {
        assert_eq!(parse_target("1234:420"), Ok((1234, 420)));
        for bad in ["", "1234", "1234:", ":420", "0:420", "1234:0", "x:1", "1:0x1a4", r"C:\Temp\r.txt", "1:2:3"] {
            assert!(parse_target(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn result_reaches_window_through_pipe() {
        let channel = Channel::open().unwrap();
        report_to(&channel.target(), &Ok("autostart".into())).unwrap();
        assert_eq!(channel.finish(), Some(Ok("autostart".into())));
        let channel = Channel::open().unwrap();
        report_to(&channel.target(), &Err("отказ".into())).unwrap();
        assert_eq!(channel.finish(), Some(Err("отказ".into())));
    }

    #[test]
    fn silent_helper_gives_no_report() {
        assert_eq!(Channel::open().unwrap().finish(), None);
    }

    /// Вместо канала подсунут описатель файла (например, открытого по ссылке на системный файл) — помощник
    /// отказывается писать, файл не тронут.
    #[test]
    fn refuses_file_handle_target() {
        let path = std::env::temp_dir().join(format!("awg-ui-elevated-test-{}.txt", std::process::id()));
        let file = std::fs::OpenOptions::new().create_new(true).read(true).write(true).open(&path).unwrap();
        let target = format!("{}:{}", std::process::id(), file.as_raw_handle() as usize);
        let refused = report_to(&target, &Err("x".repeat(100)));
        let len = std::fs::metadata(&path).unwrap().len();
        drop(file);
        std::fs::remove_file(&path).unwrap();
        assert!(matches!(&refused, Err(e) if e.contains("not a pipe")), "{refused:?}");
        assert_eq!(len, 0, "файл не должен меняться");
    }

    #[test]
    fn refuses_unknown_handle_and_process() {
        assert!(report_to(&format!("{}:{}", std::process::id(), 0x7ff_fff0usize), &Ok(String::new())).is_err());
        assert!(report_to(&format!("{}:4", u32::MAX - 3), &Ok(String::new())).is_err(), "нет такого процесса");
    }
}
