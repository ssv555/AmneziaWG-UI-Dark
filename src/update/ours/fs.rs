//! Файловые операции транзакции над набором файлов и возврата (`fileset`, `fallback`): за трейтом, чтобы тест мог
//! сломать любой шаг посреди набора, не трогая настоящие права и блокировки файлов.

use std::io;
use std::path::Path;

/// Операции, из которых состоят замена файла и откат. Остальное (чтение, проверка существования) — напрямую.
pub(crate) trait Fs {
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    /// Копия `from` в `to`, записанная на диск (`fsutil::copy_durable`): её сразу переименуют на место рабочего
    /// файла, и после пропадания питания она должна быть целой; размер.
    fn copy(&self, from: &Path, to: &Path) -> io::Result<u64>;
}

/// Настоящая файловая система.
pub(crate) struct RealFs;

impl Fs for RealFs {
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        // С записью на диск: шаги замены набора должны ложиться в том порядке, в каком их ждёт `swap::recover`.
        crate::fsutil::rename_durably(from, to)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    fn copy(&self, from: &Path, to: &Path) -> io::Result<u64> {
        crate::fsutil::copy_durable(from, to)
    }
}
