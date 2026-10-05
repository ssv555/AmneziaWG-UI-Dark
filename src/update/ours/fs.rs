//! Файловые операции транзакции над набором файлов и возврата (`fileset`, `fallback`): за трейтом, чтобы тест мог
//! сломать любой шаг посреди набора, не трогая настоящие права и блокировки файлов.

use std::io;
use std::path::Path;

/// Операции, из которых состоят замена файла и откат. Остальное (чтение, проверка существования) — напрямую.
pub(super) trait Fs {
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    /// Копия `from` в `to`; размер.
    fn copy(&self, from: &Path, to: &Path) -> io::Result<u64>;
}

/// Настоящая файловая система.
pub(super) struct RealFs;

impl Fs for RealFs {
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    fn copy(&self, from: &Path, to: &Path) -> io::Result<u64> {
        std::fs::copy(from, to)
    }
}
