//! Журнал событий: подключения, отключения, потеря и восстановление связи.
//! В памяти — последние записи для окна; на диске — `events.log` в корзине логов
//! (`C:\Temp\<пользователь>\awg-ui\logs\`), ротация при 1 МиБ.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::Write;
use std::path::PathBuf;

use crate::fmt;
use crate::health::Level;
use crate::i18n::tr;

const KEEP: usize = 500;
const ROTATE_BYTES: u64 = 1024 * 1024;
const TIME_FORMAT: &str = "%Y-%m-%d %H:%M:%S";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Severity {
    Info,
    Warn,
    Bad,
}

impl Severity {
    fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "INFO",
            Severity::Warn => "WARN",
            Severity::Bad => "FAIL",
        }
    }

    fn parse(s: &str) -> Severity {
        match s {
            "WARN" => Severity::Warn,
            "FAIL" => Severity::Bad,
            _ => Severity::Info,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    pub at: u64,
    pub tunnel: String,
    pub severity: Severity,
    pub text: String,
    /// Показать уведомлением Windows.
    pub notify: bool,
}

impl Event {
    pub fn new(at: u64, tunnel: &str, severity: Severity, text: &str, notify: bool) -> Event {
        Event { at, tunnel: tunnel.to_string(), severity, text: text.to_string(), notify }
    }

    fn line(&self) -> String {
        format!("{}\t{}\t{}\t{}", fmt::local_time(self.at, TIME_FORMAT), self.severity.as_str(), self.tunnel, self.text)
    }

    fn parse_line(line: &str) -> Option<Event> {
        use chrono::TimeZone;
        let mut parts = line.splitn(4, '\t');
        let (time, sev, tunnel, text) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
        let naive = chrono::NaiveDateTime::parse_from_str(time, TIME_FORMAT).ok()?;
        let at = chrono::Local.from_local_datetime(&naive).earliest()?.timestamp().max(0) as u64;
        Some(Event::new(at, tunnel, Severity::parse(sev), text, false))
    }
}

pub struct EventLog {
    pub items: VecDeque<Event>,
    file: Option<PathBuf>,
}

impl EventLog {
    /// `file` = None — только память (демо-режим).
    pub fn open(file: Option<PathBuf>) -> EventLog {
        let mut items = VecDeque::new();
        if let Some(path) = &file {
            if std::fs::metadata(path).map(|m| m.len() > ROTATE_BYTES).unwrap_or(false) {
                let _ = std::fs::rename(path, path.with_extension("1.log"));
            }
            if let Ok(text) = std::fs::read_to_string(path) {
                items.extend(text.lines().filter_map(Event::parse_line));
                while items.len() > KEEP {
                    items.pop_front();
                }
            }
        }
        EventLog { items, file }
    }

    pub fn push(&mut self, event: Event) {
        if let Some(path) = &self.file {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                let _ = writeln!(f, "{}", event.line());
            }
        }
        self.items.push_back(event);
        while self.items.len() > KEEP {
            self.items.pop_front();
        }
    }
}

/// События из смены уровней туннелей между двумя опросами.
/// `user` — туннели, которые сейчас переключает пользователь: их отключение не авария и без уведомления.
pub fn transitions(
    prev: &BTreeMap<String, Level>,
    cur: &BTreeMap<String, (Level, String)>,
    user: &BTreeSet<String>,
    now: u64,
) -> Vec<Event> {
    use Level::*;
    let mut out = Vec::new();
    for (name, (c, text)) in cur {
        let p = prev.get(name).copied().unwrap_or(Off);
        let by_user = user.contains(name);
        let e = |sev, text: &str, notify| Event::new(now, name, sev, text, notify);
        let event = match (p, *c) {
            (Off, Ok | Warn | Bad) => Some(e(Severity::Info, &tr("ev.connected"), !by_user)),
            (Ok | Warn | Bad, Off) if by_user => Some(e(Severity::Info, &tr("ev.disconnected"), false)),
            (Ok | Warn | Bad, Off) => Some(e(Severity::Bad, &tr("ev.dropped"), true)),
            (Ok, Bad) | (Warn, Bad) => Some(e(Severity::Bad, text, true)),
            (Ok, Warn) | (Bad, Warn) => Some(e(Severity::Warn, text, true)),
            (Warn | Bad, Ok) => Some(e(Severity::Info, &tr("ev.restored"), true)),
            _ => None,
        };
        out.extend(event);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cur(items: &[(&str, Level)]) -> BTreeMap<String, (Level, String)> {
        items.iter().map(|(n, l)| (n.to_string(), (*l, "текст".to_string()))).collect()
    }

    #[test]
    fn transitions_cover_drop_and_recovery() {
        let prev: BTreeMap<String, Level> = [("a".to_string(), Level::Ok), ("b".to_string(), Level::Bad)].into();
        let ev = transitions(&prev, &cur(&[("a", Level::Off), ("b", Level::Ok), ("c", Level::Ok)]), &BTreeSet::new(), 1);
        let texts: Vec<(&str, &str, bool)> = ev.iter().map(|e| (e.tunnel.as_str(), e.text.as_str(), e.notify)).collect();
        assert_eq!(texts, vec![("a", "Tunnel went down", true), ("b", "Link restored", true), ("c", "Connected", true)]);
    }

    #[test]
    fn user_disconnect_is_quiet() {
        let prev: BTreeMap<String, Level> = [("a".to_string(), Level::Ok)].into();
        let user: BTreeSet<String> = ["a".to_string()].into();
        let ev = transitions(&prev, &cur(&[("a", Level::Off)]), &user, 1);
        assert_eq!(ev.len(), 1);
        assert!(!ev[0].notify && ev[0].severity == Severity::Info);
    }

    #[test]
    fn same_level_no_event() {
        let prev: BTreeMap<String, Level> = [("a".to_string(), Level::Ok)].into();
        assert!(transitions(&prev, &cur(&[("a", Level::Ok)]), &BTreeSet::new(), 1).is_empty());
    }

    #[test]
    fn log_file_roundtrip() {
        let dir = std::env::temp_dir().join(format!("awg-ui-events-{}", std::process::id()));
        let path = dir.join("events.log");
        let mut log = EventLog::open(Some(path.clone()));
        log.push(Event::new(1_790_000_000, "a", Severity::Warn, "пинг не проходит", true));
        let back = EventLog::open(Some(path));
        assert_eq!(back.items.len(), 1);
        assert_eq!((back.items[0].at, back.items[0].severity), (1_790_000_000, Severity::Warn));
        assert_eq!(back.items[0].text, "пинг не проходит");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
