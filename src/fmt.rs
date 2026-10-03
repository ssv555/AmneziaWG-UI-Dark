//! Форматирование чисел и времени: «число единица» через один пробел, без набивки пробелами.
//! Колонки не прыгают за счёт раскладки: значения прижаты к правому краю ячейки постоянной ширины,
//! цифры моноширинные. Слова (с, мин, назад) — из текущего языка.

use crate::i18n::trf;

const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];

/// `1.97 MiB`, `512 B`.
pub fn bytes(b: f64) -> String {
    let (mut v, mut u) = (b.max(0.0), 0);
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{v:.0} {}", UNITS[u])
    } else {
        format!("{v:.2} {}", UNITS[u])
    }
}

/// `767 B/s`, `43.67 KiB/s`.
pub fn rate(b: f64) -> String {
    trf("unit.per_sec", &[&bytes(b)])
}

/// Доля 0..=1: `34.5 %`.
pub fn percent(share: f64) -> String {
    format!("{:.1} %", share * 100.0)
}

pub fn ago(secs: u64) -> String {
    match secs {
        0..=59 => trf("time.s_ago", &[&secs.to_string()]),
        60..=3599 => trf("time.m_ago", &[&(secs / 60).to_string(), &format!("{:02}", secs % 60)]),
        _ => trf("time.h_ago", &[&(secs / 3600).to_string(), &format!("{:02}", secs % 3600 / 60)]),
    }
}

/// Длительность: `12 h 05 min`, `4 min`, `35 s`.
pub fn duration(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    match s {
        0..=59 => trf("dur.s", &[&s.to_string()]),
        60..=3599 => trf("dur.m", &[&(s / 60).to_string()]),
        _ => trf("dur.h", &[&(s / 3600).to_string(), &format!("{:02}", s % 3600 / 60)]),
    }
}

/// Местные дата и время из unix-секунд.
pub fn local_time(unix: u64, format: &str) -> String {
    use chrono::TimeZone;
    chrono::Local.timestamp_opt(unix as i64, 0).single().map(|t| t.format(format).to_string()).unwrap_or_default()
}

/// Путь для показа: начало из %TEMP% или %USERPROFILE% заменено именем переменной,
/// чтобы на снимках окна не было имени учётной записи.
pub fn short_path(path: &std::path::Path) -> String {
    let text = path.display().to_string();
    ["TEMP", "USERPROFILE"]
        .iter()
        .find_map(|var| with_var(&text, var, &std::env::var(var).ok()?))
        .unwrap_or(text)
}

/// `text`, начинающийся с `base` (без учёта регистра, по границе папки), → `%var%` + остаток.
fn with_var(text: &str, var: &str, base: &str) -> Option<String> {
    let base = base.trim_end_matches('\\');
    let rest = text.get(base.len()..)?;
    let head = text.get(..base.len())?;
    (!base.is_empty() && head.eq_ignore_ascii_case(base) && (rest.is_empty() || rest.starts_with('\\')))
        .then(|| format!("%{var}%{rest}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_prefix_becomes_variable() {
        let base = r"C:\Users\Someone\AppData\Local\Temp";
        assert_eq!(with_var(r"c:\users\someone\appdata\local\temp\demo", "TEMP", base).as_deref(), Some(r"%TEMP%\demo"));
        assert_eq!(with_var(r"C:\Users\Someone", "USERPROFILE", r"C:\Users\Someone\").as_deref(), Some("%USERPROFILE%"));
        assert_eq!(with_var(r"C:\Users\SomeoneElse\x", "USERPROFILE", r"C:\Users\Someone"), None, "только целая папка");
        assert_eq!(with_var(r"D:\Tools\app", "TEMP", base), None);
    }

    // Тесты идут на языке по умолчанию (английский): глобальный язык в тестах не переключается.
    #[test]
    fn unit_follows_number_with_one_space() {
        assert_eq!(bytes(512.0), "512 B");
        assert_eq!(bytes(2_065_694.0), "1.97 MiB");
        assert_eq!(rate(767.0), "767 B/s");
        assert_eq!(rate(44_718.0), "43.67 KiB/s");
        assert_eq!(percent(0.345), "34.5 %");
        assert_eq!(percent(1.0), "100.0 %");
    }

    #[test]
    fn human_time() {
        assert_eq!(ago(96), "1 min 36 s ago");
        assert_eq!(duration(43_500.0), "12 h 05 min");
    }
}
