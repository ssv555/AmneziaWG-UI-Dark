//! Режим 2 «Встроенный движок»: туннель держит `tunnel.dll` (amneziawg-windows, MIT) внутри службы Windows,
//! которую создаёт эта программа. Окно DLL не загружает — её грузит только процесс службы.
//!
//! Служба: `"<Program Files>\AmneziaWG UI Dark\awg-ui.exe" --tunnel-service "<конфиг>"` от SYSTEM.
//! Имя службы = имя туннеля: так требует движок (оно же имя адаптера и UAPI-канала, как у оригинала).

use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::OnceLock;
use std::time::Duration;

use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_WITH_ALTERED_SEARCH_PATH};
use windows_sys::Win32::System::Services::{
    ChangeServiceConfig2W, CreateServiceW, SERVICE_ALL_ACCESS, SERVICE_AUTO_START, SERVICE_CONFIG_DESCRIPTION,
    SERVICE_CONFIG_SERVICE_SID_INFO, SERVICE_DESCRIPTIONW, SERVICE_ERROR_NORMAL, SERVICE_QUERY_STATUS, SERVICE_SID_INFO,
    SERVICE_SID_TYPE_UNRESTRICTED, SERVICE_WIN32_OWN_PROCESS,
};

use crate::i18n::{tr, trf};
use crate::scm::{stop_and_wait, Handle, Service, ServiceControl, State};
use crate::win::wide;

/// Ключ командной строки процесса службы.
pub const SERVICE_FLAG: &str = "--tunnel-service";
/// Файлы движка; при включении режима копируются в Program Files (служба SYSTEM не запускает файлы,
/// которые может подменить обычная программа пользователя).
pub const FILES: [&str; 3] = ["awg-ui.exe", "tunnel.dll", "wintun.dll"];
const INSTALL_DIR: &str = "AmneziaWG UI Dark";
const START_TIMEOUT: Duration = Duration::from_secs(25);
const STOP_TIMEOUT: Duration = Duration::from_secs(20);

/// SHA-256 DLL движка, вшитые при сборке (`build.rs` берёт их из `engine\out`). Нет — сборка без движка.
const PINNED: [(&str, Option<&str>); 2] =
    [("tunnel.dll", option_env!("AWG_TUNNEL_SHA256")), ("wintun.dll", option_env!("AWG_WINTUN_SHA256"))];

/// `C:\Program Files\AmneziaWG UI Dark`.
pub fn install_dir() -> PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| crate::win::program_files().join(INSTALL_DIR)).clone()
}

/// Файлы движка в папке программы (Program Files) совпадают с вшитыми суммами — встроенный режим доступен.
pub fn installed_files_ok() -> Result<(), String> {
    let dir = install_dir();
    for (name, _) in PINNED {
        let path = dir.join(name);
        verify(name, &std::fs::read(&path).map_err(|_| trf("eng.missing", &[&path.display().to_string()]))?)?;
    }
    Ok(())
}

/// DLL движка доверенная: её SHA-256 вшита при сборке или указана в проверенном локальном манифесте в папке
/// программы (его кладёт туда обновление движка из нашего подписанного релиза).
pub(crate) fn verify(name: &str, data: &[u8]) -> Result<(), String> {
    let pinned = PINNED.iter().find(|(n, _)| *n == name).and_then(|(_, h)| *h);
    let local = crate::update::ours::local_manifest(&install_dir());
    let actual = crate::update::sign::sha256_hex(data);
    if trusted(name, &actual, pinned, local.as_ref()) {
        Ok(())
    } else if pinned.is_none() && local.is_none() {
        Err(tr("eng.no_engine_build"))
    } else {
        Err(trf("eng.untrusted", &[name]))
    }
}

/// Почему DLL не подходит, для журнала: путь, фактическая и ожидаемые суммы. `None` — файл доверенный.
pub(crate) fn rejection(path: &Path, name: &str, data: &[u8]) -> Option<String> {
    let pinned = PINNED.iter().find(|(n, _)| *n == name).and_then(|(_, h)| *h);
    let local = crate::update::ours::local_manifest(&install_dir());
    describe_rejection(path, name, &crate::update::sign::sha256_hex(data), pinned, local.as_ref())
}

