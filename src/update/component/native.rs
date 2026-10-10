//! Оригинальный AmneziaWG (MSI): копия — установщик своей версии и туннели; возврат — удаление текущей версии и
//! установка из копии с туннелями обеих копий. msiexec ждётся со сроком и с продлением аренды ядра; туннели возврата
//! лежат в хранилище до конца работы, чтобы оборванный возврат можно было доделать при следующем запуске.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{ComponentOps, Journal, RestoreJob};
use crate::events::Severity;
use crate::i18n::{tr, trf};
use crate::update::busy::Busy;
use crate::update::backup::{check_msi, merge_configs, CONFIGS, INSTALLER};
use crate::update::core_link::CoreLink;
use crate::update::jsonstore::move_file;
use crate::update::manager::{Fetched, Manager, BACKUPS};
use crate::update::{native, Action, Component};

/// Аренда туннелей на время работы MSI. Истекает сама: работа зависла или менеджер пропал — надзор ядра через
/// столько снова поднимает желаемые туннели. Пока msiexec идёт, аренда продлевается (`RENEW_EVERY`): срок — это
/// окно, в котором VPN лежит после смерти агента, а не предел длительности установки.
const HOLD_LEASE: Duration = Duration::from_secs(15 * 60);
/// Продление аренды, пока msiexec идёт: три попытки до истечения `HOLD_LEASE`, одна неудача ядра не роняет аренду.
const RENEW_EVERY: Duration = Duration::from_secs(5 * 60);
/// Дольше msiexec не ждётся: верхняя граница аренды ядра (`retry::MAX_LEASE`) та же, дольше туннели без надзора
/// не держатся. Зависший установщик (ждёт чужую установку, повис в своём действии) — ошибка работы, процесс не
/// убивается: прерванная посередине транзакция установщика хуже лишнего процесса.
const MSI_DEADLINE: Duration = Duration::from_secs(60 * 60);
/// Опрос процесса msiexec.
const POLL: Duration = Duration::from_millis(500);
/// Папка хранилища с туннелями идущего возврата (`<папка>\configs`). Есть при открытии менеджера — возврат оборван
/// до того, как туннели вернулись в AmneziaWG (`recover`).
const RESTORE: &str = "restore";

/// Запущенный msiexec.
pub(in crate::update) trait MsiChild {
    /// `Ok(Some(code))` — завершился (код выхода, `None` — без кода); `Ok(None)` — ещё идёт.
    fn try_wait(&mut self) -> Result<Option<Option<i32>>, String>;
}

impl MsiChild for std::process::Child {
    fn try_wait(&mut self) -> Result<Option<Option<i32>>, String> {
        std::process::Child::try_wait(self).map(|s| s.map(|s| s.code())).map_err(|e| format!("msiexec: {e}"))
    }
}

/// Машина с AmneziaWG: что из неё нужно компоненту. Настоящая — `Windows`; в тестах — подделка, чтобы ни один тест
/// не трогал установленный AmneziaWG и его туннели.
pub(in crate::update) trait Host: Send + Sync {
    fn installed(&self) -> Option<native::Installed>;
    /// Папка туннелей AmneziaWG (`*.conf.dpapi`).
    fn config_dir(&self) -> PathBuf;
    /// Работает ли окно AmneziaWG в сеансе пользователя. Смотрится прямо перед MSI: он идёт 1–3 с и закрывает окно
    /// в самом начале, окно программы со своим опросом раз в секунду этого не застаёт.
    fn ui_open(&self) -> bool;
    /// Установщик из копии годится для версии `version`: подпись, UpgradeCode, ProductVersion.
    fn check_backup_msi(&self, msi: &Path, version: &str) -> Result<(), String>;
    fn spawn_msiexec(&self, args: &[String]) -> Result<Box<dyn MsiChild>, String>;
    /// Идёт установщик Windows (`win::installer_running`): msiexec умершего агента живёт в задании ядра и доделывает
    /// удаление AmneziaWG уже при новом агенте.
    fn installer_running(&self) -> bool;
    fn now(&self) -> Instant;
    fn sleep(&self, d: Duration);
}

struct Windows;

