//! Файловые помощники хранилища обновлений: JSON с атомарной записью, перенос файла, размер папки, ротация журналов,
//! имена папок резервных копий.

use std::path::{Path, PathBuf};

use serde::Serialize;

use super::Component;
use crate::events::Severity;
use crate::monitor::Shared;

/// Имя папки копии: `<id>-<компонент>-<версия>`, в версии — только буквы, цифры, `.`, `-`, `_`.
pub(super) fn backup_name(id: u64, c: Component, version: &str) -> String {
    let c = match c {
        Component::Native => "native",
        Component::Engine => "engine",
        Component::App => "app",
    };
    let mut v: String = version.chars().map(|ch| if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') { ch } else { '_' }).collect();
    while v.contains("..") {
        v = v.replace("..", "_");
    }
    format!("{id}-{c}-{}", v.trim_matches('.'))
}

/// Имя папки копии из истории годится как один элемент пути внутри `backups` (то же правило, что для имён файлов
/// манифеста).
pub(super) use crate::fsutil::plain_name as safe_name;

pub(super) fn load_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, String> {
    let text = std::fs::read(path).map_err(|e| crate::fsutil::io_ctx(path, e))?;
    serde_json::from_slice(&text).map_err(|e| crate::fsutil::io_ctx(path, e))
}

/// Нет файла — пусто; испорчен — файл отодвигается (`set_aside`), пусто и событие в журнале: иначе следующая запись
/// затёрла бы его молча.
pub(super) fn load_or_default<T: serde::de::DeserializeOwned + Default>(path: &Path, shared: &Shared) -> T {
    load_json(path).unwrap_or_else(|e| {
        if path.exists() {
            shared.log("", Severity::Bad, &set_aside(path, &e));
        }
        T::default()
    })
}

/// Нечитаемый файл хранилища `path` (ошибка чтения `error`) отодвигается в `<имя>.unreadable-<дата>` рядом, как
/// файлы настроек (`ini::quarantine`): его можно разобрать или вернуть, а новый файл начинается пустым. Возвращает
/// текст для журнала — с новым именем либо с причиной, почему отодвинуть не вышло (тогда следующая запись его затрёт).
pub(super) fn set_aside(path: &Path, error: &str) -> String {
    let name = path.display().to_string();
    match crate::ini::quarantine(path) {
        Ok(to) => crate::i18n::trf("updm.unreadable", &[&name, error, &to.display().to_string()]),
        Err(why) => crate::i18n::trf("updm.unreadable_kept", &[&name, error, &why]),
    }
}

/// Запись через временный файл: оборванная запись не портит прежний.
pub(super) fn save_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let text = serde_json::to_vec_pretty(value).map_err(|e| crate::fsutil::io_ctx(path, e))?;
    crate::fsutil::write_atomic(path, &text).map_err(|e| crate::fsutil::io_ctx(path, e))
}

pub(super) fn move_file(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::rename(from, to)
        .or_else(|_| std::fs::copy(from, to).and_then(|_| std::fs::remove_file(from)))
        .map_err(|e| crate::fsutil::io_ctx_move(from, to, e))
}

/// Размер файлов папки, байт (ссылки не учитываются).
pub(super) fn dir_size(dir: &Path) -> u64 {
    let Ok(list) = std::fs::read_dir(dir) else { return 0 };
    list.flatten()
        .filter_map(|e| e.metadata().ok().map(|m| (e.path(), m)))
        .map(|(p, m)| if m.is_dir() { dir_size(&p) } else if m.is_file() { m.len() } else { 0 })
        .sum()
}

/// Журналы в `dir` сверх `keep` удаляются — старые по номеру строки истории в начале имени (`<id>-<что>.log`).
/// Возвращает ошибки удаления.
pub(super) fn rotate_logs(dir: &Path, keep: usize) -> Vec<String> {
    let Ok(list) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut files: Vec<(u64, String, PathBuf)> = list
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            (name.split('-').next().and_then(|s| s.parse().ok()).unwrap_or(0), name, e.path())
        })
        .collect();
    files.sort();
    let excess = files.len().saturating_sub(keep);
    files
        .into_iter()
        .take(excess)
        .filter_map(|(_, _, p)| std::fs::remove_file(&p).err().map(|e| crate::fsutil::io_ctx(&p, e)))
        .collect()
}

#[cfg(test)]
pub(super) fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("awg-updm-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::manager::LOGS_MAX;

    #[test]
    fn logs_keep_newest() {
        let dir = temp("logs");
        std::fs::create_dir_all(&dir).unwrap();
        for id in 1..=25u64 {
            std::fs::write(dir.join(format!("{id}-install.log")), b"x").unwrap();
        }
        assert!(rotate_logs(&dir, LOGS_MAX).is_empty());
        let mut left: Vec<u64> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().split('-').next().unwrap().parse().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, (6..=25).collect::<Vec<_>>(), "удалены старые по номеру, не по имени (10 < 9 строкой)");
        assert!(rotate_logs(&dir.join("none"), LOGS_MAX).is_empty(), "нет папки — нечего делать");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn backup_names_are_single_path_elements() {
        assert_eq!(backup_name(7, Component::Native, "1.0.5"), "7-native-1.0.5");
        for v in [r"..\..\x", "a/b:c", "..", "3.1 · wintun 0.14"] {
            let n = backup_name(1, Component::Engine, v);
            assert!(safe_name(&n) && !n.contains(['/', '\\', ':', ' ']) && !n.contains(".."), "{n}");
        }
        assert!(!safe_name("") && !safe_name("..") && !safe_name(r"a\b") && !safe_name("a/b"));
    }
}
