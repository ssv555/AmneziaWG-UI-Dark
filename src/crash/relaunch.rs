//! Окно после сбоя видеоадаптера запускает себя заново.
//!
//! Сброс драйвера видеокарты (переустановка драйвера, TDR) роняет окно паникой внутри egui-wgpu или wgpu; ядро и
//! туннели это не затрагивает, но окно показывало сообщение и закрывалось насовсем — до ручного запуска. Паника в
//! слое GPU (`is_gpu_failure`) теперь перезапускает окно тем же exe с теми же аргументами (`relaunch_args`): окно,
//! спрятанное в трей, возвращается в трей, видимое — видимым. Новый процесс ждёт выхода прежнего (`take_after_crash`)
//! до проверки «один экземпляр»: иначе он увидел бы умирающее окно и вышел. Не больше `LIMIT` перезапусков за
//! `WINDOW_SECS` (`guard`, история — `HISTORY_FILE` рядом с `crash.log`): драйвер, который роняет каждое новое окно,
//! не превращается в бесконечный цикл; сверх предела — прежнее сообщение пользователю и выход.

use std::path::Path;
use std::time::Duration;

use crate::i18n::{tr, trf};

/// Аргумент нового окна: pid упавшего, выхода которого оно ждёт.
pub const AFTER_CRASH_FLAG: &str = "--after-crash";
/// Запуск, спрятанный в трей (как из автозапуска).
pub const TRAY_FLAG: &str = "--tray";
/// Причина перезапуска (сообщение паники) для нового окна. Через окружение, а не аргументом: текст паники длинный и
/// с кавычками, а аргументы окна остаются теми, что разбирает `main`.
pub const CAUSE_ENV: &str = "AWG_UI_CRASH_CAUSE";
/// История перезапусков рядом с `crash.log`: время каждого (секунды Unix), по строке.
pub const HISTORY_FILE: &str = "relaunch-history.txt";
/// Не больше стольких перезапусков за `WINDOW_SECS`.
pub const LIMIT: usize = 3;
pub const WINDOW_SECS: u64 = 600;
/// Сколько новое окно ждёт выхода прежнего.
pub const WAIT_PREVIOUS: Duration = Duration::from_secs(10);
/// Пауза перед новой попыткой, когда окно после перезапуска не открылось: драйвер видеокарты ещё поднимается.
pub const START_RETRY_PAUSE: Duration = Duration::from_secs(3);

/// Крейты слоя GPU: паника с местом в их исходниках — сбой видеоадаптера, а не ошибка окна.
const GPU_CRATES: [&str; 5] = ["egui-wgpu", "wgpu", "wgpu-core", "wgpu-hal", "wgpu-types"];
/// Сообщения wgpu о потере устройства (`DeviceError::Lost` «Parent device is lost», `hal::DeviceError::Lost`
/// «Device is lost», `ErrorType::DeviceLost`) — в нижнем регистре: так они видны, даже если панику бросил не wgpu, а
/// код, получивший от него ошибку.
const DEVICE_LOST: [&str; 2] = ["device is lost", "devicelost"];

/// Паника вызвана слоем GPU: место в исходниках egui-wgpu / wgpu / wgpu-core / wgpu-hal / wgpu-types или сообщение
/// о потере устройства. `location` — файл места паники.
pub fn is_gpu_failure(location: Option<&str>, message: &str) -> bool {
    if location.is_some_and(in_gpu_crate) {
        return true;
    }
    let message = message.to_ascii_lowercase();
    DEVICE_LOST.iter().any(|s| message.contains(s))
}

/// Путь лежит в исходниках крейта слоя GPU: один из элементов пути — `<крейт>-<версия>` (так их раскладывает cargo в
/// `registry\src`). `wgpu-core-30.0.1` — это `wgpu-core`, а не `wgpu`: после имени должен идти номер версии.
fn in_gpu_crate(file: &str) -> bool {
    file.split(['\\', '/']).any(|part| {
        GPU_CRATES.iter().any(|name| {
            part.strip_prefix(name).and_then(|rest| rest.strip_prefix('-')).is_some_and(|v| v.starts_with(|c: char| c.is_ascii_digit()))
        })
    })
}

