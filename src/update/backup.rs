//! Резервные копии компонентов: папка `backups\<id>-<компонент>-<версия>\` (описание, MSI и туннели AmneziaWG,
//! наборы файлов движка и программы), загрузка и проверка MSI, слияние туннелей при возврате.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::busy::Busy;
use super::history::{backup_id, entry, History};
use super::jsonstore::{backup_name, dir_size, load_json, safe_name, save_json};
use super::manager::{Manager, BACKUPS, DOWNLOADS};
use super::{feed, native, sign};
use super::{Action, Component};
use crate::events::Severity;
use crate::i18n::{tr, trf};
use crate::monitor::{unix_now, Shared};

/// Предел загрузки MSI AmneziaWG, байт (DLL и exe наших релизов ограничивает `ours`).
const MSI_MAX: u64 = 64 * 1024 * 1024;

/// Описание копии в её папке: что за компонент и какая версия.
pub(super) const BACKUP_INFO: &str = "component.json";
/// MSI установленной версии AmneziaWG в копии.
pub(super) const INSTALLER: &str = "installer.msi";
/// Туннели AmneziaWG в копии.
pub(super) const CONFIGS: &str = "configs";

/// Описание копии (`component.json`).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub(super) struct BackupInfo {
    pub(super) component: Component,
    pub(super) version: String,
}

/// Папка копии на диске с её описанием: из неё `History::adopt` собирает строку «Резервная копия».
pub(super) struct StoredBackup {
    pub(super) name: String,
    pub(super) info: BackupInfo,
    /// Когда копия закончена: время изменения `component.json` (его `Manager::backup` пишет последним).
    pub(super) at: u64,
    pub(super) size: u64,
}

/// Папки копий, на которые не ссылается ни одна строка истории (история отодвинута как нечитаемая, строки потеряны
/// иначе), снова становятся строками (`History::adopt`): по событию в журнале на каждую. Папка без `component.json`
/// или с испорченным не трогается (в ней может быть то, что владельцу ещё нужно) и строкой не становится —
/// предупреждение с путём. Номера всех папок копий с этого момента не выдаются (`History::reserve_through`).
/// `name_of` — имя компонента для журнала. Возвращает, добавлены ли строки (тогда историю нужно сохранить).
pub(super) fn adopt_orphans(history: &mut History, backups: &Path, name_of: &dyn Fn(Component) -> String, shared: &Shared) -> bool {
    let list = match std::fs::read_dir(backups) {
        Ok(list) => list,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return false,
        Err(e) => {
            shared.log("", Severity::Bad, &crate::fsutil::io_ctx(backups, e));
            return false;
        }
    };
    // Файлы в `backups` — не копии (копия всегда папка), их не касаемся.
    let mut dirs: Vec<PathBuf> = list.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.path()).collect();
    // Порядок по номеру: свободный номер для занятого достаётся папкам одинаково при каждом открытии.
    dirs.sort_by_key(|p| (p.file_name().and_then(|n| n.to_str()).and_then(backup_id), p.clone()));
    for id in dirs.iter().filter_map(|p| p.file_name().and_then(|n| n.to_str()).and_then(backup_id)) {
        history.reserve_through(id);
    }
    let mut adopted = false;
    for dir in dirs {
        let name = dir.file_name().and_then(|n| n.to_str()).map(str::to_string);
        if name.as_deref().is_some_and(|n| history.references(n)) {
            continue;
        }
        match stored_backup(&dir, name) {
            Ok(b) => {
                let (component, version, name) = (b.info.component, b.info.version.clone(), b.name.clone());
                if let Some(id) = history.adopt(b) {
                    adopted = true;
                    shared.log("", Severity::Warn, &trf("updm.backup_adopted", &[&name, &name_of(component), &version, &id.to_string()]));
                }
            }
            Err(why) => shared.log("", Severity::Warn, &trf("updm.backup_not_adopted", &[&dir.display().to_string(), &why])),
        }
    }
    adopted
}

/// Папка копии `dir` с именем `name` (`None` — имя не UTF-8) и годным описанием.
fn stored_backup(dir: &Path, name: Option<String>) -> Result<StoredBackup, String> {
    let name = name.filter(|n| safe_name(n)).ok_or_else(|| tr("updm.backup_bad_name"))?;
    let file = dir.join(BACKUP_INFO);
    let info: BackupInfo = load_json(&file)?;
    let at = std::fs::metadata(&file)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or_else(unix_now, |d| d.as_secs());
    Ok(StoredBackup { name, info, at, size: dir_size(dir) })
}