fn describe_rejection(path: &Path, name: &str, actual: &str, pinned: Option<&str>, local: Option<&crate::update::sign::Manifest>) -> Option<String> {
    if trusted(name, actual, pinned, local) {
        return None;
    }
    let from_manifest = local.into_iter().flat_map(|m| m.engine.files.iter()).filter(|f| f.name == name).map(|f| f.sha256.as_str());
    let expected: Vec<&str> = pinned.into_iter().chain(from_manifest).collect();
    let expected = if expected.is_empty() { tr("eng.no_engine_build") } else { expected.join(", ") };
    Some(trf("eng.rejected", &[&path.display().to_string(), actual, &expected]))
}

/// Сумма `actual` файла `name` совпадает с вшитой (`pinned`) или с записью локального манифеста.
pub(crate) fn trusted(name: &str, actual: &str, pinned: Option<&str>, local: Option<&crate::update::sign::Manifest>) -> bool {
    pinned == Some(actual) || local.is_some_and(|m| m.engine.files.iter().any(|f| f.name == name && f.sha256 == actual))
}

fn exe_dir() -> PathBuf {
    std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)).unwrap_or_default()
}

/// Скопировать движок в Program Files (одинаковые файлы не трогаются). DLL копируются только с вшитыми
/// суммами: подложенную рядом с exe библиотеку служба SYSTEM не получит. Возвращает путь к exe службы.
pub fn deploy() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let src = exe_dir();
    let dst = install_dir();
    std::fs::create_dir_all(&dst).map_err(|e| crate::fsutil::io_ctx(&dst, e))?;
    for name in FILES {
        let from = if name == FILES[0] { exe.clone() } else { src.join(name) };
        let data = std::fs::read(&from).map_err(|_| trf("eng.missing", &[&from.display().to_string()]))?;
        if name != FILES[0] {
            verify(name, &data)?;
        }
        let to = dst.join(name);
        if std::fs::read(&to).is_ok_and(|have| have == data) {
            continue;
        }
        // Занят файл — значит, работает туннель этого режима со старой версией.
        std::fs::write(&to, &data).map_err(|e| format!("{}: {e} — {}", to.display(), tr("eng.busy")))?;
    }
    Ok(dst.join(FILES[0]))
}

/// Чья служба с именем туннеля: нет такой, наша (exe в папке движка с ключом службы) или чужая.
#[derive(PartialEq)]
enum Owner {
    Free,
    Ours,
    Foreign,
}

fn owner(tunnel: &str) -> Owner {
    let ours = install_dir().join(FILES[0]).to_string_lossy().into_owned();
    match crate::win::service_command(tunnel) {
        None => Owner::Free,
        Some(cmd)
            if cmd.contains(SERVICE_FLAG)
                && crate::win::exe_from_command_line(&cmd).is_some_and(|p| p.to_string_lossy().eq_ignore_ascii_case(&ours)) =>
        {
            Owner::Ours
        }
        Some(_) => Owner::Foreign,
    }
}

/// Имя туннеля занято чужой службой Windows (туннель с таким именем не подключить).
pub fn is_foreign(tunnel: &str) -> bool {
    owner(tunnel) == Owner::Foreign
}

/// Туннель поднят нашей службой (канал с тем же именем может принадлежать и оригинальному AmneziaWG).
pub fn is_running(tunnel: &str) -> bool {
    if owner(tunnel) != Owner::Ours {
        return false;
    }
    // Не открылась — для опроса «поднят ли» это «нет»: монитор спрашивает по кругу, ошибка прозвучит при действии.
    Handle::scm_connect().and_then(|scm| Service::open(&scm, tunnel, SERVICE_QUERY_STATUS)).is_ok_and(|svc| svc.is_running())
}

/// Имя туннеля, которое примет движок: `^[a-zA-Z0-9_=+.-]{1,32}$`, не зарезервированное имя устройства.
pub fn valid_name(name: &str) -> bool {
    const RESERVED: [&str; 4] = ["CON", "PRN", "AUX", "NUL"];
    let upper = name.to_ascii_uppercase();
    let device = RESERVED.contains(&upper.as_str())
        || ((upper.starts_with("COM") || upper.starts_with("LPT")) && upper.len() == 4 && upper.as_bytes()[3].is_ascii_digit());
    (1..=32).contains(&name.len())
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"_=+.-".contains(&b))
        && !device
}