/// Аргументы нового окна: прежние, без `--after-crash <pid>` прошлого перезапуска, плюс `--after-crash old_pid`.
/// `shown` — было ли окно видно: видно — без `--tray`, спрятано — с одним `--tray`; `None` (окно ещё не создано) —
/// `--tray` как был.
pub fn relaunch_args(original: &[String], old_pid: u32, shown: Option<bool>) -> Vec<String> {
    let mut out = Vec::new();
    let mut args = original.iter();
    while let Some(arg) = args.next() {
        if arg == AFTER_CRASH_FLAG {
            args.next();
            continue;
        }
        if arg == TRAY_FLAG && shown.is_some() {
            continue;
        }
        out.push(arg.clone());
    }
    if shown == Some(false) {
        out.push(TRAY_FLAG.into());
    }
    out.extend([AFTER_CRASH_FLAG.to_string(), old_pid.to_string()]);
    out
}

/// pid прежнего окна из `--after-crash <pid>`; нет флага или pid не число — `None`.
pub fn after_crash_pid(args: &[String]) -> Option<u32> {
    let at = args.iter().position(|a| a == AFTER_CRASH_FLAG)?;
    args.get(at + 1)?.parse().ok()
}

/// Решение предела перезапусков.
#[derive(Debug, PartialEq, Eq)]
pub enum Guard {
    /// Перезапуск разрешён; новая история (прошлые в окне `WINDOW_SECS` и `now`) — записать до запуска.
    Allow(Vec<u64>),
    /// Предел достигнут: столько перезапусков за последние `WINDOW_SECS`.
    Refuse(usize),
}

/// Предел: не больше `LIMIT` перезапусков за `WINDOW_SECS` до `now`. Запись старше `WINDOW_SECS` выпадает. Запись из
/// будущего (часы перевели назад) считается свежей: лишний отказ лучше, чем бесконечный цикл.
pub fn guard(history: &[u64], now: u64) -> Guard {
    let mut recent: Vec<u64> = history.iter().copied().filter(|&t| now.saturating_sub(t) < WINDOW_SECS).collect();
    if recent.len() >= LIMIT {
        return Guard::Refuse(recent.len());
    }
    recent.push(now);
    Guard::Allow(recent)
}

/// История перезапусков. Файла нет — пусто. Не читается или испорчен — тоже пусто, а второе значение — причина: её
/// надо записать, иначе предел молча сбросился бы.
pub fn read_history(path: &Path) -> (Vec<u64>, Option<String>) {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Vec::new(), None),
        Err(e) => return (Vec::new(), Some(crate::fsutil::io_ctx(path, e))),
    };
    let mut times = Vec::new();
    for (n, line) in text.lines().enumerate().filter(|(_, l)| !l.trim().is_empty()) {
        match line.trim().parse() {
            Ok(t) => times.push(t),
            Err(_) => return (Vec::new(), Some(format!("{}: line {}: {line:?} is not a time", path.display(), n + 1))),
        }
    }
    (times, None)
}

/// Записать историю целиком и атомарно: сбой посреди записи не оставит половину файла.
pub fn write_history(path: &Path, times: &[u64]) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| crate::fsutil::io_ctx(dir, e))?;
    }
    let text: String = times.iter().map(|t| format!("{t}\n")).collect();
    crate::fsutil::write_atomic(path, text.as_bytes()).map_err(|e| crate::fsutil::io_ctx(path, e))
}

