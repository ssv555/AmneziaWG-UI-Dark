//! Форматирование чисел и времени: «число единица» через один пробел, без набивки пробелами.
//! Колонки не прыгают за счёт раскладки: значения прижаты к правому краю ячейки постоянной ширины,
//! цифры моноширинные. Слова (с, мин, назад) — из текущего языка.

use crate::i18n::trf;

const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];

/// Шаг между единицами `UNITS`.
const UNIT_STEP: f64 = 1024.0;

/// Единица для `b` байт: наибольшая, в которой число не меньше 1. Одна на `bytes` и шкалу графика — подписи шкалы
/// в тех же единицах, что значения рядом.
pub fn unit_of(b: f64) -> usize {
    let (mut v, mut u) = (b.max(0.0), 0);
    while v >= UNIT_STEP && u < UNITS.len() - 1 {
        v /= UNIT_STEP;
        u += 1;
    }
    u
}

/// Байт в одной `unit` (`unit_of`).
pub fn unit_size(unit: usize) -> f64 {
    UNIT_STEP.powi(unit.min(UNITS.len() - 1) as i32)
}

/// `1.97 MiB`, `512 B`.
pub fn bytes(b: f64) -> String {
    let u = unit_of(b);
    let v = b.max(0.0) / unit_size(u);
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

/// Скорость `b` байт/с в заданной единице и с заданным числом знаков: `0.5 KiB/s`, `200 MiB/s` (подписи шкалы).
pub fn rate_in(b: f64, unit: usize, decimals: usize) -> String {
    let unit = unit.min(UNITS.len() - 1);
    trf("unit.per_sec", &[&format!("{:.*} {}", decimals, b.max(0.0) / unit_size(unit), UNITS[unit])])
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

// Единственное место, где живут форматы даты и времени (правило владельца): дата везде `ГГГГ.ММ.ДД`, время `ЧЧ:ММ`,
// секунды только там, где они есть. Формат не зависит от языка интерфейса; охранный тест `dates_only_in_fmt`
// не пускает другие форматы в остальной код.
const DATE: &str = "%Y.%m.%d";
const DATE_TIME: &str = "%Y.%m.%d %H:%M";
const DATE_TIME_SEC: &str = "%Y.%m.%d %H:%M:%S";
const TIME_SEC: &str = "%H:%M:%S";
/// Для имён файлов: без точек и пробелов.
const FILE_STAMP: &str = "%Y%m%d-%H%M%S";
/// Прежний формат строк `events.log` (до единого формата): читается, пишется уже новый.
const LEGACY_DATE_TIME_SEC: &str = "%Y-%m-%d %H:%M:%S";

/// Местные дата и время из unix-секунд; `format` — только константы этого модуля.
fn local_time(unix: u64, format: &str) -> String {
    use chrono::TimeZone;
    chrono::Local.timestamp_opt(unix as i64, 0).single().map(|t| t.format(format).to_string()).unwrap_or_default()
}

/// `2026.10.05`.
pub fn date(unix: u64) -> String {
    local_time(unix, DATE)
}

/// `2026.10.05 14:07`.
pub fn date_time(unix: u64) -> String {
    local_time(unix, DATE_TIME)
}

/// `2026.10.05 14:07:33` — там, где секунды есть (журналы).
pub fn date_time_sec(unix: u64) -> String {
    local_time(unix, DATE_TIME_SEC)
}

/// `14:07:33`.
pub fn time_sec(unix: u64) -> String {
    local_time(unix, TIME_SEC)
}

/// `20261005-140733` — часть имени файла.
pub fn file_stamp(unix: u64) -> String {
    local_time(unix, FILE_STAMP)
}

/// Обратное к `date_time_sec`: unix-секунды из строки журнала. Читает и прежний формат с дефисами.
pub fn parse_date_time_sec(text: &str) -> Option<u64> {
    use chrono::TimeZone;
    let naive = [DATE_TIME_SEC, LEGACY_DATE_TIME_SEC].iter().find_map(|f| chrono::NaiveDateTime::parse_from_str(text, f).ok())?;
    chrono::Local.from_local_datetime(&naive).earliest().map(|t| t.timestamp().max(0) as u64)
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
    fn rate_in_keeps_the_given_unit() {
        assert_eq!(unit_of(0.0), 0);
        assert_eq!(unit_of(1023.0), 0);
        assert_eq!(unit_of(1024.0), 1);
        assert_eq!(unit_of(3.0 * 1024.0 * 1024.0), 2);
        assert_eq!(rate_in(512.0, 1, 1), "0.5 KiB/s");
        assert_eq!(rate_in(200.0 * 1024.0 * 1024.0, 2, 0), "200 MiB/s");
        assert_eq!(rate_in(0.0, 2, 0), "0 MiB/s");
    }

    /// Форма, а не цифры: часовой пояс машины сдвигает число и час.
    #[test]
    fn dates_are_year_first_with_dots() {
        let at = 1_791_209_253; // 2026-10-05 14:07:33 UTC
        let shape = |s: &str, pattern: &str| {
            assert_eq!(s.len(), pattern.len(), "{s}");
            for (c, p) in s.chars().zip(pattern.chars()) {
                assert!(if p == '9' { c.is_ascii_digit() } else { c == p }, "{s} против {pattern}");
            }
        };
        shape(&date(at), "9999.99.99");
        shape(&date_time(at), "9999.99.99 99:99");
        shape(&date_time_sec(at), "9999.99.99 99:99:99");
        shape(&time_sec(at), "99:99:99");
        assert!(date(at).starts_with("2026.1"), "{}", date(at));
        assert_eq!(date_time_sec(at)[..16], date_time(at), "минуты совпадают");
    }

    #[test]
    fn log_time_round_trips_and_old_format_is_still_read() {
        let at = 1_791_209_253;
        assert_eq!(parse_date_time_sec(&date_time_sec(at)), Some(at));
        let old = local_time(at, LEGACY_DATE_TIME_SEC);
        assert!(old.contains('-'), "{old}");
        assert_eq!(parse_date_time_sec(&old), Some(at), "events.log прежней версии читается");
        assert_eq!(parse_date_time_sec("not a time"), None);
    }

    /// Правило владельца: формат даты один. Ни строк `strftime`, ни ручной сборки «число-число-число» вне этого
    /// файла; `chrono` тоже только здесь (кроме разбора RFC 3339 в `update/feed.rs`). Иглы собраны из частей, чтобы
    /// тест не нашёл сам себя.
    #[test]
    fn dates_only_in_fmt() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        collect_rs(&root, &mut files);
        assert!(files.len() > 10, "src/ not scanned: {}", root.display());
        let chrono_path = concat!("chro", "no::");
        let needles = [
            concat!("%", "Y"), concat!("%", "m"), concat!("%", "d"), concat!("%", "H"), concat!("%", "M"), concat!("%", "S"),
            concat!("%", "b"), concat!("%", "F"), concat!("%", "T"), concat!("{:02}", ".{:02}"), concat!("{:02}", "-{:02}"),
            concat!("{:02}", ":{:02}"), concat!("{:04}", "-"), concat!("{:04}", "."), chrono_path,
        ];
        let own = std::path::Path::new("fmt.rs");
        let feed = std::path::Path::new("update").join("feed.rs");
        let mut bad = Vec::new();
        for path in &files {
            let rel = path.strip_prefix(&root).unwrap();
            if rel == own {
                continue;
            }
            let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            for (n, line) in text.lines().enumerate() {
                // Разбор RFC 3339 из GitHub ничего не форматирует.
                for needle in needles.iter().filter(|x| line.contains(*x) && !(rel == feed && **x == chrono_path)) {
                    bad.push(format!("src/{}:{}: {needle}", rel.display(), n + 1));
                }
            }
        }
        assert!(bad.is_empty(), "date formats outside fmt.rs (use fmt::date, date_time, date_time_sec, time_sec):\n{}", bad.join("\n"));
    }

    fn collect_rs(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
            let path = entry.unwrap_or_else(|e| panic!("{}: {e}", dir.display())).path();
            if path.is_dir() {
                collect_rs(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    #[test]
    fn human_time() {
        assert_eq!(ago(96), "1 min 36 s ago");
        assert_eq!(duration(43_500.0), "12 h 05 min");
    }
}
