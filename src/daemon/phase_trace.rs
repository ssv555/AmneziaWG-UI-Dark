//! Хронометраж пути запроса к ядру — для тестов изоляции (`server::tests::isolation_*`), где редко, без
//! воспроизведения, падала проверка «Switch быстрее секунды». Каждая фаза пути (канал клиента, приём на сервере, поток
//! запроса, блокировки ядра, запись настроек, вызовы движка) ставит отметку `mark`. Отметки собирает только поток,
//! которому тест выдал запись (`Trace::install`), и потоки, которым её передали (`Carry`); упал тест — запись и сведения
//! о машине уходят в `target/test-logs/<тест>-<unix-время>.log`.
//!
//! Только для тестов: без `cfg(test)` `mark` и `Carry` — пустые встраиваемые функции и тип нулевого размера, в exe
//! пользователя ничего из этого не попадает.

pub(crate) use imp::*;

#[cfg(not(test))]
mod imp {
    /// Без тестов передавать нечего.
    #[derive(Clone)]
    pub(crate) struct Carry;

    impl Carry {
        #[inline(always)]
        pub(crate) fn here() -> Carry {
            Carry
        }

        #[inline(always)]
        pub(crate) fn adopt(self) {}
    }

    #[inline(always)]
    pub(crate) fn mark(_phase: &'static str) {}

    pub(crate) struct Span;

    impl Span {
        #[inline(always)]
        pub(crate) fn new(_begin: &'static str, _end: &'static str) -> Span {
            Span
        }
    }
}

#[cfg(test)]
mod imp {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use crate::crash::lock;

    /// Отметок в записи не больше этого: старые вытесняются (тест длится десятки секунд, Switch — раз в 100 мс).
    const CAPACITY: usize = 50_000;

    thread_local! {
        static CURRENT: RefCell<Option<Arc<Trace>>> = const { RefCell::new(None) };
    }

    struct Mark {
        at: Instant,
        thread: String,
        phase: &'static str,
    }

    /// Запись отметок одного ядра под тестом.
    pub(crate) struct Trace {
        started: Instant,
        marks: Mutex<VecDeque<Mark>>,
        /// Вытеснено старых отметок (в журнале — чтобы было видно, что начало записи потеряно).
        dropped: Mutex<usize>,
    }

    impl Trace {
        pub(crate) fn new() -> Arc<Trace> {
            Arc::new(Trace { started: Instant::now(), marks: Mutex::new(VecDeque::new()), dropped: Mutex::new(0) })
        }

        /// Отметки этого потока идут в эту запись.
        pub(crate) fn install(self: &Arc<Self>) {
            CURRENT.with(|c| *c.borrow_mut() = Some(self.clone()));
        }

        fn push(&self, phase: &'static str) {
            let at = Instant::now();
            let current = std::thread::current();
            // Поток теста назван полным путём теста (он уже в заголовке журнала) — в записи просто «test».
            let thread = current.name().map_or_else(|| format!("{:?}", current.id()), |n| if n.contains("::") { "test".into() } else { n.to_string() });
            let mut marks = lock(&self.marks);
            if marks.len() == CAPACITY {
                marks.pop_front();
                *lock(&self.dropped) += 1;
            }
            marks.push_back(Mark { at, thread, phase });
        }

