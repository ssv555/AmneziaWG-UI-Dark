//! Паники: причина — в журнал, процесс не остаётся полуживым.
//!
//! Ядро. Запрос окна выполняется изолированно (`isolate`): паника в нём становится ответом с ошибкой, место
//! соединения и пометки «занят» возвращают охранники, блокировки берутся через `lock` и переживают отравление.
//! Вторичные потоки (пинг, статистика, ежедневная проверка обновлений) идут в `nonfatal_loop`: паника шага —
//! запись в журнал и пауза, ядро продолжает держать туннели. Паника потока VPN (опрос, надзор, сеть, приём канала)
//! — ядро без этого потока выглядело бы живым, но показывало бы застывшее состояние или не отвечало: причина
//! пишется в журнал событий, `core_failure()` видит главный цикл, служба останавливается с кодом сбоя, и диспетчер
//! служб её перезапускает.
//!
//! Окно. Паника любого потока — запись в `crash.log`, сообщение пользователю и выход: окно без потока опроса
//! показывало бы застывшие данные, а при `windows_subsystem = "windows"` stderr никто не видит.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::ops::ControlFlow;
use std::panic::PanicHookInfo;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

/// Первая паника фонового потока ядра — по ней главный цикл останавливает службу.
static CORE_FAILURE: OnceLock<String> = OnceLock::new();
/// Окно уже сообщает о сбое: остальные паникующие потоки ждут, пока первый завершит процесс.
static WINDOW_REPORTING: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// Поток выполняет запрос внутри `isolate`: его паника не роняет ядро.
    static ISOLATED: Cell<bool> = const { Cell::new(false) };
    /// Описание последней паники изолированного потока (с местом в коде) — для `isolate`.
    static LAST: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Текст паники: поток, место в коде, сообщение.
fn describe(thread: Option<&str>, location: Option<String>, payload: &dyn Any) -> String {
    let message = payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("non-string panic payload");
    let thread = thread.unwrap_or("unnamed");
    match location {
        Some(at) => format!("panic in thread '{thread}' at {at}: {message}"),
        None => format!("panic in thread '{thread}': {message}"),
    }
}

fn describe_hook(info: &PanicHookInfo) -> String {
    let location = info.location().map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()));
    describe(std::thread::current().name(), location, info.payload())
}

/// Блокировка долгоживущего общего состояния, которая переживает панику другого потока. Годится только для
/// состояния, которое остаётся согласованным между отдельными изменениями (флаги, словари, снимки): паника в
/// середине изменения такого значения не ломает его инвариантов. Сама паника уже записана хуком; здесь —
/// одна запись о том, что блокировка восстановлена, и отравление снимается, чтобы не писать её снова.
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| {
        note_recovered(std::any::type_name::<T>());
        m.clear_poison();
        poisoned.into_inner()
    })
}

fn note_recovered(what: &str) {
    if let Some(log) = CORE_LOG.get() {
        core_event(log, &format!("lock recovered after a panic: {what}"));
    }
}

/// Журнал событий ядра — его задаёт `install_core`.
static CORE_LOG: OnceLock<PathBuf> = OnceLock::new();

/// Только дописывает строку (`events::append_event`): живой журнал ядра у `Shared`, второй `EventLog` из хука
/// паники или из `lock` мог бы перекрутить его ротацию и разбирал бы файл под чужими блокировками.
fn core_event(log: &Path, text: &str) {
    let event = crate::events::Event::new(crate::monitor::unix_now(), "", crate::events::Severity::Bad, text, false);
    if let Err(e) = crate::events::append_event(log, &event) {
        eprintln!("crash: cannot write {}: {e}", log.display());
    }
}

/// Поток с именем: имя попадает в запись о панике.
pub fn spawn_named(name: &str, f: impl FnOnce() + Send + 'static) {
    if let Err(e) = std::thread::Builder::new().name(name.into()).spawn(f) {
        // Без потока работа не выполняется — это сбой, а не тихий пропуск (как и у `std::thread::spawn`).
        panic!("spawn thread {name}: {e}");
    }
}

/// Хук ядра. `log` — журнал событий ядра.
pub fn install_core(log: PathBuf) {
    // `set` отказывает, только если хук уже стоит: прежний путь журнала остаётся в силе.
    let _ = CORE_LOG.set(log);
    std::panic::set_hook(Box::new(|info| {
        let text = describe_hook(info);
        if ISOLATED.with(Cell::get) {
            // Запишет и вернёт окну `isolate` — уже с тем, что ядро продолжает работу.
            LAST.with(|l| *l.borrow_mut() = Some(text));
            return;
        }
        if let Some(log) = CORE_LOG.get() {
            core_event(log, &text);
        }
        // Важна первая причина: следующие паники — обычно её следствие (отравленные блокировки).
        let _ = CORE_FAILURE.set(text);
    }));
}

