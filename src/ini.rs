//! Минимальный INI: `[секция]`, `ключ=значение`, комментарии `;` и `#`. Порядок секций и ключей сохраняется.

use std::io;
use std::path::{Path, PathBuf};

/// Файл настроек есть, но не читается; что с ним сделали и как об этом сказать пользователю.
#[derive(Debug)]
pub struct Unreadable {
    pub path: PathBuf,
    pub error: String,
    /// Куда файл отодвинут, либо почему не вышло.
    pub kept_as: Result<PathBuf, String>,
}

impl Unreadable {
    /// Текст для журнала. Собирается при показе, а не при чтении: язык включают уже после чтения настроек.
    pub fn text(&self) -> String {
        let path = self.path.display().to_string();
        match &self.kept_as {
            Ok(to) => crate::i18n::trf("ini.unreadable", &[&path, &self.error, &to.display().to_string()]),
            Err(why) => crate::i18n::trf("ini.unreadable_kept", &[&path, &self.error, why]),
        }
    }
}

/// Отодвинуть нечитаемый `path` в `<имя>.unreadable-<дата>` рядом; занятое имя не затирается.
fn quarantine(path: &Path) -> Result<PathBuf, String> {
    let stamp = crate::fmt::file_stamp(crate::monitor::unix_now());
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let target = (1..100)
        .map(|n| if n == 1 { format!("{name}.unreadable-{stamp}") } else { format!("{name}.unreadable-{stamp}-{n}") })
        .map(|n| path.with_file_name(n))
        .find(|p| !p.exists())
        .ok_or_else(|| crate::fsutil::io_ctx(path, "no free name for the backup"))?;
    std::fs::rename(path, &target).map_err(|e| crate::fsutil::io_ctx_move(path, &target, e))?;
    Ok(target)
}

#[derive(Default, Debug, PartialEq)]
pub struct Ini {
    sections: Vec<(String, Vec<(String, String)>)>,
}

