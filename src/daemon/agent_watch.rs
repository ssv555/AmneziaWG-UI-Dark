//! Сторож агента в ядре: запускает `awg-ui.exe --agent` в объекте задания, следит за процессом, перезапускает его с
//! паузой и снимает зависшего. Поток вторичный: его паника не останавливает ядро (`crash::nonfatal_loop`), туннели
//! от агента не зависят. Ядро говорит с агентом только отсюда, только `Hello` и не держа своих блокировок
//! (`switching`, конфиг): у сторожа одно своё состояние — `AgentCell`.
//!
//! Решения (пауза перед перезапуском, промахи `Hello`) — чистые функции со временем параметром, как в `retry`;
//! процесс — за трейтами `Launcher`/`Child`, проверки идут на подделке.

use std::ops::ControlFlow;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::null;
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, GetExitCodeProcess, ResumeThread, TerminateProcess, WaitForSingleObject, CREATE_NO_WINDOW, CREATE_SUSPENDED,
    PROCESS_INFORMATION, STARTUPINFOW,
};

use super::agent::proto::{AgentRequest, AgentResponse};
use super::pipe::Timeouts;
use super::proto::{AgentExit, AgentStatus};
use crate::crash::lock;
use crate::events::Severity;
use crate::i18n::trf;

/// Первая пауза перед перезапуском; каждая следующая подряд — вдвое дольше, не больше `RESTART_CAP`. Предела числа
/// попыток нет: агент, который падает при каждом запуске (сломанная сборка), перезапускается раз в минуту бессрочно —
/// ядро и VPN при этом работают, а исправить сборку может только владелец.
pub(super) const RESTART_FIRST: Duration = Duration::from_secs(1);
const RESTART_CAP: Duration = Duration::from_secs(60);
/// Проработал столько — следующий выход считается первым (пауза снова 1 с). 5 минут: падение раньше — это цикл
/// сбоев, и паузы между перезапусками растут до минуты; падение реже — отдельные случаи, их лучше поднять сразу.
const STABLE_RUN: Duration = Duration::from_secs(5 * 60);
/// Выходы подряд, о каждом из которых пишется в журнал; дальше — только каждый `LOG_EVERY`-й (при паузе в минуту —
/// раз в час): цикл сбоев не должен забить журнал записями о себе.
const LOG_FIRST: u32 = 5;
const LOG_EVERY: u32 = 60;
/// Как часто сторож спрашивает агента `Hello` и сколько ждёт ответа.
const PROBE_EVERY: Duration = Duration::from_secs(5);
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// Столько промахов `Hello` подряд — агент завис: завершить и запустить заново (снимается за 15-30 с).
const MAX_MISSES: u32 = 3;
/// Срок на первый запуск: пока агент ни разу не ответил на `Hello`, отсутствие канала промахом не считается. Свежий
/// агент открывает канал не сразу: первый запуск только что обновлённого exe задерживает проверка антивирусом (десятки
/// секунд), а без канала `Hello` не ждёт срока, а сразу промах — три промаха (`hung_after`, ~30 с) снимали бы
/// медленный, но здоровый запуск как зависание. 60 с — вдвое больше этого срока: запуск, которому нужно дольше, уже
/// не медленный, а сломанный, и его надо перезапускать.
const START_GRACE: Duration = Duration::from_secs(60);
/// Сколько ждать, пока завершённый процесс действительно выйдет.
const KILL_WAIT: Duration = Duration::from_secs(10);
/// Предел памяти процесса агента. Агент держит в памяти загрузки обновлений (`update::net` читает ответ целиком,
/// с пределом в десятки мегабайт), журнал, статистику и пинг — обычно 5-15 МБ. 512 МиБ — с большим запасом; утечка
/// упирается в предел и становится падением, после которого сторож поднимает агента заново, а не съедает память
/// машины вместе с VPN.
const AGENT_MEMORY_LIMIT: usize = 512 << 20;
/// Код выхода процесса, завершённого сторожем.
const KILLED: u32 = 0xA6E7;