impl Host for Windows {
    fn installed(&self) -> Option<native::Installed> {
        native::installed()
    }
    fn config_dir(&self) -> PathBuf {
        native::config_dir()
    }
    fn ui_open(&self) -> bool {
        native::ui_open()
    }
    fn check_backup_msi(&self, msi: &Path, version: &str) -> Result<(), String> {
        check_msi(native::verify_msi(msi), native::msi_identity(msi), version, "updm.wrong_msi_version")
    }
    fn spawn_msiexec(&self, args: &[String]) -> Result<Box<dyn MsiChild>, String> {
        native::spawn_msiexec(args).map(|c| Box::new(c) as Box<dyn MsiChild>)
    }
    fn installer_running(&self) -> bool {
        crate::win::installer_running()
    }
    fn now(&self) -> Instant {
        Instant::now()
    }
    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

pub(in crate::update) struct NativeOps {
    /// Туннели трогает только ядро: MSI убирает их службы, ядро на это время снимает их с надзора.
    pub(in crate::update) core: Arc<dyn CoreLink>,
    pub(in crate::update) host: Arc<dyn Host>,
    /// Доделка оборванного возврата отложена: шёл установщик Windows (`recover`); такт менеджера повторяет её.
    deferred: AtomicBool,
}

/// Релиз оригинального AmneziaWG, соответствующий установленной версии: метка без `v` и с ней.
fn installed_release(m: &Manager, version: &str) -> Result<crate::update::feed::Release, String> {
    m.sources.native_by_tag(version).or_else(|e| m.sources.native_by_tag(&format!("v{version}")).map_err(|_| e))
}

impl ComponentOps for NativeOps {
    fn name(&self) -> String {
        tr("updm.native")
    }

    fn installed(&self) -> Option<String> {
        self.host.installed().map(|i| i.version)
    }

    fn found(&self, f: &Fetched) -> Result<String, String> {
        f.native.as_ref().map(|r| r.version.clone()).map_err(String::clone)
    }

    fn release_date(&self, m: &Manager, version: &str) -> Result<Option<u64>, String> {
        installed_release(m, version).map(|rel| Some(rel.published))
    }

    fn backup_into(&self, m: &Manager, version: &str, dir: &Path) -> Result<(), String> {
        let rel = installed_release(m, version)?;
        let msi = m.download_msi(&rel, version)?;
        move_file(&msi, &dir.join(INSTALLER))?;
        native::copy_configs(&self.host.config_dir(), &dir.join(CONFIGS)).map(drop)
    }

    fn update(&self, m: &Manager, f: &Fetched, to: &str, id: u64) -> Result<(), String> {
        let rel = f.native.as_ref().map_err(String::clone)?;
        let msi = m.download_msi(rel, to)?;
        m.set_busy(Busy::Install { what: Component::Native, version: to.to_string() });
        m.note_native_ui(self.host.ui_open());
        self.during_msi(m, |run| run.msiexec(&native::install_args(&msi, &m.log_path(id, "install"))))
    }

    /// Возврат к копии `job.name`. `job.current_backup` — копия состояния перед возвратом (её только что сделал
    /// менеджер): удаление AmneziaWG стирает его папку данных, поэтому туннели берутся из неё, а из старой копии —
    /// только те, которых в текущем наборе нет. Старая версия не встала — ставится обратно прежняя со своими
    /// туннелями. Туннели режима 1, работавшие до возврата, подключает снова ядро (`during_msi`).
    /// Слитые туннели лежат в `RESTORE` (не в `downloads`, которые чистятся при каждом запуске) до тех пор, пока не
    /// вернутся в AmneziaWG: процесс умер между удалением и возвратом — их доделает `recover` при следующем запуске.
    fn restore(&self, m: &Manager, job: &RestoreJob) -> Result<(), String> {
        let (id, name, dir) = (job.id, job.name, job.dir);
        let msi = dir.join(INSTALLER);
        if !msi.is_file() {
            return Err(trf("updm.bad_backup", &[name, INSTALLER]));
        }
        // Всё, что может не сойтись, — до удаления текущей версии.
        self.host.check_backup_msi(&msi, &job.info.version).map_err(|e| trf("updm.bad_backup", &[name, &e]))?;
        let current = job.current_backup.map(|n| m.dir.join(BACKUPS).join(n));
        let staged = self.staged(m);
        let has_configs = merge_configs(current.as_ref().map(|d| d.join(CONFIGS)).as_deref(), &dir.join(CONFIGS), &staged)?;
        m.note_native_ui(self.host.ui_open());
        self.during_msi(m, |run| {
            if let Some(i) = self.host.installed() {
                run.msiexec(&native::uninstall_args(&i.product_code, &m.log_path(id, "uninstall")))?;
            }
            if let Err(e) = run.msiexec(&native::install_args(&msi, &m.log_path(id, "install"))) {
                let Some(prev) = current else { return Err(e) };
                return Err(match run.reinstall(id, &prev) {
                    Ok(()) => {
                        // Прежние туннели на месте вместе с прежней версией: слитый набор больше не нужен.
                        self.unstage(m);
                        trf("updm.rolled_back", &[&e])
                    }
                    Err(e2) => trf("updm.rollback_failed", &[&e, &e2, &prev.display().to_string()]),
                });
            }
            if has_configs {
                native::copy_configs(&staged, &self.host.config_dir())?;
                self.unstage(m);
            }
            Ok(())
        })
    }

