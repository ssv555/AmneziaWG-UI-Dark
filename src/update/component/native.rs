//! Оригинальный AmneziaWG (MSI): копия — установщик своей версии и туннели; возврат — удаление текущей версии и
//! установка из копии с туннелями обеих копий.

use std::path::Path;

use super::{ComponentOps, Journal, RestoreJob};
use crate::events::Severity;
use crate::i18n::{tr, trf};
use crate::monitor::Shared;
use crate::update::busy::Busy;
use crate::update::backup::{check_msi, merge_configs, CONFIGS, INSTALLER};
use crate::update::jsonstore::move_file;
use crate::update::manager::{Fetched, Manager, BACKUPS, DOWNLOADS};
use crate::update::{native, Action, Component};

pub(in crate::update) struct NativeOps;

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
        native::install(&msi, &m.log_path(id, "install"))
    }

    /// Возврат к копии `job.name`. `job.current_backup` — копия состояния перед возвратом (её только что сделал
    /// менеджер): удаление AmneziaWG стирает его папку данных, поэтому туннели берутся из неё, а из старой копии —
    /// только те, которых в текущем наборе нет. Старая версия не встала — ставится обратно прежняя со своими
    /// туннелями. Туннели режима 1, работавшие до возврата, подключаются снова.
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
        let running = running_native(&m.shared);
        if let Some(i) = native::installed() {
            native::uninstall(&i.product_code, &m.log_path(id, "uninstall"))?;
        }
        if let Err(e) = native::install(&msi, &m.log_path(id, "install")) {
            let Some(prev) = current else { return Err(e) };
            return Err(match reinstall(m, id, &prev) {
                Ok(()) => {
                    reconnect(&m.shared, &running);
                    trf("updm.rolled_back", &[&e])
                }
                Err(e2) => trf("updm.rollback_failed", &[&e, &e2, &prev.display().to_string()]),
            });
        }
        if has_configs {
            native::restore_configs(&staged)?;
        }
        reconnect(&m.shared, &running);
        Ok(())
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

/// Установка обратно версии из копии `dir` (сделанной перед возвратом) вместе с её туннелями.
fn reinstall(m: &Manager, id: u64, dir: &Path) -> Result<(), String> {
    native::install(&dir.join(INSTALLER), &m.log_path(id, "reinstall"))?;
    let configs = dir.join(CONFIGS);
    if configs.is_dir() {
        native::restore_configs(&configs)?;
    }
    Ok(())
}

/// Работающие туннели режима 1: их службы пропадают при удалении AmneziaWG. В режиме 2 туннели — свои.
fn running_native(shared: &Shared) -> Vec<String> {
    if !shared.host().is_some_and(|h| h.native_services()) {
        return Vec::new();
    }
    shared.running_names()
}

/// Подключить туннели снова; ошибки — в журнал событий туннеля.
fn reconnect(shared: &Shared, tunnels: &[String]) {
    let host = shared.host();
    for t in tunnels {
        let result = host.as_ref().ok_or_else(|| "core: no tunnel host".to_string()).and_then(|h| h.connect(t));
        if let Err(e) = result {
            shared.log(t, Severity::Bad, &trf("updm.reconnect_failed", &[&e]));
        }
    }
}