/// Итог попытки перезапуска — в строку `crash.log`.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Паника не в слое GPU (или окно не запустилось): перезапуск не помог бы.
    NotGpu,
    Relaunched(u32),
    Failed(String),
    /// Столько перезапусков уже было за последние `WINDOW_SECS`.
    LimitReached(usize),
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Outcome::NotGpu => write!(f, "not relaunched: not a video adapter failure"),
            Outcome::Relaunched(pid) => write!(f, "relaunched as pid {pid}"),
            Outcome::Failed(e) => write!(f, "relaunch failed: {e}"),
            Outcome::LimitReached(n) => write!(f, "limit reached: {n} relaunches in the last {} min, not relaunched", WINDOW_SECS / 60),
        }
    }
}

/// Перезапуск после паники в слое GPU: проверить предел по истории `history`, записать в неё `now` и запустить новое
/// окно (`spawn` — его pid). Второе значение — заметка для `crash.log`, если история не прочиталась.
pub fn relaunch(history: &Path, now: u64, spawn: impl FnOnce() -> Result<u32, String>) -> (Outcome, Option<String>) {
    let (past, problem) = read_history(history);
    let note = problem.map(|p| format!("relaunch history unreadable, treated as empty: {p}"));
    let outcome = match guard(&past, now) {
        Guard::Refuse(recent) => Outcome::LimitReached(recent),
        // Без записи следующий сбой не увидел бы этот перезапуск, и предел не сработал бы никогда: тогда лучше не
        // перезапускать вовсе.
        Guard::Allow(next) => match write_history(history, &next) {
            Err(e) => Outcome::Failed(format!("relaunch history not saved: {e}")),
            Ok(()) => spawn().map_or_else(Outcome::Failed, Outcome::Relaunched),
        },
    };
    (outcome, note)
}

/// Запустить новое окно: этот же exe, аргументы `args`, причина — в окружении (`CAUSE_ENV`). pid нового процесса.
pub fn spawn_window(args: &[String], cause: &str) -> Result<u32, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    std::process::Command::new(&exe).args(args).env(CAUSE_ENV, cause).spawn().map(|child| child.id()).map_err(|e| crate::fsutil::io_ctx(&exe, e))
}

/// Чем кончилось ожидание прежнего окна.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Waited {
    Exited,
    TimedOut,
    Failed(String),
}

/// Новое окно после перезапуска: что сказать пользователю.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AfterCrash {
    pub old_pid: u32,
    /// Сообщение паники прежнего окна; пусто — не передано.
    pub cause: String,
    pub waited: Waited,
    pub timeout: Duration,
}

impl AfterCrash {
    /// Текст подсказки и события журнала: окно перезапущено, причина, где подробности.
    pub fn notice(&self, crash_log: &Path) -> String {
        let cause = if self.cause.trim().is_empty() { tr("crash.cause_unknown") } else { self.cause.trim().to_string() };
        trf("crash.relaunched", &[&cause, &crash_log.display().to_string()])
    }

    /// Предупреждение для журнала, если прежнее окно не дождались.
    pub fn wait_warning(&self) -> Option<String> {
        let pid = self.old_pid.to_string();
        match &self.waited {
            Waited::Exited => None,
            Waited::TimedOut => Some(trf("crash.wait_timeout", &[&pid, &self.timeout.as_secs().to_string()])),
            Waited::Failed(e) => Some(trf("crash.wait_failed", &[&pid, e])),
        }
    }
}

/// Запуск после перезапуска (`--after-crash <pid>`): дождаться выхода прежнего окна (не дольше `timeout`, `wait` —
/// ожидание процесса) и забрать причину из окружения. Не перезапуск — `None`. Причина убирается из окружения в любом
/// случае: процессы, которые окно запустит потом, не должны её унаследовать.
pub fn after_crash(args: &[String], timeout: Duration, wait: impl FnOnce(u32, Duration) -> Waited) -> Option<AfterCrash> {
    let cause = std::env::var(CAUSE_ENV).unwrap_or_default();
    std::env::remove_var(CAUSE_ENV);
    let old_pid = after_crash_pid(args)?;
    let waited = wait(old_pid, timeout);
    Some(AfterCrash { old_pid, cause, waited, timeout })
}