    /// Удаление AmneziaWG стирает его данные посередине возврата: строка истории — до работы, с отметкой.
    fn journal(&self, action: Action) -> Journal {
        if action == Action::Restore {
            Journal::Started
        } else {
            Journal::After
        }
    }

    /// Туннели оборванного возврата (`RESTORE` остался): AmneziaWG стоит — копируются в его папку, и остаток
    /// убирается; не стоит (процесс умер между удалением и установкой) — остаются до следующего запуска, строка
    /// говорит, где они и что делать. Без этого надзор после аренды счёл бы туннели исчезнувшими из AmneziaWG и
    /// вывел бы их из желаемого набора.
    ///
    /// Идёт установщик Windows — ничего не копируется: msiexec умершего агента (`spawn_msiexec` без выхода из задания)
    /// доделывает удаление AmneziaWG, пока новый агент уже открыл менеджер; запись в реестре пропадает последней, и
    /// скопированные сейчас туннели удаление стёрло бы вместе с папкой данных, а остаток был бы уже убран. Остаток
    /// лежит, доделку повторяет такт менеджера (`recover_deferred`); строка — один раз.
    fn recover(&self, m: &Manager) -> Result<Vec<String>, String> {
        let staged = self.staged(m);
        if !staged.is_dir() {
            self.deferred.store(false, Ordering::SeqCst);
            return Ok(Vec::new());
        }
        if self.host.installer_running() {
            let first = !self.deferred.swap(true, Ordering::SeqCst);
            return Ok(if first { vec![trf("updm.restore_configs_wait_installer", &[&staged.display().to_string()])] } else { Vec::new() });
        }
        self.deferred.store(false, Ordering::SeqCst);
        if self.host.installed().is_none() {
            return Ok(vec![trf("updm.restore_configs_kept", &[&staged.display().to_string()])]);
        }
        let to = self.host.config_dir();
        let count = native::copy_configs(&staged, &to)?;
        let root = m.dir.join(RESTORE);
        std::fs::remove_dir_all(&root).map_err(|e| crate::fsutil::io_ctx(&root, e))?;
        Ok(vec![trf("updm.restore_configs_recovered", &[&count.to_string(), &to.display().to_string()])])
    }

    fn recover_deferred(&self) -> bool {
        self.deferred.load(Ordering::SeqCst)
    }
}

impl NativeOps {
    pub(in crate::update) fn new(core: Arc<dyn CoreLink>, host: Arc<dyn Host>) -> NativeOps {
        NativeOps { core, host, deferred: AtomicBool::new(false) }
    }

    /// Папка слитых туннелей идущего возврата.
    fn staged(&self, m: &Manager) -> PathBuf {
        m.dir.join(RESTORE).join(CONFIGS)
    }

    /// Туннели возврата вернулись в AmneziaWG: остаток убирается, иначе следующий запуск копировал бы их снова.
    /// Не убрался — предупреждение, не ошибка возврата: повторная копия тех же файлов безвредна.
    fn unstage(&self, m: &Manager) {
        let root = m.dir.join(RESTORE);
        if let Err(e) = std::fs::remove_dir_all(&root) {
            if root.exists() {
                m.shared.log("", Severity::Warn, &crate::fsutil::io_ctx(&root, e));
            }
        }
    }