/// Как сторож проверяет агента: `Hello` раз в `every`, ответа ждёт `timeout`; пока агент ни разу не ответил, у него
/// есть `start_grace` на запуск (`START_GRACE`).
#[derive(Clone, Copy)]
pub(super) struct Probe {
    pub(super) every: Duration,
    pub(super) timeout: Duration,
    pub(super) start_grace: Duration,
}

impl Probe {
    const LIVE: Probe = Probe { every: PROBE_EVERY, timeout: PROBE_TIMEOUT, start_grace: START_GRACE };

    /// Через сколько молчания агент считается зависшим.
    fn hung_after(self) -> Duration {
        (self.every + self.timeout).saturating_mul(MAX_MISSES)
    }
}

/// Что запускать агентом и где его спрашивать. У ядра — своя программа с `--agent` и канал агента (`live`); тест
/// изоляции ядра подставляет свой процесс, свой канал и короткие сроки — живые службу и каналы он не трогает.
pub(super) struct AgentSpec {
    /// Хвост командной строки после пути к своей программе.
    pub(super) args: String,
    pub(super) pipe: String,
    pub(super) probe: Probe,
}

impl AgentSpec {
    pub(super) fn live() -> AgentSpec {
        AgentSpec { args: super::agent::AGENT_FLAG.to_string(), pipe: super::agent::PIPE_NAME.to_string(), probe: Probe::LIVE }
    }
}

/// Пауза перед `failures`-м перезапуском подряд (с 1): 1, 2, 4 … 60 с.
fn restart_delay(failures: u32) -> Duration {
    RESTART_FIRST.saturating_mul(1u32 << failures.saturating_sub(1).min(16)).min(RESTART_CAP)
}

/// Решение после выхода агента: когда запускать снова и писать ли об этом в журнал.
#[derive(Debug, PartialEq)]
struct Restart {
    delay: Duration,
    /// Выход по счёту в нынешней серии (с 1).
    failures: u32,
    log: bool,
}

/// Серия выходов агента; сбрасывается после стабильной работы (`STABLE_RUN`).
#[derive(Default)]
struct Restarts {
    failures: u32,
}

impl Restarts {
    /// Агент, запущенный в `started`, вышел (или не запустился) в `now`.
    fn after_exit(&mut self, started: Instant, now: Instant) -> Restart {
        if now.saturating_duration_since(started) >= STABLE_RUN {
            self.failures = 0;
        }
        self.failures = self.failures.saturating_add(1);
        let log = self.failures <= LOG_FIRST || self.failures % LOG_EVERY == 0;
        Restart { delay: restart_delay(self.failures), failures: self.failures, log }
    }
}

/// Вердикт сторожа по очередному `Hello`.
#[derive(Debug, PartialEq)]
enum Verdict {
    Alive,
    /// Агент ещё не отвечал, но срок на запуск не вышел: не промах.
    Starting,
    Missed(u32),
    /// `MAX_MISSES` промахов подряд у агента, который уже отвечал: процесс завершить.
    Hung,
    /// Агент не ответил ни разу за `start_grace` после запуска: процесс завершить.
    NeverStarted,
}

/// Решения сторожа по `Hello` у одного запуска агента; время — параметром.
struct Watchdog {
    started: Instant,
    start_grace: Duration,
    /// Агент хоть раз ответил: с этого момента молчание — промах, а не медленный запуск.
    answered_once: bool,
    misses: u32,
}

impl Watchdog {
    fn new(started: Instant, start_grace: Duration) -> Watchdog {
        Watchdog { started, start_grace, answered_once: false, misses: 0 }
    }

    /// Итог очередного `Hello` в момент `now`.
    fn answer(&mut self, answered: bool, now: Instant) -> Verdict {
        if answered {
            self.answered_once = true;
            self.misses = 0;
            return Verdict::Alive;
        }
        if !self.answered_once {
            return if now.saturating_duration_since(self.started) >= self.start_grace { Verdict::NeverStarted } else { Verdict::Starting };
        }
        self.misses += 1;
        if self.misses >= MAX_MISSES {
            Verdict::Hung
        } else {
            Verdict::Missed(self.misses)
        }
    }
}

