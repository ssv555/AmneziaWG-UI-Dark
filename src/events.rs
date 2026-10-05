//! Журнал событий: подключения, отключения, потеря и восстановление связи.
//! В памяти — последние записи для окна; на диске — `events.log` в корзине логов
//! (`C:\Temp\<пользователь>\awg-ui\logs\`), ротация при 1 МиБ.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::Write;
use std::path::PathBuf;

use crate::fmt;
use crate::health::Level;
use crate::i18n::{tr, trf};

const KEEP: usize = 500;
const ROTATE_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
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

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
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
        format!("{}\t{}\t{}\t{}", fmt::date_time_sec(self.at), self.severity.as_str(), self.tunnel, self.text)
    }

    fn parse_line(line: &str) -> Option<Event> {
        let mut parts = line.splitn(4, '\t');
        let (time, sev, tunnel, text) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
        let at = fmt::parse_date_time_sec(time)?;
        Some(Event::new(at, tunnel, Severity::parse(sev), text, false))
    }
}

pub struct EventLog {
    pub items: VecDeque<Event>,
    file: Option<PathBuf>,
    /// Номер `items[0]`; номера идут подряд — окно спрашивает у ядра события новее последнего полученного.
    first_seq: u64,
    /// Номера начинаются заново при каждом открытии журнала (перезапуск ядра): по этому коду окно узнаёт новый
    /// отсчёт. Не ноль — ноль значит «ядро не сообщает код» (старая версия).
    instance: u64,
    /// Сколько событий подгружено из файла при открытии: у них номера 1..=loaded, это не новые события.
    loaded: u64,
    /// Сколько байт сейчас в файле: ротация по размеру без обращения к диску на каждую запись.
    bytes: u64,
    /// Запись в файл не удалась — об этом уже сказано событием, повторять на каждой записи незачем.
    write_failed: bool,
}

/// Новый код отсчёта: время, номер процесса и счётчик — два журнала, открытые в одну наносекунду, не совпадут.
fn new_instance() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    (nanos.rotate_left(17) ^ u64::from(std::process::id()) ^ count) | 1
}