        /// Текст журнала падения: что упало, сведения о машине, фазы упавшего запроса (с `request_from` по `failed_at`)
        /// с временем каждой, затем вся запись.
        pub(crate) fn failure_text(&self, failure: &str, request_from: Instant, failed_at: Instant, facts: &[(String, String)]) -> String {
            let marks = lock(&self.marks);
            // Время берётся до блокировки записи: отметки разных потоков могут встать в неё не по порядку.
            let mut sorted: Vec<&Mark> = marks.iter().collect();
            sorted.sort_by_key(|m| m.at);
            let mut out = format!("Failure: {failure}\n\n[environment]\n");
            for (name, value) in facts {
                out += &format!("{name}: {value}\n");
            }
            out += &format!(
                "\n[failed request] {:.3} ms .. {:.3} ms after the trace start; +delta = time since the previous mark\n",
                ms(request_from.saturating_duration_since(self.started)),
                ms(failed_at.saturating_duration_since(self.started))
            );
            let window: Vec<&Mark> = sorted.iter().copied().filter(|m| m.at >= request_from && m.at <= failed_at).collect();
            let mut previous = request_from;
            let mut largest: Option<(Duration, &str)> = None;
            for m in &window {
                let delta = m.at.saturating_duration_since(previous);
                if largest.is_none_or(|(d, _)| delta > d) {
                    largest = Some((delta, m.phase));
                }
                out += &self.line(m, delta);
                previous = m.at;
            }
            let tail = failed_at.saturating_duration_since(previous);
            out += &format!("after the last mark until the failure: {:.3} ms\n", ms(tail));
            match largest {
                Some((d, phase)) => out += &format!("largest gap: {:.3} ms, before \"{phase}\"\n", ms(d)),
                None => out += "no marks in the failed request (nothing reached a traced phase)\n",
            }
            out += &format!("\n[full trace] {} marks, {} older dropped\n", marks.len(), *lock(&self.dropped));
            let mut previous = self.started;
            for m in &sorted {
                out += &self.line(m, m.at.saturating_duration_since(previous));
                previous = m.at;
            }
            out
        }

        fn line(&self, m: &Mark, delta: Duration) -> String {
            format!("{:>12.3} ms  +{:>10.3} ms  [{:<16}] {}\n", ms(m.at.saturating_duration_since(self.started)), ms(delta), m.thread, m.phase)
        }

        /// Журнал падения теста, в котором стоит эта запись: в `target/test-logs/`. Возвращает строку для сообщения
        /// проверки — путь журнала или почему он не записан.
        pub(crate) fn write_failure(&self, failure: &str, request_from: Instant, extra: &[(&str, String)]) -> String {
            let failed_at = Instant::now();
            let mut facts = environment();
            facts.extend(extra.iter().map(|(k, v)| (k.to_string(), v.clone())));
            let text = self.failure_text(failure, request_from, failed_at, &facts);
            let unix = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
            let dir = log_dir();
            match write_log(&dir, &test_name(), unix, &text) {
                Ok(path) => format!("timing trace: {}", path.display()),
                Err(e) => format!("timing trace not written to {}: {e}", dir.display()),
            }
        }
    }

    fn ms(d: Duration) -> f64 {
        d.as_secs_f64() * 1000.0
    }

    /// Отметка фазы в запись этого потока; у потока без записи — ничего.
    pub(crate) fn mark(phase: &'static str) {
        CURRENT.with(|c| {
            if let Some(trace) = c.borrow().as_ref() {
                trace.push(phase);
            }
        });
    }

    /// Отметка `begin` сейчас и `end` при выходе из области (на любом пути, в том числе раннем `return`).
    pub(crate) struct Span(&'static str);

    impl Span {
        pub(crate) fn new(begin: &'static str, end: &'static str) -> Span {
            mark(begin);
            Span(end)
        }
    }

    impl Drop for Span {
        fn drop(&mut self) {
            mark(self.0);
        }
    }

    /// Запись потока, передаваемая в поток, который он запускает.
    #[derive(Clone)]
    pub(crate) struct Carry(Option<Arc<Trace>>);

    impl Carry {
        pub(crate) fn here() -> Carry {
            Carry(CURRENT.with(|c| c.borrow().clone()))
        }

        pub(crate) fn adopt(self) {
            if let Some(trace) = self.0 {
                trace.install();
            }
        }
    }

    /// `<dir>/<test>-<unix>.log`; пишется через временный файл — оборванная запись не оставляет полфайла.
    fn write_log(dir: &Path, test: &str, unix: u64, text: &str) -> std::io::Result<PathBuf> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("{test}-{unix}.log"));
        let partial = path.with_extension("log.partial");
        std::fs::write(&partial, text)?;
        std::fs::rename(&partial, &path)?;
        Ok(path)
    }

