//! Мелкие файловые помощники, которые нужны многим местам и обязаны вести себя везде одинаково: атомарная запись,
//! текст ошибки с путём, проверка «простого» имени файла, ожидание условия с таймаутом.

use std::fmt::Display;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Временный файл рядом с `path`: полное имя + `.tmp` (`a.conf.dpapi` -> `a.conf.dpapi.tmp`). Замена расширения
/// склеила бы временные файлы разных имён с одним началом (`x.conf` и `x.dpapi` -> оба `x.tmp`).
fn temp_name(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".tmp");
    PathBuf::from(name)
}

/// Записать `bytes` в `path` так, чтобы читатель видел либо прежнее содержимое, либо новое целиком — и после
/// внезапного пропадания питания тоже: данные идут во временный файл рядом, сбрасываются на диск
/// (`FlushFileBuffers`) и переименовываются поверх с `MOVEFILE_WRITE_THROUGH`. Сбой на любом шаге — временный
/// файл убирается, прежний `path` не тронут. Папка `path` должна существовать.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = temp_name(path);
    let written = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        replace_durably(&tmp, path)
    })();
    if written.is_err() {
        // Вернуть нужно первую ошибку; не удалось убрать и сам временный файл — она ничего не добавляет: файл
        // перезапишется при следующей записи.
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// Переименовать `from` поверх `to` и вернуться, только когда переименование уже на диске. Обычный `rename` NTFS
/// может отложить: после пропадания питания на месте `to` остался бы прежний файл, а при иной очерёдности записи —
/// пустой (так теряется, например, набор туннелей, которые надо поднять после перезагрузки).
fn replace_durably(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH};
    let wide = |p: &Path| p.as_os_str().encode_wide().chain(Some(0)).collect::<Vec<u16>>();
    if unsafe { MoveFileExW(wide(from).as_ptr(), wide(to).as_ptr(), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Текст ошибки операции над файлом: `<путь>: <причина>`.
pub fn io_ctx(path: impl AsRef<Path>, e: impl Display) -> String {
    format!("{}: {e}", path.as_ref().display())
}

/// Текст ошибки переноса файла: `<откуда> → <куда>: <причина>`.
pub fn io_ctx_move(from: impl AsRef<Path>, to: impl AsRef<Path>, e: impl Display) -> String {
    format!("{} → {}: {e}", from.as_ref().display(), to.as_ref().display())
}

/// Имя годится как один элемент пути: не пусто, не `.`, без `..` в любом месте, без `/`, `\`, `:` и управляющих
/// символов. Строже, чем нужно только для обхода каталогов: имя приходит из подписанного манифеста и из истории
/// на диске, и оба должны проходить одну и ту же проверку.
pub fn plain_name(name: &str) -> bool {
    !name.is_empty() && name != "." && !name.contains("..") && !name.chars().any(|c| matches!(c, '/' | '\\' | ':') || c.is_control())
}

/// Ждать, пока `cond` не станет истиной, проверяя её сразу и затем каждые `step`, но не дольше `timeout`.
/// `true` — условие выполнилось; `false` — вышло время (последняя проверка была в самом конце срока).
pub fn wait_until(timeout: Duration, step: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let until = Instant::now() + timeout;
    loop {
        if cond() {
            return true;
        }
        if Instant::now() >= until {
            return false;
        }
        std::thread::sleep(step);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("awg-fsutil-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn write_atomic_replaces_content_and_leaves_no_temp() {
        let d = dir("replace");
        let f = d.join("a.conf.dpapi");
        write_atomic(&f, b"one").unwrap();
        write_atomic(&f, b"two").unwrap();
        assert_eq!(std::fs::read(&f).unwrap(), b"two");
        assert!(!d.join("a.conf.dpapi.tmp").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn durable_replace_overwrites_and_reports_failures() {
        let d = dir("durable");
        let (from, to) = (d.join("new"), d.join("cur"));
        std::fs::write(&to, b"old").unwrap();
        std::fs::write(&from, b"new").unwrap();
        replace_durably(&from, &to).unwrap();
        assert_eq!(std::fs::read(&to).unwrap(), b"new", "существующий файл заменён");
        assert!(!from.exists());
        let e = replace_durably(&d.join("absent"), &to).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::NotFound, "{e}");
        assert_eq!(std::fs::read(&to).unwrap(), b"new", "сбой не трогает цель");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn temp_name_keeps_the_whole_file_name() {
        assert_eq!(temp_name(Path::new(r"C:\x\a.conf.dpapi")), PathBuf::from(r"C:\x\a.conf.dpapi.tmp"));
        assert_ne!(temp_name(Path::new("x.conf")), temp_name(Path::new("x.dpapi")));
    }

    #[test]
    fn write_atomic_failure_keeps_old_file_and_removes_temp() {
        let d = dir("fail");
        let f = d.join("target");
        write_atomic(&f, b"old").unwrap();
        // Цель — каталог: переименование поверх него невозможно, прежнего файла это не касается.
        let blocked = d.join("blocked");
        std::fs::create_dir_all(blocked.join("inner")).unwrap();
        assert!(write_atomic(&blocked, b"new").is_err());
        assert!(!d.join("blocked.tmp").exists(), "временный файл убран после сбоя");
        assert_eq!(std::fs::read(&f).unwrap(), b"old");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn write_atomic_without_folder_fails_loudly() {
        let d = dir("nofolder");
        assert!(write_atomic(&d.join("missing").join("f"), b"x").is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn io_ctx_prefixes_the_path() {
        assert_eq!(io_ctx(Path::new("a/b.txt"), "boom"), format!("{}: boom", Path::new("a/b.txt").display()));
        assert_eq!(io_ctx(PathBuf::from("c"), std::io::Error::other("x")), "c: x");
        assert_eq!(io_ctx_move("a", PathBuf::from("b"), "boom"), "a → b: boom");
    }

    #[test]
    fn plain_name_takes_the_strictest_rules() {
        for ok in ["tunnel.dll", "awg-ui.exe", "1-engine-3.1", "a b"] {
            assert!(plain_name(ok), "{ok}");
        }
        for bad in ["", ".", "..", "a..b", "..a", "a/b", r"a\b", "c:x", "a\nb", "a\0b"] {
            assert!(!plain_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn wait_until_reports_both_outcomes() {
        let mut calls = 0;
        assert!(wait_until(Duration::from_secs(5), Duration::from_millis(1), || {
            calls += 1;
            calls == 3
        }));
        assert_eq!(calls, 3);
        assert!(!wait_until(Duration::from_millis(30), Duration::from_millis(5), || false));
        assert!(wait_until(Duration::ZERO, Duration::from_millis(5), || true), "условие проверяется и при нулевом сроке");
    }
}