impl Manager {
    /// Копия текущей версии `version` компонента `c` со строкой в истории: номер строки и имя папки копии; не вышло —
    /// `None` (и строка с ошибкой).
    pub(super) fn backup(&self, c: Component, version: &str) -> Option<(u64, String)> {
        let id = self.next_id();
        let name = backup_name(id, c, version);
        let dir = self.dir.join(BACKUPS).join(&name);
        self.set_busy(Busy::Backup { what: c, version: version.to_string() });
        let result = std::fs::create_dir_all(&dir)
            .map_err(|e| crate::fsutil::io_ctx(&dir, e))
            .and_then(|()| self.ops(c).backup_into(self, version, &dir))
            .and_then(|()| save_json(&dir.join(BACKUP_INFO), &BackupInfo { component: c, version: version.to_string() }))
            .map(|()| dir_size(&dir));
        if result.is_err() {
            // Недоделанная копия этой же работы — не копия. Ошибка работы ниже уходит в историю и журнал; сбой уборки
            // её не меняет, а папка с этим именем при повторе очищается заново.
            let _ = std::fs::remove_dir_all(&dir);
        }
        let mut e = entry(id, c, Action::Backup, Some(version.to_string()), None);
        match &result {
            Ok(size) => e = e.with_backup(name.clone(), *size),
            Err(err) => {
                e.finish(Err(err.clone()));
                self.shared.log("", Severity::Bad, &trf("updm.backup_failed", &[&self.name(c), err]));
            }
        }
        self.record(e);
        result.ok().map(|_| (id, name))
    }

    /// MSI версии `version` из релиза: SHA-256 из релиза обязателен и должен совпасть, подпись — AmneziaWG,
    /// UpgradeCode — amd64, ProductVersion — `version`.
    pub(super) fn download_msi(&self, rel: &feed::Release, version: &str) -> Result<PathBuf, String> {
        let file = native::msi_asset(version);
        let asset = rel.asset(&file).ok_or_else(|| trf("updm.no_asset", &[&rel.tag, &file]))?;
        let sha256 = asset.sha256.clone().ok_or_else(|| trf("updm.no_digest", &[&file]))?;
        let downloads = self.dir.join(DOWNLOADS);
        std::fs::create_dir_all(&downloads).map_err(|e| crate::fsutil::io_ctx(&downloads, e))?;
        let dest = downloads.join("native.msi");
        let written = self.sources.download(&asset.url, &dest, MSI_MAX, &mut |done, total| {
            self.set_busy(Busy::Download { what: Component::Native, version: version.to_string(), progress: progress(done, total) });
        })?;
        sign::check_file(&dest, &sign::FileEntry { name: file, sha256, size: written })?;
        check_msi(native::verify_msi(&dest), native::msi_identity(&dest), version, "updm.msi_unexpected_version")?;
        Ok(dest)
    }
}

/// Установщик годится: подпись AmneziaWG (`verified`), UpgradeCode — amd64, ProductVersion — `version` (из
/// описания копии при возврате, ожидаемая при загрузке). Иначе ничего не ставится и не удаляется; другая версия —
/// ошибка по ключу `wrong_version` («{0}» — версия установщика, «{1}» — ожидаемая).
pub(super) fn check_msi(
    verified: Result<String, String>,
    identity: Result<(String, String), String>,
    version: &str,
    wrong_version: &str,
) -> Result<(), String> {
    verified?;
    let (upgrade_code, product_version) = identity?;
    if !upgrade_code.trim().eq_ignore_ascii_case(native::UPGRADE_CODE) {
        return Err(trf("updm.wrong_upgrade_code", &[upgrade_code.trim()]));
    }
    if product_version.trim() != version.trim() {
        return Err(trf(wrong_version, &[product_version.trim(), version.trim()]));
    }
    Ok(())
}

