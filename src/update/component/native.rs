//! Оригинальный AmneziaWG (MSI): копия — установщик своей версии и туннели; возврат — удаление текущей версии и
//! установка из копии с туннелями обеих копий.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use super::{ComponentOps, Journal, RestoreJob};
use crate::i18n::{tr, trf};
use crate::update::busy::Busy;
use crate::update::backup::{check_msi, merge_configs, CONFIGS, INSTALLER};
use crate::update::core_link::CoreLink;
use crate::update::jsonstore::move_file;
use crate::update::manager::{Fetched, Manager, BACKUPS, DOWNLOADS};
use crate::update::{native, Action, Component};

/// Аренда туннелей на время работы MSI. Истекает сама: работа зависла или менеджер пропал — надзор ядра через
/// столько снова поднимает желаемые туннели.
const HOLD_LEASE: Duration = Duration::from_secs(15 * 60);

pub(in crate::update) struct NativeOps {
    /// Туннели трогает только ядро: MSI убирает их службы, ядро на это время снимает их с надзора.
    pub(in crate::update) core: Arc<dyn CoreLink>,
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
        native::installed().map(|i| i.version)
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
        native::backup_configs(&dir.join(CONFIGS)).map(drop)
    }

    fn update(&self, m: &Manager, f: &Fetched, to: &str, id: u64) -> Result<(), String> {
        let rel = f.native.as_ref().map_err(String::clone)?;
        let msi = m.download_msi(rel, to)?;
        m.set_busy(Busy::Install { what: Component::Native, version: to.to_string() });
        self.during_msi(|| native::install(&msi, &m.log_path(id, "install")))
    }

    /// Возврат к копии `job.name`. `job.current_backup` — копия состояния перед возвратом (её только что сделал
    /// менеджер): удаление AmneziaWG стирает его папку данных, поэтому туннели берутся из неё, а из старой копии —
    /// только те, которых в текущем наборе нет. Старая версия не встала — ставится обратно прежняя со своими
    /// туннелями. Туннели режима 1, работавшие до возврата, подключает снова ядро (`during_msi`).
    fn restore(&self, m: &Manager, job: &RestoreJob) -> Result<(), String> {
        let (id, name, dir) = (job.id, job.name, job.dir);
        let msi = dir.join(INSTALLER);
        if !msi.is_file() {
            return Err(trf("updm.bad_backup", &[name, INSTALLER]));
        }
        // Всё, что может не сойтись, — до удаления текущей версии.
        check_msi(native::verify_msi(&msi), native::msi_identity(&msi), &job.info.version, "updm.wrong_msi_version")
            .map_err(|e| trf("updm.bad_backup", &[name, &e]))?;
        let current = job.current_backup.map(|n| m.dir.join(BACKUPS).join(n));
        let staged = m.dir.join(DOWNLOADS).join(CONFIGS);
        let has_configs = merge_configs(current.as_ref().map(|d| d.join(CONFIGS)).as_deref(), &dir.join(CONFIGS), &staged)?;
        self.during_msi(|| {
            if let Some(i) = native::installed() {
                native::uninstall(&i.product_code, &m.log_path(id, "uninstall"))?;
            }
            if let Err(e) = native::install(&msi, &m.log_path(id, "install")) {
                let Some(prev) = current else { return Err(e) };
                return Err(match reinstall(m, id, &prev) {
                    Ok(()) => trf("updm.rolled_back", &[&e]),
                    Err(e2) => trf("updm.rollback_failed", &[&e, &e2, &prev.display().to_string()]),
                });
            }
            if has_configs {
                native::restore_configs(&staged)?;
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
}

impl NativeOps {
    /// Работа MSI над туннелями режима 1: их службы MSI убирает, поэтому на время работы ядро снимает туннели с
    /// надзора (иначе надзор пересоздавал бы службы наперегонки с MSI, а после — счёл бы их отключёнными в окне
    /// AmneziaWG) и показывает занятыми. Какие туннели, решает ядро: у менеджера в агенте своих сведений о них нет.
    /// После — аренда возвращается при любом исходе, и неработающие желаемые ядро подключает само, обычным
    /// переключением под своей блокировкой. Ядро недоступно или аренду не дало — MSI не запускается: туннели без
    /// аренды выпали бы из желаемого набора, а гонка с надзором хуже отложенного обновления.
    fn during_msi(&self, work: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
        let tunnels = self.core.hold_native(HOLD_LEASE).map_err(|e| trf("updm.hold_failed", &[&e]))?;
        if tunnels.is_empty() {
            return work();
        }
        let result = work();
        // Аренда не вернулась — она истечёт сама, и надзор поднимет туннели позже: это ошибка работы, но не её отмена.
        match (result, self.core.release(&tunnels)) {
            (result, Ok(())) => result,
            (Ok(()), Err(e)) => Err(trf("updm.release_failed", &[&e])),
            (Err(work), Err(e)) => Err(format!("{work}; {}", trf("updm.release_failed", &[&e]))),
        }
    }
}

/// Установка обратно версии из копии `dir` (сделанной перед возвратом) вместе с её туннелями.
fn reinstall(m: &Manager, id: u64, dir: &Path) -> Result<(), String> {
    native::install(&dir.join(INSTALLER), &m.log_path(id, "reinstall"))?;
    let configs = dir.join(CONFIGS);
    if configs.is_dir() {
        native::restore_configs(&configs)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::update::core_link::fake::RecordingCore;

    /// Ядро режима 1, у которого работают или желаемы `a` и `b`.
    fn native_core() -> Arc<RecordingCore> {
        Arc::new(RecordingCore { native: vec!["a".into(), "b".into()], ..Default::default() })
    }

    /// Туннели на время MSI — только через ядро и только те, что назвало ядро: аренда до работы, возврат после (и при
    /// ошибке MSI тоже). Менеджер живёт в агенте без хоста туннелей и без снимка опроса (`agent::updates::start`), так
    /// что набор не может браться из его собственного состояния — иначе MSI шёл бы без аренды и VPN не возвращался.
    #[test]
    fn msi_runs_under_a_core_hold_of_the_tunnels_the_core_names() {
        for outcome in [Ok(()), Err("msiexec 1603".to_string())] {
            let core = native_core();
            let ops = NativeOps { core: core.clone() };
            let during = Mutex::new(Vec::new());
            let r = ops.during_msi(|| {
                during.lock().unwrap().extend(core.calls());
                outcome.clone()
            });
            assert_eq!(r, outcome);
            assert_eq!(*during.lock().unwrap(), ["hold 900"], "MSI идёт уже под арендой");
            assert_eq!(core.calls(), ["hold 900", "release a,b"], "возвращаются ровно взятые ядром");
        }
    }

    /// Ядро недоступно или аренду не дало — MSI не запускается, и ошибка говорит, почему.
    #[test]
    fn msi_is_not_run_without_the_hold() {
        let core = Arc::new(RecordingCore { refuse_hold: true, ..Default::default() });
        let mut ran = false;
        let r = NativeOps { core: core.clone() }.during_msi(|| {
            ran = true;
            Ok(())
        });
        assert_eq!(r, Err(trf("updm.hold_failed", &["core: stopped"])), "без аренды MSI гонялся бы с надзором");
        assert!(!ran);
        assert_eq!(core.calls(), ["hold 900"], "возвращать нечего");
    }

    /// Режим 2 (или в режиме 1 туннелей нет): ядро ничего не взяло — MSI идёт, возвращать нечего.
    #[test]
    fn nothing_held_needs_no_release() {
        let core = Arc::new(RecordingCore::default());
        assert_eq!(NativeOps { core: core.clone() }.during_msi(|| Ok(())), Ok(()));
        assert_eq!(core.calls(), ["hold 900"]);
    }
}
