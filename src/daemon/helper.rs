//! Помощник в сеансе пользователя: действия в родном окне AmneziaWG (UI Automation), которых ядро из сеанса 0
//! сделать не может. Агент (`daemon::agent::tunnels`, SYSTEM) кладёт задание в папку `ops` (только SYSTEM и
//! администраторы), запускает `awg-ui.exe --native-op <задание>` с повышенным токеном пользователя и забирает ответ
//! из той же папки (`Jobs`).
//!
//! В заданиях и ответах бывают конфиги с закрытыми ключами, поэтому в `ops` ничего не задерживается: помощник
//! удаляет задание, как только прочёл; агент после каждого действия убирает всю папку, а при своём запуске — то,
//! что осталось от прежнего агента, погибшего посреди действия (сразу и ещё раз, когда помощник прежнего агента
//! точно завершился: он живёт не дольше `TIMEOUT`, завершая себя сам).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::conf::TunnelInfo;
use crate::events::Severity;
use crate::i18n::{tr, trf};

pub const FLAG: &str = "--native-op";
/// Сколько агент ждёт помощника, прежде чем завершить его; столько же помощник живёт и сам (`main`): помощника,
/// чей агент погиб, завершить больше некому.
const TIMEOUT: Duration = Duration::from_secs(120);
/// Сколько действие ждёт конца предыдущего. Окно ждёт ответа агента `pipe::Timeouts::CORE` (300 с): очередь и сам
/// помощник укладываются в это время, иначе действие выполнилось бы, когда пользователь уже получил ошибку (и, может
/// быть, повторил его).
const QUEUE_WAIT: Duration = Duration::from_secs(60);
/// Вторая уборка после запуска агента: помощник прежнего агента к этому времени уже завершился.
const LATE_SWEEP: Duration = Duration::from_secs(TIMEOUT.as_secs() + 15);
/// Код выхода помощника, который не уложился в `TIMEOUT` и завершил себя сам.
const EXPIRED: i32 = 3;

#[derive(Serialize, Deserialize, Debug)]
pub enum Op {
    Open,
    Edit(String),
    Import(Option<String>),
    Close,
    Delete(String),
    Details(String),
    ReadConfig(String),
    WriteConfig(String, String),
    /// «Export all tunnels to zip» в этот файл.
    Export(String),
}

#[derive(Serialize, Deserialize, Debug)]
pub enum Out {
    Ok,
    Text(String),
    Info(TunnelInfo),
    Err(String),
}

type Log = dyn Fn(Severity, &str) + Send + Sync;
/// Подготовить папку заданий (у настоящей — права `DATA_SDDL`, выставляются каждый раз).
type Prepare = dyn Fn(&Path) -> Result<(), String> + Send + Sync;
/// Запустить помощника на файле задания в сеансе `session` от имени `caller` и дождаться; `Ok` — код выхода.
type Launch = dyn Fn(u32, &str, &Path) -> Result<u32, String> + Send + Sync;

/// Сторона агента: задания помощнику в папке `ops`, по одному (два помощника сразу мешали бы друг другу в одном окне).
pub struct Jobs {
    dir: PathBuf,
    prepare: Box<Prepare>,
    launch: Box<Launch>,
    queue_wait: Duration,
    /// Идёт действие. Пока флаг стоит, файлы в `ops` трогает только оно.
    busy: Mutex<bool>,
    freed: Condvar,
    log: Box<Log>,
}

/// Очередь действия; при выходе из области действие закончено.
struct Turn<'a>(&'a Jobs);

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        *self.0.busy.lock().unwrap_or_else(PoisonError::into_inner) = false;
        self.0.freed.notify_one();
    }
}

impl Jobs {
    fn new(dir: PathBuf, prepare: Box<Prepare>, launch: Box<Launch>, queue_wait: Duration, log: Box<Log>) -> Jobs {
        Jobs { dir, prepare, launch, queue_wait, busy: Mutex::new(false), freed: Condvar::new(), log }
    }