    /// Работа MSI над туннелями режима 1: их службы MSI убирает, поэтому на время работы ядро снимает туннели с
    /// надзора (иначе надзор пересоздавал бы службы наперегонки с MSI, а после — счёл бы их отключёнными в окне
    /// AmneziaWG) и показывает занятыми. Какие туннели, решает ядро: у менеджера в агенте своих сведений о них нет.
    /// Пока msiexec идёт, аренда продлевается (`MsiRun`). После — аренда возвращается при любом исходе, и
    /// неработающие желаемые ядро подключает само, обычным переключением под своей блокировкой. Ядро недоступно или
    /// аренду не дало — MSI не запускается: туннели без аренды выпали бы из желаемого набора, а гонка с надзором
    /// хуже отложенного обновления.
    fn during_msi(&self, m: &Manager, work: impl FnOnce(&MsiRun) -> Result<(), String>) -> Result<(), String> {
        let tunnels = self.core.hold_native(HOLD_LEASE).map_err(|e| trf("updm.hold_failed", &[&e]))?;
        let run = MsiRun { ops: self, m, held: Mutex::new(tunnels.clone()), leased: !tunnels.is_empty() };
        if tunnels.is_empty() {
            return work(&run);
        }
        let result = work(&run);
        let held = run.held.into_inner().unwrap_or_else(|p| p.into_inner());
        // Аренда не вернулась — она истечёт сама, и надзор поднимет туннели позже: это ошибка работы, но не её отмена.
        match (result, self.core.release(&held)) {
            (result, Ok(())) => result,
            (Ok(()), Err(e)) => Err(trf("updm.release_failed", &[&e])),
            (Err(work), Err(e)) => Err(format!("{work}; {}", trf("updm.release_failed", &[&e]))),
        }
    }
}

/// Запуски msiexec одной работы под арендой ядра: ожидание со сроком `MSI_DEADLINE` и продлением аренды каждые
/// `RENEW_EVERY`, пока процесс идёт.
struct MsiRun<'a> {
    ops: &'a NativeOps,
    m: &'a Manager,
    /// Взятые ядром туннели, с каждым продлением — объединение: вернуть надо все.
    held: Mutex<Vec<String>>,
    /// Аренда есть (режим 1 с туннелями); без неё продлевать нечего.
    leased: bool,
}

impl MsiRun<'_> {
    /// msiexec с `args` до конца или до `MSI_DEADLINE`; по сроку — ошибка, процесс остаётся (журнал установщика назван
    /// в `args` после `/l*v`).
    fn msiexec(&self, args: &[String]) -> Result<(), String> {
        let host = &*self.ops.host;
        let mut child = host.spawn_msiexec(args)?;
        let started = host.now();
        let mut renew_at = started + RENEW_EVERY;
        loop {
            if let Some(code) = child.try_wait()? {
                return native::exit_result(code);
            }
            let now = host.now();
            if now.duration_since(started) >= MSI_DEADLINE {
                let log = args.iter().skip_while(|a| a.as_str() != "/l*v").nth(1).cloned().unwrap_or_default();
                return Err(trf("updm.msi_timeout", &[&(MSI_DEADLINE.as_secs() / 60).to_string(), &log]));
            }
            if self.leased && now >= renew_at {
                self.renew();
                renew_at = now + RENEW_EVERY;
            }
            host.sleep(POLL);
        }
    }

    /// Продление аренды тем же `HoldNative`: ядро отдаёт работающие и желаемые — взятые раньше среди них. Не вышло —
    /// предупреждение в журнал, аренда ещё идёт, следующее продление через `RENEW_EVERY`.
    fn renew(&self) {
        match self.ops.core.hold_native(HOLD_LEASE) {
            Ok(tunnels) => {
                let mut held = self.held.lock().unwrap_or_else(|p| p.into_inner());
                for t in tunnels {
                    if !held.contains(&t) {
                        held.push(t);
                    }
                }
            }
            Err(e) => self.m.shared.log("", Severity::Warn, &trf("updm.renew_failed", &[&e])),
        }
    }

    /// Установка обратно версии из копии `dir` (сделанной перед возвратом) вместе с её туннелями.
    fn reinstall(&self, id: u64, dir: &Path) -> Result<(), String> {
        self.msiexec(&native::install_args(&dir.join(INSTALLER), &self.m.log_path(id, "reinstall")))?;
        let configs = dir.join(CONFIGS);
        if configs.is_dir() {
            native::copy_configs(&configs, &self.ops.host.config_dir())?;
        }
        Ok(())
    }
}

/// Настоящие компоненты AmneziaWG на этой машине.
pub(super) fn real(core: Arc<dyn CoreLink>) -> NativeOps {
    NativeOps::new(core, Arc::new(Windows))
}