/// Запускает процесс агента.
trait Launcher {
    fn launch(&mut self) -> Result<Box<dyn Child>, String>;
}

/// Запущенный агент.
trait Child {
    fn pid(&self) -> u32;
    /// Ждать выхода не дольше `timeout`: `Some(код)` — процесс завершился.
    fn wait(&mut self, timeout: Duration) -> Option<u32>;
    /// Ответил ли агент на `Hello` за `Probe::timeout`.
    fn hello(&mut self) -> bool;
    /// Завершить принудительно; выход потом подтверждает `wait`.
    fn kill(&mut self);
}

/// Состояние агента для окна (`CoreState::agent`). Пишет только сторож, читает `Core::state`; блокировка — только на
/// копирование значения.
#[derive(Default)]
pub struct AgentCell(Mutex<Option<AgentStatus>>);

impl AgentCell {
    pub fn get(&self) -> Option<AgentStatus> {
        lock(&self.0).clone()
    }

    fn set(&self, status: AgentStatus) {
        *lock(&self.0) = Some(status);
    }
}

/// Время и паузы сторожа — параметрами, чтобы цикл проверялся без часов.
struct Env<'a> {
    cell: &'a AgentCell,
    log: &'a dyn Fn(Severity, &str),
    now: &'a dyn Fn() -> Instant,
    unix_now: &'a dyn Fn() -> u64,
    sleep: &'a dyn Fn(Duration),
    probe: Probe,
}

/// Поток сторожа: запуск, надзор и перезапуск агента — бессрочно. Паника шага пишется в журнал, и после паузы шаг
/// идёт снова (`nonfatal_loop`); процесс агента при раскрутке стека завершается (`ProcessChild::drop`), поэтому
/// двух агентов сразу не бывает.
pub(super) fn run(cell: &AgentCell, log: &dyn Fn(Severity, &str), spec: &AgentSpec) {
    let now = Instant::now;
    let unix_now = crate::monitor::unix_now;
    let sleep = std::thread::sleep;
    let env = Env { cell, log, now: &now, unix_now: &unix_now, sleep: &sleep, probe: spec.probe };
    let mut launcher = JobLauncher { job: None, spec };
    let mut restarts = Restarts::default();
    let report = |panic: &str, wait: Duration| log(Severity::Bad, &trf("core.secondary_failed", &[panic, &wait.as_secs().to_string()]));
    // Период ноль: паузы между запусками выдерживает сам `cycle`; `nonfatal_loop` ждёт только после паники.
    crate::crash::nonfatal_loop(Duration::ZERO, &sleep, &report, || {
        cycle(&mut launcher, &mut restarts, &env);
        ControlFlow::Continue(())
    });
}

/// Один запуск агента: запустить, следить до выхода, выждать паузу перед следующим запуском.
fn cycle(launcher: &mut dyn Launcher, restarts: &mut Restarts, env: &Env) {
    env.cell.set(AgentStatus::Starting);
    let started = (env.now)();
    let exit = match launcher.launch() {
        Ok(mut child) => {
            env.cell.set(AgentStatus::Up { pid: child.pid() });
            watch(child.as_mut(), env.probe, started, env.now)
        }
        Err(e) => AgentExit::NotStarted(e),
    };
    let restart = restarts.after_exit(started, (env.now)());
    env.cell.set(AgentStatus::Down { since: (env.unix_now)(), last_exit: exit.clone() });
    if restart.log {
        let text = trf("agent.exited", &[&describe_with(&exit, env.probe), &restart.delay.as_secs().to_string(), &restart.failures.to_string()]);
        (env.log)(Severity::Warn, &text);
    }
    (env.sleep)(restart.delay);
}

