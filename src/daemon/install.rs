//! Установка, обновление и удаление ядра. Нужны права администратора: окно запускает себя с `--install-core`
//! или `--uninstall-core` через запрос UAC и ждёт. Это единственный запрос UAC — дальше окно работает без прав
//! администратора, а всё, что их требует, делает ядро.

use std::path::Path;
use std::ptr::{null, null_mut};
use std::time::Duration;

use windows_sys::Win32::System::Services::{
    ChangeServiceConfig2W, ChangeServiceConfigW, CreateServiceW, SC_ACTION, SC_ACTION_RESTART, SERVICE_ALL_ACCESS,
    SERVICE_AUTO_START, SERVICE_CONFIG_DESCRIPTION, SERVICE_DESCRIPTIONW, SERVICE_ERROR_NORMAL,
    SERVICE_NO_CHANGE, SERVICE_WIN32_OWN_PROCESS,
};

use super::{data_dir, Config, DATA_SDDL, SERVICE, SERVICE_FLAG};
use crate::engine::{install_dir, FILES};
use crate::i18n::{tr, trf};
use crate::scm::{stop_and_wait, Handle, Service, ServiceControl, State};
use crate::win::wide;

pub const INSTALL_FLAG: &str = "--install-core";
pub const UNINSTALL_FLAG: &str = "--uninstall-core";
/// Примечание к успешной установке для окна: у прежней версии был автозапуск, окну включить его по-новому.
pub const AUTOSTART_NOTE: &str = "autostart";
const WAIT: Duration = Duration::from_secs(20);