#[cfg(test)]
mod tests {
    use std::panic::{catch_unwind, AssertUnwindSafe};

    use super::*;
    use crate::update::backup::BackupInfo;
    use crate::update::core_link::fake::RecordingCore;
    use crate::update::jsonstore::temp;

    /// Ядро режима 1, у которого работают или желаемы `a` и `b`.
    fn native_core() -> Arc<RecordingCore> {
        Arc::new(RecordingCore { native: vec!["a".into(), "b".into()], ..Default::default() })
    }

    /// Поддельная машина: установленная версия, своя папка туннелей во временной папке, поддельные часы (сон
    /// двигает время) и msiexec, который удаляет/ставит AmneziaWG по аргументам. `hang` — msiexec не завершается;
    /// `die_on_install` — процесс «умирает» (паника) на установке, после удаления.
    struct FakeHost {
        installed: Mutex<Option<String>>,
        configs: PathBuf,
        now: Mutex<Instant>,
        spawned: Mutex<Vec<String>>,
        hang: bool,
        die_on_install: bool,
        /// Установщик Windows (чужой или msiexec умершего агента) работает до этого момента поддельных часов.
        installer_until: Mutex<Option<Instant>>,
    }

    impl FakeHost {
        fn new(dir: &Path, installed: Option<&str>) -> Arc<FakeHost> {
            let configs = dir.join("awg-configs");
            std::fs::create_dir_all(&configs).unwrap();
            Arc::new(FakeHost {
                installed: Mutex::new(installed.map(str::to_string)),
                configs,
                now: Mutex::new(Instant::now()),
                spawned: Mutex::new(Vec::new()),
                hang: false,
                die_on_install: false,
                installer_until: Mutex::new(None),
            })
        }
        fn spawned(&self) -> Vec<String> {
            self.spawned.lock().unwrap().clone()
        }
    }

    /// Завершённый msiexec с кодом 0 либо (`hang`) не завершающийся никогда либо (`die`) паника при опросе.
    struct FakeChild {
        hang: bool,
        die: bool,
    }

    impl MsiChild for FakeChild {
        fn try_wait(&mut self) -> Result<Option<Option<i32>>, String> {
            if self.die {
                panic!("agent killed while msiexec runs");
            }
            Ok(if self.hang { None } else { Some(Some(0)) })
        }
    }

    impl Host for FakeHost {
        fn installed(&self) -> Option<native::Installed> {
            self.installed.lock().unwrap().clone().map(|version| native::Installed { version, product_code: "{P}".into() })
        }
        fn config_dir(&self) -> PathBuf {
            self.configs.clone()
        }
        fn ui_open(&self) -> bool {
            false
        }
        fn check_backup_msi(&self, _msi: &Path, _version: &str) -> Result<(), String> {
            Ok(())
        }
        fn spawn_msiexec(&self, args: &[String]) -> Result<Box<dyn MsiChild>, String> {
            self.spawned.lock().unwrap().push(args[..2].join(" "));
            let mut die = false;
            match args[0].as_str() {
                // Удаление стирает папку данных AmneziaWG вместе с туннелями.
                "/x" => {
                    *self.installed.lock().unwrap() = None;
                    let _ = std::fs::remove_dir_all(&self.configs);
                }
                "/i" if self.die_on_install => die = true,
                "/i" => *self.installed.lock().unwrap() = Some("installed".into()),
                a => panic!("unexpected msiexec action {a}"),
            }
            Ok(Box::new(FakeChild { hang: self.hang, die }))
        }
        fn installer_running(&self) -> bool {
            self.installer_until.lock().unwrap().is_some_and(|until| self.now() < until)
        }
        fn now(&self) -> Instant {
            *self.now.lock().unwrap()
        }
        fn sleep(&self, d: Duration) {
            *self.now.lock().unwrap() += d;
        }
    }

    fn manager(dir: &Path) -> Arc<Manager> {
        Manager::for_tests(dir, super::super::Components::real(Arc::new(RecordingCore::default())))
    }

    fn names(dir: &Path) -> Vec<String> {
        let Ok(list) = std::fs::read_dir(dir) else { return Vec::new() };
        let mut v: Vec<String> = list.map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        v.sort();
        v
    }

    /// Копия `name` с установщиком и туннелями `configs`.
    fn backup(m: &Manager, name: &str, configs: &[&str]) {
        let dir = m.dir.join(BACKUPS).join(name);
        std::fs::create_dir_all(dir.join(CONFIGS)).unwrap();
        std::fs::write(dir.join(INSTALLER), b"msi").unwrap();
        for c in configs {
            std::fs::write(dir.join(CONFIGS).join(c), format!("{name}:{c}")).unwrap();
        }
    }

