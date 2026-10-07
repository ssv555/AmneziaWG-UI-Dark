//! Журнал шагов на диске: план работы, которую нельзя оставить на полпути (замена набора файлов, возврат MSI), пишется
//! целиком и атомарно до первого шага и убирается после последнего. Следующий запуск (ядра, агента, установки) читает
//! план и доводит дело до согласного состояния — вперёд или назад; как именно, решает владелец плана, здесь только
//! хранение. Общий для `ours::swap` и любого другого многошагового действия в `update`.
//!
//! Файл — JSON через `fsutil::write_atomic`: после пропадания питания он либо есть целиком, либо его нет. Испорченный
//! (не разбирается) отодвигается как `<имя>.unreadable-<дата>` (`ini::quarantine`), чтобы не блокировать работу
//! вечно, и об этом сообщается вызывающему: решать, что делать с файлами, без плана он не может.

use std::marker::PhantomData;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::Serialize;

/// Журнал одного вида работы: файл `path`, в нём план типа `T`.
pub(crate) struct Journal<T> {
    path: PathBuf,
    _plan: PhantomData<T>,
}

impl<T: Serialize + DeserializeOwned> Journal<T> {
    pub(crate) fn at(path: PathBuf) -> Journal<T> {
        Journal { path, _plan: PhantomData }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Записать план до первого шага; папка создаётся. Пока файл есть, работа считается незавершённой.
    pub(crate) fn begin(&self, plan: &T) -> Result<(), String> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| crate::fsutil::io_ctx(dir, e))?;
        }
        let text = serde_json::to_vec_pretty(plan).map_err(|e| crate::fsutil::io_ctx(&self.path, e))?;
        crate::fsutil::write_atomic(&self.path, &text).map_err(|e| crate::fsutil::io_ctx(&self.path, e))
    }

    /// Незавершённый план: `None` — файла нет. Файл не разбирается — он отодвинут (`quarantine`), ошибка называет
    /// обе причины и новое имя.
    pub(crate) fn pending(&self) -> Result<Option<T>, String> {
        let text = match std::fs::read(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(crate::fsutil::io_ctx(&self.path, e)),
        };
        match serde_json::from_slice(&text) {
            Ok(plan) => Ok(Some(plan)),
            Err(e) => {
                let shown = self.path.display().to_string();
                Err(match crate::ini::quarantine(&self.path) {
                    Ok(kept) => format!("{shown}: {e}; kept as {}", kept.display()),
                    Err(e2) => format!("{shown}: {e}; {e2}"),
                })
            }
        }
    }

    /// Работа завершена (или отменена): убрать план. Файла уже нет — не ошибка.
    pub(crate) fn finish(&self) -> Result<(), String> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(crate::fsutil::io_ctx(&self.path, e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct Plan {
        steps: Vec<String>,
    }

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("awg-journal-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn plan_lives_from_begin_to_finish() {
        let d = dir("life");
        let j: Journal<Plan> = Journal::at(d.join("deep").join("swap.json"));
        assert_eq!(j.pending().unwrap(), None, "без файла плана нет");
        let plan = Plan { steps: vec!["a".into(), "b".into()] };
        j.begin(&plan).unwrap();
        assert_eq!(j.pending().unwrap(), Some(plan));
        assert!(!d.join("deep").join("swap.json.tmp").exists(), "запись атомарная, временного файла не остаётся");
        j.finish().unwrap();
        assert_eq!(j.pending().unwrap(), None);
        j.finish().unwrap_or_else(|e| panic!("повторное завершение — не ошибка: {e}"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn unreadable_plan_is_moved_aside_and_reported() {
        let d = dir("bad");
        std::fs::create_dir_all(&d).unwrap();
        let j: Journal<Plan> = Journal::at(d.join("swap.json"));
        std::fs::write(j.path(), "{broken").unwrap();
        let e = j.pending().unwrap_err();
        assert!(e.contains("swap.json") && e.contains("unreadable-"), "{e}");
        assert!(!j.path().exists(), "испорченный план не блокирует следующую работу");
        let kept = std::fs::read_dir(&d).unwrap().flatten().find(|e| e.file_name().to_string_lossy().contains(".unreadable-")).expect("kept");
        assert_eq!(std::fs::read_to_string(kept.path()).unwrap(), "{broken");
        assert_eq!(j.pending().unwrap(), None);
        let _ = std::fs::remove_dir_all(&d);
    }
}