/// Имя туннеля из пути к его конфигу: `<имя>.conf` или `<имя>.conf.dpapi`.
pub fn tunnel_name(conf: &Path) -> String {
    let file = conf.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
    let base = file.strip_suffix(".dpapi").unwrap_or(&file);
    base.strip_suffix(".conf").unwrap_or(base).to_string()
}

/// Процесс службы: прочитать конфиг и отдать его `tunnel.dll`, который сам говорит с диспетчером служб
/// и держит туннель до остановки. Возвращает код выхода процесса.
pub fn run_service(conf: &Path) -> i32 {
    let name = tunnel_name(conf);
    let Ok(text) = crate::store::read(conf) else { return 2 };
    let dll = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("tunnel.dll"))).unwrap_or_default();
    unsafe {
        let lib = LoadLibraryExW(wide(&dll.to_string_lossy()).as_ptr(), null_mut(), LOAD_WITH_ALTERED_SEARCH_PATH);
        if lib.is_null() {
            return 3;
        }
        let Some(entry) = GetProcAddress(lib, c"WireGuardTunnelService".as_ptr().cast()) else { return 4 };
        // Экспорт cgo: bool WireGuardTunnelService(uint16_t *conf, uint16_t *name).
        let run: unsafe extern "C" fn(*const u16, *const u16) -> u8 = std::mem::transmute(entry);
        i32::from(run(wide(&text).as_ptr(), wide(&name).as_ptr()) == 0)
    }
}

/// Поднять туннель: служба с этим именем (своя старая — пересоздаётся, чужая — ошибка), запуск, ожидание.
pub fn connect(conf: &Path) -> Result<(), String> {
    let tunnel = tunnel_name(conf);
    if !valid_name(&tunnel) {
        return Err(trf("eng.bad_name", &[&tunnel]));
    }
    match owner(&tunnel) {
        Owner::Foreign => return Err(trf("eng.name_taken", &[&tunnel])),
        Owner::Ours => disconnect(&tunnel)?,
        Owner::Free => {}
    }
    let exe = deploy()?;
    let command = format!("\"{}\" {SERVICE_FLAG} \"{}\"", exe.display(), conf.display());
    let display = format!("{}: {tunnel}", crate::APP_TITLE);
    let dependencies: Vec<u16> = "Nsi\0TcpIp\0\0".encode_utf16().collect();
    unsafe {
        let scm = Handle::scm()?;
        let svc = CreateServiceW(
            scm.0,
            wide(&tunnel).as_ptr(),
            wide(&display).as_ptr(),
            SERVICE_ALL_ACCESS,
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_AUTO_START,
            SERVICE_ERROR_NORMAL,
            wide(&command).as_ptr(),
            null(),
            null_mut(),
            dependencies.as_ptr(),
            null(), // LocalSystem
            null(),
        );
        let svc = Service::from_handle(Handle::new(svc).map_err(|e| format!("CreateService {tunnel}: {e}"))?, &tunnel);
        // Как у оригинала: свой SID службы (правила брандмауэра движка) и описание.
        let sid = SERVICE_SID_INFO { dwServiceSidType: SERVICE_SID_TYPE_UNRESTRICTED };
        ChangeServiceConfig2W(svc.raw(), SERVICE_CONFIG_SERVICE_SID_INFO, (&sid as *const SERVICE_SID_INFO).cast());
        let mut text = wide(&tr("eng.service_desc"));
        let desc = SERVICE_DESCRIPTIONW { lpDescription: text.as_mut_ptr() };
        ChangeServiceConfig2W(svc.raw(), SERVICE_CONFIG_DESCRIPTION, (&desc as *const SERVICE_DESCRIPTIONW).cast());
        start_or_remove(&svc, &tunnel, START_TIMEOUT)
    }
}

/// Запустить только что созданную службу туннеля и дождаться `Running`. Не запустилась или не поднялась за
/// `timeout` — остановить и удалить: иначе туннель мог бы подняться уже после сообщения об ошибке, а имя осталось бы занятым.
fn start_or_remove(svc: &dyn ServiceControl, tunnel: &str, timeout: Duration) -> Result<(), String> {
    if let Err(e) = svc.start() {
        // Не стартовала — ждать нечего; удалить, чтобы повторное подключение создало службу заново.
        return Err(match svc.delete() {
            Ok(()) => e,
            Err(d) => format!("{e}; {d}"),
        });
    }
    let Err(st) = svc.wait_state(State::Running, timeout) else { return Ok(()) };
    // Ошибки остановки и удаления здесь вторичны: пользователю уходит причина, почему туннель не поднялся.
    let _ = stop_and_delete(svc, tunnel, STOP_TIMEOUT);
    Err(trf("eng.start_failed", &[tunnel, &st.win32_exit.to_string(), &st.specific_exit.to_string()]))
}