    fn restore(ops: &NativeOps, m: &Manager) -> Result<(), String> {
        let info = BackupInfo { component: Component::Native, version: "1.0".into() };
        let job = RestoreJob { id: 3, name: "3-native-1.0", dir: &m.dir.join(BACKUPS).join("3-native-1.0"), info: &info, current_backup: Some("4-native-1.1") };
        ops.restore(m, &job)
    }

    /// Туннели на время MSI — только через ядро и только те, что назвало ядро: аренда до работы, возврат после (и при
    /// ошибке MSI тоже). Менеджер живёт в агенте без хоста туннелей и без снимка опроса (`agent::updates::start`), так
    /// что набор не может браться из его собственного состояния — иначе MSI шёл бы без аренды и VPN не возвращался.
    #[test]
    fn msi_runs_under_a_core_hold_of_the_tunnels_the_core_names() {
        let dir = temp("native-hold");
        let m = manager(&dir);
        for outcome in [Ok(()), Err("msiexec 1603".to_string())] {
            let core = native_core();
            let ops = NativeOps::new(core.clone(), FakeHost::new(&dir, Some("1.1")));
            let during = Mutex::new(Vec::new());
            let r = ops.during_msi(&m, |_| {
                during.lock().unwrap().extend(core.calls());
                outcome.clone()
            });
            assert_eq!(r, outcome);
            assert_eq!(*during.lock().unwrap(), ["hold 900"], "MSI идёт уже под арендой");
            assert_eq!(core.calls(), ["hold 900", "release a,b"], "возвращаются ровно взятые ядром");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Ядро недоступно или аренду не дало — MSI не запускается, и ошибка говорит, почему.
    #[test]
    fn msi_is_not_run_without_the_hold() {
        let dir = temp("native-nohold");
        let m = manager(&dir);
        let core = Arc::new(RecordingCore { refuse_hold: true, ..Default::default() });
        let mut ran = false;
        let r = NativeOps::new(core.clone(), FakeHost::new(&dir, Some("1.1"))).during_msi(&m, |_| {
            ran = true;
            Ok(())
        });
        assert_eq!(r, Err(trf("updm.hold_failed", &["core: stopped"])), "без аренды MSI гонялся бы с надзором");
        assert!(!ran);
        assert_eq!(core.calls(), ["hold 900"], "возвращать нечего");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Режим 2 (или в режиме 1 туннелей нет): ядро ничего не взяло — MSI идёт, возвращать и продлевать нечего.
    #[test]
    fn nothing_held_needs_no_release() {
        let dir = temp("native-mode2");
        let m = manager(&dir);
        let core = Arc::new(RecordingCore::default());
        let host = FakeHost::new(&dir, Some("1.1"));
        let ops = NativeOps::new(core.clone(), host.clone());
        assert_eq!(ops.during_msi(&m, |run| run.msiexec(&native::install_args(Path::new("x.msi"), Path::new("x.log")))), Ok(()));
        assert_eq!(core.calls(), ["hold 900"]);
        assert_eq!(host.spawned(), ["/i x.msi"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Зависший msiexec: аренда продлевается каждые 5 минут (иначе через 15 минут надзор пересоздавал бы службы под
    /// работающим установщиком), через 60 минут — понятная ошибка с журналом установщика, аренда возвращена ядру
    /// (VPN поднимается), процесс не убит; работа не остаётся «занято» навсегда.
    #[test]
    fn hung_msiexec_renews_the_lease_then_fails_at_the_deadline_and_releases() {
        let dir = temp("native-hang");
        let m = manager(&dir);
        let core = native_core();
        let mut host = FakeHost::new(&dir, Some("1.1"));
        Arc::get_mut(&mut host).unwrap().hang = true;
        let ops = NativeOps::new(core.clone(), host.clone());
        let t0 = host.now();
        let r = ops.during_msi(&m, |run| run.msiexec(&native::install_args(Path::new("x.msi"), Path::new(r"C:\l\7-install.log"))));
        assert_eq!(r, Err(trf("updm.msi_timeout", &["60", r"C:\l\7-install.log"])));
        let waited = host.now().duration_since(t0);
        assert!(waited >= MSI_DEADLINE && waited < MSI_DEADLINE + Duration::from_secs(5), "{waited:?}");
        let calls = core.calls();
        let holds = calls.iter().filter(|c| *c == "hold 900").count();
        assert_eq!(holds, 1 + 11, "первая аренда и продления на 5, 10, …, 55-й минуте: {calls:?}");
        assert_eq!(calls.last().map(String::as_str), Some("release a,b"), "аренда возвращена по сроку");
        assert_eq!(host.spawned(), ["/i x.msi"], "процесс один, не перезапускается");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Ядро не продлило аренду — предупреждение в журнал и работа идёт дальше: аренда ещё действует.
    #[test]
    fn failed_renewal_is_logged_not_fatal() {
        let dir = temp("native-renew-fail");
        let m = manager(&dir);
        let core = Arc::new(RecordingCore { native: vec!["a".into()], ..Default::default() });
        let mut host = FakeHost::new(&dir, Some("1.1"));
        Arc::get_mut(&mut host).unwrap().hang = true;
        let ops = NativeOps::new(core.clone(), host.clone());
        let run = MsiRun { ops: &ops, m: &m, held: Mutex::new(vec!["a".into()]), leased: true };
        // Первое продление проходит, после него ядро «пропадает».
        run.renew();
        core.refuse_hold_now();
        run.renew();
        assert_eq!(*run.held.lock().unwrap(), ["a"]);
        let logged: Vec<String> = m.shared.events_since(0).into_iter().map(|(_, e)| e.text).collect();
        assert!(logged.iter().any(|t| t.contains(&trf("updm.renew_failed", &["core: stopped"]))), "{logged:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Возврат целиком на поддельной машине: удаление, установка, туннели — текущий набор плюс те, что были только в
    /// старой копии; остаток `restore\configs` убран, аренда возвращена.
    #[test]
    fn restore_uninstalls_installs_and_puts_merged_tunnels_back() {
        let dir = temp("native-restore");
        let m = manager(&dir);
        let host = FakeHost::new(&dir, Some("1.1"));
        std::fs::write(host.configs.join("cur.conf.dpapi"), b"cur-now").unwrap();
        backup(&m, "3-native-1.0", &["old.conf.dpapi", "cur.conf.dpapi"]);
        backup(&m, "4-native-1.1", &["cur.conf.dpapi"]);
        let core = native_core();
        let ops = NativeOps::new(core.clone(), host.clone());
        assert_eq!(restore(&ops, &m), Ok(()));
        assert_eq!(host.spawned(), ["/x {P}".to_string(), format!("/i {}", m.dir.join(BACKUPS).join("3-native-1.0").join(INSTALLER).display())]);
        assert_eq!(names(&host.configs), ["cur.conf.dpapi", "old.conf.dpapi"]);
        assert_eq!(std::fs::read(host.configs.join("cur.conf.dpapi")).unwrap(), b"4-native-1.1:cur.conf.dpapi", "текущий набор главнее старой копии");
        assert!(!m.dir.join(RESTORE).exists(), "туннели вернулись — остатка нет");
        assert_eq!(core.calls(), ["hold 900", "release a,b"]);
        assert!(ops.recover(&m).unwrap().is_empty(), "доделывать нечего");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Процесс умер после удаления AmneziaWG (его папка данных стёрта): слитые туннели лежат в хранилище, не в
    /// `downloads`. Следующее открытие: AmneziaWG ещё не стоит — остаются с понятной строкой; стоит (поставили
    /// вручную или установка всё же прошла) — копируются в его папку, остаток убран. Раньше `clean_downloads`
    /// стирал их, и надзор после аренды выводил туннели из желаемого набора.
    #[test]
    fn restore_interrupted_after_uninstall_is_finished_at_the_next_open() {
        let dir = temp("native-interrupted");
        let m = manager(&dir);
        let mut host = FakeHost::new(&dir, Some("1.1"));
        Arc::get_mut(&mut host).unwrap().die_on_install = true;
        std::fs::write(host.configs.join("cur.conf.dpapi"), b"cur").unwrap();
        backup(&m, "3-native-1.0", &["old.conf.dpapi"]);
        backup(&m, "4-native-1.1", &["cur.conf.dpapi"]);
        let ops = NativeOps::new(native_core(), host.clone());
        let died = catch_unwind(AssertUnwindSafe(|| restore(&ops, &m)));
        assert!(died.is_err(), "процесс «умер» на установке");
        assert!(host.installed().is_none() && !host.configs.exists(), "AmneziaWG удалён вместе с туннелями");
        let staged = m.dir.join(RESTORE).join(CONFIGS);
        assert_eq!(names(&staged), ["cur.conf.dpapi", "old.conf.dpapi"]);

        // Новый агент, AmneziaWG не установлен: туннели не теряются, строка говорит, где они.
        let lines = ops.recover(&m).unwrap();
        assert_eq!(lines, [trf("updm.restore_configs_kept", &[&staged.display().to_string()])]);
        assert_eq!(names(&staged), ["cur.conf.dpapi", "old.conf.dpapi"]);
        assert!(!host.configs.exists(), "в несуществующий AmneziaWG ничего не пишется");

        // AmneziaWG поставлен: туннели на месте, остаток убран, повторное открытие ничего не делает.
        *host.installed.lock().unwrap() = Some("1.0".into());
        let lines = ops.recover(&m).unwrap();
        assert_eq!(lines, [trf("updm.restore_configs_recovered", &["2", &host.configs.display().to_string()])]);
        assert_eq!(names(&host.configs), ["cur.conf.dpapi", "old.conf.dpapi"]);
        assert!(!m.dir.join(RESTORE).exists());
        assert!(ops.recover(&m).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Агент умер сразу после запуска `msiexec /x`: процесс удаления живёт в задании ядра и ещё идёт, когда новый
    /// агент открывает менеджер (запись об установке пропадает последней). Копировать туннели сейчас нельзя —
    /// удаление стёрло бы их вместе с папкой данных, а остаток был бы уже убран: доделка откладывается (строка один
    /// раз), остаток цел, менеджер повторяет её на такте; установщик закончил — обычный путь (AmneziaWG нет — лежат;
    /// поставлен — копируются).
    #[test]
    fn recover_waits_for_a_running_installer_and_keeps_the_staged_tunnels() {
        let dir = temp("native-recover-msi");
        let m = manager(&dir);
        let host = FakeHost::new(&dir, Some("1.1"));
        std::fs::write(host.configs.join("cur.conf.dpapi"), b"cur").unwrap();
        let ops = NativeOps::new(native_core(), host.clone());
        let staged = m.dir.join(RESTORE).join(CONFIGS);
        std::fs::create_dir_all(&staged).unwrap();
        for c in ["cur.conf.dpapi", "old.conf.dpapi"] {
            std::fs::write(staged.join(c), c).unwrap();
        }
        *host.installer_until.lock().unwrap() = Some(host.now() + Duration::from_secs(3));

        let lines = ops.recover(&m).unwrap();
        assert_eq!(lines, [trf("updm.restore_configs_wait_installer", &[&staged.display().to_string()])]);
        assert!(ops.recover_deferred(), "менеджер должен повторить доделку на такте");
        assert_eq!(names(&staged), ["cur.conf.dpapi", "old.conf.dpapi"], "остаток цел");
        assert_eq!(names(&host.configs), ["cur.conf.dpapi"], "в папку AmneziaWG под идущим удалением ничего не пишется");
        host.sleep(Duration::from_secs(1));
        assert!(ops.recover(&m).unwrap().is_empty(), "повтор под тем же установщиком молчит");
        assert!(ops.recover_deferred());

        // Удаление закончилось: AmneziaWG нет, его папка стёрта — туннели лежат с обычной строкой, отметка снята.
        host.sleep(Duration::from_secs(3));
        *host.installed.lock().unwrap() = None;
        std::fs::remove_dir_all(&host.configs).unwrap();
        let lines = ops.recover(&m).unwrap();
        assert_eq!(lines, [trf("updm.restore_configs_kept", &[&staged.display().to_string()])]);
        assert!(!ops.recover_deferred());
        assert_eq!(names(&staged), ["cur.conf.dpapi", "old.conf.dpapi"]);

        // AmneziaWG поставлен заново — туннели на месте, остаток убран.
        *host.installed.lock().unwrap() = Some("1.0".into());
        let lines = ops.recover(&m).unwrap();
        assert_eq!(lines, [trf("updm.restore_configs_recovered", &["2", &host.configs.display().to_string()])]);
        assert_eq!(names(&host.configs), ["cur.conf.dpapi", "old.conf.dpapi"]);
        assert!(!m.dir.join(RESTORE).exists());
        assert!(!ops.recover_deferred());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
