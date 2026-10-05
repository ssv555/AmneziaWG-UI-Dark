//! Резервные копии движка и сборки программы и возврат из них: файлы как есть в папке копии.

use std::path::{Path, PathBuf};

use super::fileset::{copy, install_set};
use super::fs::RealFs;
use super::{InstallTarget, APP_EXE, APP_SET, ENGINE_FILES, MANIFEST, MANIFEST_SIG, VERSION_TXT};
use crate::i18n::{tr, trf};

impl InstallTarget {
    /// Копия установленного движка (DLL и локальный манифест) в `dir`; размер копии в байтах.
    pub(super) fn backup_engine(&self, dir: &Path) -> Result<u64, String> {
        if !self.dir.join(ENGINE_FILES[0]).exists() {
            return Err(tr("updo.no_engine"));
        }
        let mut size = 0;
        for name in ENGINE_FILES.into_iter().chain([MANIFEST, MANIFEST_SIG]) {
            let from = self.dir.join(name);
            if ENGINE_FILES.contains(&name) || from.exists() {
                size += copy(&from, &dir.join(name))?;
            }
        }
        Ok(size)
    }

    /// Вернуть движок из копии `dir` (сделанной `backup_engine`).
    pub(super) fn restore_engine(&self, dir: &Path) -> Result<(), String> {
        let files: Vec<(String, PathBuf)> = ENGINE_FILES
            .into_iter()
            .chain([MANIFEST, MANIFEST_SIG])
            .filter(|n| ENGINE_FILES.contains(n) || dir.join(n).exists())
            .map(|n| (n.to_string(), dir.join(n)))
            .collect();
        install_set(&self.dir, &files, &RealFs)?;
        // Копия без манифеста — движок из сборки: снова действуют вшитые суммы.
        if !dir.join(MANIFEST).exists() {
            for name in [MANIFEST, MANIFEST_SIG] {
                let path = self.dir.join(name);
                if path.exists() {
                    std::fs::remove_file(&path).map_err(|e| crate::fsutil::io_ctx(&path, e))?;
                }
            }
        }
        Ok(())
    }

    /// Копия установленной сборки программы в `dir`; размер в байтах. Полный набор: exe, DLL движка (если стоят),
    /// локальный манифест с подписью (если есть) и версия (`version.txt`; `version` — версия работающего exe ядра).
    /// exe доверяет DLL по вшитым суммам или по манифесту — поэтому они возвращаются только вместе.
    pub(super) fn backup_app(&self, dir: &Path, version: &str) -> Result<u64, String> {
        let mut size = 0;
        for name in APP_SET {
            let from = self.dir.join(name);
            if name == APP_EXE || from.exists() {
                size += copy(&from, &dir.join(name))?;
            }
        }
        let path = dir.join(VERSION_TXT);
        std::fs::write(&path, version).map_err(|e| crate::fsutil::io_ctx(&path, e))?;
        Ok(size)
    }

    /// Вернуть сборку программы из копии `dir`.
    pub(super) fn restore_app(&self, dir: &Path) -> Result<(), String> {
        let version = read_version(dir).ok_or_else(|| trf("updo.no_version", &[&dir.display().to_string()]))?;
        let (files, remove) = app_backup_set(dir);
        self.install_app(&files, &remove, &version)
    }
}

/// Набор из копии `dir` (сделанной `backup_app`) и что убрать: копия без манифеста — сборка с вшитыми суммами,
/// локальный манифест убирается (как в `restore_engine`).
fn app_backup_set(dir: &Path) -> (Vec<(String, PathBuf)>, Vec<&'static str>) {
    let files = APP_SET
        .into_iter()
        .filter(|n| *n == APP_EXE || dir.join(n).exists())
        .map(|n| (n.to_string(), dir.join(n)))
        .collect();
    let remove = if dir.join(MANIFEST).exists() { Vec::new() } else { vec![MANIFEST, MANIFEST_SIG] };
    (files, remove)
}

