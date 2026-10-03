//! Минимальный INI: `[секция]`, `ключ=значение`, комментарии `;` и `#`. Порядок секций и ключей сохраняется.

use std::io;
use std::path::Path;

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

    pub fn load(path: &Path) -> Ini {
        std::fs::read_to_string(path).map(|t| Ini::parse(&t)).unwrap_or_default()
    }

    /// Запись через временный файл: при сбое посреди записи старый файл остаётся целым.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, self.to_text())?;
        std::fs::rename(&tmp, path)
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

    #[test]
    fn set_overwrites() {
        let mut ini = Ini::default();
        ini.set("a", "k", 1);
        ini.set("a", "k", 2);
        assert_eq!(ini.section("a").len(), 1);
        assert_eq!(ini.get("a", "k"), Some("2"));
    }
}
