//! Процесс `--restart-core`: перезапуск ядра новой сборкой и автоматический возврат прежней, если оно не поднялось.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::fileset::{aside_name, keep_name};
use super::fs::{Fs, RealFs};
use super::{app_version, InstallTarget, APP_SET, MANIFEST, MANIFEST_SIG, RESTART_FLAG};
use crate::i18n::{tr, trf};
use crate::scm::{stop_and_wait, Handle, Service, ServiceControl, State, Status};
use windows_sys::Win32::System::Services::SERVICE_ALL_ACCESS;

/// Попыток запуска службы после замены: первая и ещё три через `START_PAUSE`.
const START_ATTEMPTS: u32 = 4;
const START_PAUSE: Duration = Duration::from_secs(5);
/// Ожидание SERVICE_RUNNING: ядро сообщает его, только когда действительно готово.
const READY_TIMEOUT: Duration = Duration::from_secs(30);
const STOP_TIMEOUT: Duration = Duration::from_secs(30);

/// Процесс `--restart-core <заменённый набор>`: только от SYSTEM; через 2 с остановить службу ядра (ждать до
/// 30 с) и запустить снова, дождавшись готовности (с повторами). Новое ядро так и не поднялось — вернуть весь
/// набор, заменённый обновлением (`roll_back_set`), и запустить прежнюю сборку; исход — в журнал ядра и в
/// историю обновлений. Код: 0 — новое ядро работает; 2 — не SYSTEM; 3, 4 — нет доступа к службе; 5 — служба
/// не остановилась; 6 — возвращена прежняя сборка и работает; 7 — возвращать нечего; 8 — вернуть удалось не
/// всё (ядро не запускается: набор смешанный); 9 — прежняя возвращена, но не запустилась; 10 — имя канала ядра
/// так и не освободилось (заняла другая программа): новая сборка остаётся, ядро стоит. При 7–10 ядро стоит — следует
/// ещё один запуск уже под действиями диспетчера при сбое (`hand_over`): поднялось — код 0 или 6.
pub fn restart_core() -> i32 {
    use crate::events::Severity;
    if crate::win::current_user_sid().as_deref() != Ok(crate::win::LOCAL_SYSTEM_SID) {
        return 2;
    }
    let set = restart_set(std::env::args().skip(1)).unwrap_or_else(|bad| {
        core_log(Severity::Warn, &trf("updo.restart_bad_arg", &[&bad]));
        Vec::new()
    });
    std::thread::sleep(Duration::from_secs(2));
    let scm = match Handle::scm_connect() {
        Ok(scm) => scm,
        Err(e) => {
            core_log(Severity::Bad, &e);
            return 3;
        }
    };
    let svc = match Service::open(&scm, crate::daemon::SERVICE, SERVICE_ALL_ACCESS) {
        Ok(svc) => svc,
        Err(e) => {
            core_log(Severity::Bad, &e);
            return 4;
        }
    };
    restart_with_fallback(&InstallTarget::current(), &set, &mut CoreService(svc), &RealFs)
}

/// Исход запуска службы ядра.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Start {
    Running,
    /// Не поднялось.
    Failed,
    /// Не поднялось, потому что имя канала ядра заняла другая программа (`service::PIPE_TAKEN`): сборка не виновата.
    PipeTaken,
}

/// Служба ядра для процесса перезапуска; в тестах — подмена.
trait CoreHost {
    /// Остановить и дождаться остановки (уже стоит — да).
    fn stop(&mut self) -> bool;
    /// Запустить и дождаться готовности, с повторами; не вышло — служба остановлена.
    fn start(&mut self) -> Start;
    /// Действия диспетчера служб при сбое ядра (перезапуск через 5 с): `false` — снять, `true` — поставить обратно.
    fn failure_actions(&mut self, on: bool) -> Result<(), String>;
    /// Запись в журнал событий ядра.
    fn log(&mut self, severity: crate::events::Severity, text: &str);
}

/// Настоящая служба ядра: `CoreHost` поверх `ServiceControl` (запуск/остановка и ожидание — в нём, здесь только политика
/// перезапуска: сроки и повторы).
struct CoreService(Service);

impl CoreHost for CoreService {
    fn stop(&mut self) -> bool {
        match stop_service(&self.0) {
            Ok(()) => true,
            Err(e) => {
                core_log(crate::events::Severity::Warn, &e);
                false
            }
        }
    }

    fn start(&mut self) -> Start {
        start_with_retries(START_ATTEMPTS, START_PAUSE, || {
            start_service(&self.0, &mut |e| core_log(crate::events::Severity::Warn, &trf("updo.start_refused", &[e])))
        })
    }

    fn failure_actions(&mut self, on: bool) -> Result<(), String> {
        if on {
            crate::daemon::install::set_core_failure_actions(&self.0)
        } else {
            self.0.clear_failure_actions()
        }
    }

    fn log(&mut self, severity: crate::events::Severity, text: &str) {
        core_log(severity, text)
    }
}

/// Шаги `restart_core` после открытия службы: `target` — папка программы и хранилище обновлений (история), `set` —
/// заменённый набор, `fs` — файловые операции возврата. На время работы действия диспетчера при сбое снимаются:
/// иначе после первого неудачного старта новой сборки диспетчер сам запускал бы её через 5 с — параллельно нашим
/// повторам, посреди возврата файлов (старт на полупереименованном наборе) или прежнюю сборку до записи истории
/// (запущенное ядро затёрло бы запись своей копией). На каждом выходе действия ставятся обратно; ядро при старте
/// ставит их и само (`install::reapply_failure_actions`) — на случай снятого посреди работы помощника.
fn restart_with_fallback(target: &InstallTarget, set: &[Replaced], core: &mut dyn CoreHost, fs: &dyn Fs) -> i32 {
    use crate::events::Severity;
    // Не снялись — работа продолжается: гонка с диспетчером хуже обновления без неё, но не хуже отказа от обновления.
    if let Err(e) = core.failure_actions(false) {
        core.log(Severity::Warn, &trf("updo.failure_actions", &[&tr("updo.failure_actions_off"), &e]));
    }
    let code = restart_steps(target, set, core, fs);
    if let Err(e) = core.failure_actions(true) {
        core.log(Severity::Bad, &trf("updo.failure_actions", &[&tr("updo.failure_actions_on"), &e]));
    }
    hand_over(code, core)
}

