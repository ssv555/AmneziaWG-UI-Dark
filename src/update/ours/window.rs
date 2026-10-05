//! Окно и сборка программы: выкладка новой сборки для окна (ядро — владелец папки) и самообновление окна из неё.

use std::path::{Path, PathBuf};

use super::backup::read_version;
use super::fileset::copy;
use super::{read_manifest, InstallTarget, APP_EXE, MANIFEST, MANIFEST_SIG, PARENT_ENV, VERSION_TXT};
use crate::i18n::{tr, trf};
use crate::update::{feed, sign};

impl InstallTarget {
    /// exe для окна, манифест с подписью (по нему окно проверяет exe) и версия в `window_dir` — папке, откуда окно
    /// берёт новую сборку (SYSTEM и администраторы — полный доступ, владелец окна — чтение).
    #[allow(dead_code)]
    pub(super) fn publish_for_window(&self, files: &[(String, PathBuf)], version: &str) -> Result<(), String> {
        let owner = crate::daemon::Config::load().owner_sid;
        let sid_ok = crate::win::is_sid(&owner);
        let read = if sid_ok { format!("(A;OICI;FR;;;{owner})") } else { String::new() };
        crate::win::protect_dir(&self.window_dir, &format!("{}{read}", crate::daemon::DATA_SDDL))?;
        publish_files(&self.window_dir, files, version)
    }
}

/// exe, манифест и подпись из набора в `dir` (нет в наборе — прежний файл убирается); `version.txt` пишется
/// последним — окно не возьмёт недописанное.
fn publish_files(dir: &Path, files: &[(String, PathBuf)], version: &str) -> Result<(), String> {
    // Прежний `version.txt` убирается первым: не убрался — окно прочло бы старую версию рядом с наполовину заменёнными файлами.
    let version_file = dir.join(VERSION_TXT);
    match std::fs::remove_file(&version_file) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(crate::fsutil::io_ctx(&version_file, e)),
        _ => {}
    }
    for name in [APP_EXE, MANIFEST, MANIFEST_SIG] {
        let to = dir.join(name);
        match files.iter().find(|(n, _)| n == name) {
            Some((_, src)) => drop(copy(src, &to)?),
            None if to.exists() => std::fs::remove_file(&to).map_err(|e| crate::fsutil::io_ctx(&to, e))?,
            None => {}
        }
    }
    let path = dir.join(VERSION_TXT);
    std::fs::write(&path, version).map_err(|e| crate::fsutil::io_ctx(&path, e))
}

/// Окно после самообновления: дождаться выхода прежнего процесса (он держит мьютекс одного экземпляра) и убрать
/// отодвинутый `awg-ui.exe.old` рядом с exe.
pub fn window_startup_cleanup() {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
    if let Some(pid) = std::env::var(PARENT_ENV).ok().and_then(|p| p.parse::<u32>().ok()) {
        std::env::remove_var(PARENT_ENV);
        unsafe {
            let h = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
            if !h.is_null() {
                WaitForSingleObject(h, 10_000);
                CloseHandle(h);
            }
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        // Отодвинутая прежняя сборка: не убралась (держит антивирус) — уберётся при следующем запуске, окну это не мешает.
        let _ = std::fs::remove_file(old_exe(&exe));
    }
}

/// `awg-ui.exe` → `awg-ui.exe.old`.
pub fn old_exe(exe: &Path) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(".old");
    exe.with_file_name(name)
}

/// Окно обновляет себя сборкой, которую выложило ядро (`InstallTarget::window_dir`): версия там равна версии ядра и новее
/// своей. exe проверяется по подписанному манифесту рядом с ним; свой exe отодвигается в `.old`, на его место
/// пишутся проверенные байты, новый процесс запускается с теми же аргументами.
/// `Ok(false)` — обновлять нечем; `Ok(true)` — новый процесс запущен, этому пора выйти.
pub fn self_update_window(core_version: &str) -> Result<bool, String> {
    let own = env!("CARGO_PKG_VERSION");
    let dir = InstallTarget::current().window_dir;
    if core_version == own || read_version(&dir).as_deref() != Some(core_version) || !feed::newer(core_version, own) {
        return Ok(false);
    }
    let data = window_exe(&dir, core_version, sign::manifest)?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let old = old_exe(&exe);
    // Остаток прошлого обновления; не убрался — следующее `rename` ниже скажет об этом само.
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&exe, &old).map_err(|e| crate::fsutil::io_ctx(&exe, e))?;
    if let Err(e) = std::fs::write(&exe, &data) {
        let written = crate::fsutil::io_ctx(&exe, e);
        // Откат: без `exe` на месте программа не запустится в следующий раз, поэтому его сбой — часть ошибки.
        let _ = std::fs::remove_file(&exe);
        return Err(match std::fs::rename(&old, &exe) {
            Ok(()) => written,
            Err(e2) => format!("{written}; {}", crate::fsutil::io_ctx_move(&old, &exe, e2)),
        });
    }
    std::process::Command::new(&exe)
        .args(std::env::args_os().skip(1))
        .env(PARENT_ENV, std::process::id().to_string())
        .spawn()
        .map(|_| true)
        .map_err(|e| crate::fsutil::io_ctx(&exe, e))
}