/// Версия из `version.txt` в `dir`.
pub fn read_version(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join(VERSION_TXT)).ok()?;
    let v = text.trim();
    (!v.is_empty() && v.len() <= 64 && !v.contains(['\r', '\n'])).then(|| v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
        use crate::update::ours::window::old_exe;
    use crate::update::ours::testutil::*;

    #[test]
    fn engine_backup_and_restore() {
        let dir = temp("engine");
        let (inst, backup) = (dir.join("inst"), dir.join("backup"));
        std::fs::create_dir_all(&inst).unwrap();
        assert!(target(&inst).backup_engine(&backup).is_err());
        std::fs::write(inst.join("tunnel.dll"), "t1").unwrap();
        std::fs::write(inst.join("wintun.dll"), "w1").unwrap();
        assert_eq!(target(&inst).backup_engine(&backup).unwrap(), 4);
        assert!(!backup.join(MANIFEST).exists());
        // «Обновление»: новые DLL и манифест; возврат из копии без манифеста убирает и манифест.
        for (n, d) in [("tunnel.dll", "t2"), ("wintun.dll", "w2"), (MANIFEST, "{}"), (MANIFEST_SIG, "s")] {
            std::fs::write(inst.join(n), d).unwrap();
        }
        target(&inst).restore_engine(&backup).unwrap();
        assert_eq!(std::fs::read_to_string(inst.join("tunnel.dll")).unwrap(), "t1");
        assert_eq!(std::fs::read_to_string(inst.join("wintun.dll")).unwrap(), "w1");
        assert!(!inst.join(MANIFEST).exists() && !inst.join(MANIFEST_SIG).exists());
        // Копия с манифестом возвращает и его.
        let backup2 = dir.join("backup2");
        for (n, d) in [(MANIFEST, "{}"), (MANIFEST_SIG, "s")] {
            std::fs::write(inst.join(n), d).unwrap();
        }
        assert_eq!(target(&inst).backup_engine(&backup2).unwrap(), 7);
        std::fs::remove_file(inst.join(MANIFEST)).unwrap();
        target(&inst).restore_engine(&backup2).unwrap();
        assert_eq!(std::fs::read_to_string(inst.join(MANIFEST)).unwrap(), "{}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn app_backup_writes_version() {
        let dir = temp("app");
        let (inst, backup) = (dir.join("inst"), dir.join("backup"));
        std::fs::create_dir_all(&inst).unwrap();
        std::fs::write(inst.join(APP_EXE), "exe").unwrap();
        assert_eq!(target(&inst).backup_app(&backup, "0.4.0").unwrap(), 3);
        assert_eq!(read_version(&backup).as_deref(), Some("0.4.0"));
        std::fs::write(backup.join(VERSION_TXT), " 0.4.1\r\n").unwrap();
        assert_eq!(read_version(&backup).as_deref(), Some("0.4.1"));
        std::fs::write(backup.join(VERSION_TXT), "0.4\n0.5").unwrap();
        assert_eq!(read_version(&backup), None);
        std::fs::remove_file(backup.join(VERSION_TXT)).unwrap();
        assert_eq!(read_version(&backup), None);
        assert_eq!(old_exe(Path::new(r"C:\x\awg-ui.exe")), Path::new(r"C:\x\awg-ui.exe.old"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn app_backup_and_restore_full_set() {
        let dir = temp("appset");
        let (inst, backup) = (dir.join("inst"), dir.join("backup"));
        let old = [(APP_EXE, "exe1"), ("tunnel.dll", "t1"), ("wintun.dll", "w1"), (MANIFEST, "m1"), (MANIFEST_SIG, "s1")];
        write_all(&inst, &old);
        assert_eq!(target(&inst).backup_app(&backup, "0.4.0").unwrap(), 12);
        let mut want: Vec<String> = APP_SET.iter().chain([&VERSION_TXT]).map(|s| s.to_string()).collect();
        want.sort();
        assert_eq!(names(&backup), want);
        // «Обновление» всего набора; возврат ставит весь набор из копии.
        write_all(&inst, &[(APP_EXE, "exe2"), ("tunnel.dll", "t2"), ("wintun.dll", "w2"), (MANIFEST, "m2"), (MANIFEST_SIG, "s2")]);
        let (files, remove) = app_backup_set(&backup);
        assert!(remove.is_empty());
        target(&inst).install_app_with(&RealFs, &files, &remove, || Ok(()), |_, _| Ok(())).unwrap();
        for (n, d) in old {
            assert_eq!(read(&inst, n), d, "{n}");
        }
        // Копия без манифеста (сборка с вшитыми суммами): возврат убирает локальный манифест.
        let backup2 = dir.join("backup2");
        std::fs::remove_file(inst.join(MANIFEST)).unwrap();
        std::fs::remove_file(inst.join(MANIFEST_SIG)).unwrap();
        assert_eq!(target(&inst).backup_app(&backup2, "0.3.0").unwrap(), 8);
        assert!(!backup2.join(MANIFEST).exists());
        write_all(&inst, &[(APP_EXE, "exe3"), (MANIFEST, "m3"), (MANIFEST_SIG, "s3")]);
        let (files, remove) = app_backup_set(&backup2);
        assert_eq!(files.len(), 3);
        target(&inst).install_app_with(&RealFs, &files, &remove, || Ok(()), |_, _| Ok(())).unwrap();
        assert_eq!(read(&inst, APP_EXE), "exe1");
        assert!(!inst.join(MANIFEST).exists() && !inst.join(MANIFEST_SIG).exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