/// Ядро стоит после всех шагов (коды 7–10: возвращать нечего или вернулось не всё, прежняя не поднялась, имя канала
/// занято): ещё один запуск уже с действиями при сбое. Все неудачные старты до этого шли со снятыми действиями, и без
/// него диспетчер не перезапускал бы службу до перезагрузки — VPN лежал бы и при проходящей причине (антивирус держит
/// только что переименованный exe, занятый диспетчер); с ним неудача попадает под перезапуск диспетчера каждые 5 с,
/// как обещает `updo.restart_pipe_gave_up`. Код: поднялась прежняя (9) — 6; поднялась новая (7, 10) — 0; смешанный набор
/// (8) остаётся 8 — ядро работает, но набор не тот, что в истории и журнале.
fn hand_over(code: i32, core: &mut dyn CoreHost) -> i32 {
    if !(7..=10).contains(&code) {
        return code;
    }
    core.log(crate::events::Severity::Warn, &tr("updo.restart_handover"));
    if core.start() != Start::Running {
        return code;
    }
    match code {
        9 => 6,
        8 => 8,
        _ => 0,
    }
}

/// Остановка, запуск новой сборки и возврат прежней, если она не поднялась (см. `restart_with_fallback`). Журнал и
/// история пишутся, только пока ядро стоит: запущенное ядро читает их при старте и затёрло бы запись своей копией.
fn restart_steps(target: &InstallTarget, set: &[Replaced], core: &mut dyn CoreHost, fs: &dyn Fs) -> i32 {
    use crate::events::Severity;
    let (dir, store) = (target.dir.as_path(), target.store.as_path());
    if !core.stop() {
        return 5;
    }
    match start_new(core) {
        Start::Running => return 0,
        Start::PipeTaken => return 10,
        Start::Failed => {}
    }
    let version = app_version();
    let failed = trf("updo.restart_not_started", &[&version]);
    // Неудачный запуск уже остановил службу; не остановилась — возврат всё равно лучше, чем стоять на новой.
    if !core.stop() {
        core.log(Severity::Warn, &tr("updo.restart_stop_failed"));
    }
    match roll_back_set(dir, set, fs) {
        Fallback::Nothing => {
            core.log(Severity::Bad, &tr("updo.restart_no_way_back"));
            record(core, store, &version, &failed, None);
            7
        }
        Fallback::Partial(e) => {
            core.log(Severity::Bad, &trf("updo.restart_failed", &[&version]));
            core.log(Severity::Bad, &trf("updo.restart_back_failed", &[&e]));
            record(core, store, &version, &failed, Some(Err(e)));
            8
        }
        Fallback::Restored => {
            core.log(Severity::Bad, &trf("updo.restart_failed", &[&version]));
            core.log(Severity::Warn, &tr("updo.restart_back"));
            let id = record(core, store, &version, &failed, Some(Ok(())));
            if core.start() == Start::Running {
                return 6;
            }
            let text = tr("updo.restart_back_not_started");
            core.log(Severity::Bad, &text);
            if let Some(Err(e)) = id.map(|id| crate::update::manager::amend_stored(store, id, &text)) {
                core.log(Severity::Bad, &e);
            }
            9
        }
    }
}

/// Кругов запуска (`CoreHost::start`, с повторами), пока имя канала занято другой программой: около 10 минут.
const TAKEN_ROUNDS: u32 = 8;

/// Запуск новой сборки. Имя канала ядра занято другой программой — сборка не виновата, и прежняя не
/// возвращается: иначе любая программа учётной записи владельца, заняв имя на время перезапуска, откатывала бы
/// обновление без UAC. Ждём, пока имя освободится (`TAKEN_ROUNDS` кругов); не освободилось — ядро стоит на новой
/// сборке, его поднимет диспетчер служб (действия при сбое) или перезагрузка.
fn start_new(core: &mut dyn CoreHost) -> Start {
    use crate::events::Severity;
    let mut outcome = core.start();
    if outcome == Start::PipeTaken {
        core.log(Severity::Warn, &tr("updo.restart_pipe_taken"));
    }
    for _ in 1..TAKEN_ROUNDS {
        if outcome != Start::PipeTaken {
            break;
        }
        outcome = core.start();
    }
    if outcome == Start::PipeTaken {
        core.log(Severity::Bad, &tr("updo.restart_pipe_gave_up"));
    }
    outcome
}

/// Записать автоматический возврат в историю (`manager::record_fallback`); не вышло — ошибка в журнал.
fn record(core: &mut dyn CoreHost, store: &Path, version: &str, failed: &str, back: Option<Result<(), String>>) -> Option<u64> {
    crate::update::manager::record_fallback(store, version, failed, back).unwrap_or_else(|e| {
        core.log(crate::events::Severity::Bad, &e);
        None
    })
}

/// Файл набора, заменённый обновлением: имя в папке программы и отодвинутый прежний (`<имя>.old-<random>`);
/// `None` — до обновления файла не было, при возврате новый отодвигается.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Replaced {
    pub(super) name: &'static str,
    pub(super) aside: Option<String>,
}

/// Заменённый набор из аргументов после `--restart-core`: по одному на файл — имя отодвинутого прежнего или голое
/// имя файла, которого до обновления не было (`Installed::restart_args`). Ядро прошлых версий передаёт только
/// прежний exe — это набор из одного файла. Чужой или повторный аргумент — `Err` с ним, набор отвергается
/// целиком: вернув часть, получили бы смешанную сборку.
pub(super) fn restart_set(mut args: impl Iterator<Item = String>) -> Result<Vec<Replaced>, String> {
    if !args.any(|a| a == RESTART_FLAG) {
        return Ok(Vec::new());
    }
    let mut set: Vec<Replaced> = Vec::new();
    for arg in args {
        match replaced_arg(&arg) {
            Some(r) if !set.iter().any(|s| s.name == r.name) => set.push(r),
            _ => return Err(arg),
        }
    }
    Ok(set)
}