    /// Настоящие: `ProgramData\AmneziaWG UI Dark\ops` и `awg-ui.exe --native-op` в сеансе пользователя
    /// (`session::run_elevated`). Создание ничего не трогает на диске — уборку запускает агент (`sweep_at_start`).
    pub fn real(log: Box<Log>) -> Jobs {
        let exe = crate::engine::install_dir().join(crate::engine::FILES[0]);
        let launch = move |session: u32, caller: &str, input: &Path| {
            super::session::run_elevated(session, caller, &exe, &format!("{FLAG} \"{}\"", input.display()), TIMEOUT)
        };
        let prepare = |dir: &Path| crate::win::protect_dir(dir, super::DATA_SDDL);
        Jobs::new(super::data_dir().join("ops"), Box::new(prepare), Box::new(launch), QUEUE_WAIT, log)
    }

    /// Выполнить `op` в сеансе `session` и вернуть ответ помощника. Предыдущее действие не закончилось за
    /// `queue_wait` — отказ: в очередь за ним не встаём.
    pub fn run(&self, session: u32, caller_sid: &str, op: &Op) -> Result<Out, String> {
        let _turn = self.turn(self.queue_wait).ok_or_else(|| tr("agent.helper_busy"))?;
        (self.prepare)(&self.dir)?;
        let input = self.dir.join(format!("{}.in.json", crate::store::random_hex()));
        let output = out_path(&input);
        let ran = write_job(&input, op).and_then(|()| (self.launch)(session, caller_sid, &input));
        let out = std::fs::read(&output);
        // Своё задание и ответ — и всё, что оставил помощник погибшего агента: под очередью других действий нет.
        self.sweep();
        answer(ran?, out, &output)
    }

    /// При запуске агента: убрать оставшееся от прежнего сразу и ещё раз через `LATE_SWEEP`.
    pub fn sweep_at_start(self: &Arc<Self>) {
        self.sweep_now();
        let jobs = self.clone();
        crate::crash::spawn_named("agent-ops-sweep", move || {
            std::thread::sleep(LATE_SWEEP);
            jobs.sweep_now();
        });
    }

    /// Уборка вне действия; число убранного — в журнал. Идёт действие — уберёт оно само, когда закончится.
    fn sweep_now(&self) {
        let Some(_turn) = self.turn(Duration::ZERO) else { return };
        if let Err(e) = (self.prepare)(&self.dir) {
            return (self.log)(Severity::Bad, &trf("agent.ops_not_removed", &[&e]));
        }
        let removed = self.sweep();
        if removed > 0 {
            (self.log)(Severity::Warn, &trf("agent.ops_swept", &[&removed.to_string()]));
        }
    }

    /// Удалить все файлы в папке заданий; сколько удалено. Только под очередью (`turn`). Неудача — в журнал с путём
    /// (содержимое не пишется: в нём ключи).
    fn sweep(&self) -> usize {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return 0,
            Err(e) => {
                (self.log)(Severity::Warn, &trf("agent.ops_not_removed", &[&crate::fsutil::io_ctx(&self.dir, e)]));
                return 0;
            }
        };
        let mut removed = 0;
        for entry in entries {
            let path = match entry {
                Ok(entry) => entry.path(),
                Err(e) => {
                    (self.log)(Severity::Warn, &trf("agent.ops_not_removed", &[&crate::fsutil::io_ctx(&self.dir, e)]));
                    continue;
                }
            };
            match std::fs::remove_file(&path) {
                Ok(()) => removed += 1,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => (self.log)(Severity::Warn, &trf("agent.ops_not_removed", &[&crate::fsutil::io_ctx(&path, e)])),
            }
        }
        removed
    }

    /// Дождаться очереди не дольше `wait`; `None` — действие так и не закончилось.
    fn turn(&self, wait: Duration) -> Option<Turn<'_>> {
        // Флаг меняется одной записью: отравленный замок не ломает его смысла.
        let busy = self.busy.lock().unwrap_or_else(PoisonError::into_inner);
        let (mut busy, _) = self.freed.wait_timeout_while(busy, wait, |busy| *busy).unwrap_or_else(PoisonError::into_inner);
        if *busy {
            return None;
        }
        *busy = true;
        Some(Turn(self))
    }
}