    /// `target/test-logs`: каталог сборки — `CARGO_TARGET_DIR` (его ставит и cargo test), иначе предок exe тестов
    /// (`target/<профиль>/deps/<exe>`).
    fn log_dir() -> PathBuf {
        let target = std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from).or_else(|| {
            let exe = std::env::current_exe().ok()?;
            let found = exe.ancestors().find(|p| p.file_name().is_some_and(|n| n == "target")).map(Path::to_path_buf);
            found.or_else(|| exe.ancestors().nth(3).map(Path::to_path_buf))
        });
        target.unwrap_or_else(|| PathBuf::from("target")).join("test-logs")
    }

    /// Имя теста — имя его потока у libtest (`daemon::server::tests::isolation_…`), последняя часть пути.
    fn test_name() -> String {
        let current = std::thread::current();
        let name = current.name().unwrap_or("unnamed-test");
        name.rsplit("::").next().unwrap_or(name).chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect()
    }

    /// Сведения о машине в момент падения: насколько она занята и свеж ли exe (первый запуск после сборки проверяет
    /// антивирус). Не прочитанное — с причиной, а не пропуском.
    fn environment() -> Vec<(String, String)> {
        let cpus = std::thread::available_parallelism().map_or_else(|e| format!("unknown: {e}"), |n| n.to_string());
        let exe_age = std::env::current_exe()
            .and_then(std::fs::metadata)
            .and_then(|m| m.modified())
            .map_or_else(|e| format!("unknown: {e}"), |t| t.elapsed().map_or_else(|e| format!("unknown: {e}"), |d| format!("{:.1} s", d.as_secs_f64())));
        vec![
            ("logical CPUs".into(), cpus),
            ("threads in this process".into(), win::process_threads()),
            ("system CPU busy, 250 ms sample after the failure".into(), win::cpu_busy(Duration::from_millis(250))),
            ("test process age".into(), win::process_age()),
            ("test binary age (since build)".into(), exe_age),
            ("RUST_TEST_THREADS".into(), std::env::var("RUST_TEST_THREADS").unwrap_or_else(|_| "not set".into())),
            ("command line".into(), std::env::args().collect::<Vec<_>>().join(" ")),
        ]
    }

    mod win {
        use std::time::Duration;
        use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32};
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes, GetSystemTimes};

        fn ticks(t: FILETIME) -> u64 {
            (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)
        }

        fn last_error() -> String {
            format!("unknown: {}", std::io::Error::last_os_error())
        }

        pub(super) fn process_threads() -> String {
            let pid = std::process::id();
            unsafe {
                let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
                if snap == INVALID_HANDLE_VALUE {
                    return last_error();
                }
                let mut entry: THREADENTRY32 = std::mem::zeroed();
                entry.dwSize = size_of::<THREADENTRY32>() as u32;
                let mut count = 0usize;
                let mut ok = Thread32First(snap, &mut entry);
                while ok != 0 {
                    if entry.th32OwnerProcessID == pid {
                        count += 1;
                    }
                    ok = Thread32Next(snap, &mut entry);
                }
                CloseHandle(snap);
                count.to_string()
            }
        }

        /// Доля занятого времени всех процессоров за `sample` (ядро системы + пользователь − простой).
        pub(super) fn cpu_busy(sample: Duration) -> String {
            let times = || unsafe {
                let (mut idle, mut kernel, mut user): (FILETIME, FILETIME, FILETIME) = (std::mem::zeroed(), std::mem::zeroed(), std::mem::zeroed());
                (GetSystemTimes(&mut idle, &mut kernel, &mut user) != 0).then(|| (ticks(idle), ticks(kernel) + ticks(user)))
            };
            let Some((idle0, total0)) = times() else { return last_error() };
            std::thread::sleep(sample);
            let Some((idle1, total1)) = times() else { return last_error() };
            let (idle, total) = (idle1.saturating_sub(idle0), total1.saturating_sub(total0));
            if total == 0 {
                return "unknown: no CPU time passed in the sample".into();
            }
            format!("{:.0} %", 100.0 * total.saturating_sub(idle) as f64 / total as f64)
        }

        pub(super) fn process_age() -> String {
            unsafe {
                let (mut created, mut exited, mut kernel, mut user): (FILETIME, FILETIME, FILETIME, FILETIME) = (std::mem::zeroed(), std::mem::zeroed(), std::mem::zeroed(), std::mem::zeroed());
                if GetProcessTimes(GetCurrentProcess(), &mut created, &mut exited, &mut kernel, &mut user) == 0 {
                    return last_error();
                }
                // FILETIME — сотни наносекунд с 1601 года; до 1970 — 11 644 473 600 с.
                let created_unix = (ticks(created) / 10_000_000).saturating_sub(11_644_473_600);
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
                format!("{} s (CPU: kernel {:.1} s, user {:.1} s)", now.saturating_sub(created_unix), ticks(kernel) as f64 / 1e7, ticks(user) as f64 / 1e7)
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn failure_log_is_named_by_test_and_time_and_lists_the_phases_of_the_failed_request() {
            let trace = Trace::new();
            trace.install();
            mark("before the request");
            std::thread::sleep(Duration::from_millis(2));
            let request_from = Instant::now();
            mark("client: connect");
            let carry = Carry::here();
            std::thread::Builder::new()
                .name("pipe-request".into())
                .spawn(move || {
                    carry.adopt();
                    mark("switch: switching lock held");
                })
                .unwrap()
                .join()
                .unwrap();
            std::thread::spawn(|| mark("untraced thread")).join().unwrap();
            mark("client: reply received");
            let facts = vec![("logical CPUs".to_string(), "4".to_string())];
            let text = trace.failure_text("Switch Connect answered in 1.2s", request_from, Instant::now(), &facts);

            let dir = std::env::temp_dir().join(format!("awg-ui-phase-trace-{}", std::process::id()));
            let path = write_log(&dir, "isolation_x", 1_700_000_000, &text).unwrap();
            assert_eq!(path, dir.join("isolation_x-1700000000.log"));
            let written = std::fs::read_to_string(&path).unwrap();
            std::fs::remove_dir_all(&dir).unwrap();

            assert_eq!(written, text);
            assert!(written.starts_with("Failure: Switch Connect answered in 1.2s\n"), "{written}");
            assert!(written.contains("logical CPUs: 4"), "{written}");
            let failed = written.split("[full trace]").next().unwrap();
            for phase in ["client: connect", "[pipe-request    ] switch: switching lock held", "client: reply received", "largest gap"] {
                assert!(failed.contains(phase), "failed-request section lacks {phase:?}:\n{written}");
            }
            assert!(!failed.contains("before the request"), "marks before the request are only in the full trace:\n{written}");
            assert!(written.contains("before the request"), "{written}");
            assert!(!written.contains("untraced thread"), "a thread without the carried trace records nothing:\n{written}");
        }

        #[test]
        fn trace_keeps_only_the_newest_marks_and_says_how_many_were_dropped() {
            let trace = Trace::new();
            for _ in 0..CAPACITY + 3 {
                trace.push("tick");
            }
            let text = trace.failure_text("x", Instant::now(), Instant::now(), &[]);
            assert!(text.contains(&format!("{CAPACITY} marks, 3 older dropped")), "{}", &text[..300]);
        }

        #[test]
        fn marks_are_listed_in_time_order_even_when_recorded_out_of_order() {
            let trace = Trace::new();
            let from = Instant::now();
            std::thread::sleep(Duration::from_millis(2));
            trace.push("second phase");
            // Поток, взявший время раньше, встал в запись позже.
            lock(&trace.marks).push_back(Mark { at: from + Duration::from_millis(1), thread: "pipe-accept".into(), phase: "first phase" });
            let text = trace.failure_text("x", from, Instant::now(), &[]);
            for section in text.split("[full trace]") {
                let (first, second) = (section.find("first phase").unwrap(), section.find("second phase").unwrap());
                assert!(first < second, "{text}");
            }
        }

        #[test]
        fn environment_facts_are_read_or_say_why_not() {
            let facts = environment();
            for name in ["logical CPUs", "threads in this process", "system CPU busy", "test process age", "test binary age"] {
                let (_, value) = facts.iter().find(|(k, _)| k.starts_with(name)).unwrap_or_else(|| panic!("no {name} in {facts:?}"));
                assert!(!value.is_empty() && !value.starts_with("unknown"), "{name}: {value}");
            }
        }
    }
}