/// Следить за агентом до его выхода; завис или не запустился — завершить.
fn watch(child: &mut dyn Child, probe: Probe, started: Instant, now: &dyn Fn() -> Instant) -> AgentExit {
    let mut dog = Watchdog::new(started, probe.start_grace);
    loop {
        if let Some(code) = child.wait(probe.every) {
            return AgentExit::Code(code);
        }
        let exit = match dog.answer(child.hello(), now()) {
            Verdict::Hung => AgentExit::Hung,
            // `AgentExit` уходит окну в `State`, а выпущенное окно не разберёт новый вариант (протокол: варианты
            // не добавляются без нужды). «Не запустился» едет как `NotStarted` с текстом причины.
            Verdict::NeverStarted => AgentExit::NotStarted(trf("agent.start_timeout", &[&probe.start_grace.as_secs().to_string()])),
            Verdict::Alive | Verdict::Starting | Verdict::Missed(_) => continue,
        };
        child.kill();
        // Не вышел и после этого — его держит ядро Windows; `ProcessChild::drop` повторит завершение.
        child.wait(KILL_WAIT);
        return exit;
    }
}

/// Причина выхода — для журнала и `--status`, на языке журнала; сроки зависания — живые.
pub fn describe(exit: &AgentExit) -> String {
    describe_with(exit, Probe::LIVE)
}

/// То же при сроках сторожа `probe`.
pub(super) fn describe_with(exit: &AgentExit, probe: Probe) -> String {
    match exit {
        AgentExit::Code(code) => trf("agent.exit_code", &[&code.to_string()]),
        AgentExit::Hung => trf("agent.exit_hung", &[&probe.hung_after().as_secs().to_string()]),
        AgentExit::NotStarted(e) => trf("agent.exit_not_started", &[e.as_str()]),
    }
}

/// Запуск настоящего процесса: своя программа с `spec.args` (у ядра — `--agent`) в объекте задания ядра. Задание
/// создаётся при первом запуске и живёт, пока жив поток сторожа (то есть ядро): закрытие его описателя при выходе
/// ядра — даже аварийном — завершает агента (`KILL_ON_JOB_CLOSE`), сирот не остаётся. Не создалось — запуск не
/// удался и будет повторён.
struct JobLauncher<'a> {
    job: Option<OwnedHandle>,
    spec: &'a AgentSpec,
}

impl Launcher for JobLauncher<'_> {
    fn launch(&mut self) -> Result<Box<dyn Child>, String> {
        let job = match self.job.take() {
            Some(job) => job,
            None => create_job()?,
        };
        let raw = job.as_raw_handle();
        self.job = Some(job);
        let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
        Ok(Box::new(spawn_in_job(&exe, self.spec, raw)?))
    }
}