fn write_job(input: &Path, op: &Op) -> Result<(), String> {
    let bytes = serde_json::to_vec(op).map_err(|e| format!("helper: {e}"))?;
    std::fs::write(input, bytes).map_err(|e| crate::fsutil::io_ctx(input, e))
}

/// Ответ помощника по коду выхода и файлу ответа. Не 0 — ответа нет: причина — код, а не «файл не найден».
fn answer(code: u32, out: std::io::Result<Vec<u8>>, output: &Path) -> Result<Out, String> {
    match code {
        0 => {}
        c if c == EXPIRED as u32 => return Err(tr("core.helper_timeout")),
        c => return Err(trf("core.helper_exit", &[&c.to_string()])),
    }
    let out = out.map_err(|e| crate::fsutil::io_ctx(output, e))?;
    serde_json::from_slice(&out).map_err(|e| format!("helper: {e}"))
}

fn out_path(input: &Path) -> PathBuf {
    let name = input.file_name().map(|n| n.to_string_lossy().replace(".in.json", ".out.json")).unwrap_or_default();
    input.with_file_name(name)
}

/// Сторона помощника: прочитать задание, выполнить в родном окне, записать ответ. Код выхода 0 — ответ записан.
/// Дольше `TIMEOUT` помощник не живёт: агент, который его ждал, мог погибнуть.
pub fn main(input: &Path) -> i32 {
    crate::crash::spawn_named("helper-expiry", || {
        std::thread::sleep(TIMEOUT);
        std::process::exit(EXPIRED);
    });
    serve(input, &|op| execute(&crate::backend::native_exe(), op))
}

fn serve(input: &Path, execute: &dyn Fn(Op) -> Out) -> i32 {
    let job = std::fs::read(input);
    // Задание (в нём бывает конфиг с ключами) лежит не дольше, чем нужно, чтобы его прочесть. Не удалилось — его
    // уберёт агент после действия или при своём запуске (`Jobs::sweep`); своего журнала у помощника нет.
    std::fs::remove_file(input).ok();
    let out = match job.map_err(|e| e.to_string()).and_then(|b| serde_json::from_slice::<Op>(&b).map_err(|e| e.to_string())) {
        Ok(op) => execute(op),
        Err(e) => Out::Err(crate::fsutil::io_ctx(input, e)),
    };
    match serde_json::to_vec(&out).map(|b| std::fs::write(out_path(input), b)) {
        Ok(Ok(())) => 0,
        _ => 1,
    }
}