/// Фоновый поток ядра упал — ядро должно остановиться с кодом сбоя.
pub fn core_failure() -> Option<&'static str> {
    CORE_FAILURE.get().map(String::as_str)
}

/// Выполнить `f` так, что его паника не роняет процесс: `Err` с описанием паники. Охранники внутри `f`
/// (места соединений, пометки «занят») срабатывают при раскрутке стека.
pub fn isolate<R>(f: impl FnOnce() -> R) -> Result<R, String> {
    let was = ISOLATED.with(|i| i.replace(true));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    ISOLATED.with(|i| i.set(was));
    result.map_err(|payload| {
        // Хук не установлен (тесты) или поставлен чужой — описание из самой паники, без места в коде.
        LAST.with(|l| l.borrow_mut().take()).unwrap_or_else(|| describe(std::thread::current().name(), None, payload.as_ref()))
    })
}

/// Пауза после паники шага вторичного потока: первая — `period`, но не меньше 10 с (паника каждые полсекунды
/// забила бы журнал), каждая следующая подряд — вдвое дольше, не больше 10 минут. Так детерминированная паника
/// пишет в журнал не чаще нескольких раз в час, а случайная (редкий ответ сети) не выключает поток надолго.
pub fn backoff(period: Duration, failures: u32) -> Duration {
    const FIRST: Duration = Duration::from_secs(10);
    const CAP: Duration = Duration::from_secs(600);
    let doubled = period.max(FIRST).saturating_mul(1u32 << failures.saturating_sub(1).min(16));
    doubled.min(CAP)
}

/// Цикл вторичного потока ядра (пинг, статистика, ежедневная проверка обновлений): туннели от него не зависят, и
/// его паника не должна останавливать ядро — иначе детерминированная паника перезапускала бы службу каждые
/// несколько секунд, и надзор за туннелями не успевал бы работать. Каждый шаг `body` выполняется под `isolate`:
/// паника уходит в `report` (журнал) с паузой до следующего шага (`backoff`), и цикл идёт дальше. Между удачными
/// шагами — `period`. `sleep` и остановка через `ControlFlow::Break` — для проверок; в ядре цикл не кончается.
pub fn nonfatal_loop(
    period: Duration,
    sleep: &dyn Fn(Duration),
    report: &dyn Fn(&str, Duration),
    mut body: impl FnMut() -> ControlFlow<()>,
) {
    let mut failures = 0;
    let mut wait = period;
    loop {
        sleep(wait);
        match isolate(&mut body) {
            Ok(ControlFlow::Break(())) => return,
            Ok(ControlFlow::Continue(())) => {
                failures = 0;
                wait = period;
            }
            Err(panic) => {
                failures += 1;
                wait = backoff(period, failures);
                report(&panic, wait);
            }
        }
    }
}

/// Для проверок: идёт ли код сейчас под `isolate` (его паника тогда не станет `core_failure`).
#[cfg(test)]
pub(crate) fn isolated_now() -> bool {
    ISOLATED.with(Cell::get)
}

/// Каталог журналов окна: `log_dir` из настроек, относительный — от папки программы. Здесь `crash.log` и журнал
/// ошибок действий окна.
pub fn window_log_dir(base: &Path, log_dir: &str) -> PathBuf {
    base.join(log_dir)
}

/// Хук окна: `log_dir` — каталог журналов окна (`crash.log` в нём).
pub fn install_window(log_dir: PathBuf) {
    std::panic::set_hook(Box::new(move |info| {
        let text = describe_hook(info);
        report_window_failure(&log_dir, &text, "crash.window");
    }));
}

/// Окно не может продолжать: причина — в `crash.log` и пользователю в сообщении, процесс завершается с кодом 1.
/// `key` — текст сообщения (`{0}` — причина, `{1}` — где подробности).
pub fn report_window_failure(log_dir: &Path, text: &str, key: &str) -> ! {
    if WINDOW_REPORTING.swap(true, Ordering::SeqCst) {
        // О сбое уже сообщает другой поток и сам завершит процесс; этот (часто — UI-поток, упавший на отравленной
        // блокировке) ждёт, чтобы окно не закрылось раньше, чем пользователь прочтёт сообщение.
        loop {
            std::thread::park();
        }
    }
    let file = log_dir.join("crash.log");
    let saved = match append_crash(&file, text) {
        Ok(()) => file.display().to_string(),
        Err(e) => crate::fsutil::io_ctx(&file, e),
    };
    show_error(&crate::i18n::trf(key, &[text, &saved]));
    std::process::exit(1);
}

fn append_crash(file: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(file)?;
    let now = crate::fmt::date_time_sec(crate::monitor::unix_now());
    writeln!(f, "{now} v{} {text}", env!("CARGO_PKG_VERSION"))
}