fn create_job() -> Result<OwnedHandle, String> {
    unsafe {
        let raw = CreateJobObjectW(null(), null());
        if raw.is_null() {
            return Err(format!("CreateJobObject: {}", std::io::Error::last_os_error()));
        }
        let job = OwnedHandle::from_raw_handle(raw);
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        // BREAKAWAY_OK: самообновление запускает `--restart-core` с CREATE_BREAKAWAY_FROM_JOB — иначе помощник
        // перезапуска погиб бы вместе с ядром, которое он останавливает. DIE_ON_UNHANDLED_EXCEPTION: упавший агент
        // выходит сразу, а не висит в отчёте об ошибке Windows до сторожа.
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            | JOB_OBJECT_LIMIT_BREAKAWAY_OK
            | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION
            | JOB_OBJECT_LIMIT_PROCESS_MEMORY;
        info.ProcessMemoryLimit = AGENT_MEMORY_LIMIT;
        let size = size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32;
        if SetInformationJobObject(raw, JobObjectExtendedLimitInformation, (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(), size) == 0 {
            return Err(format!("SetInformationJobObject: {}", std::io::Error::last_os_error()));
        }
        Ok(job)
    }
}

/// Процесс создаётся приостановленным и входит в задание до первой своей инструкции: между запуском и назначением
/// заданию он не успеет ничего породить вне его.
fn spawn_in_job(exe: &std::path::Path, spec: &AgentSpec, job: std::os::windows::io::RawHandle) -> Result<ProcessChild, String> {
    unsafe {
        let mut cmd: Vec<u16> = crate::win::wide(&format!("\"{}\" {}", exe.display(), spec.args));
        let mut si: STARTUPINFOW = std::mem::zeroed();
        si.cb = size_of::<STARTUPINFOW>() as u32;
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        if CreateProcessW(null(), cmd.as_mut_ptr(), null(), null(), 0, CREATE_SUSPENDED | CREATE_NO_WINDOW, null(), null(), &si, &mut pi) == 0 {
            return Err(format!("CreateProcess {}: {}", exe.display(), std::io::Error::last_os_error()));
        }
        let thread = OwnedHandle::from_raw_handle(pi.hThread);
        // Описатель процесса у `ProcessChild` с этой строки: любая ошибка ниже завершает процесс в его `drop`.
        let child = ProcessChild {
            process: OwnedHandle::from_raw_handle(pi.hProcess),
            pid: pi.dwProcessId,
            exited: false,
            pipe: spec.pipe.clone(),
            probe_timeout: spec.probe.timeout,
        };
        if AssignProcessToJobObject(job, pi.hProcess) == 0 {
            return Err(format!("AssignProcessToJobObject: {}", std::io::Error::last_os_error()));
        }
        if ResumeThread(thread.as_raw_handle()) == u32::MAX {
            return Err(format!("ResumeThread: {}", std::io::Error::last_os_error()));
        }
        Ok(child)
    }
}

struct ProcessChild {
    process: OwnedHandle,
    pid: u32,
    exited: bool,
    /// Канал агента и срок ответа на `Hello`.
    pipe: String,
    probe_timeout: Duration,
}

impl Child for ProcessChild {
    fn pid(&self) -> u32 {
        self.pid
    }

    fn wait(&mut self, timeout: Duration) -> Option<u32> {
        let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
        match unsafe { WaitForSingleObject(self.process.as_raw_handle(), ms) } {
            WAIT_OBJECT_0 => {
                self.exited = true;
                let mut code = u32::MAX;
                if unsafe { GetExitCodeProcess(self.process.as_raw_handle(), &mut code) } == 0 {
                    code = u32::MAX;
                }
                Some(code)
            }
            WAIT_TIMEOUT => None,
            // Описатель не годится для ожидания — следить за процессом нельзя, оставлять его без надзора тоже: он
            // завершается, и сторож запускает нового (код выхода — `u32::MAX`, он попадёт в журнал).
            _ => {
                self.kill();
                self.exited = true;
                Some(u32::MAX)
            }
        }
    }

    fn hello(&mut self) -> bool {
        probe(&self.pipe, self.probe_timeout)
    }

    fn kill(&mut self) {
        // Не завершился — процесс уже вышел или описатель негоден; выход подтвердит или опровергнет `wait`.
        unsafe { TerminateProcess(self.process.as_raw_handle(), KILLED) };
    }
}

impl Drop for ProcessChild {
    fn drop(&mut self) {
        if !self.exited {
            self.kill();
        }
    }
}

/// `Hello` агенту на канале `pipe` со сроком `timeout`. Сам запрос идёт в коротком потоке: подключение к каналу, у
/// которого нет свободного экземпляра, ждёт дольше срока (`pipe::connect`). Поток по сроку не ждётся, он завершится
/// сам — все его шаги ограничены по времени (подключение ≤ 10 с, запись и чтение — `timeout`).
fn probe(pipe: &str, timeout: Duration) -> bool {
    let pipe = pipe.to_string();
    let (tx, rx) = mpsc::channel();
    let spawned = std::thread::Builder::new().name("agent-hello".into()).spawn(move || {
        let timeouts = Timeouts { send: timeout, reply: timeout };
        let answer = super::pipe::call_with::<_, AgentResponse>(&pipe, &AgentRequest::Hello, timeouts);
        // Не отправился — сторож уже не ждёт (срок вышел, промах засчитан): ответ никому не нужен.
        tx.send(matches!(answer, Ok(AgentResponse::Hello { .. }))).unwrap_or(());
    });
    // Поток не создался — проверить агента нечем: промах, как и молчание.
    spawned.is_ok() && rx.recv_timeout(timeout).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::rc::Rc;

    #[test]
    fn backoff_doubles_from_one_second_to_a_minute() {
        let start = Instant::now();
        let mut r = Restarts::default();
        let delays: Vec<u64> = (0..9).map(|_| r.after_exit(start, start + Duration::from_secs(1)).delay.as_secs()).collect();
        assert_eq!(delays, [1, 2, 4, 8, 16, 32, 60, 60, 60]);
    }

    #[test]
    fn a_stable_run_resets_the_backoff() {
        let start = Instant::now();
        let mut r = Restarts::default();
        for _ in 0..4 {
            r.after_exit(start, start);
        }
        // Чуть меньше порога — серия продолжается.
        assert_eq!(r.after_exit(start, start + STABLE_RUN - Duration::from_secs(1)).delay, Duration::from_secs(16));
        let after_stable = r.after_exit(start, start + STABLE_RUN);
        assert_eq!((after_stable.delay, after_stable.failures), (RESTART_FIRST, 1));
    }

    /// Цикл сбоев не прекращает попыток (пауза остаётся минутой) и пишет в журнал редко.
    #[test]
    fn a_crash_loop_never_stops_trying_and_logs_sparsely() {
        let start = Instant::now();
        let mut r = Restarts::default();
        let mut logged = 0;
        for _ in 0..10_000 {
            let restart = r.after_exit(start, start);
            assert!(restart.delay <= RESTART_CAP && restart.delay >= RESTART_FIRST);
            logged += u32::from(restart.log);
        }
        assert_eq!(r.after_exit(start, start).delay, RESTART_CAP);
        assert_eq!(logged, LOG_FIRST + (10_000 / LOG_EVERY));
    }

    #[test]
    fn three_missed_hellos_in_a_row_mean_hung() {
        let start = Instant::now();
        let mut dog = Watchdog::new(start, START_GRACE);
        // Первый ответ закрывает срок на запуск: дальше действует правило промахов, даже внутри `START_GRACE`.
        assert_eq!(dog.answer(true, start), Verdict::Alive);
        assert_eq!(dog.answer(false, start), Verdict::Missed(1));
        assert_eq!(dog.answer(false, start), Verdict::Missed(2));
        // Ответ между промахами обнуляет счёт.
        assert_eq!(dog.answer(true, start), Verdict::Alive);
        assert_eq!(dog.answer(false, start), Verdict::Missed(1));
        assert_eq!(dog.answer(false, start), Verdict::Missed(2));
        assert_eq!(dog.answer(false, start), Verdict::Hung);
    }

    /// Пока агент не ответил, молчание — запуск, а не промах: ни три, ни тридцать `Hello` внутри срока его не снимают.
    #[test]
    fn silence_within_the_start_grace_is_not_a_miss() {
        let start = Instant::now();
        let mut dog = Watchdog::new(start, START_GRACE);
        for secs in (0..60).step_by(5) {
            assert_eq!(dog.answer(false, start + Duration::from_secs(secs)), Verdict::Starting, "at {secs} s");
        }
        // Ответил поздно, но в срок — дальше обычная жизнь, счёт промахов с нуля.
        assert_eq!(dog.answer(true, start + Duration::from_secs(59)), Verdict::Alive);
        assert_eq!(dog.answer(false, start + Duration::from_secs(64)), Verdict::Missed(1));
    }

    #[test]
    fn no_answer_past_the_start_grace_means_never_started() {
        let start = Instant::now();
        let mut dog = Watchdog::new(start, START_GRACE);
        assert_eq!(dog.answer(false, start + START_GRACE - Duration::from_millis(1)), Verdict::Starting);
        assert_eq!(dog.answer(false, start + START_GRACE), Verdict::NeverStarted);
    }

    /// Поддельный агент: выходит с заданным кодом после заданного числа ожиданий; отвечает по сценарию. Каждое
    /// ожидание сдвигает часы сторожа на свой срок — так проходит время между `Hello`.
    struct FakeChild {
        exit_after_waits: Option<(u32, u32)>,
        answers: VecDeque<bool>,
        killed: Rc<Cell<u32>>,
        clock: Rc<Cell<Duration>>,
    }

    impl Child for FakeChild {
        fn pid(&self) -> u32 {
            42
        }
        fn wait(&mut self, timeout: Duration) -> Option<u32> {
            if self.killed.get() > 0 {
                return Some(KILLED);
            }
            self.clock.set(self.clock.get() + timeout);
            match &mut self.exit_after_waits {
                Some((0, code)) => Some(*code),
                Some((n, _)) => {
                    *n -= 1;
                    None
                }
                None => None,
            }
        }
        fn hello(&mut self) -> bool {
            self.answers.pop_front().unwrap_or(false)
        }
        fn kill(&mut self) {
            self.killed.set(self.killed.get() + 1);
        }
    }

    struct FakeLauncher(VecDeque<Result<FakeChild, String>>);

    impl Launcher for FakeLauncher {
        fn launch(&mut self) -> Result<Box<dyn Child>, String> {
            self.0.pop_front().expect("unexpected launch").map(|c| Box::new(c) as Box<dyn Child>)
        }
    }

    /// `clock` — смещение часов сторожа от начала теста; сдвигают его ожидания `FakeChild`.
    fn with_env(f: impl FnOnce(&Env, &RefCell<Vec<Duration>>, &RefCell<Vec<String>>, &Rc<Cell<Duration>>)) {
        let cell = AgentCell::default();
        let slept = RefCell::new(Vec::new());
        let logged = RefCell::new(Vec::new());
        let log = |_: Severity, text: &str| logged.borrow_mut().push(text.to_string());
        let clock = Rc::new(Cell::new(Duration::ZERO));
        let base = Instant::now();
        let now = || base + clock.get();
        let unix = || 1_700_000_000;
        let sleep = |d: Duration| slept.borrow_mut().push(d);
        let env = Env { cell: &cell, log: &log, now: &now, unix_now: &unix, sleep: &sleep, probe: Probe::LIVE };
        f(&env, &slept, &logged, &clock);
    }

    /// Агент молчит — после трёх промахов сторож завершает его; состояние «не работает, завис».
    #[test]
    fn a_silent_agent_is_killed_after_three_misses() {
        with_env(|env, slept, logged, clock| {
            let killed = Rc::new(Cell::new(0));
            let child = FakeChild { exit_after_waits: None, answers: [true, false, false, false].into(), killed: killed.clone(), clock: clock.clone() };
            let mut launcher = FakeLauncher([Ok(child)].into());
            cycle(&mut launcher, &mut Restarts::default(), env);
            assert_eq!(killed.get(), 1);
            assert_eq!(env.cell.get(), Some(AgentStatus::Down { since: 1_700_000_000, last_exit: AgentExit::Hung }));
            assert_eq!(*slept.borrow(), [RESTART_FIRST]);
            assert_eq!(logged.borrow().len(), 1);
        });
    }

    /// Запуск не удался и агент падает — сторож пробует снова с растущей паузой, код выхода попадает в состояние.
    #[test]
    fn failed_launches_and_crashes_are_retried_with_backoff() {
        with_env(|env, slept, _, clock| {
            let killed = Rc::new(Cell::new(0));
            let crash = |code| Ok(FakeChild { exit_after_waits: Some((1, code)), answers: [true].into(), killed: killed.clone(), clock: clock.clone() });
            let mut launcher = FakeLauncher([Err("CreateProcess: denied".to_string()), crash(2), crash(3)].into());
            let mut restarts = Restarts::default();
            cycle(&mut launcher, &mut restarts, env);
            assert!(matches!(env.cell.get(), Some(AgentStatus::Down { last_exit: AgentExit::NotStarted(e), .. }) if e.contains("denied")));
            cycle(&mut launcher, &mut restarts, env);
            cycle(&mut launcher, &mut restarts, env);
            assert!(matches!(env.cell.get(), Some(AgentStatus::Down { last_exit: AgentExit::Code(3), .. })));
            assert_eq!(*slept.borrow(), [1, 2, 4].map(Duration::from_secs));
            assert_eq!(killed.get(), 0, "вышедший сам агент не завершается");
        });
    }

    /// Ответы по сценарию: `silent` промахов подряд, потом `then`.
    fn script(silent: usize, then: &[bool]) -> VecDeque<bool> {
        std::iter::repeat(false).take(silent).chain(then.iter().copied()).collect()
    }

    /// Медленный запуск (антивирус сканирует свежий exe): 8 `Hello` подряд (40 с) без канала, потом агент отвечает и
    /// работает. Раньше три промаха снимали его как зависшего уже на 15 с.
    #[test]
    fn a_slow_start_within_the_grace_is_not_killed() {
        with_env(|env, slept, logged, clock| {
            let killed = Rc::new(Cell::new(0));
            let child = FakeChild { exit_after_waits: Some((12, 0)), answers: script(8, &[true; 4]), killed: killed.clone(), clock: clock.clone() };
            let mut launcher = FakeLauncher([Ok(child)].into());
            cycle(&mut launcher, &mut Restarts::default(), env);
            assert_eq!(killed.get(), 0);
            assert_eq!(env.cell.get(), Some(AgentStatus::Down { since: 1_700_000_000, last_exit: AgentExit::Code(0) }));
            assert_eq!(*slept.borrow(), [RESTART_FIRST]);
            assert!(logged.borrow().iter().all(|t| !t.contains("did not answer")), "{:?}", logged.borrow());
        });
    }

    /// Агент не ответил ни разу за срок на запуск: завершён и записан как «не запустился», а не как «завис».
    #[test]
    fn no_answer_past_the_grace_is_killed_as_not_started() {
        with_env(|env, slept, logged, clock| {
            let killed = Rc::new(Cell::new(0));
            let child = FakeChild { exit_after_waits: None, answers: VecDeque::new(), killed: killed.clone(), clock: clock.clone() };
            let mut launcher = FakeLauncher([Ok(child)].into());
            cycle(&mut launcher, &mut Restarts::default(), env);
            assert_eq!(killed.get(), 1);
            let text = trf("agent.start_timeout", &[&START_GRACE.as_secs().to_string()]);
            assert_eq!(env.cell.get(), Some(AgentStatus::Down { since: 1_700_000_000, last_exit: AgentExit::NotStarted(text.clone()) }));
            assert_ne!(text, trf("agent.exit_hung", &[&Probe::LIVE.hung_after().as_secs().to_string()]));
            assert!(clock.get() >= START_GRACE && clock.get() < START_GRACE + PROBE_EVERY, "killed at {:?}", clock.get());
            assert_eq!(*slept.borrow(), [RESTART_FIRST]);
            assert!(logged.borrow()[0].contains(&text), "{:?}", logged.borrow());
        });
    }

    /// Агент ответил на последней секунде срока и замолчал: правило трёх промахов (зависание), а не «не запустился».
    #[test]
    fn a_hang_after_the_first_answer_is_killed_after_three_misses() {
        with_env(|env, _, _, clock| {
            let killed = Rc::new(Cell::new(0));
            let child = FakeChild { exit_after_waits: None, answers: script(11, &[true]), killed: killed.clone(), clock: clock.clone() };
            let mut launcher = FakeLauncher([Ok(child)].into());
            cycle(&mut launcher, &mut Restarts::default(), env);
            assert_eq!(killed.get(), 1);
            assert_eq!(env.cell.get(), Some(AgentStatus::Down { since: 1_700_000_000, last_exit: AgentExit::Hung }));
            // 12 `Hello` до ответа и ещё три промаха: 15 ожиданий по 5 с.
            assert_eq!(clock.get(), PROBE_EVERY * 15);
        });
    }
}