/// Туннели для возврата AmneziaWG — в папку `to`: весь текущий набор `current` и из старой копии `old` только
/// файлы с именами, которых в текущем наборе нет. `false` — ни одного источника нет, возвращать нечего.
pub(super) fn merge_configs(current: Option<&Path>, old: &Path, to: &Path) -> Result<bool, String> {
    // Остаток прошлой работы: не убрался — в набор попали бы чужие файлы, поэтому это ошибка (нет папки — не остаток).
    if let Err(e) = std::fs::remove_dir_all(to) {
        if e.kind() != std::io::ErrorKind::NotFound {
            return Err(crate::fsutil::io_ctx(to, e));
        }
    }
    let mut any = false;
    // Сначала старая копия, поверх неё текущая: при совпадении имени (без учёта регистра) остаётся текущий файл.
    for src in [Some(old), current].into_iter().flatten() {
        if src.is_dir() {
            native::copy_configs(src, to)?;
            any = true;
        }
    }
    Ok(any)
}

fn progress(done: u64, total: Option<u64>) -> String {
    match total {
        Some(t) if t > 0 => format!("{} %", done.min(t) * 100 / t),
        _ => format!("{:.1} MB", done as f64 / (1024.0 * 1024.0)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::jsonstore::temp;

    #[test]
    fn configs_come_from_current_set_then_missing_old() {
        let dir = temp("configs");
        let (cur, old, to) = (dir.join("cur"), dir.join("old"), dir.join("to"));
        for d in [&cur, &old, &to] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(old.join("a.conf.dpapi"), b"old-a").unwrap();
        std::fs::write(old.join("B.conf.dpapi"), b"old-b").unwrap();
        std::fs::write(cur.join("b.conf.dpapi"), b"cur-b").unwrap();
        std::fs::write(cur.join("c.conf.dpapi"), b"cur-c").unwrap();
        std::fs::write(to.join("stale.conf.dpapi"), b"x").unwrap();
        assert_eq!(merge_configs(Some(&cur), &old, &to), Ok(true));
        let read = |n: &str| std::fs::read(to.join(n)).unwrap();
        assert_eq!(read("a.conf.dpapi"), b"old-a", "нет в текущем — из старой копии");
        assert_eq!(read("b.conf.dpapi"), b"cur-b", "есть в текущем (имя без учёта регистра) — текущий");
        assert_eq!(read("c.conf.dpapi"), b"cur-c");
        assert!(!to.join("stale.conf.dpapi").exists(), "остаток прошлой работы убран");
        assert_eq!(std::fs::read_dir(&to).unwrap().count(), 3);
        // Без текущей копии — старая целиком; нет ни одной — возвращать нечего.
        assert_eq!(merge_configs(None, &old, &to), Ok(true));
        assert_eq!(read("b.conf.dpapi"), b"old-b");
        assert_eq!(merge_configs(Some(&dir.join("none1")), &dir.join("none2"), &to), Ok(false));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_aborts_before_uninstall_on_bad_msi() {
        let signed = || Ok::<_, String>("Privacy Technologies OU".to_string());
        let id = |code: &str, v: &str| Ok::<_, String>((code.to_string(), v.to_string()));
        assert_eq!(check_msi(signed(), id(native::UPGRADE_CODE, "1.0.5"), "1.0.5", "updm.wrong_msi_version"), Ok(()));
        assert_eq!(check_msi(signed(), id("{876b57e4-4490-4442-a983-721ee141b00d}", "1.0.5"), "1.0.5", "updm.wrong_msi_version"), Ok(()), "регистр не важен");
        assert_eq!(check_msi(Err("unsigned".into()), id(native::UPGRADE_CODE, "1.0.5"), "1.0.5", "updm.wrong_msi_version"), Err("unsigned".to_string()));
        assert_eq!(check_msi(signed(), Err("no table".into()), "1.0.5", "updm.wrong_msi_version"), Err("no table".to_string()), "свойства не прочитаны");
        assert!(check_msi(signed(), id("{00000000-0000-0000-0000-000000000000}", "1.0.5"), "1.0.5", "updm.wrong_msi_version").is_err(), "чужой UpgradeCode");
        assert!(check_msi(signed(), id(native::UPGRADE_CODE, "1.0.4"), "1.0.5", "updm.wrong_msi_version").is_err(), "версия не та, что в копии");
        assert_eq!(
            check_msi(signed(), id(native::UPGRADE_CODE, "1.0.6"), "1.0.5", "updm.msi_unexpected_version"),
            Err(trf("updm.msi_unexpected_version", &["1.0.6", "1.0.5"])),
            "загрузка: установщик не той версии, что ожидалась"
        );
    }

    #[test]
    fn progress_is_percent_or_megabytes() {
        assert_eq!(progress(45, Some(100)), "45 %");
    }
}