/// Байты exe из `dir`, проверенные по манифесту там же (`check` — проверка подписи): манифест для версии ядра,
/// размер и SHA-256 совпадают с `app`. Ставить нужно именно эти байты — файл второй раз не читается.
fn window_exe(dir: &Path, core_version: &str, check: impl Fn(&[u8], &str) -> Result<sign::Manifest, String>) -> Result<Vec<u8>, String> {
    let path = dir.join(APP_EXE);
    let data = std::fs::read(&path).map_err(|e| crate::fsutil::io_ctx(&path, e))?;
    let m = read_manifest(dir, check).ok_or_else(|| tr("updo.window_no_manifest"))?;
    if m.version != core_version {
        return Err(trf("updo.window_version", &[&m.version, core_version]));
    }
    if !m.app.matches(&data) {
        return Err(trf("updo.window_mismatch", &[APP_EXE]));
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::ours::testutil::*;

    #[test]
    fn window_update_verified_by_manifest() {
        let dir = temp("window");
        let (src, win) = (dir.join("src"), dir.join("win"));
        let exe = b"new window build".as_slice();
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join(APP_EXE), exe).unwrap();
        std::fs::write(src.join(MANIFEST), JSON).unwrap();
        std::fs::write(src.join(MANIFEST_SIG), SIG).unwrap();
        let files: Vec<(String, PathBuf)> = [APP_EXE, MANIFEST, MANIFEST_SIG].iter().map(|n| (n.to_string(), src.join(n))).collect();
        std::fs::create_dir_all(&win).unwrap();
        publish_files(&win, &files, "0.4.0").unwrap();
        assert_eq!(read_version(&win).as_deref(), Some("0.4.0"));
        // Подпись проверяется тестовым ключом; запись `app` подменяется на суммы `exe` — тех байтов, что в фикстуре,
        // нет. Подменённый манифест по-прежнему должен пройти подпись.
        let entry = sign::FileEntry { name: APP_EXE.into(), sha256: sign::sha256_hex(exe), size: exe.len() as u64 };
        let check = |j: &[u8], s: &str| {
            test_key(j, s).map(|mut m| {
                m.app = entry.clone();
                m
            })
        };
        assert_eq!(window_exe(&win, "0.4.0", check).unwrap(), exe);
        // Чужой ключ, другая версия ядра.
        assert_eq!(window_exe(&win, "0.4.0", sign::manifest).unwrap_err(), tr("updo.window_no_manifest"));
        assert_eq!(window_exe(&win, "0.4.1", check).unwrap_err(), trf("updo.window_version", &["0.4.0", "0.4.1"]));
        // Подменённый exe другой длины и той же длины.
        std::fs::write(win.join(APP_EXE), b"evil window build").unwrap();
        assert_eq!(window_exe(&win, "0.4.0", check).unwrap_err(), trf("updo.window_mismatch", &[APP_EXE]));
        std::fs::write(win.join(APP_EXE), b"new window builx").unwrap();
        assert_eq!(window_exe(&win, "0.4.0", check).unwrap_err(), trf("updo.window_mismatch", &[APP_EXE]));
        // Набор без манифеста убирает прежний — окно не обновится.
        publish_files(&win, &files[..1], "0.4.0").unwrap();
        assert!(!win.join(MANIFEST).exists() && !win.join(MANIFEST_SIG).exists());
        assert_eq!(window_exe(&win, "0.4.0", check).unwrap_err(), tr("updo.window_no_manifest"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