/// Установить или обновить ядро из этого exe (и файлов движка рядом с ним). `owner_sid` — учётная запись окна:
/// установку может подтвердить паролем другой администратор, и тогда «текущий пользователь» здесь — он, а не
/// владелец. Возвращает `true`, если у прежней версии был автозапуск окна — включить его по-новому должно само
/// окно (ключ `Run` — в разделе реестра его пользователя).
pub fn install(owner_sid: &str) -> Result<bool, String> {
    if !crate::win::is_sid(owner_sid) {
        return Err(format!("owner SID: {owner_sid}"));
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let src = exe.parent().map(Path::to_path_buf).unwrap_or_default();
    let dst = install_dir();
    std::fs::create_dir_all(&dst).map_err(|e| crate::fsutil::io_ctx(&dst, e))?;
    // Замена файлов, оборванная обновлением, доводится до уборки её `.old-`: иначе уборка снесла бы прежние файлы,
    // по которым ещё можно вернуться.
    let recovered = crate::update::ours::recover_swaps(&mut |severity, text| super::log_notice(super::events_file(), severity, text));
    remove_old_copies_after(&dst, recovered);
    stop()?;

    // Файлы: exe всегда, DLL движка — если они есть и совпадают с вшитыми суммами (иначе режим 2 недоступен).
    place(&dst.join(FILES[0]), &std::fs::read(&exe).map_err(|e| crate::fsutil::io_ctx(&exe, e))?)?;
    // Движок, обновлённый из релиза, установка ядра из папки окна не откатывает, если там движок не новее.
    let keep_engine = updated_engine(&dst).is_some_and(|v| !option_env!("AWG_ENGINE_TAG").is_some_and(|own| crate::update::feed::newer(own, &v)));
    // DLL, не прошедшие проверку: журнал ядра ещё не доступен (папки данных может не быть) — записываются ниже.
    let mut rejected = Vec::new();
    for dll in FILES[1..].iter().filter(|_| !keep_engine) {
        // Нет DLL в папке окна — режим 1 без движка, не ошибка; иная ошибка чтения — ошибка установки.
        let data = match std::fs::read(src.join(dll)) {
            Ok(data) => data,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(crate::fsutil::io_ctx(src.join(dll), e)),
        };
        // Сумма не сошлась с вшитой — DLL не наша и не ставится (см. выше: иначе режим 2 недоступен).
        match crate::engine::rejection(&src.join(dll), dll, &data) {
            None => place(&dst.join(dll), &data)?,
            Some(why) => rejected.push(why),
        }
    }

    // Данные ядра; при первой установке — режим, пинг, статистика и журнал из папки окна.
    let data = data_dir();
    crate::win::protect_dir(&data, DATA_SDDL)?;
    // Установка из-за этого не падает (без движка работает режим 1), но тихий пропуск оставил бы пользователя гадать,
    // почему режим 2 недоступен: путь и суммы — в журнал ядра.
    for why in &rejected {
        super::log_notice(super::events_file(), crate::events::Severity::Warn, why);
    }
    // Нечитаемый core.ini отодвигается до проверки «первая установка»: настройки берутся заново из папки окна, а
    // прежний файл остаётся рядом копией, `config.save()` ниже его не затрёт.
    let (mut config, unreadable) = Config::load_guarded();
    if let Some(problem) = &unreadable {
        super::log_unreadable(problem);
    }
    let first = !Config::path().exists();
    if first {
        let gui = crate::settings::Settings::from_ini(&crate::ini::Ini::load(&src.join("Settings.ini")));
        // Режим 2 — только если файлы движка на месте.
        config.mode = if crate::engine::installed_files_ok().is_ok() { gui.mode() } else { crate::settings::Mode::Overlay };
        config.language = gui.language;
        // Пинг — настройка агента. Без `agent.ini` агент взял бы умолчания (в `core.ini` пинга нет) вместо выбора
        // в окне; есть файл (переустановка) — он главнее.
        let agent_ini = super::agent::AgentConfig::path();
        if !agent_ini.exists() {
            super::agent::AgentConfig { ping: gui.view.ping, ping_host: gui.ping_host }.save_to(&agent_ini)?;
        }
        let events = data.join("logs").join("events.log");
        let carried = [
            copy_if_missing(&src.join("Stats.ini"), &data.join("Stats.ini")),
            copy_if_missing(&src.join(&gui.log_dir).join("events.log"), &events),
        ];
        // Установка из-за этого не падает (статистика и журнал — не причина оставить машину без ядра), но потерю
        // истории надо показать: первой записью журнала ядра.
        let mut log = crate::events::EventLog::open(Some(events));
        for e in carried.into_iter().filter_map(Result::err) {
            log.push(crate::events::Event::new(crate::monitor::unix_now(), "", crate::events::Severity::Warn, &trf("core.carry_failed", &[&e]), false));
        }
    }
    config.owner_sid = owner_sid.to_string();
    config.save()?;

    create_service(&dst.join(FILES[0]))?;
    // Прежние версии запускали окно с правами администратора через задачи планировщика — больше не нужно.
    Ok(crate::win::remove_legacy_tasks())
}

/// Удалить службу ядра. Файлы, туннели и данные остаются (туннели режима 2 продолжают работать).
pub fn uninstall() -> Result<(), String> {
    stop()?;
    match open_core()? {
        Some(svc) => svc.delete(),
        None => Ok(()),
    }
}

/// Служба ядра с полными правами; не установлена — `None`.
fn open_core() -> Result<Option<Service>, String> {
    Service::try_open(&Handle::scm_connect()?, SERVICE, SERVICE_ALL_ACCESS)
}

/// Ядро установлено (служба есть)?
pub fn installed() -> bool {
    crate::win::service_command(SERVICE).is_some()
}

/// Остановить службу ядра перед заменой файлов; не установлена или уже стоит — не ошибка.
fn stop() -> Result<(), String> {
    match open_core()? {
        Some(svc) => stop_core(&svc, WAIT),
        None => Ok(()),
    }
}

fn stop_core(svc: &dyn ServiceControl, timeout: Duration) -> Result<(), String> {
    stop_and_wait(svc, timeout).map_err(|e| format!("{}: {e}", tr("core.stop_failed")))
}

/// Служба ядра: создать или обновить путь, автозапуск, перезапуск при сбое, запустить и дождаться.
fn create_service(exe: &Path) -> Result<(), String> {
    let command = wide(&format!("\"{}\" {SERVICE_FLAG}", exe.display()));
    let dependencies: Vec<u16> = "Nsi\0TcpIp\0\0".encode_utf16().collect();
    unsafe {
        let scm = Handle::scm()?;
        let svc = if let Some(existing) = Service::try_open(&scm, SERVICE, SERVICE_ALL_ACCESS)? {
            let ok = ChangeServiceConfigW(
                existing.raw(),
                SERVICE_NO_CHANGE,
                SERVICE_AUTO_START,
                SERVICE_NO_CHANGE,
                command.as_ptr(),
                null(),
                null_mut(),
                dependencies.as_ptr(),
                null(),
                null(),
                null(),
            );
            if ok == 0 {
                return Err(format!("ChangeServiceConfig {SERVICE}: {}", std::io::Error::last_os_error()));
            }
            existing
        } else {
            let display = wide(&format!("{} Core", crate::APP_TITLE));
            let h = CreateServiceW(
                scm.0,
                wide(SERVICE).as_ptr(),
                display.as_ptr(),
                SERVICE_ALL_ACCESS,
                SERVICE_WIN32_OWN_PROCESS,
                SERVICE_AUTO_START,
                SERVICE_ERROR_NORMAL,
                command.as_ptr(),
                null(),
                null_mut(),
                dependencies.as_ptr(),
                null(),
                null(),
            );
            Service::from_handle(Handle::new(h).map_err(|e| format!("CreateService {SERVICE}: {e}"))?, SERVICE)
        };
        let mut text = wide(&tr("core.service_desc"));
        let desc = SERVICE_DESCRIPTIONW { lpDescription: text.as_mut_ptr() };
        ChangeServiceConfig2W(svc.raw(), SERVICE_CONFIG_DESCRIPTION, (&desc as *const SERVICE_DESCRIPTIONW).cast());
        set_core_failure_actions(&svc)?;
        start_core(&svc, WAIT)
    }
}

/// Действия диспетчера при сбое службы ядра: упало — Windows поднимет снова через 5 секунд (сбросом счётчика раз в
/// сутки). И не только при падении процесса: ядро, которое не поднялось и остановилось с ошибкой (например, новая
/// версия после обновления), тоже. Одно определение для установки, перезапуска после обновления (`--restart-core`
/// снимает действия на время своей работы и ставит обратно) и старта ядра (`reapply_failure_actions`).
pub(crate) fn set_core_failure_actions(svc: &Service) -> Result<(), String> {
    crate::scm::set_failure_actions(svc, RESET_PERIOD, &mut failure_actions())
}

/// Старт службы ядра: довести замену набора файлов, оборванную обрывом (`update::ours::swap`), до туннелей, агента и
/// уборки `.old-`. Язык — из `core.ini` (его же выберет `server::run`): тексты исхода идут в журнал событий напрямую,
/// агента ещё нет. Здесь, а не в `server.rs`: ядро само в подсистему обновлений не ходит (ограда `core_does_not_reach_into_agent_work`),
/// а уборка отодвинутых файлов и так его дело. Возвращает, можно ли убирать `.old-`/`.new-` в этот старт
/// (`remove_old_copies_after`): план не прочитался — нельзя.
pub(crate) fn recover_swaps_at_start() -> bool {
    crate::i18n::set(&super::lang_dir(), &Config::load().language);
    let recovered = crate::update::ours::recover_swaps(&mut |severity, text| super::log_notice(super::events_file(), severity, text));
    cleanup_allowed(recovered)
}

/// Уборка `.old-`/`.new-` после доведения замены с исходом `outcome`: план не прочитался (отодвинут как
/// `.unreadable-`) — уборки в этот старт нет: без плана неизвестно, какой из файлов — единственная целая копия
/// (замена могла встать между «прежний отодвинут» и «новый на месте»), и `.old-` рядом с отодвинутым планом — путь
/// назад для разбора вручную; об этом говорит сама запись `updo.swap_journal_bad`. Иначе — `remove_old_copies`.
pub(crate) fn remove_old_copies_after(dir: &Path, outcome: crate::update::ours::swap::Recovery) {
    if cleanup_allowed(outcome) {
        remove_old_copies(dir);
    }
}

fn cleanup_allowed(outcome: crate::update::ours::swap::Recovery) -> bool {
    outcome != crate::update::ours::swap::Recovery::Unreadable
}

/// Поднявшееся ядро ставит действия при сбое своей службы заново: помощник `--restart-core`, снятый посреди работы,
/// оставил бы службу без перезапуска при сбое до следующей установки.
pub(crate) fn reapply_failure_actions() -> Result<(), String> {
    let svc = Service::open(&Handle::scm_connect()?, SERVICE, reapply_access())?;
    set_core_failure_actions(&svc)
}

/// Права, с которыми старт ядра открывает свою службу, чтобы поставить действия при сбое (`set_core_failure_actions`).
fn reapply_access() -> u32 {
    crate::scm::failure_actions_access(&failure_actions())
}

/// Запустить службу ядра и дождаться `Running`.
fn start_core(svc: &dyn ServiceControl, timeout: Duration) -> Result<(), String> {
    svc.start()?;
    svc.wait_state(State::Running, timeout).map_err(|st| crate::i18n::trf("core.start_failed", &[&st.win32_exit.to_string()]))
}

/// Сброс счётчика сбоев службы, секунды (сутки).
const RESET_PERIOD: u32 = 86_400;

/// Действия при сбое службы ядра: каждый раз — перезапуск через 5 секунд.
fn failure_actions() -> [SC_ACTION; 3] {
    [SC_ACTION { Type: SC_ACTION_RESTART, Delay: 5000 }; 3]
}

/// Записать файл; занят (его держит работающая служба туннеля) — отодвинуть занятый под другим именем
/// (работающий exe переименовать можно) и положить новый. Отодвинутые убираются при следующей установке.
fn place(path: &Path, data: &[u8]) -> Result<(), String> {
    if std::fs::read(path).is_ok_and(|have| have == data) {
        return Ok(());
    }
    // Не записалось (файл занят службой) — не ошибка: ниже его отодвигают и пишут заново, и ошибки идут уже оттуда.
    if crate::fsutil::write_atomic(path, data).is_ok() {
        return Ok(());
    }
    let aside = path.with_file_name(format!("{}.old-{}", path.file_name().unwrap_or_default().to_string_lossy(), crate::store::random_hex()));
    std::fs::rename(path, &aside).map_err(|e| crate::fsutil::io_ctx(path, e))?;
    crate::fsutil::write_atomic(path, data).map_err(|e| crate::fsutil::io_ctx(path, e))
}

/// Убрать из `dir` отодвинутые обновлением прежние файлы (`.old-`) и не вставшие на место новые (`.new-`), кроме тех,
/// что держит незавершённый план замены (`update::ours::swap`): их доводит `recover_swaps`, а до него они — путь назад.
pub(crate) fn remove_old_copies(dir: &Path) {
    remove_old_copies_except(dir, &crate::update::ours::swap_protected());
}

pub(crate) fn remove_old_copies_except(dir: &Path, keep: &[String]) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if (name.contains(".old-") || name.contains(".new-")) && !keep.contains(&name) {
            // Отодвинутый файл мог быть занят работающей службой: уберётся при следующей установке или запуске службы.
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Версия движка в `dir`, если его поставило обновление: подписанный манифест сходится со всеми DLL.
fn updated_engine(dir: &Path) -> Option<String> {
    let m = crate::update::ours::local_manifest(dir)?;
    let all = m.engine.files.iter().all(|f| crate::update::sign::check_file(&dir.join(&f.name), f).is_ok());
    all.then_some(m.engine.version)
}

/// Перенести файл, если источник есть, а назначения ещё нет; иначе ничего не делает.
fn copy_if_missing(from: &Path, to: &Path) -> Result<(), String> {
    if !from.exists() || to.exists() {
        return Ok(());
    }
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir).map_err(|e| crate::fsutil::io_ctx(dir, e))?;
    }
    std::fs::copy(from, to).map(drop).map_err(|e| crate::fsutil::io_ctx_move(from, to, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carrying_data_over_reports_a_failed_copy() {
        let dir = std::env::temp_dir().join(format!("awg-ui-carry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (from, blocker) = (dir.join("Stats.ini"), dir.join("blocker"));
        std::fs::write(&from, "x").unwrap();
        std::fs::write(&blocker, "x").unwrap();
        assert!(copy_if_missing(&from, &blocker.join("Stats.ini")).is_err(), "родитель назначения — файл");
        assert!(copy_if_missing(&dir.join("none"), &dir.join("to")).is_ok(), "источника нет — переносить нечего");
        assert!(copy_if_missing(&from, &blocker).is_ok(), "назначение уже есть — не затирается");
        assert_eq!(std::fs::read_to_string(&blocker).unwrap(), "x");
        let to = dir.join("data").join("Stats.ini");
        copy_if_missing(&from, &to).unwrap();
        assert!(to.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn failure_actions_restart_every_time() {
        let actions = failure_actions();
        assert_eq!(actions.len(), 3);
        for a in actions {
            assert_eq!(a.Type, SC_ACTION_RESTART);
            assert_eq!(a.Delay, 5000);
        }
        assert_eq!(RESET_PERIOD, 86_400);
    }

    /// Старт ядра ставит действия при сбое заново (`reapply_failure_actions`): дескриптор только со сменой настроек
    /// давал «отказано в доступе» при каждом старте — действия «перезапуск» требуют и права запуска.
    #[test]
    fn reapplying_core_failure_actions_opens_the_service_with_the_start_right() {
        use windows_sys::Win32::System::Services::{SERVICE_CHANGE_CONFIG, SERVICE_START};
        assert_eq!(reapply_access() & SERVICE_CHANGE_CONFIG, SERVICE_CHANGE_CONFIG);
        assert_eq!(reapply_access() & SERVICE_START, SERVICE_START);
    }

    use crate::scm::fake::{FakeService, Reaction};
    use crate::scm::Status;

    const SHORT: Duration = Duration::from_millis(30);

    #[test]
    fn stopping_an_already_stopped_core_is_fine() {
        let svc = FakeService::in_state(State::Stopped);
        assert_eq!(stop_core(&svc, SHORT), Ok(()));
        assert_eq!(svc.calls(), ["stop"]);
    }

    #[test]
    fn core_that_does_not_stop_in_time_fails_the_install() {
        let mut svc = FakeService::in_state(State::Running);
        svc.on_stop = Reaction::Stuck(State::StopPending);
        let e = stop_core(&svc, SHORT).unwrap_err();
        assert!(e.contains(&tr("core.stop_failed")), "{e}");
    }

    #[test]
    fn core_start_waits_for_running() {
        let mut svc = FakeService::in_state(State::Stopped);
        svc.on_start = Reaction::Goes(Status::new(State::Running), 3);
        assert_eq!(start_core(&svc, Duration::from_secs(5)), Ok(()));
    }

    #[test]
    fn core_that_dies_on_start_reports_its_exit_code() {
        let mut svc = FakeService::in_state(State::Stopped);
        svc.on_start = Reaction::Goes(Status { state: State::Stopped, win32_exit: 1067, specific_exit: 0 }, 1);
        let e = start_core(&svc, Duration::from_secs(5)).unwrap_err();
        assert!(e.contains("1067"), "{e}");
    }

    #[test]
    fn core_that_hangs_on_start_times_out() {
        let mut svc = FakeService::in_state(State::Stopped);
        svc.on_start = Reaction::Stuck(State::StartPending);
        assert!(start_core(&svc, SHORT).is_err());
        let mut refused = FakeService::in_state(State::Stopped);
        refused.on_start = Reaction::Fails("StartService: denied");
        assert_eq!(start_core(&refused, SHORT), Err("StartService: denied".to_string()));
    }
}