/// `<файл набора>.old-<буквы и цифры>` или голое имя файла набора; путь, чужой файл, пустой суффикс — `None`.
fn replaced_arg(arg: &str) -> Option<Replaced> {
    APP_SET.into_iter().find_map(|name| {
        if arg == name {
            return Some(Replaced { name, aside: None });
        }
        let suffix = arg.strip_prefix(name)?.strip_prefix(".old-")?;
        (!suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_alphanumeric())).then(|| Replaced { name, aside: Some(arg.to_string()) })
    })
}

/// Исход возврата набора.
#[derive(Debug, PartialEq)]
enum Fallback {
    /// Ни одного отодвинутого прежнего файла нет (нечего возвращать или уже возвращено) — ничего не тронуто.
    Nothing,
    /// Весь набор на месте.
    Restored,
    /// Возвращено не всё: ошибки с путями; не вернувшиеся прежние файлы сохранены как `.keep-`.
    Partial(String),
}

/// Вернуть в `dir` набор `set`, заменённый обновлением (ядро стоит); `fs` — файловые операции.
/// Порядок — чтобы смешанный набор не стал доверенным ни на одном шаге, в том числе при обрыве посередине: exe
/// доверяет DLL по своим вшитым суммам или по локальному манифесту, а манифест ручается за свои DLL перед
/// любым exe. Поэтому сначала отодвигается новый манифест с подписью (не вышло — DLL и exe не трогаются), затем
/// по файлу возвращаются DLL и exe, и только если всё это удалось — прежний манифест. Файл возвращается
/// переименованиями: новый уходит в `.old-` (его удалит ядро при старте), прежний — на место; не встал — новый
/// возвращается, файл не пропадает. Прежние файлы, оставшиеся не на месте, — `.keep-` (ядро их не удаляет).
/// Повторный запуск с тем же набором ничего не трогает: отодвинутых уже нет.
fn roll_back_set(dir: &Path, set: &[Replaced], fs: &dyn Fs) -> Fallback {
    let aside = |r: &Replaced| r.aside.as_ref().map(|a| dir.join(a));
    if !set.iter().filter_map(aside).any(|p| regular_file(&p)) {
        return Fallback::Nothing;
    }
    let mut errors = Vec::new();
    for r in set {
        if let Some(a) = aside(r).filter(|a| !regular_file(a)) {
            errors.push(trf("updo.rollback_missing", &[&dir.join(r.name).display().to_string(), &a.display().to_string()]));
        }
    }
    let (anchors, body): (Vec<&Replaced>, Vec<&Replaced>) = set.iter().partition(|r| [MANIFEST, MANIFEST_SIG].contains(&r.name));
    let mut anchors_off = true;
    for &r in &anchors {
        if let Err(e) = retire(&dir.join(r.name), fs) {
            errors.push(e);
            anchors_off = false;
        }
    }
    if anchors_off {
        for &r in &body {
            let path = dir.join(r.name);
            let result = match aside(r) {
                Some(a) if regular_file(&a) => swap_back(&path, &a, fs),
                Some(_) => Ok(()), // отодвинутого нет — уже в ошибках, новый остаётся на месте
                None => retire(&path, fs).map(drop),
            };
            errors.extend(result.err());
        }
    }
    if errors.is_empty() {
        for &r in &anchors {
            if let Some(a) = aside(r) {
                let path = dir.join(r.name);
                errors.extend(fs.rename(&a, &path).err().map(|e| crate::fsutil::io_ctx_move(&a, &path, e)));
            }
        }
    }
    for r in set {
        if let Some(a) = aside(r).filter(|a| regular_file(a)) {
            errors.push(keep_aside(&dir.join(r.name), &a, fs));
        }
    }
    if errors.is_empty() {
        Fallback::Restored
    } else {
        Fallback::Partial(errors.join("; "))
    }
}

/// Обычный файл (не папка и не ссылка).
fn regular_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file())
}

/// Отодвинуть `path` в `.old-` (нет его — ничего); имя отодвинутого.
fn retire(path: &Path, fs: &dyn Fs) -> Result<Option<PathBuf>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let to = aside_name(path);
    fs.rename(path, &to).map_err(|e| crate::fsutil::io_ctx(&path, e))?;
    Ok(Some(to))
}

/// Поставить прежний `aside` на место `path`, отодвинув текущий; прежний не встал — текущий возвращается.
fn swap_back(path: &Path, aside: &Path, fs: &dyn Fs) -> Result<(), String> {
    let current = retire(path, fs)?;
    let Err(e) = fs.rename(aside, path) else { return Ok(()) };
    let e = crate::fsutil::io_ctx_move(&aside, &path, e);
    match current.map(|c| fs.rename(&c, path).map_err(|e2| format!("{} → {}: {e2}", c.display(), path.display()))) {
        Some(Err(e2)) => Err(format!("{e}; {e2}")),
        _ => Err(e),
    }
}

/// Прежний файл `aside`, не вставший на место `path`, — в `.keep-` (ядро его не удалит); текст для журнала.
fn keep_aside(path: &Path, aside: &Path, fs: &dyn Fs) -> String {
    let (shown, keep) = (path.display().to_string(), keep_name(path));
    match fs.rename(aside, &keep) {
        Ok(()) => trf("updo.rollback_kept_file", &[&shown, &keep.display().to_string()]),
        Err(e) => trf("updo.rollback_keep_failed", &[&shown, &aside.display().to_string(), &e.to_string()]),
    }
}

/// Запускать `start` до первого успеха, не больше `attempts` раз, с паузой `pause` между попытками; исход —
/// последней попытки.
fn start_with_retries(attempts: u32, pause: Duration, mut start: impl FnMut() -> Start) -> Start {
    let mut outcome = Start::Failed;
    for i in 0..attempts {
        if i > 0 {
            std::thread::sleep(pause);
        }
        outcome = start();
        if outcome == Start::Running {
            break;
        }
    }
    outcome
}