/// Сообщение об ошибке. Показывается из отдельного потока: модальный цикл сообщения в UI-потоке раздавал бы
/// сообщения окну, которое как раз упало, и его обработчики могли бы паниковать снова.
fn show_error(text: &str) {
    let text: Vec<u16> = crate::win::wide(&text);
    // В потоке только вызов MessageBoxW — паниковать там нечему, так что ожидание `join` конечно: оно длится,
    // пока пользователь не закроет сообщение.
    let shown = std::thread::Builder::new().name("crash-message".into()).spawn({
        let text = text.clone();
        move || message_box(&text)
    });
    match shown {
        Ok(handle) => {
            if handle.join().is_err() {
                message_box(&text);
            }
        }
        // Поток не создался — показать здесь: так лучше, чем никак.
        Err(_) => message_box(&text),
    }
}

fn message_box(text: &[u16]) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK, MB_SETFOREGROUND, MB_TOPMOST};
    let title: Vec<u16> = crate::win::wide(crate::APP_TITLE);
    unsafe { MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), MB_OK | MB_ICONERROR | MB_SETFOREGROUND | MB_TOPMOST) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_log_dir_is_relative_to_the_program_unless_absolute() {
        assert_eq!(window_log_dir(Path::new("app"), "logs"), Path::new("app").join("logs"));
        assert_eq!(window_log_dir(Path::new("app"), r"D:ogs"), Path::new(r"D:ogs"));
    }

    #[test]
    fn panic_text_has_thread_place_and_message() {
        let p: Box<dyn Any + Send> = Box::new("boom");
        assert_eq!(describe(Some("monitor"), Some("src/x.rs:1:2".into()), p.as_ref()), "panic in thread 'monitor' at src/x.rs:1:2: boom");
        let p: Box<dyn Any + Send> = Box::new(format!("index {}", 3));
        assert_eq!(describe(None, None, p.as_ref()), "panic in thread 'unnamed': index 3");
        let p: Box<dyn Any + Send> = Box::new(42u8);
        assert!(describe(None, None, p.as_ref()).ends_with("non-string panic payload"));
    }

    #[test]
    fn isolate_turns_panic_into_error_and_resets_the_flag() {
        assert_eq!(isolate(|| 7), Ok(7));
        let r: Result<(), String> = isolate(|| panic!("bad request"));
        assert!(r.unwrap_err().contains("bad request"));
        assert!(!ISOLATED.with(Cell::get), "поток после запроса снова не изолирован");
    }

    #[test]
    fn backoff_doubles_from_at_least_ten_seconds_up_to_ten_minutes() {
        let s = Duration::from_secs;
        assert_eq!(backoff(Duration::from_millis(500), 1), s(10), "короткий период — не чаще раза в 10 с");
        assert_eq!(backoff(s(30), 1), s(30));
        assert_eq!(backoff(s(30), 2), s(60));
        assert_eq!(backoff(s(30), 3), s(120));
        assert_eq!(backoff(s(30), 6), s(600));
        assert_eq!(backoff(s(30), u32::MAX), s(600), "без переполнения");
    }

    #[test]
    fn nonfatal_loop_reports_a_panic_and_ticks_again() {
        let slept = RefCell::new(Vec::new());
        let reports = RefCell::new(Vec::new());
        let mut ticks = 0;
        let period = Duration::from_secs(15);
        nonfatal_loop(period, &|d| slept.borrow_mut().push(d), &|p, wait| reports.borrow_mut().push((p.to_string(), wait)), || {
            ticks += 1;
            match ticks {
                1 | 2 => {
                    assert!(isolated_now(), "паника шага не станет сбоем ядра");
                    panic!("bad stats {ticks}")
                }
                3 => ControlFlow::Continue(()),
                _ => ControlFlow::Break(()),
            }
        });
        assert_eq!(ticks, 4, "после паник цикл идёт дальше");
        assert!(core_failure().is_none());
        let reports = reports.into_inner();
        assert_eq!(reports.len(), 2);
        assert!(reports[0].0.contains("bad stats 1"));
        let s = Duration::from_secs;
        assert_eq!(slept.into_inner(), vec![period, s(15), s(30), period], "пауза растёт, удачный шаг её сбрасывает");
        assert!(!ISOLATED.with(Cell::get));
    }

    #[test]
    fn lock_survives_poisoning() {
        let m = std::sync::Arc::new(Mutex::new(vec![1]));
        let m2 = m.clone();
        let _ = std::thread::spawn(move || {
            let mut g = m2.lock().unwrap();
            g.push(2);
            panic!("poison it");
        })
        .join();
        assert!(m.is_poisoned());
        assert_eq!(*lock(&m), vec![1, 2]);
        assert!(!m.is_poisoned(), "отравление снято — запись о восстановлении одна");
    }
}