fn execute(exe: &Path, op: Op) -> Out {
    use crate::native;
    let done = |r: Result<(), String>| r.map_or_else(Out::Err, |()| Out::Ok);
    match op {
        Op::Open => done(std::process::Command::new(exe).spawn().map(drop).map_err(|e| crate::fsutil::io_ctx(exe, e))),
        Op::Edit(t) => done(native::edit(exe, &t)),
        Op::Import(file) => done(native::import(exe, file.as_deref().map(Path::new))),
        Op::Close => {
            crate::win::close_session_processes("amneziawg.exe");
            Out::Ok
        }
        Op::Delete(t) => done(native::delete(exe, &t)),
        Op::Details(t) => native::read_details(exe, &t).map_or_else(Out::Err, Out::Info),
        Op::ReadConfig(t) => native::read_config(exe, &t).map_or_else(Out::Err, Out::Text),
        Op::WriteConfig(t, text) => done(native::write_config(exe, &t, &text)),
        Op::Export(zip) => done(native::export_all(exe, Path::new(&zip)).map(drop)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    const SECRET: &str = "[Interface]\nPrivateKey = cGxhY2Vob2xkZXIga2V5IG5vdCByZWFs\n";

    fn dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("awg-ops-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn files(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    }

    type Logged = Arc<Mutex<Vec<(Severity, String)>>>;

    fn jobs(dir: &Path, queue_wait: Duration, launch: impl Fn(u32, &str, &Path) -> Result<u32, String> + Send + Sync + 'static) -> (Arc<Jobs>, Logged) {
        let logged: Logged = Arc::default();
        let sink = logged.clone();
        let prepare = |dir: &Path| std::fs::create_dir_all(dir).map_err(|e| e.to_string());
        let log = move |severity: Severity, text: &str| sink.lock().unwrap().push((severity, text.to_string()));
        (Arc::new(Jobs::new(dir.to_path_buf(), Box::new(prepare), Box::new(launch), queue_wait, Box::new(log))), logged)
    }

    /// Помощник «как настоящий» — через `serve`, ответ задаёт тест.
    fn helper_answering(out: fn(Op) -> Out) -> impl Fn(u32, &str, &Path) -> Result<u32, String> + Send + Sync {
        move |_, _, input| Ok(serve(input, &|op| out(op)) as u32)
    }

    /// Агент погиб посреди `ReadConfig`: задание и ответ с ключом остались. После следующего действия папка пуста.
    #[test]
    fn job_files_and_leftovers_are_removed_after_each_job() {
        let dir = dir("after");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("dead.in.json"), SECRET).unwrap();
        std::fs::write(dir.join("dead.out.json"), SECRET).unwrap();
        let (jobs, logged) = jobs(&dir, QUEUE_WAIT, helper_answering(|op| match op {
            Op::ReadConfig(_) => Out::Text(SECRET.into()),
            other => Out::Err(format!("{other:?}")),
        }));
        let out = jobs.run(1, "S-1-5-21-1", &Op::ReadConfig("office".into())).unwrap();
        assert!(matches!(out, Out::Text(t) if t == SECRET));
        assert!(files(&dir).is_empty(), "{:?}", files(&dir));
        assert!(logged.lock().unwrap().is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Помощник удаляет задание, как только прочёл: ключи не лежат всё время его работы.
    #[test]
    fn helper_removes_its_job_as_soon_as_it_is_read() {
        let dir = dir("helper");
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("a.in.json");
        write_job(&input, &Op::WriteConfig("office".into(), SECRET.into())).unwrap();
        let seen = Mutex::new(Vec::new());
        let code = serve(&input, &|op| {
            seen.lock().unwrap().push(input.exists());
            assert!(matches!(op, Op::WriteConfig(_, ref text) if text == SECRET));
            Out::Ok
        });
        assert_eq!((code, seen.into_inner().unwrap()), (0, vec![false]));
        assert_eq!(files(&dir), ["a.out.json"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Запуск не удался — задание всё равно убрано.
    #[test]
    fn failed_launch_still_removes_the_job() {
        let dir = dir("failed");
        let (jobs, _) = jobs(&dir, QUEUE_WAIT, |_, _, _| Err("CreateProcessAsUser: Access is denied.".into()));
        let r = jobs.run(1, "S-1-5-21-1", &Op::WriteConfig("office".into(), SECRET.into()));
        assert_eq!(r.unwrap_err(), "CreateProcessAsUser: Access is denied.");
        assert!(files(&dir).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Помощник завершился без ответа (нет прав на `ops`, паника): ошибка называет код выхода, а не «файл не найден».
    #[test]
    fn exit_without_an_answer_names_the_exit_code() {
        let dir = dir("code");
        let (jobs, _) = jobs(&dir, QUEUE_WAIT, |_, _, _| Ok(1));
        let r = jobs.run(1, "S-1-5-21-1", &Op::Open).unwrap_err();
        assert_eq!(r, trf("core.helper_exit", &["1"]));
        let (expired, _) = jobs_expired(&dir);
        assert_eq!(expired.run(1, "S-1-5-21-1", &Op::Open).unwrap_err(), tr("core.helper_timeout"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn jobs_expired(dir: &Path) -> (Arc<Jobs>, Logged) {
        jobs(dir, QUEUE_WAIT, |_, _, _| Ok(EXPIRED as u32))
    }

    /// Запуск агента: всё, что осталось от прежнего, убрано, число — в журнал.
    #[test]
    fn start_sweep_empties_the_folder_and_logs_the_count() {
        let dir = dir("start");
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["x.in.json", "x.out.json", "y.out.json"] {
            std::fs::write(dir.join(name), SECRET).unwrap();
        }
        let (jobs, logged) = jobs(&dir, QUEUE_WAIT, |_, _, _| panic!("no job at start"));
        jobs.sweep_now();
        assert!(files(&dir).is_empty());
        assert_eq!(*logged.lock().unwrap(), [(Severity::Warn, trf("agent.ops_swept", &["3"]))]);
        jobs.sweep_now();
        assert_eq!(logged.lock().unwrap().len(), 1, "пустая папка — без записи");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Уборка вне действия не трогает файлы идущего действия.
    #[test]
    fn sweep_waits_for_no_running_job() {
        let dir = dir("busy-sweep");
        let (entered_tx, entered) = mpsc::channel();
        let (release, release_rx) = mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        let (jobs, _) = jobs(&dir, QUEUE_WAIT, move |_, _, input| {
            entered_tx.send(()).unwrap();
            release_rx.lock().unwrap().recv().unwrap();
            assert!(input.exists(), "задание идущего действия удалено уборкой");
            Ok(1)
        });
        let running = jobs.clone();
        let job = std::thread::spawn(move || running.run(1, "S-1-5-21-1", &Op::Open));
        entered.recv().unwrap();
        jobs.sweep_now();
        release.send(()).unwrap();
        assert!(job.join().unwrap().is_err());
        assert!(files(&dir).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Долгое действие идёт, окно шлёт ещё одно: оно не ждёт дольше `queue_wait` и не выполняется позже, когда
    /// окно уже показало ошибку. Освободилось вовремя — следующее выполняется.
    #[test]
    fn an_action_does_not_queue_behind_a_running_one_past_the_deadline() {
        let dir = dir("queue");
        let (entered_tx, entered) = mpsc::channel();
        let (release, release_rx) = mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        let launched = Arc::new(Mutex::new(Vec::new()));
        let log = launched.clone();
        let (jobs, _) = jobs(&dir, Duration::from_secs(1), move |session, _, input| {
            log.lock().unwrap().push(session);
            if session == 1 {
                entered_tx.send(()).unwrap();
                release_rx.lock().unwrap().recv().unwrap();
            }
            Ok(serve(input, &|_| Out::Ok) as u32)
        });
        let running = jobs.clone();
        let first = std::thread::spawn(move || running.run(1, "S-1-5-21-1", &Op::Details("a".into())));
        entered.recv().unwrap();
        assert_eq!(jobs.run(2, "S-1-5-21-1", &Op::Delete("b".into())).unwrap_err(), tr("agent.helper_busy"));
        let queued = jobs.clone();
        let third = std::thread::spawn(move || queued.run(3, "S-1-5-21-1", &Op::Delete("b".into())));
        std::thread::sleep(Duration::from_millis(50));
        release.send(()).unwrap();
        assert!(matches!(first.join().unwrap(), Ok(Out::Ok)));
        assert!(matches!(third.join().unwrap(), Ok(Out::Ok)), "освободилось в срок — выполняется");
        assert_eq!(*launched.lock().unwrap(), [1, 3], "отказанное действие не запускалось");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Очередь и сам помощник укладываются в ожидание окна с запасом: позже него действие не выполняется.
    #[test]
    fn queue_and_helper_fit_in_the_window_wait() {
        let window_waits = crate::daemon::pipe::Timeouts::CORE.reply;
        assert!(QUEUE_WAIT + TIMEOUT + Duration::from_secs(30) <= window_waits, "{window_waits:?}");
        assert!(LATE_SWEEP > TIMEOUT);
    }
}