/// Запустить службу и дождаться SERVICE_RUNNING; не дождались — остановить, чтобы следующая попытка шла с нуля.
fn start_service(svc: &dyn ServiceControl, refused: &mut dyn FnMut(&str)) -> Start {
    let outcome = match svc.start() {
        Ok(()) => start_outcome(svc.wait_state(State::Running, READY_TIMEOUT)),
        Err(e) => {
            // Исход `Failed` не несёт причины, а служба отказала сама (нет прав, помечена на удаление) — причина в журнал.
            refused(&e);
            Start::Failed
        }
    };
    if outcome != Start::Running {
        // Остановка здесь — уборка перед следующей попыткой; если служба не остановилась, это скажет `CoreHost::stop`
        // (его зовёт `restart_with_fallback` до возврата прежней сборки), а исход запуска уже `Failed`/`PipeTaken`.
        let _ = stop_service(svc);
    }
    outcome
}

/// Исход запуска по ожиданию `Running`: служба встала с кодом `PIPE_TAKEN` — имя канала занято, иначе — не поднялась.
fn start_outcome(waited: Result<(), Status>) -> Start {
    use windows_sys::Win32::Foundation::ERROR_SERVICE_SPECIFIC_ERROR;
    match waited {
        Ok(()) => Start::Running,
        Err(st)
            if st.state == State::Stopped
                && st.win32_exit == ERROR_SERVICE_SPECIFIC_ERROR
                && st.specific_exit == crate::daemon::service::PIPE_TAKEN =>
        {
            Start::PipeTaken
        }
        Err(_) => Start::Failed,
    }
}

/// Остановить службу и дождаться остановки (уже стоит — сразу да).
fn stop_service(svc: &dyn ServiceControl) -> Result<(), String> {
    stop_and_wait(svc, STOP_TIMEOUT)
}