/// `after_crash` с настоящим ожиданием процесса и сроком `WAIT_PREVIOUS`.
pub fn take_after_crash(args: &[String]) -> Option<AfterCrash> {
    after_crash(args, WAIT_PREVIOUS, wait_process)
}

/// Дождаться выхода процесса `pid`, не дольше `timeout`.
fn wait_process(pid: u32, timeout: Duration) -> Waited {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        let e = std::io::Error::last_os_error();
        // Несуществующий pid Windows не открывает: прежнее окно уже вышло.
        if e.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
            return Waited::Exited;
        }
        return Waited::Failed(format!("OpenProcess {pid}: {e}"));
    }
    let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
    let waited = unsafe { WaitForSingleObject(handle, ms) };
    let error = std::io::Error::last_os_error();
    unsafe { CloseHandle(handle) };
    match waited {
        WAIT_OBJECT_0 => Waited::Exited,
        WAIT_TIMEOUT => Waited::TimedOut,
        _ => Waited::Failed(format!("WaitForSingleObject {pid}: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Настоящая паника окна на pc-dev 2026.10.10 после переустановки драйвера NVIDIA.
    const REAL_LOCATION: &str =
        r"C:\Users\ssv55\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\egui-wgpu-0.36.2\src\renderer.rs";
    const REAL_MESSAGE: &str = "Failed to create staging buffer for index data. Index count: 20634. Required index buffer size: 82536. Actual size 115056 and capacity: 115056 (bytes)";

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("awg-ui-relaunch-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn real_egui_wgpu_panic_is_a_gpu_failure() {
        assert!(is_gpu_failure(Some(REAL_LOCATION), REAL_MESSAGE));
    }

    #[test]
    fn panic_in_every_gpu_crate_is_a_gpu_failure() {
        for file in [
            "/home/u/.cargo/registry/src/index.crates.io-x/wgpu-30.0.1/src/backend/wgpu_core.rs",
            r"C:\c\registry\src\i\wgpu-core-30.0.1\src\device\mod.rs",
            r"C:\c\registry\src\i\wgpu-hal-30.0.1\src\dx12\device.rs",
            r"C:\c\registry\src\i\wgpu-types-30.0.1\src\lib.rs",
        ] {
            assert!(is_gpu_failure(Some(file), "boom"), "{file}");
        }
    }

    #[test]
    fn panic_in_our_code_or_other_crates_is_not() {
        assert!(!is_gpu_failure(Some(r"src\app.rs"), "index out of bounds"));
        assert!(!is_gpu_failure(Some(r"C:\Shared\Projects\awg-ui\src\app.rs"), "called `Option::unwrap()` on a `None` value"));
        assert!(!is_gpu_failure(Some(r"C:\c\registry\src\i\egui-0.36.2\src\context.rs"), "boom"));
        assert!(!is_gpu_failure(Some(r"C:\c\registry\src\i\egui-winit-0.36.2\src\lib.rs"), "boom"));
        // Имя крейта без версии после него — не исходники крейта (наша папка, чужой проект).
        assert!(!is_gpu_failure(Some(r"C:\work\wgpu-notes\src\main.rs"), "boom"));
        assert!(!is_gpu_failure(None, "boom"));
    }

    #[test]
    fn device_lost_message_is_a_gpu_failure_from_any_place() {
        assert!(is_gpu_failure(Some(r"src\app.rs"), "render: Parent device is lost"));
        assert!(is_gpu_failure(None, "Device is lost"));
        assert!(is_gpu_failure(None, "error type DeviceLost"));
    }

    #[test]
    fn relaunch_args_follow_the_window_visibility() {
        assert_eq!(relaunch_args(&args(&["--tray"]), 7, Some(true)), args(&["--after-crash", "7"]), "видимое окно — без --tray");
        assert_eq!(relaunch_args(&args(&[]), 7, Some(false)), args(&["--tray", "--after-crash", "7"]), "спрятанное — с --tray");
        assert_eq!(relaunch_args(&args(&["--tray", "--tray"]), 7, Some(false)), args(&["--tray", "--after-crash", "7"]), "--tray один");
        assert_eq!(relaunch_args(&args(&["--tray", "--demo"]), 7, None), args(&["--tray", "--demo", "--after-crash", "7"]), "неизвестно — как было");
        assert_eq!(relaunch_args(&args(&["--demo"]), 7, None), args(&["--demo", "--after-crash", "7"]));
    }

    #[test]
    fn relaunch_args_replace_the_previous_after_crash() {
        let old = args(&["--after-crash", "5", "--tray", "--after-crash", "6"]);
        assert_eq!(relaunch_args(&old, 7, Some(false)), args(&["--tray", "--after-crash", "7"]));
        assert_eq!(after_crash_pid(&relaunch_args(&old, 7, None)), Some(7));
        assert_eq!(after_crash_pid(&args(&["--after-crash"])), None);
        assert_eq!(after_crash_pid(&args(&["--after-crash", "x"])), None);
        assert_eq!(after_crash_pid(&args(&["--tray"])), None);
    }

    #[test]
    fn guard_allows_three_per_ten_minutes() {
        let now = 1_000_000;
        assert_eq!(guard(&[], now), Guard::Allow(vec![now]));
        assert_eq!(guard(&[now - 100, now - 50], now), Guard::Allow(vec![now - 100, now - 50, now]), "третий разрешён");
        assert_eq!(guard(&[now - 100, now - 50, now - 10], now), Guard::Refuse(3), "четвёртый — нет");
    }

    #[test]
    fn guard_drops_entries_older_than_the_window() {
        let now = 1_000_000;
        let edge = now - WINDOW_SECS;
        assert_eq!(guard(&[edge, now - 50, now - 10], now), Guard::Allow(vec![now - 50, now - 10, now]), "ровно 10 мин назад — выпала");
        assert_eq!(guard(&[edge + 1, now - 50, now - 10], now), Guard::Refuse(3), "на секунду моложе — ещё считается");
        assert_eq!(guard(&[now + 30, now - 50, now - 10], now), Guard::Refuse(3), "запись из будущего — свежая");
    }

    #[test]
    fn history_missing_is_empty_without_a_problem() {
        let dir = temp("missing");
        assert_eq!(read_history(&dir.join(HISTORY_FILE)), (vec![], None));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn history_roundtrip_and_corrupt_is_empty_with_a_problem() {
        let dir = temp("corrupt");
        let file = dir.join(HISTORY_FILE);
        write_history(&file, &[10, 20]).unwrap();
        assert_eq!(read_history(&file), (vec![10, 20], None));
        std::fs::write(&file, "10\nnot a time\n").unwrap();
        let (times, problem) = read_history(&file);
        assert!(times.is_empty());
        assert!(problem.unwrap().contains("line 2"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn relaunch_records_spawns_and_stops_at_the_limit() {
        let dir = temp("limit");
        let file = dir.join("logs").join(HISTORY_FILE);
        let now = 5_000;
        for (i, pid) in [101, 102, 103].into_iter().enumerate() {
            assert_eq!(relaunch(&file, now + i as u64, || Ok(pid)), (Outcome::Relaunched(pid), None));
        }
        assert_eq!(read_history(&file).0, vec![now, now + 1, now + 2]);
        let mut spawned = false;
        let (outcome, _) = relaunch(&file, now + 3, || {
            spawned = true;
            Ok(104)
        });
        assert_eq!(outcome, Outcome::LimitReached(3));
        assert!(!spawned, "сверх предела новое окно не запускается");
        assert_eq!(relaunch(&file, now + WINDOW_SECS + 1, || Ok(105)).0, Outcome::Relaunched(105), "старые записи выпали");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn relaunch_with_corrupt_history_relaunches_and_says_so() {
        let dir = temp("corrupt-relaunch");
        let file = dir.join(HISTORY_FILE);
        std::fs::write(&file, "garbage").unwrap();
        let (outcome, note) = relaunch(&file, 100, || Ok(9));
        assert_eq!(outcome, Outcome::Relaunched(9));
        assert!(note.unwrap().starts_with("relaunch history unreadable, treated as empty"));
        assert_eq!(read_history(&file), (vec![100], None), "испорченная история заменена");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn relaunch_spawn_error_and_unsaved_history_are_failures() {
        let dir = temp("spawn-error");
        let file = dir.join(HISTORY_FILE);
        assert_eq!(relaunch(&file, 100, || Err("denied".into())).0, Outcome::Failed("denied".into()));
        // История не пишется (на её месте папка) — не перезапускать: предел без истории не работает.
        let blocked = dir.join("blocked");
        std::fs::create_dir_all(&blocked).unwrap();
        let mut spawned = false;
        let (outcome, _) = relaunch(&blocked, 100, || {
            spawned = true;
            Ok(1)
        });
        assert!(matches!(&outcome, Outcome::Failed(e) if e.starts_with("relaunch history not saved")), "{outcome:?}");
        assert!(!spawned);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn outcome_text_for_crash_log() {
        assert_eq!(Outcome::Relaunched(42).to_string(), "relaunched as pid 42");
        assert_eq!(Outcome::Failed("x".into()).to_string(), "relaunch failed: x");
        assert_eq!(Outcome::LimitReached(3).to_string(), "limit reached: 3 relaunches in the last 10 min, not relaunched");
        assert_eq!(Outcome::NotGpu.to_string(), "not relaunched: not a video adapter failure");
    }

    #[test]
    fn after_crash_waits_for_the_old_pid_and_reports_a_timeout() {
        let timeout = Duration::from_secs(10);
        let mut asked = None;
        let after = after_crash(&args(&["--tray", "--after-crash", "77"]), timeout, |pid, t| {
            asked = Some((pid, t));
            Waited::TimedOut
        })
        .unwrap();
        assert_eq!(asked, Some((77, timeout)));
        assert_eq!(after.old_pid, 77);
        assert_eq!(after.wait_warning(), Some(trf("crash.wait_timeout", &["77", "10"])));
        let exited = AfterCrash { waited: Waited::Exited, ..after.clone() };
        assert_eq!(exited.wait_warning(), None);
        let failed = AfterCrash { waited: Waited::Failed("denied".into()), ..after };
        assert_eq!(failed.wait_warning(), Some(trf("crash.wait_failed", &["77", "denied"])));
    }

    #[test]
    fn ordinary_start_does_not_wait() {
        let mut waited = false;
        assert!(after_crash(&args(&["--tray"]), WAIT_PREVIOUS, |_, _| {
            waited = true;
            Waited::Exited
        })
        .is_none());
        assert!(!waited);
    }

    #[test]
    fn notice_names_the_cause_and_the_log() {
        let after = AfterCrash { old_pid: 1, cause: REAL_MESSAGE.into(), waited: Waited::Exited, timeout: WAIT_PREVIOUS };
        let log = Path::new(r"D:\awg\logs\crash.log");
        assert_eq!(after.notice(log), trf("crash.relaunched", &[REAL_MESSAGE, r"D:\awg\logs\crash.log"]));
        let unknown = AfterCrash { cause: String::new(), ..after };
        assert_eq!(unknown.notice(log), trf("crash.relaunched", &[&tr("crash.cause_unknown"), r"D:\awg\logs\crash.log"]));
    }
}