/// Остановить службу и пометить на удаление (удаляется и не остановившаяся: имя должно освободиться).
/// Ошибка — текст для пользователя.
fn stop_and_delete(svc: &dyn ServiceControl, tunnel: &str, timeout: Duration) -> Result<(), String> {
    let stopped = stop_and_wait(svc, timeout);
    let deleted = svc.delete();
    // Не остановилась — это важнее, чем исход удаления.
    stopped.map_err(|_| trf("eng.stop_failed", &[tunnel]))?;
    deleted
}

/// Опустить туннель: остановить и удалить службу. Чужие службы не трогаются.
pub fn disconnect(tunnel: &str) -> Result<(), String> {
    match owner(tunnel) {
        Owner::Free => return Ok(()),
        Owner::Foreign => return Err(trf("eng.name_taken", &[tunnel])),
        Owner::Ours => {}
    }
    let svc = Service::open(&Handle::scm_connect()?, tunnel, SERVICE_ALL_ACCESS)?;
    let result = stop_and_delete(&svc, tunnel, STOP_TIMEOUT);
    drop(svc);
    // Удаление завершается, когда закрыты все дескрипторы; ждём, чтобы имя можно было занять снова.
    crate::fsutil::wait_until(STOP_TIMEOUT, Duration::from_millis(200), || crate::win::service_command(tunnel).is_none());
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_engine_rules() {
        let (max, long) = ("x".repeat(32), "x".repeat(33));
        for ok in ["office", "home.nl-01.full.v4.opt", "a_b=c+d", max.as_str()] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in ["", "two words", "slash/name", "кириллица", "nul", "COM1", "lpt9", long.as_str()] {
            assert!(!valid_name(bad), "{bad}");
        }
        assert!(valid_name("COM10") && valid_name("console"));
    }

    #[test]
    fn foreign_dll_is_refused() {
        // С вшитой суммой — «не тот файл», без неё — «сборка без движка»; пропустить чужой файл нельзя никак.
        assert!(verify("tunnel.dll", b"not the engine").is_err());
        assert!(verify("wintun.dll", b"").is_err());
    }

    #[test]
    fn rejection_names_path_actual_and_expected_sums() {
        use crate::update::sign::{Engine, FileEntry, Manifest};
        let path = Path::new(r"C:\w\tunnel.dll");
        let (pinned, actual, listed) = ("a".repeat(64), "b".repeat(64), "c".repeat(64));
        let entry = |name: &str, sha256: &str| FileEntry { name: name.into(), sha256: sha256.into(), size: 1 };
        let manifest = Manifest {
            version: "1".into(),
            published: "2026-01-01T00:00:00Z".into(),
            app: entry("awg-ui.exe", &"0".repeat(64)),
            engine: Engine { version: "1".into(), wintun: "0.14".into(), files: vec![entry("tunnel.dll", &listed), entry("wintun.dll", &"d".repeat(64))] },
        };

        assert_eq!(describe_rejection(path, "tunnel.dll", &pinned, Some(&pinned), None), None, "вшитая сумма совпала");
        assert_eq!(describe_rejection(path, "tunnel.dll", &listed, None, Some(&manifest)), None, "сумма из манифеста совпала");

        let text = describe_rejection(path, "tunnel.dll", &actual, Some(&pinned), Some(&manifest)).expect("сумма не сошлась");
        assert!(text.contains(r"C:\w\tunnel.dll"), "{text}");
        assert!(text.contains(&actual) && text.contains(&pinned) && text.contains(&listed), "{text}");
        assert!(!text.contains(&"d".repeat(64)), "суммы чужого файла в сообщении нет: {text}");

        let bare = describe_rejection(path, "tunnel.dll", &actual, None, None).expect("без вшитых сумм");
        assert!(bare.contains(&actual) && bare.contains(&tr("eng.no_engine_build")), "{bare}");
    }

    #[test]
    fn foreign_dll_has_a_rejection_text() {
        let text = rejection(Path::new("tunnel.dll"), "tunnel.dll", b"not the engine").expect("чужой файл отвергнут");
        assert!(text.contains("tunnel.dll") && text.contains(&crate::update::sign::sha256_hex(b"not the engine")), "{text}");
    }

    #[test]
    fn install_dir_is_under_program_files() {
        let dir = install_dir().to_string_lossy().to_lowercase();
        assert!(dir.ends_with(r"\amneziawg ui dark") && dir.contains("program files"), "{dir}");
    }

    #[test]
    fn tunnel_name_strips_extensions() {
        assert_eq!(tunnel_name(Path::new(r"C:\t\office.conf")), "office");
        assert_eq!(tunnel_name(Path::new(r"C:\t\office.conf.dpapi")), "office");
        assert_eq!(tunnel_name(Path::new("a.b.conf")), "a.b");
    }

    use crate::scm::fake::{FakeService, Reaction};
    use crate::scm::Status;

    const SHORT: Duration = Duration::from_millis(30);

    #[test]
    fn connect_keeps_a_service_that_reached_running() {
        let svc = FakeService::in_state(State::Stopped);
        assert_eq!(start_or_remove(&svc, "office", SHORT), Ok(()));
        assert_eq!(svc.calls(), ["start"], "поднялась — не останавливается и не удаляется");
    }

    #[test]
    fn connect_removes_a_service_that_refused_to_start() {
        let mut svc = FakeService::in_state(State::Stopped);
        svc.on_start = Reaction::Fails("StartService office: denied");
        assert_eq!(start_or_remove(&svc, "office", SHORT), Err("StartService office: denied".to_string()));
        assert_eq!(svc.calls(), ["start", "delete"], "имя не остаётся занятым");
    }

    #[test]
    fn connect_times_out_then_stops_and_removes_the_service() {
        let mut svc = FakeService::in_state(State::Stopped);
        svc.on_start = Reaction::Stuck(State::StartPending);
        let e = start_or_remove(&svc, "office", SHORT).unwrap_err();
        assert!(e.contains("office"), "{e}");
        // Зависший запуск остановлен, иначе туннель поднялся бы уже после сообщения об ошибке.
        assert_eq!(svc.calls(), ["start", "stop", "delete"]);
    }

    #[test]
    fn connect_reports_the_exit_codes_of_a_service_that_died_on_start() {
        let mut svc = FakeService::in_state(State::Stopped);
        let died = Status { state: State::Stopped, win32_exit: 1066, specific_exit: 7 };
        svc.on_start = Reaction::Goes(died, 2);
        let e = start_or_remove(&svc, "office", Duration::from_secs(5)).unwrap_err();
        assert!(e.contains("1066") && e.contains('7'), "{e}");
        assert_eq!(svc.calls(), ["start", "stop", "delete"]);
    }

    #[test]
    fn disconnect_stops_then_removes() {
        let svc = FakeService::in_state(State::Running);
        assert_eq!(stop_and_delete(&svc, "office", SHORT), Ok(()));
        assert_eq!(svc.calls(), ["stop", "delete"]);
    }

    #[test]
    fn disconnecting_an_already_stopped_service_only_removes_it() {
        let svc = FakeService::in_state(State::Stopped);
        assert_eq!(stop_and_delete(&svc, "office", SHORT), Ok(()));
        assert_eq!(svc.calls(), ["stop", "delete"]);
    }

    #[test]
    fn disconnect_times_out_but_still_marks_the_service_for_deletion() {
        let mut svc = FakeService::in_state(State::Running);
        svc.on_stop = Reaction::Stuck(State::StopPending);
        let e = stop_and_delete(&svc, "office", SHORT).unwrap_err();
        assert!(e.contains("office"), "{e}");
        assert_eq!(svc.calls(), ["stop", "delete"], "удаление помечено и у не остановившейся: иначе имя занято навсегда");
    }

    #[test]
    fn disconnect_reports_a_failed_deletion_of_a_stopped_service() {
        let mut svc = FakeService::in_state(State::Running);
        svc.delete_error = Some("DeleteService office: denied");
        assert_eq!(stop_and_delete(&svc, "office", SHORT), Err("DeleteService office: denied".to_string()));
    }
}