impl Ini {
    pub fn parse(text: &str) -> Ini {
        let mut ini = Ini::default();
        let mut current = None;
        for line in text.lines() {
            let line = line.trim().trim_start_matches('\u{feff}');
            if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
                continue;
            }
            if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                current = Some(ini.section_index(name.trim()));
                continue;
            }
            if let (Some(i), Some((k, v))) = (current, line.split_once('=')) {
                ini.sections[i].1.push((k.trim().to_string(), v.trim().to_string()));
            }
        }
        ini
    }

    /// Прочитать файл настроек. Нет файла — не ошибка (первый запуск): пустой `Ini`. Файл есть, но не читается
    /// (не UTF-8, заблокирован, нет прав) — ошибка: молча принять его за пустой нельзя, ближайшая запись
    /// затёрла бы то, что пользователь настроил.
    pub fn read(path: &Path) -> io::Result<Ini> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(Ini::parse(&text)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Ini::default()),
            Err(e) => Err(e),
        }
    }

    /// Чтение без записи (языковые файлы, статистика, чужие файлы): сбой чтения даёт пустой `Ini`, файл остаётся
    /// на месте. Для файла, который программа потом перезаписывает, — `load_guarded`.
    pub fn load(path: &Path) -> Ini {
        Ini::read(path).unwrap_or_default()
    }

    /// Чтение файла, который программа перезапишет (Settings.ini, core.ini). Нечитаемый файл не пропадает под
    /// записью: он отодвигается в `<имя>.unreadable-<дата>` — безопаснее отказа от записи (тот оставил бы
    /// настройки, которые не сохраняются, и вечную ошибку), а копию пользователь разберёт вручную. Результат
    /// отодвигания возвращается тоже: не вышло — запись может затереть файл, об этом надо сказать.
    pub fn load_guarded(path: &Path) -> (Ini, Option<Unreadable>) {
        match Ini::read(path) {
            Ok(ini) => (ini, None),
            Err(e) => {
                let kept_as = quarantine(path);
                (Ini::default(), Some(Unreadable { path: path.to_path_buf(), error: e.to_string(), kept_as }))
            }
        }
    }

    /// Запись через временный файл: при сбое посреди записи старый файл остаётся целым.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        crate::fsutil::write_atomic(path, self.to_text().as_bytes())
    }

    fn section_index(&mut self, name: &str) -> usize {
        match self.sections.iter().position(|(n, _)| n == name) {
            Some(i) => i,
            None => {
                self.sections.push((name.to_string(), Vec::new()));
                self.sections.len() - 1
            }
        }
    }

    pub fn section_names(&self) -> impl Iterator<Item = &str> {
        self.sections.iter().map(|(n, _)| n.as_str())
    }

    pub fn section(&self, name: &str) -> &[(String, String)] {
        self.sections.iter().find(|(n, _)| n == name).map(|(_, kv)| kv.as_slice()).unwrap_or(&[])
    }

    pub fn get(&self, section: &str, key: &str) -> Option<&str> {
        self.section(section).iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    pub fn get_or<T: std::str::FromStr>(&self, section: &str, key: &str, default: T) -> T {
        self.get(section, key).and_then(|v| v.parse().ok()).unwrap_or(default)
    }

    pub fn get_bool(&self, section: &str, key: &str, default: bool) -> bool {
        match self.get(section, key) {
            Some("1") | Some("true") => true,
            Some("0") | Some("false") => false,
            _ => default,
        }
    }

    pub fn set(&mut self, section: &str, key: &str, value: impl ToString) {
        let i = self.section_index(section);
        let value = value.to_string();
        match self.sections[i].1.iter_mut().find(|(k, _)| k == key) {
            Some(kv) => kv.1 = value,
            None => self.sections[i].1.push((key.to_string(), value)),
        }
    }

    pub fn set_bool(&mut self, section: &str, key: &str, value: bool) {
        self.set(section, key, if value { "1" } else { "0" });
    }

    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for (name, kv) in &self.sections {
            out.push_str(&format!("[{name}]\r\n"));
            for (k, v) in kv {
                out.push_str(&format!("{k}={v}\r\n"));
            }
            out.push_str("\r\n");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_keeps_order_and_values() {
        let mut ini = Ini::default();
        ini.set("window", "x", 10);
        ini.set("window", "width", 1100.5);
        ini.set_bool("view", "groups", true);
        ini.set("assign", "home.nl-ams.full", "Home");
        let back = Ini::parse(&ini.to_text());
        assert_eq!(back, ini);
        assert_eq!(back.get_or("window", "width", 0.0f32), 1100.5);
        assert!(back.get_bool("view", "groups", false));
        assert_eq!(back.section_names().collect::<Vec<_>>(), vec!["window", "view", "assign"]);
    }

    #[test]
    fn parse_skips_comments_and_garbage() {
        let ini = Ini::parse("\u{feff}; c\n# c\n[a]\nk = v \nnoequals\n[b]\nx=1\n");
        assert_eq!(ini.get("a", "k"), Some("v"));
        assert_eq!(ini.get_or("b", "x", 0), 1);
        assert_eq!(ini.get_or("b", "missing", 7), 7);
    }

    fn dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("awg-ini-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn backups(dir: &Path) -> Vec<PathBuf> {
        let mut found: Vec<PathBuf> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).filter(|p| p.to_string_lossy().contains(".unreadable-")).collect();
        found.sort();
        found
    }

    #[test]
    fn absent_file_is_empty_and_not_an_error() {
        let dir = dir("absent");
        let path = dir.join("Settings.ini");
        assert_eq!(Ini::read(&path).unwrap(), Ini::default());
        let (ini, problem) = Ini::load_guarded(&path);
        assert_eq!(ini, Ini::default());
        assert!(problem.is_none());
        assert!(backups(&dir).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn normal_file_is_read_and_left_alone() {
        let dir = dir("normal");
        let path = dir.join("Settings.ini");
        std::fs::write(&path, "[a]\r\nk=v\r\n").unwrap();
        let (ini, problem) = Ini::load_guarded(&path);
        assert_eq!(ini.get("a", "k"), Some("v"));
        assert!(problem.is_none());
        assert!(path.exists() && backups(&dir).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn corrupt_file_is_an_error_for_read_and_moved_aside_by_guarded_load() {
        let dir = dir("corrupt");
        let path = dir.join("Settings.ini");
        let bytes = [b'[', b'a', b']', b'\n', b'k', b'=', 0xff, 0xfe, 0xfd, b'\n'];
        std::fs::write(&path, bytes).unwrap();
        assert!(Ini::read(&path).is_err(), "не UTF-8 — не пустые настройки");
        assert!(path.exists(), "чтение файл не трогает");

        let (ini, problem) = Ini::load_guarded(&path);
        assert_eq!(ini, Ini::default());
        let problem = problem.expect("нечитаемый файл — сообщение");
        let kept = problem.kept_as.clone().expect("файл отодвинут");
        assert!(!path.exists(), "место свободно: запись не затрёт исходный файл");
        assert_eq!(std::fs::read(&kept).unwrap(), bytes, "содержимое сохранено побайтно");
        assert!(kept.file_name().unwrap().to_string_lossy().starts_with("Settings.ini.unreadable-"));
        assert!(problem.text().contains(&kept.display().to_string()));

        // Следующая запись создаёт новый файл, копия остаётся.
        let mut fresh = Ini::default();
        fresh.set("a", "k", 1);
        fresh.save(&path).unwrap();
        assert_eq!(std::fs::read(&kept).unwrap(), bytes);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn repeated_failures_do_not_overwrite_earlier_backups() {
        let dir = dir("repeat");
        let path = dir.join("core.ini");
        for _ in 0..2 {
            std::fs::write(&path, [0xffu8, 0xfe]).unwrap();
            assert!(Ini::load_guarded(&path).1.is_some_and(|p| p.kept_as.is_ok()));
        }
        assert_eq!(backups(&dir).len(), 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn locked_file_reports_that_it_could_not_be_moved() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = dir("locked");
        let path = dir.join("Settings.ini");
        std::fs::write(&path, "[a]\r\nk=v\r\n").unwrap();
        // Без разрешения делить файл: и чтение, и переименование падают с нарушением совместного доступа.
        let _hold = std::fs::OpenOptions::new().read(true).share_mode(0).open(&path).unwrap();
        assert!(Ini::read(&path).is_err());
        let (ini, problem) = Ini::load_guarded(&path);
        assert_eq!(ini, Ini::default());
        let problem = problem.unwrap();
        assert!(problem.kept_as.is_err(), "отодвинуть заблокированный файл не вышло — это видно в сообщении");
        assert!(problem.text().contains("Settings.ini"));
        assert!(backups(&dir).is_empty());
        drop(_hold);
        assert!(path.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn set_overwrites() {
        let mut ini = Ini::default();
        ini.set("a", "k", 1);
        ini.set("a", "k", 2);
        assert_eq!(ini.section("a").len(), 1);
        assert_eq!(ini.get("a", "k"), Some("2"));
    }
}