impl EventLog {
    /// `file` = None — только память (демо-режим).
    pub fn open(file: Option<PathBuf>) -> EventLog {
        let mut items = VecDeque::new();
        let mut bytes = 0;
        // Сбои чтения и ротации при открытии: журнала ещё нет, они становятся его первыми событиями (только в памяти).
        let mut notices = Vec::new();
        if let Some(path) = &file {
            if std::fs::metadata(path).map(|m| m.len() > ROTATE_BYTES).unwrap_or(false) {
                notices.extend(rotate(path));
            }
            // Нет файла — не ошибка (первый запуск); `unwrap_or(0)` ниже: размер нужен только для ротации на ходу.
            bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            match std::fs::read_to_string(path) {
                Ok(text) => {
                    items.extend(text.lines().filter_map(Event::parse_line));
                    while items.len() > KEEP {
                        items.pop_front();
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => notices.push(trf("ev.log_read_failed", &[&path.display().to_string(), &e.to_string()])),
            }
        }
        let loaded = items.len() as u64;
        let mut log = EventLog { items, file, first_seq: 1, instance: new_instance(), loaded, bytes, write_failed: false };
        for text in notices {
            log.remember(Event::new(crate::monitor::unix_now(), "", Severity::Bad, &text, false));
        }
        log
    }

    pub fn instance(&self) -> u64 {
        self.instance
    }

    pub fn loaded(&self) -> u64 {
        self.loaded
    }

    pub fn push(&mut self, event: Event) {
        if let Some(path) = self.file.clone() {
            self.write(&path, &event);
        }
        self.remember(event);
    }

    /// Строка в файл; ротация по размеру — на ходу, а не только при старте: ядро работает месяцами.
    fn write(&mut self, path: &std::path::Path, event: &Event) {
        let line = format!("{}\n", event.line());
        if self.bytes + line.len() as u64 > ROTATE_BYTES {
            if let Some(text) = rotate(path) {
                self.remember(Event::new(event.at, "", Severity::Bad, &text, false));
            }
            self.bytes = 0;
        }
        match append(path, &line) {
            Ok(()) => {
                self.bytes += line.len() as u64;
                self.write_failed = false;
            }
            Err(e) if !self.write_failed => {
                // Событие о сбое — только в память: в файл оно всё равно не запишется.
                self.write_failed = true;
                let text = trf("ev.log_write_failed", &[&path.display().to_string(), &e.to_string()]);
                self.remember(Event::new(event.at, "", Severity::Bad, &text, false));
            }
            // Повторный сбой того же файла: о нём уже сообщено событием выше, пока запись не удастся.
            Err(_) => {}
        }
    }

    fn remember(&mut self, event: Event) {
        self.items.push_back(event);
        while self.items.len() > KEEP {
            self.items.pop_front();
            self.first_seq += 1;
        }
    }

    /// События с номером больше `after` — вместе с номерами.
    pub fn since(&self, after: u64) -> Vec<(u64, Event)> {
        let skip = (after + 1).saturating_sub(self.first_seq) as usize;
        self.items.iter().enumerate().skip(skip).map(|(i, e)| (self.first_seq + i as u64, e.clone())).collect()
    }
}

/// Дописать одно событие в файл журнала, не открывая `EventLog`: без чтения, разбора и ротации. Для мест, где
/// живой журнал держит другой владелец (хук паники, остановка службы, обновление): второй `EventLog` на том же
/// файле мог бы провернуть ротацию за спиной первого, а хуку паники нечего разбирать мегабайт.
pub fn append_event(path: &std::path::Path, event: &Event) -> std::io::Result<()> {
    append(path, &format!("{}\n", event.line()))
}

fn append(path: &std::path::Path, line: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::OpenOptions::new().create(true).append(true).open(path)?.write_all(line.as_bytes())
}

/// Старый файл уходит в `.1.log` (прежний `.1.log` затирается): журнал занимает не больше двух лимитов.
/// Не вышло — текст события о сбое: служба и окно без консоли, `eprintln!` никто не прочёл бы.
fn rotate(path: &std::path::Path) -> Option<String> {
    let to = path.with_extension("1.log");
    std::fs::rename(path, &to).err().map(|e| trf("ev.log_rotate_failed", &[&path.display().to_string(), &e.to_string()]))
}

/// Позиция окна в журнале ядра: номер последнего полученного события и код отсчёта, к которому он относится.
#[derive(Default)]
pub struct Cursor {
    instance: u64,
    seq: u64,
    synced: bool,
}

/// Что сделать с ответом ядра: события для журнала окна и можно ли о них уведомлять.
pub struct Batch {
    pub events: Vec<(u64, Event)>,
    pub quiet: bool,
}

impl Cursor {
    /// Что спросить у ядра: события новее этого номера.
    pub fn after(&self) -> u64 {
        self.seq
    }

    /// Принять ответ ядра. Код отсчёта сменился (ядро перезапущено) — присланные события посчитаны от
    /// старого номера и не годятся: курсор встаёт после подгруженных из файла (их окно уже видело), а новые
    /// придут со следующим опросом. Иначе номера нового ядра шли бы с единицы, и окно молчало бы, пока
    /// их не станет больше старого номера.
    pub fn accept(&mut self, instance: u64, loaded: u64, events: Vec<(u64, Event)>) -> Batch {
        if self.synced && instance != self.instance {
            self.instance = instance;
            self.seq = loaded;
            return Batch { events: Vec::new(), quiet: true };
        }
        // Первое знакомство: пришло всё, что есть в журнале, — это история, а не новости.
        let quiet = !self.synced;
        self.synced = true;
        self.instance = instance;
        if let Some((last, _)) = events.last() {
            self.seq = *last;
        }
        Batch { events, quiet }
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

    #[test]
    fn since_returns_newer_events_with_numbers_after_rotation() {
        let mut log = EventLog::open(None);
        for i in 0..KEEP + 3 {
            log.push(Event::new(i as u64, "t", Severity::Info, "x", false));
        }
        let all = log.since(0);
        assert_eq!(all.len(), KEEP);
        assert_eq!(all[0].0, 4, "первые три вытеснены");
        let last = all.last().unwrap().0;
        assert!(log.since(last).is_empty());
        assert_eq!(log.since(last - 2).iter().map(|(n, _)| *n).collect::<Vec<_>>(), vec![last - 1, last]);
    }

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

    /// `events.log` до единого формата даты (с дефисами) читается; новые строки пишутся с точками.
    #[test]
    fn old_dashed_log_lines_are_still_read_and_new_ones_use_dots() {
        let old = Event::parse_line("2026-10-05 14:07:33\tWARN\ta\tпинг").expect("old line");
        let new = Event::parse_line("2026.10.05 14:07:33\tWARN\ta\tпинг").expect("new line");
        assert_eq!(old, new);
        assert_eq!((old.severity, old.tunnel.as_str(), old.text.as_str()), (Severity::Warn, "a", "пинг"));
        assert!(Event::new(old.at, "a", Severity::Info, "x", false).line().starts_with("2026.1"), "новая строка — ГГГГ.ММ.ДД");
        assert_eq!(Event::parse_line("garbage\tWARN\ta\tx"), None, "непонятное время — строка пропускается");
    }

    #[test]
    fn append_event_neither_rotates_nor_reads_the_file() {
        let dir = temp_dir("append");
        let path = dir.join("events.log");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, vec![b'a'; ROTATE_BYTES as usize + 10]).unwrap();
        append_event(&path, &Event::new(1_790_000_000, "", Severity::Bad, "паника", false)).unwrap();
        assert!(!path.with_extension("1.log").exists(), "ротация — дело владельца журнала, не сторонней записи");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.ends_with("\tFAIL\t\tпаника\n"), "строка дописана в конец");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("awg-ui-events-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn unreadable_log_file_is_reported_not_silently_dropped() {
        let dir = temp_dir("unreadable");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("events.log");
        std::fs::write(&path, [0xFF, 0xFE, 0xFD]).unwrap(); // не UTF-8: история не читается
        let log = EventLog::open(Some(path));
        let shown = log.since(0);
        assert_eq!(shown.len(), 1, "{shown:?}");
        assert_eq!(shown[0].1.severity, Severity::Bad);
        assert!(shown[0].1.text.contains("events.log"), "в сообщении путь файла: {}", shown[0].1.text);
        // Нет файла вовсе — первый запуск, сообщения нет.
        assert!(EventLog::open(Some(dir.join("none.log"))).since(0).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn log_rotates_by_size_without_restart() {
        let dir = temp_dir("rotate");
        let path = dir.join("events.log");
        let mut log = EventLog::open(Some(path.clone()));
        let text = "x".repeat(4000);
        let lines = (ROTATE_BYTES as usize / 4000) + 20;
        for i in 0..lines {
            log.push(Event::new(1_790_000_000 + i as u64, "a", Severity::Info, &text, false));
        }
        assert!(path.with_extension("1.log").exists(), "ротация на ходу, без перезапуска");
        let live = std::fs::metadata(&path).unwrap().len();
        assert!(live < ROTATE_BYTES, "текущий файл не растёт без границы: {live}");
        // Две генерации вместе хранят все записанные строки: ничего не потеряно при переименовании.
        let kept = |p: PathBuf| std::fs::read_to_string(p).unwrap().lines().count();
        assert_eq!(kept(path.clone()) + kept(path.with_extension("1.log")), lines);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn size_counter_starts_from_existing_file() {
        let dir = temp_dir("counter");
        let path = dir.join("events.log");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, vec![b'a'; ROTATE_BYTES as usize - 10]).unwrap();
        let mut log = EventLog::open(Some(path.clone()));
        log.push(Event::new(1_790_000_000, "a", Severity::Info, "хватит, чтобы перейти границу", false));
        assert!(path.with_extension("1.log").exists(), "счётчик начат с размера файла, а не с нуля");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn write_failure_is_reported_once() {
        let dir = temp_dir("failure");
        std::fs::create_dir_all(&dir).unwrap();
        let blocker = dir.join("not-a-dir");
        std::fs::write(&blocker, "x").unwrap();
        let mut log = EventLog::open(Some(blocker.join("events.log")));
        for _ in 0..3 {
            log.push(Event::new(1, "a", Severity::Info, "e", false));
        }
        let bad: Vec<&Event> = log.items.iter().filter(|e| e.severity == Severity::Bad).collect();
        assert_eq!(bad.len(), 1, "сбой записи — одно событие, а не по одному на запись");
        assert_eq!(log.items.len(), 4, "сами события в памяти остались");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn every_open_gets_its_own_instance() {
        let a = EventLog::open(None);
        let b = EventLog::open(None);
        assert_ne!(a.instance(), 0);
        assert_ne!(a.instance(), b.instance());
    }

    fn numbered(range: std::ops::RangeInclusive<u64>) -> Vec<(u64, Event)> {
        range.map(|n| (n, Event::new(n, "t", Severity::Info, "x", true))).collect()
    }

    #[test]
    fn cursor_first_contact_is_quiet_then_follows_numbers() {
        let mut c = Cursor::default();
        let first = c.accept(7, 0, numbered(1..=5));
        assert!(first.quiet && first.events.len() == 5);
        assert_eq!(c.after(), 5);
        let next = c.accept(7, 0, numbered(6..=6));
        assert!(!next.quiet && next.events.len() == 1);
        assert_eq!(c.after(), 6);
        assert!(c.accept(7, 0, Vec::new()).events.is_empty());
        assert_eq!(c.after(), 6, "пустой ответ номер не сбрасывает");
    }

    #[test]
    fn cursor_resyncs_after_core_restart() {
        let mut c = Cursor::default();
        c.accept(7, 0, numbered(1..=900));
        assert_eq!(c.after(), 900);
        // Ядро перезапущено: подгрузило 3 события из файла, номера пошли с единицы, 900 для него «из будущего».
        let restart = c.accept(8, 3, Vec::new());
        assert!(restart.events.is_empty());
        assert_eq!(c.after(), 3, "курсор встал после подгруженных из файла");
        // Первое новое событие нового ядра (номер 4) доходит и с уведомлением.
        let next = c.accept(8, 3, numbered(4..=4));
        assert!(!next.quiet);
        assert_eq!(next.events[0].0, 4);
    }

    #[test]
    fn cursor_drops_stale_batch_on_restart() {
        // Ответ нового ядра на старый номер содержит чужую выборку (здесь — с 51): после смены кода её не берём.
        let mut c = Cursor::default();
        c.accept(7, 0, numbered(1..=50));
        let batch = c.accept(8, 10, numbered(51..=60));
        assert!(batch.events.is_empty());
        assert_eq!(c.after(), 10);
    }

    #[test]
    fn old_core_without_instance_keeps_working() {
        let mut c = Cursor::default();
        c.accept(0, 0, numbered(1..=2));
        let next = c.accept(0, 0, numbered(3..=3));
        assert_eq!(next.events.len(), 1);
        assert_eq!(c.after(), 3);
    }
}
