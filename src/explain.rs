//! Ошибки для человека: короткая фраза на языке интерфейса вместо технического текста Windows, WinHTTP или HTTP.
//! Технический текст не теряется — он идёт в журнал событий и в подсказку, но не в основной текст окна
//! (docs/ui-guidelines.md, «Errors and text»).

use crate::i18n::{tr, trf};

/// Известная причина сбоя. Ключ i18n — `cause.*`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    NameNotResolved,
    NoConnection,
    Timeout,
    SecureChannel,
    AccessDenied,
    NotFound,
    InUse,
    DiskFull,
    ServiceMissing,
    ServiceStopped,
    ServiceDisabled,
    Cancelled,
    RateLimited,
    HttpNotFound,
    ServerError,
}

impl Cause {
    fn key(self) -> &'static str {
        match self {
            Cause::NameNotResolved => "cause.name_not_resolved",
            Cause::NoConnection => "cause.no_connection",
            Cause::Timeout => "cause.timeout",
            Cause::SecureChannel => "cause.secure_channel",
            Cause::AccessDenied => "cause.access_denied",
            Cause::NotFound => "cause.not_found",
            Cause::InUse => "cause.in_use",
            Cause::DiskFull => "cause.disk_full",
            Cause::ServiceMissing => "cause.service_missing",
            Cause::ServiceStopped => "cause.service_stopped",
            Cause::ServiceDisabled => "cause.service_disabled",
            Cause::Cancelled => "cause.cancelled",
            Cause::RateLimited => "cause.rate_limited",
            Cause::HttpNotFound => "cause.http_not_found",
            Cause::ServerError => "cause.server_error",
        }
    }

    pub fn text(self) -> String {
        tr(self.key())
    }
}

/// Код Windows (`GetLastError`, `(os error N)`) -> причина. Коды WinHTTP (12xxx) и Winsock (10xxx, 11xxx) — тоже
/// коды `GetLastError`, у них одно пространство номеров.
fn win32(code: u32) -> Option<Cause> {
    Some(match code {
        12007 | 11001 | 11004 => Cause::NameNotResolved,
        12029 | 12030 | 12031 | 1231 | 1232 | 10050 | 10051 | 10054 | 10061 | 10065 => Cause::NoConnection,
        12002 | 1460 | 121 | 258 | 10060 => Cause::Timeout,
        12175 | 12157 | 12044 | 12045 | 12037 | 12038 => Cause::SecureChannel,
        5 => Cause::AccessDenied,
        2 | 3 => Cause::NotFound,
        32 | 33 => Cause::InUse,
        39 | 112 => Cause::DiskFull,
        1060 => Cause::ServiceMissing,
        1062 => Cause::ServiceStopped,
        1058 => Cause::ServiceDisabled,
        1223 => Cause::Cancelled,
        _ => return None,
    })
}

fn http(status: u32) -> Option<Cause> {
    Some(match status {
        403 | 429 => Cause::RateLimited,
        404 | 410 => Cause::HttpNotFound,
        500..=599 => Cause::ServerError,
        _ => return None,
    })
}

/// Числа сразу после каждого вхождения `marker` (пробелы между ними пропускаются).
fn numbers_after<'a>(raw: &'a str, marker: &'a str) -> impl Iterator<Item = u32> + 'a {
    raw.match_indices(marker).filter_map(move |(at, m)| {
        let tail = raw[at + m.len()..].trim_start();
        let digits: String = tail.chars().take_while(char::is_ascii_digit).collect();
        digits.parse().ok()
    })
}

/// Причина сбоя по техническому тексту: `HTTP status 403`, `(os error 5)`, `WinHttpSendRequest: error 12007`.
/// Не узнали — `None`: тогда показывается только общая фраза, а текст — в подсказке и журнале.
pub fn cause(raw: &str) -> Option<Cause> {
    let http_codes = numbers_after(raw, "HTTP status").chain(numbers_after(raw, "HTTP "));
    http_codes
        .filter_map(http)
        .chain(numbers_after(raw, "os error").filter_map(win32))
        .chain(numbers_after(raw, ": error").filter_map(win32))
        .next()
}

/// Фраза для тесного места (ячейка, строка состояния, крупная строка карточки): «что не получилось: почему», если
/// причина известна, иначе только «что не получилось». Технический текст — в `log_line` и подсказке.
pub fn short(sentence: &str, raw: &str) -> String {
    match cause(raw) {
        Some(c) => trf("cause.joined", &[sentence, &c.text()]),
        None => sentence.to_string(),
    }
}

/// Строка журнала событий: фраза для человека и технические подробности в скобках в конце.
pub fn log_line(sentence: &str, raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        return short(sentence, raw);
    }
    format!("{} ({raw})", short(sentence, raw))
}