/// Запись в журнал событий ядра (`<данные ядра>\logs\events.log`).
fn core_log(severity: crate::events::Severity, text: &str) {
    let file = crate::daemon::events_file();
    let event = crate::events::Event::new(crate::monitor::unix_now(), "", severity, text, false);
    if let Err(e) = crate::events::append_event(&file, &event) {
        eprintln!("update: cannot write {}: {e}", file.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::ours::APP_EXE;
    use crate::update::ours::testutil::*;

    /// Обновление всего набора в `inst` (прежний — `old`), как его делает `update_app`; аргументы перезапуска.
    fn updated_set(inst: &Path, src: &Path, old: &[(&str, &str)]) -> Vec<Replaced> {
        write_all(inst, old);
        let new: Vec<(&str, String)> = APP_SET.iter().map(|n| (*n, format!("new {n}"))).collect();
        write_all(src, &new.iter().map(|(n, d)| (*n, d.as_str())).collect::<Vec<_>>());
        let files: Vec<(String, PathBuf)> = APP_SET.iter().map(|n| (n.to_string(), src.join(n))).collect();
        let mut args = Vec::new();
        target(inst).install_app_with(&RealFs, &files, &[], || Ok(()), |_, set| {
            args = set.to_vec();
            Ok(())
        })
        .unwrap();
        restart_set([RESTART_FLAG.to_string()].into_iter().chain(args)).unwrap()
    }

    const OLD_SET: [(&str, &str); 5] = [(APP_EXE, "exe1"), ("tunnel.dll", "t1"), ("wintun.dll", "w1"), (MANIFEST, "m1"), (MANIFEST_SIG, "s1")];

    /// Переименование с записью шагов (`куда`).
    fn recording(log: &std::cell::RefCell<Vec<String>>) -> RenameFs<impl Fn(&Path, &Path) -> std::io::Result<()> + '_> {
        RenameFs::new(move |from: &Path, to: &Path| {
            log.borrow_mut().push(to.file_name().unwrap().to_string_lossy().into_owned());
            std::fs::rename(from, to)
        })
    }

    #[test]
    fn fallback_restores_whole_set_and_is_idempotent() {
        let dir = temp("fb-full");
        let (inst, src) = (dir.join("inst"), dir.join("src"));
        let set = updated_set(&inst, &src, &OLD_SET);
        assert_eq!(set.len(), APP_SET.len());
        let log = std::cell::RefCell::new(Vec::new());
        assert_eq!(roll_back_set(&inst, &set, &recording(&log)), Fallback::Restored);
        for (n, d) in OLD_SET {
            assert_eq!(read(&inst, n), d, "{n}");
        }
        // Новые ушли в `.old-` (их удалит ядро), `.keep-` нет.
        let all = names(&inst);
        assert_eq!(all.iter().filter(|n| n.contains(".old-")).count(), APP_SET.len(), "{all:?}");
        assert!(!all.iter().any(|n| n.contains(".keep-")), "{all:?}");
        // Порядок: новый манифест с подписью отодвигается первым, прежний встаёт последним — ни на одном шаге
        // манифест не ручается за DLL при чужом exe.
        let log = log.into_inner();
        let pos = |name: &str| log.iter().position(|n| n == name).unwrap_or_else(|| panic!("{name}: {log:?}"));
        let last_body = [APP_EXE, "tunnel.dll", "wintun.dll"].iter().map(|n| pos(n)).max().unwrap();
        let first_body = log.iter().position(|n| n.starts_with(APP_EXE) || n.contains(".dll")).unwrap();
        assert!(log[..2].iter().all(|n| n.starts_with(MANIFEST) && n.contains(".old-")), "{log:?}");
        assert!(first_body >= 2 && pos(MANIFEST) > last_body && pos(MANIFEST_SIG) > last_body, "{log:?}");
        // Второй запуск с теми же аргументами ничего не трогает.
        let before = names(&inst);
        assert_eq!(roll_back_set(&inst, &set, &RealFs), Fallback::Nothing);
        assert_eq!(names(&inst), before);
        for (n, d) in OLD_SET {
            assert_eq!(read(&inst, n), d, "{n}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn fallback_with_missing_file_restores_others_and_reports() {
        let dir = temp("fb-missing");
        let (inst, src) = (dir.join("inst"), dir.join("src"));
        let set = updated_set(&inst, &src, &OLD_SET);
        let lost = set.iter().find(|r| r.name == "wintun.dll").unwrap().aside.clone().unwrap();
        std::fs::remove_file(inst.join(&lost)).unwrap();
        let Fallback::Partial(e) = roll_back_set(&inst, &set, &RealFs) else { panic!("partial expected") };
        assert!(e.contains(&lost) && e.contains("wintun.dll"), "{e}");
        // exe и вторая DLL вернулись; на месте пропавшей — новая (файл не пропадает).
        assert_eq!(read(&inst, APP_EXE), "exe1");
        assert_eq!(read(&inst, "tunnel.dll"), "t1");
        assert_eq!(read(&inst, "wintun.dll"), "new wintun.dll");
        // Набор не целый — прежний манифест не ставится (он ручался бы за смешанные DLL), он сохранён как `.keep-`;
        // нового манифеста на месте тоже нет.
        assert!(!inst.join(MANIFEST).exists() && !inst.join(MANIFEST_SIG).exists());
        let all = names(&inst);
        for (n, d) in [(MANIFEST, "m1"), (MANIFEST_SIG, "s1")] {
            let keep = all.iter().find(|k| k.starts_with(&format!("{n}.keep-"))).unwrap_or_else(|| panic!("{n}: {all:?}"));
            assert_eq!(read(&inst, keep), d);
            assert!(e.contains(keep.as_str()), "{e}");
        }
        // Повторный запуск ничего не трогает.
        assert_eq!(roll_back_set(&inst, &set, &RealFs), Fallback::Nothing);
        assert_eq!(names(&inst), all);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn fallback_keeps_new_file_when_old_cannot_be_put_back() {
        let dir = temp("fb-busy");
        let (inst, src) = (dir.join("inst"), dir.join("src"));
        let set = updated_set(&inst, &src, &OLD_SET);
        let exe_aside = set.iter().find(|r| r.name == APP_EXE).unwrap().aside.clone().unwrap();
        let mv = |from: &Path, to: &Path| {
            if from.file_name().is_some_and(|n| n.to_string_lossy() == exe_aside) && to.file_name().is_some_and(|n| n == APP_EXE) {
                Err(std::io::Error::other("busy"))
            } else {
                std::fs::rename(from, to)
            }
        };
        let Fallback::Partial(e) = roll_back_set(&inst, &set, &RenameFs::new(mv)) else { panic!("partial expected") };
        assert!(e.contains("busy"), "{e}");
        assert_eq!(read(&inst, APP_EXE), "new awg-ui.exe", "новый exe вернулся на место");
        let all = names(&inst);
        let keep = all.iter().find(|n| n.starts_with("awg-ui.exe.keep-")).unwrap_or_else(|| panic!("{all:?}"));
        assert_eq!(read(&inst, keep), "exe1");
        assert!(e.contains(keep.as_str()), "{e}");
        assert!(!all.iter().any(|n| n.starts_with("awg-ui.exe.old-")), "отодвинутый прежний не остаётся под `.old-`: {all:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Подменённая служба: исходы запусков по очереди, журнал — в память, вызовы (`stop`, `start`, `off`, `on`) — по порядку.
    struct FakeCore {
        starts: Vec<Start>,
        stop_ok: bool,
        /// Смена действий при сбое отказывает.
        actions_fail: bool,
        calls: Vec<&'static str>,
        log: Vec<(crate::events::Severity, String)>,
    }

    impl CoreHost for FakeCore {
        fn stop(&mut self) -> bool {
            self.calls.push("stop");
            self.stop_ok
        }
        fn start(&mut self) -> Start {
            self.calls.push("start");
            self.starts.remove(0)
        }
        fn failure_actions(&mut self, on: bool) -> Result<(), String> {
            self.calls.push(if on { "on" } else { "off" });
            if self.actions_fail {
                Err("denied".into())
            } else {
                Ok(())
            }
        }
        fn log(&mut self, severity: crate::events::Severity, text: &str) {
            self.log.push((severity, text.to_string()));
        }
    }

    fn fake(starts: &[bool]) -> FakeCore {
        fake_with(&starts.iter().map(|&ok| if ok { Start::Running } else { Start::Failed }).collect::<Vec<_>>())
    }

    fn fake_with(starts: &[Start]) -> FakeCore {
        FakeCore { starts: starts.to_vec(), stop_ok: true, actions_fail: false, calls: Vec::new(), log: Vec::new() }
    }

    /// Действия при сбое сняты первым делом и возвращены последним — на каждом исходе: ни один запуск или шаг возврата
    /// не идёт при включённом перезапуске диспетчера; отказ снять — предупреждение и работа дальше, отказ вернуть — ошибка.
    #[test]
    fn failure_actions_are_paused_for_the_whole_restart_on_every_exit() {
        use crate::events::Severity;
        let dir = temp("fb-actions");
        let (inst, src, store) = (dir.join("inst"), dir.join("src"), dir.join("store"));
        let set = updated_set(&inst, &src, &OLD_SET);
        store_with_update(&store);
        let paused = |core: &FakeCore, code: i32| {
            assert_eq!(core.calls.first(), Some(&"off"), "code {code}: {:?}", core.calls);
            let on = core.calls.iter().position(|c| *c == "on").unwrap_or_else(|| panic!("code {code}: {:?}", core.calls));
            assert!(core.calls[1..on].iter().all(|c| *c == "stop" || *c == "start"), "code {code}: {:?}", core.calls);
            assert!(core.calls[on + 1..].iter().all(|c| *c == "start"), "code {code}: после возврата действий — только передача диспетчеру: {:?}", core.calls);
        };
        // 0: новое ядро поднялось (файлы не тронуты — набор остаётся новым для следующих случаев).
        let mut core = fake(&[true]);
        assert_eq!(restart_with_fallback(&target_with_store(&inst, &store), &set, &mut core, &RealFs), 0);
        paused(&core, 0);
        assert_eq!(core.calls, ["off", "stop", "start", "on"]);
        // 5: служба не остановилась.
        let mut core = FakeCore { stop_ok: false, ..fake(&[]) };
        assert_eq!(restart_with_fallback(&target_with_store(&inst, &store), &set, &mut core, &RealFs), 5);
        assert_eq!(core.calls, ["off", "stop", "on"]);
        // 9: возвращена, не поднялась; 7: повтор — возвращать нечего. Ядро стоит — после возврата действий ещё один
        // запуск, чтобы неудача попала под перезапуск диспетчера (все предыдущие шли со снятыми действиями).
        let mut core = fake(&[false, false, false]);
        assert_eq!(restart_with_fallback(&target_with_store(&inst, &store), &set, &mut core, &RealFs), 9);
        paused(&core, 9);
        assert_eq!(core.calls.iter().rev().take(2).collect::<Vec<_>>(), [&"start", &"on"], "{:?}", core.calls);
        assert_eq!(core.log.last(), Some(&(Severity::Warn, tr("updo.restart_handover"))));
        let mut core = fake(&[false, false]);
        assert_eq!(restart_with_fallback(&target_with_store(&inst, &store), &set, &mut core, &RealFs), 7);
        paused(&core, 7);
        assert_eq!(core.calls.iter().rev().take(2).collect::<Vec<_>>(), [&"start", &"on"], "{:?}", core.calls);
        assert!(core.starts.is_empty(), "запуск после возврата действий сделан");
        // Снять не удалось — предупреждение, перезапуск всё равно идёт; вернуть не удалось — ошибка в журнале.
        let mut core = FakeCore { actions_fail: true, ..fake(&[true]) };
        assert_eq!(restart_with_fallback(&target_with_store(&inst, &store), &set, &mut core, &RealFs), 0);
        assert_eq!(core.calls, ["off", "stop", "start", "on"]);
        let texts: Vec<(Severity, bool)> = core.log.iter().map(|(s, t)| (*s, t.contains("denied"))).collect();
        assert_eq!(texts, [(Severity::Warn, true), (Severity::Bad, true)], "{:?}", core.log);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// История с одной строкой обновления программы 0.0.1 → текущая версия.
    fn store_with_update(store: &Path) {
        std::fs::create_dir_all(store).unwrap();
        let e = crate::update::HistoryEntry {
            id: 4,
            at: 1,
            component: crate::update::Component::App,
            action: crate::update::Action::Update,
            from: Some("0.0.1".into()),
            to: Some(app_version()),
            backup: Some("3-app-0.0.1".into()),
            backup_size: 1,
            ok: true,
            error: None,
            prior_backup: None,
        };
        std::fs::write(store.join("history.json"), serde_json::to_vec(&vec![e]).unwrap()).unwrap();
    }

    fn history(store: &Path) -> Vec<crate::update::HistoryEntry> {
        serde_json::from_slice(&std::fs::read(store.join("history.json")).unwrap()).unwrap()
    }

    #[test]
    fn restart_falls_back_and_records_history() {
        use crate::update::Action;
        let dir = temp("fb-flow");
        let (inst, src, store) = (dir.join("inst"), dir.join("src"), dir.join("store"));
        // Новое ядро поднялось — ничего не трогается.
        let set = updated_set(&inst, &src, &OLD_SET);
        store_with_update(&store);
        let mut core = fake(&[true]);
        assert_eq!(restart_with_fallback(&target_with_store(&inst, &store), &set, &mut core, &RealFs), 0);
        assert_eq!(read(&inst, APP_EXE), "new awg-ui.exe");
        assert!(core.log.is_empty() && history(&store)[0].ok);
        // Не поднялось — набор возвращён, прежнее запущено; история: обновление — ошибка, сверху «Возврат».
        let mut core = fake(&[false, true]);
        assert_eq!(restart_with_fallback(&target_with_store(&inst, &store), &set, &mut core, &RealFs), 6);
        for (n, d) in OLD_SET {
            assert_eq!(read(&inst, n), d, "{n}");
        }
        let h = history(&store);
        assert_eq!(h.len(), 2);
        assert_eq!((h[0].id, h[0].action, h[0].ok), (5, Action::Restore, true));
        assert_eq!((h[0].from.clone(), h[0].to.as_deref()), (Some(app_version()), Some("0.0.1")));
        assert_eq!((h[1].ok, h[1].error.clone()), (false, Some(trf("updo.restart_not_started", &[&app_version()]))));
        assert!(core.log.iter().any(|(_, t)| *t == tr("updo.restart_back")), "{:?}", core.log);
        // Повторный перезапуск после возврата: возвращать нечего, файлы не тронуты, возврат не дописан; ядро стоит —
        // последний запуск передаёт его диспетчеру.
        let before = names(&inst);
        let mut core = fake(&[false, false]);
        assert_eq!(restart_with_fallback(&target_with_store(&inst, &store), &set, &mut core, &RealFs), 7);
        assert_eq!(names(&inst), before);
        assert_eq!(history(&store).len(), 2);
        let texts: Vec<String> = core.log.iter().map(|(_, t)| t.clone()).collect();
        assert_eq!(texts[texts.len() - 2..], [tr("updo.restart_no_way_back"), tr("updo.restart_handover")], "{texts:?}");
        // Тот же случай, но запуск под диспетчером удался: ядро работает на новой сборке — код 0.
        let mut core = fake(&[false, true]);
        assert_eq!(restart_with_fallback(&target_with_store(&inst, &store), &set, &mut core, &RealFs), 0);
        assert_eq!(history(&store).len(), 2);
        // Служба не останавливается — ничего не трогается.
        let mut core = FakeCore { stop_ok: false, ..fake(&[]) };
        assert_eq!(restart_with_fallback(&target_with_store(&inst, &store), &set, &mut core, &RealFs), 5);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn restart_reports_partial_and_not_started_fallback() {
        use crate::events::Severity;
        let dir = temp("fb-flow2");
        let (inst, src, store) = (dir.join("inst"), dir.join("src"), dir.join("store"));
        // Возвращено не всё — прежнее не запускается (смешанный набор), ошибка с путём — в журнал и историю.
        let set = updated_set(&inst, &src, &OLD_SET);
        store_with_update(&store);
        let lost = set.iter().find(|r| r.name == "tunnel.dll").unwrap().aside.clone().unwrap();
        std::fs::remove_file(inst.join(&lost)).unwrap();
        // Прежняя не запускается (набор смешанный): второй запуск — только передача диспетчеру; больше подменённая
        // служба не допустила бы (`starts` кончились). Поднялось бы — код всё равно 8: набор не тот, что в истории.
        let mut core = fake(&[false, false]);
        assert_eq!(restart_with_fallback(&target_with_store(&inst, &store), &set, &mut core, &RealFs), 8);
        assert!(core.starts.is_empty());
        let bad = core.log.iter().find(|(s, t)| *s == Severity::Bad && t.contains(&lost)).map(|(_, t)| t.clone());
        assert!(bad.is_some(), "{:?}", core.log);
        let h = history(&store);
        assert!(!h[0].ok && h[0].error.as_deref().is_some_and(|e| e.contains(&lost)), "{h:?}");
        assert!(!h[1].ok);
        // Возвращено, но прежнее тоже не запустилось — строка возврата становится ошибкой.
        let (inst, src, store) = (dir.join("inst2"), dir.join("src2"), dir.join("store2"));
        let set = updated_set(&inst, &src, &OLD_SET);
        store_with_update(&store);
        let mut core = fake(&[false, false, false]);
        assert_eq!(restart_with_fallback(&target_with_store(&inst, &store), &set, &mut core, &RealFs), 9);
        let h = history(&store);
        assert_eq!((h[0].ok, h[0].error.clone()), (false, Some(tr("updo.restart_back_not_started"))));
        // Прежняя поднялась только запуском под диспетчером — код 6 (ядро работает на прежней); строка возврата в
        // истории уже записана ошибкой — её объясняют строки журнала (не поднялась, передана диспетчеру).
        let (inst, src, store) = (dir.join("inst4"), dir.join("src4"), dir.join("store4"));
        let set = updated_set(&inst, &src, &OLD_SET);
        store_with_update(&store);
        let mut core = fake(&[false, false, true]);
        assert_eq!(restart_with_fallback(&target_with_store(&inst, &store), &set, &mut core, &RealFs), 6);
        for (n, d) in OLD_SET {
            assert_eq!(read(&inst, n), d, "{n}");
        }
        assert_eq!(core.log.last(), Some(&(Severity::Warn, tr("updo.restart_handover"))));
        // Испорченная история не перезаписывается, ошибка — в журнал.
        let (inst, src, store) = (dir.join("inst3"), dir.join("src3"), dir.join("store3"));
        let set = updated_set(&inst, &src, &OLD_SET);
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(store.join("history.json"), "{broken").unwrap();
        let mut core = fake(&[false, true]);
        assert_eq!(restart_with_fallback(&target_with_store(&inst, &store), &set, &mut core, &RealFs), 6);
        assert_eq!(std::fs::read_to_string(store.join("history.json")).unwrap(), "{broken");
        assert!(core.log.iter().any(|(s, t)| *s == Severity::Bad && t.contains("history.json")), "{:?}", core.log);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn restart_accepts_only_set_files_in_install_dir() {
        let args = |v: &[&str]| [RESTART_FLAG].iter().chain(v).map(|s| s.to_string()).collect::<Vec<_>>().into_iter();
        let r = |name: &'static str, aside: Option<&str>| Replaced { name, aside: aside.map(str::to_string) };
        assert_eq!(restart_set(args(&[])), Ok(vec![]));
        assert_eq!(restart_set(["--x".to_string()].into_iter()), Ok(vec![]));
        // Прошлые версии передают только прежний exe.
        assert_eq!(restart_set(args(&["awg-ui.exe.old-0a1b"])), Ok(vec![r(APP_EXE, Some("awg-ui.exe.old-0a1b"))]));
        assert_eq!(
            restart_set(args(&["tunnel.dll.old-1", "update-manifest.json.sig.old-2", "update-manifest.json"])),
            Ok(vec![r("tunnel.dll", Some("tunnel.dll.old-1")), r(MANIFEST_SIG, Some("update-manifest.json.sig.old-2")), r(MANIFEST, None)])
        );
        for bad in [
            "awg-ui.exe.old-",
            "awg-ui.exe.keep-0a1b",
            "other.dll.old-0a1b",
            "awg-ui.exe.old-ab.dll",
            "update-manifest.json.old-sig.old-1",
            r"..\awg-ui.exe.old-0a1b",
            r"awg-ui.exe.old-0a1b\..\x",
            "awg-ui.exe.old-0a1b/x",
            r"C:\awg-ui.exe.old-0a1b",
        ] {
            assert_eq!(restart_set(args(&["tunnel.dll.old-1", bad])), Err(bad.to_string()), "{bad}");
        }
        // Повтор файла — тоже отказ всего набора.
        assert_eq!(restart_set(args(&["awg-ui.exe.old-1", "awg-ui.exe.old-2"])), Err("awg-ui.exe.old-2".to_string()));
        // Отодвинутый «прежний» — папка, а не файл: возвращать нечего.
        let dir = temp("oldexe");
        std::fs::create_dir_all(dir.join("awg-ui.exe.old-cd")).unwrap();
        write_all(&dir, &[(APP_EXE, "new")]);
        assert_eq!(roll_back_set(&dir, &[r(APP_EXE, Some("awg-ui.exe.old-cd"))], &RealFs), Fallback::Nothing);
        assert_eq!(read(&dir, APP_EXE), "new");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn start_retries_until_ready() {
        let mut calls = 0;
        let outcome = start_with_retries(4, Duration::ZERO, || {
            calls += 1;
            if calls == 3 { Start::Running } else { Start::Failed }
        });
        assert_eq!((outcome, calls), (Start::Running, 3));
        calls = 0;
        let outcome = start_with_retries(4, Duration::ZERO, || {
            calls += 1;
            Start::Failed
        });
        assert_eq!((outcome, calls), (Start::Failed, 4));
        // Исход — последней попытки: имя канала освободилось к последней, а сборка не поднялась — это сбой сборки.
        calls = 0;
        let outcome = start_with_retries(3, Duration::ZERO, || {
            calls += 1;
            if calls < 3 { Start::PipeTaken } else { Start::Failed }
        });
        assert_eq!(outcome, Start::Failed);
    }

    #[test]
    fn pipe_taken_exit_code_is_told_apart_from_a_broken_build() {
        use windows_sys::Win32::Foundation::{ERROR_SERVICE_SPECIFIC_ERROR, NO_ERROR};
        let st = |state, win32_exit, specific_exit| Status { state, win32_exit, specific_exit };
        let taken = crate::daemon::service::PIPE_TAKEN;
        assert_eq!(start_outcome(Ok(())), Start::Running);
        assert_eq!(start_outcome(Err(st(State::Stopped, ERROR_SERVICE_SPECIFIC_ERROR, taken))), Start::PipeTaken);
        assert_eq!(start_outcome(Err(st(State::Stopped, ERROR_SERVICE_SPECIFIC_ERROR, 1))), Start::Failed);
        assert_eq!(start_outcome(Err(st(State::Stopped, NO_ERROR, taken))), Start::Failed, "код службы без ERROR_SERVICE_SPECIFIC_ERROR не в счёт");
        assert_eq!(start_outcome(Err(st(State::StartPending, 0, 0))), Start::Failed, "не дождались — сбой");
    }

    use crate::scm::fake::{FakeService, Reaction};

    #[test]
    fn started_core_is_left_running() {
        let svc = FakeService::in_state(State::Stopped);
        assert_eq!(start_service(&svc, &mut |_| {}), Start::Running);
        assert_eq!(svc.calls(), ["start"]);
    }

    #[test]
    fn core_that_is_already_running_counts_as_started() {
        let svc = FakeService::in_state(State::Running);
        assert_eq!(start_service(&svc, &mut |_| {}), Start::Running);
        assert_eq!(svc.calls(), ["start"], "лишней остановки нет");
    }

    #[test]
    fn refused_start_is_a_failure_and_the_service_is_cleaned_up() {
        let mut svc = FakeService::in_state(State::Stopped);
        svc.on_start = Reaction::Fails("denied");
        let mut reasons = Vec::new();
        assert_eq!(start_service(&svc, &mut |e| reasons.push(e.to_string())), Start::Failed);
        assert_eq!(reasons.len(), 1, "причина отказа дошла до журнала: {reasons:?}");
        assert!(reasons[0].contains("denied"), "{reasons:?}");
        assert_eq!(svc.calls(), ["start", "stop"]);
    }

    #[test]
    fn core_that_stops_on_the_taken_pipe_name_is_told_apart() {
        use windows_sys::Win32::Foundation::ERROR_SERVICE_SPECIFIC_ERROR;
        let mut svc = FakeService::in_state(State::Stopped);
        let taken = Status { state: State::Stopped, win32_exit: ERROR_SERVICE_SPECIFIC_ERROR, specific_exit: crate::daemon::service::PIPE_TAKEN };
        svc.on_start = Reaction::Goes(taken, 2);
        assert_eq!(start_service(&svc, &mut |_| {}), Start::PipeTaken);
        assert_eq!(svc.calls(), ["start", "stop"], "перед следующей попыткой служба остановлена (уже стоит — без ошибки)");
    }

    #[test]
    fn stopping_an_already_stopped_core_is_a_success() {
        let svc = FakeService::in_state(State::Stopped);
        assert_eq!(stop_service(&svc), Ok(()));
    }

    /// Имя канала занято другой программой на время перезапуска: прежняя сборка не возвращается (иначе это откат
    /// без UAC), ядро ждёт, пока имя освободится.
    #[test]
    fn squatted_pipe_name_does_not_roll_back_the_update() {
        use crate::events::Severity;
        let dir = temp("squat");
        let (inst, src, store) = (dir.join("inst"), dir.join("src"), dir.join("store"));
        let set = updated_set(&inst, &src, &OLD_SET);
        store_with_update(&store);
        // Имя освободилось на третьем круге — новое ядро работает, ничего не возвращено.
        let mut core = fake_with(&[Start::PipeTaken, Start::PipeTaken, Start::Running]);
        assert_eq!(restart_with_fallback(&target_with_store(&inst, &store), &set, &mut core, &RealFs), 0);
        assert_eq!(read(&inst, APP_EXE), "new awg-ui.exe");
        assert_eq!(history(&store).len(), 1, "возврата в истории нет");
        assert_eq!(core.log, vec![(Severity::Warn, tr("updo.restart_pipe_taken"))]);
        // Имя так и не освободилось — ядро стоит на новой сборке, прежняя не возвращена; последний запуск идёт уже с
        // действиями диспетчера при сбое: дальше ядро поднимает он.
        let mut core = fake_with(&[Start::PipeTaken; TAKEN_ROUNDS as usize + 1]);
        assert_eq!(restart_with_fallback(&target_with_store(&inst, &store), &set, &mut core, &RealFs), 10);
        assert!(core.starts.is_empty(), "все круги ожидания использованы, плюс запуск после возврата действий");
        assert_eq!(core.calls.iter().rev().take(2).collect::<Vec<_>>(), [&"start", &"on"], "{:?}", core.calls);
        assert_eq!(read(&inst, APP_EXE), "new awg-ui.exe");
        assert!(set.iter().filter_map(|r| r.aside.as_ref()).all(|a| inst.join(a).is_file()), "прежние файлы на месте для ручного возврата");
        assert_eq!(history(&store).len(), 1);
        let tail: Vec<&(Severity, String)> = core.log.iter().rev().take(2).collect();
        assert_eq!(tail, [&(Severity::Warn, tr("updo.restart_handover")), &(Severity::Bad, tr("updo.restart_pipe_gave_up"))]);
        // Имя освободилось к самому последнему запуску — новое ядро работает.
        let mut starts = vec![Start::PipeTaken; TAKEN_ROUNDS as usize];
        starts.push(Start::Running);
        let mut core = fake_with(&starts);
        assert_eq!(restart_with_fallback(&target_with_store(&inst, &store), &set, &mut core, &RealFs), 0);
        assert_eq!(read(&inst, APP_EXE), "new awg-ui.exe");
        // Имя освободилось, но новая сборка всё равно не поднялась — это её сбой: обычный возврат.
        let mut core = fake_with(&[Start::PipeTaken, Start::Failed, Start::Running]);
        assert_eq!(restart_with_fallback(&target_with_store(&inst, &store), &set, &mut core, &RealFs), 6);
        for (n, d) in OLD_SET {
            assert_eq!(read(&inst, n), d, "{n}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