/// Имя варианта перечисления вместо его полного `Debug`: неожиданный ответ ядра (`State`, `Entries` …) в тексте
/// ошибки — одно слово, а не дамп всех полей.
pub fn variant_name(value: &impl std::fmt::Debug) -> String {
    let full = format!("{value:?}");
    full.split(['(', ' ', '{']).next().unwrap_or_default().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn winhttp_and_os_errors_map_to_causes() {
        assert_eq!(cause("WinHttpSendRequest: error 12007"), Some(Cause::NameNotResolved));
        assert_eq!(cause("WinHttpSendRequest: error 12029"), Some(Cause::NoConnection));
        assert_eq!(cause("WinHttpReceiveResponse: error 12002"), Some(Cause::Timeout));
        assert_eq!(cause("WinHttpSendRequest: error 12175"), Some(Cause::SecureChannel));
        assert_eq!(cause("C:\\x\\a.conf: Отказано в доступе. (os error 5)"), Some(Cause::AccessDenied));
        assert_eq!(cause("OpenService AmneziaWGTunnel$office: The specified service does not exist as an installed service. (os error 1060)"), Some(Cause::ServiceMissing));
        assert_eq!(cause("The process cannot access the file because it is being used by another process. (os error 32)"), Some(Cause::InUse));
        assert_eq!(cause("CreateProcessAsUser: The operation was canceled by the user. (os error 1223)"), Some(Cause::Cancelled));
    }

    #[test]
    fn http_statuses_map_to_causes() {
        assert_eq!(cause("HTTP status 403"), Some(Cause::RateLimited));
        assert_eq!(cause("HTTP status 429"), Some(Cause::RateLimited));
        assert_eq!(cause("release JSON: HTTP status 404"), Some(Cause::HttpNotFound));
        assert_eq!(cause("HTTP 503"), Some(Cause::ServerError));
        assert_eq!(cause("HTTP status 200 but body empty"), None);
    }

    #[test]
    fn unknown_text_has_no_cause() {
        assert_eq!(cause(""), None);
        assert_eq!(cause("timeout"), None);
        assert_eq!(cause("error"), None, "слово без кода");
        assert_eq!(cause("error 4242"), None, "неизвестный код");
        assert_eq!(cause("pipe: message over 1048576 bytes"), None);
    }

    #[test]
    fn short_text_never_carries_the_raw_detail() {
        let sentence = "Could not check";
        let raw = "WinHttpSendRequest: error 12007";
        let s = short(sentence, raw);
        assert!(s.starts_with(sentence) && s.contains(&Cause::NameNotResolved.text()), "{s}");
        assert!(!s.contains("WinHttp") && !s.contains("12007"), "{s}");
        assert_eq!(short(sentence, "something odd"), sentence);
    }

    #[test]
    fn log_line_keeps_the_raw_detail_at_the_end() {
        let line = log_line("Could not check", "WinHttpSendRequest: error 12007");
        assert!(line.starts_with("Could not check") && line.ends_with("(WinHttpSendRequest: error 12007)"), "{line}");
        assert!(line.contains(&Cause::NameNotResolved.text()));
        assert_eq!(log_line("Could not check", "  "), "Could not check");
        assert_eq!(log_line("Could not check", "odd"), "Could not check (odd)");
    }

    #[test]
    fn every_cause_has_both_languages() {
        let all = [
            Cause::NameNotResolved,
            Cause::NoConnection,
            Cause::Timeout,
            Cause::SecureChannel,
            Cause::AccessDenied,
            Cause::NotFound,
            Cause::InUse,
            Cause::DiskFull,
            Cause::ServiceMissing,
            Cause::ServiceStopped,
            Cause::ServiceDisabled,
            Cause::Cancelled,
            Cause::RateLimited,
            Cause::HttpNotFound,
            Cause::ServerError,
        ];
        for c in all {
            let (en, ru) = crate::i18n::builtin_pair(c.key()).unwrap_or_else(|| panic!("{c:?}: нет ключа {}", c.key()));
            assert!(!en.is_empty() && !ru.is_empty() && en != ru, "{c:?}");
        }
        assert!(crate::i18n::builtin_pair("cause.joined").is_some());
    }

    fn collect_rs(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())).flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_rs(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    /// Правило: ответ ядра или агента (`Response`, `AgentResponse`) и ошибки разбора не попадают в текст ошибки
    /// через `{:?}` — пользователь видел `core: Refused("busy")`, а дамп `Import(...)` мог бы нести конфиги. Имя
    /// варианта — `variant_name`. Проверяется код вне `mod tests`; `panic!` в нём — не пользовательский текст.
    #[test]
    fn no_debug_dumps_in_error_texts() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        collect_rs(&root, &mut files);
        assert!(files.len() > 10, "src/ not scanned: {}", root.display());
        let needles = [concat!("{other", ":?}"), concat!("{e", ":?}"), concat!("{req", ":?}")];
        // Модуль только для тестов и разбор аргументов командной строки (эхо введённого в консоль) — не текст окна.
        let allowed = [std::path::Path::new("daemon").join("fake.rs"), std::path::Path::new("update").join("mod.rs")];
        let mut bad = Vec::new();
        for path in files.iter().filter(|p| !allowed.iter().any(|a| p.ends_with(a))) {
            let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            // Тестовая часть файла начинается с `mod tests {` (или `mod testkit {`) после `#[cfg(test)]`.
            let code = text.split("\nmod tests {").next().unwrap_or_default().split("\nmod testkit {").next().unwrap_or_default();
            for (n, line) in code.lines().enumerate() {
                if ["panic!(", "println!("].iter().any(|m| line.contains(m)) || line.trim_start().starts_with("//") {
                    continue;
                }
                for needle in needles.iter().filter(|x| line.contains(*x)) {
                    bad.push(format!("{}:{}: {needle}", path.strip_prefix(&root).unwrap().display(), n + 1));
                }
            }
        }
        assert!(bad.is_empty(), "Debug dump in an error text (use explain::variant_name):\n{}", bad.join("\n"));
    }

    #[test]
    fn variant_name_drops_the_fields() {
        #[derive(Debug)]
        #[allow(dead_code)]
        enum R {
            Ok,
            Refused(String),
            State { tunnels: Vec<String> },
        }
        assert_eq!(variant_name(&R::Ok), "Ok");
        assert_eq!(variant_name(&R::Refused("busy".into())), "Refused");
        assert_eq!(variant_name(&R::State { tunnels: vec!["secret".into()] }), "State");
    }
}
