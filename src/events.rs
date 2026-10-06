//! Журнал событий: подключения, отключения, потеря и восстановление связи.
//! В памяти — последние записи для окна; на диске — `events.log` в корзине логов
//! (`C:\Temp\<пользователь>\awg-ui\logs\`), ротация при 1 МиБ.
//!
//! Файл ведёт только агент (`daemon::agent::journal`): он забирает события ядра по курсору (`Cursor`) и дописывает
//! их вместе со своими; у ядра журнал только в памяти. Файл пишет отдельный поток `events-writer`
//! (`EventLog::write_in_background`): запись события — только память и очередь, без обращения к диску, — зависший
//! диск не держит блокировку журнала. За строкой события ядра в файле стоит строка-метка с его местом в журнале ядра
//! (`Origin`): по последней метке перезапущенный агент продолжает с того же места, без повторов.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use crate::fmt;
use crate::health::Level;
use crate::i18n::{tr, trf};

/// Ядро держит журнал только в памяти: столько событий агент может забрать после своего перезапуска.
const KEEP: usize = 2000;
const ROTATE_BYTES: u64 = 1024 * 1024;
/// Очередь к потоку записи: столько событий переживают остановку диска; дальше — пропуск со счётчиком.
const QUEUE: usize = 1024;
/// Начало строки-метки: старые версии её не разбирают (нет времени) и пропускают, как любую непонятную строку.
const MARK: &str = "#core\t";

#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum Severity {
    Info,
    Warn,
    Bad,
}

impl Severity {
    pub(crate) fn as_str(self) -> &'static str {
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
    /// Место в журнале ядра, если событие пришло от ядра (у агента и в окне). Ядро номера шлёт рядом, поле пусто.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<Origin>,
}

/// Место события в журнале ядра: код отсчёта (`EventLog::instance`) и номер.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct Origin {
    pub instance: u64,
    pub seq: u64,
}

impl Origin {
    /// Входит ли событие в журнал, где события ядра есть подряд до `mark` включительно.
    pub fn covered_by(self, mark: Option<Origin>) -> bool {
        mark.is_some_and(|m| m.instance == self.instance && self.seq <= m.seq)
    }

    fn mark_line(self) -> String {
        format!("{MARK}{}\t{}\n", self.instance, self.seq)
    }

    fn parse_mark(line: &str) -> Option<Origin> {
        let (instance, seq) = line.strip_prefix(MARK)?.split_once('\t')?;
        Some(Origin { instance: instance.parse().ok()?, seq: seq.parse().ok()? })
    }
}

impl Event {
    pub fn new(at: u64, tunnel: &str, severity: Severity, text: &str, notify: bool) -> Event {
        Event { at, tunnel: tunnel.to_string(), severity, text: text.to_string(), notify, origin: None }
    }

    /// То же событие с местом в журнале ядра.
    pub fn from_core(self, origin: Origin) -> Event {
        Event { origin: Some(origin), ..self }
    }

    pub(crate) fn line(&self) -> String {
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
    /// Номер `items[0]`; номера идут подряд — окно спрашивает у ядра события новее последнего полученного.
    first_seq: u64,
    /// Номера начинаются заново при каждом открытии журнала (перезапуск ядра): по этому коду окно узнаёт новый
    /// отсчёт. Не ноль — ноль значит «ядро не сообщает код» (старая версия).
    instance: u64,
    /// Сколько событий подгружено из файла при открытии: у них номера 1..=loaded, это не новые события.
    loaded: u64,
    /// Окно: история агента полна по событиям ядра до этого места (`merge_history`) — такие события от ядра не
    /// повторяются.
    covered: Option<Origin>,
    persist: Persist,
}

/// Куда уходят записанные события.
enum Persist {
    /// Только память (демо-режим, окно без файла).
    Memory,
    /// Сразу в файл, в потоке вызывающего: установка, ранние отказы запуска, журнал ошибок окна — там нет путей VPN,
    /// а запись должна быть на диске до выхода.
    Direct(Box<dyn Sink>),
    /// Через очередь потоку `events-writer`.
    Queued(Feed),
}

/// Хранилище строк журнала: файл с ротацией (`LogFile`) или подмена в тесте.
pub(crate) trait Sink: Send {
    /// Записать событие. Возвращает тексты сбоев (запись, ротация) — они идут в журнал в памяти.
    fn write(&mut self, event: &Event) -> Vec<String>;
}

/// Файл журнала с ротацией по размеру.
struct LogFile {
    path: PathBuf,
    /// Сколько байт сейчас в файле: ротация по размеру без обращения к диску на каждую запись.
    bytes: u64,
    /// Запись в файл не удалась — об этом уже сказано событием, повторять на каждой записи незачем.
    write_failed: bool,
}

impl Sink for LogFile {
    /// Строка в файл; ротация по размеру — на ходу, а не только при старте: ядро работает месяцами.
    fn write(&mut self, event: &Event) -> Vec<String> {
        let mut notices = Vec::new();
        // Строка и её метка — одной записью: перезапуск агента между ними дал бы повтор или потерю события.
        let mut line = format!("{}\n", event.line());
        if let Some(origin) = event.origin {
            line.push_str(&origin.mark_line());
        }
        if self.bytes + line.len() as u64 > ROTATE_BYTES {
            notices.extend(rotate(&self.path));
            self.bytes = 0;
        }
        match append(&self.path, &line) {
            Ok(()) => {
                self.bytes += line.len() as u64;
                self.write_failed = false;
            }
            Err(e) if !self.write_failed => {
                // Событие о сбое — только в память: в файл оно всё равно не запишется.
                self.write_failed = true;
                notices.push(trf("ev.log_write_failed", &[&self.path.display().to_string(), &e.to_string()]));
            }
            // Повторный сбой того же файла: о нём уже сообщено событием выше, пока запись не удастся.
            Err(_) => {}
        }
        notices
    }
}

enum Msg {
    Event(Event),
    /// Столько событий не попало в очередь (она была полна) — в файле на этом месте дыра.
    Gap(u64),
    /// Дописать всё, что в очереди, и ответить.
    Flush(mpsc::Sender<()>),
}

/// Сторона журнала у очереди к потоку записи.
#[derive(Clone)]
pub(crate) struct Feed {
    tx: SyncSender<Msg>,
    /// Пропущено событий с последней записи о пропуске. Общий с потоком: он запишет итог, когда очередь опустеет.
    dropped: Arc<AtomicU64>,
}

impl Feed {
    /// Не ждёт никогда. `false` — потока записи больше нет.
    fn send(&self, event: Event) -> bool {
        // Сначала — запись о прежнем пропуске, чтобы дыра в файле стояла на своём месте.
        let gap = self.dropped.swap(0, Ordering::SeqCst);
        if gap > 0 && self.tx.try_send(Msg::Gap(gap)).is_err() {
            self.dropped.fetch_add(gap, Ordering::SeqCst);
        }
        match self.tx.try_send(Msg::Event(event)) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::SeqCst);
                true
            }
            Err(TrySendError::Disconnected(_)) => false,
        }
    }

    /// Дождаться, пока поток допишет очередь. `false` — не успел за `timeout`.
    pub(crate) fn flush(&self, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        let (ack, done) = mpsc::channel();
        let mut msg = Msg::Flush(ack);
        loop {
            match self.tx.try_send(msg) {
                Ok(()) => break,
                Err(TrySendError::Disconnected(_)) => return false,
                Err(TrySendError::Full(back)) if std::time::Instant::now() < deadline => {
                    msg = back;
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(TrySendError::Full(_)) => return false,
            }
        }
        done.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())).is_ok()
    }
}

/// Поток записи: очередь → `sink`. Паника `sink` не роняет ядро: сообщение в журнал в памяти, поток продолжает
/// со следующего события. Завершается, когда журнал (последний `Feed`) закрыт.
fn run_writer(rx: Receiver<Msg>, mut sink: Box<dyn Sink>, dropped: &AtomicU64, notice: impl Fn(Event)) {
    loop {
        match crate::crash::isolate(|| drain(&rx, sink.as_mut(), dropped, &notice)) {
            Ok(()) => return,
            Err(panic) => notice(failure(trf("ev.log_writer_failed", &[&panic]))),
        }
    }
}

fn drain(rx: &Receiver<Msg>, sink: &mut dyn Sink, dropped: &AtomicU64, notice: &dyn Fn(Event)) {
    loop {
        let msg = match rx.try_recv() {
            Ok(msg) => msg,
            Err(TryRecvError::Empty) => {
                // Очередь разобрана — дыра от пропуска, если была, отмечается сейчас, а не со следующим событием.
                write_gap(sink, dropped.swap(0, Ordering::SeqCst), notice);
                match rx.recv() {
                    Ok(msg) => msg,
                    Err(_) => return,
                }
            }
            Err(TryRecvError::Disconnected) => {
                write_gap(sink, dropped.swap(0, Ordering::SeqCst), notice);
                return;
            }
        };
        match msg {
            Msg::Event(event) => sink.write(&event).into_iter().map(failure).for_each(notice),
            Msg::Gap(count) => write_gap(sink, count, notice),
            Msg::Flush(ack) => {
                write_gap(sink, dropped.swap(0, Ordering::SeqCst), notice);
                // Ответ некому — ждавший уже ушёл по таймауту и сам сообщил о нём.
                let _ = ack.send(());
            }
        }
    }
}

/// Одна запись о пропуске — в файл и в память: события в памяти целы, но в файле их нет.
fn write_gap(sink: &mut dyn Sink, count: u64, notice: &dyn Fn(Event)) {
    if count == 0 {
        return;
    }
    let event = Event::new(crate::monitor::unix_now(), "", Severity::Warn, &trf("ev.log_dropped", &[&count.to_string()]), false);
    sink.write(&event).into_iter().map(failure).for_each(notice);
    notice(event);
}

/// Событие о сбое самого журнала: только в память, без уведомления.
fn failure(text: String) -> Event {
    Event::new(crate::monitor::unix_now(), "", Severity::Bad, &text, false)
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
                    for line in text.lines() {
                        match (Event::parse_line(line), Origin::parse_mark(line)) {
                            (Some(event), _) => items.push_back(event),
                            // Метка относится к строке события перед ней.
                            (None, Some(origin)) => {
                                if let Some(last) = items.back_mut() {
                                    last.origin = Some(origin);
                                }
                            }
                            (None, None) => {}
                        }
                    }
                    while items.len() > KEEP {
                        items.pop_front();
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => notices.push(trf("ev.log_read_failed", &[&path.display().to_string(), &e.to_string()])),
            }
        }
        let loaded = items.len() as u64;
        let persist = match file {
            Some(path) => Persist::Direct(Box::new(LogFile { path, bytes, write_failed: false })),
            None => Persist::Memory,
        };
        let mut log = EventLog { items, first_seq: 1, instance: new_instance(), loaded, covered: None, persist };
        for text in notices {
            log.remember(failure(text));
        }
        log
    }

    /// Дальше файл пишет поток `events-writer`: `push` больше не трогает диск. Журнал без файла не меняется.
    pub fn write_in_background(log: &Arc<Mutex<EventLog>>) {
        let sink = {
            let mut guard = crate::crash::lock(log);
            match std::mem::replace(&mut guard.persist, Persist::Memory) {
                Persist::Direct(sink) => sink,
                other => {
                    guard.persist = other;
                    return;
                }
            }
        };
        Self::start_writer(log, sink, QUEUE);
    }

    /// `sink` и `capacity` — подмена для проверок; в работе `write_in_background`.
    pub(crate) fn start_writer(log: &Arc<Mutex<EventLog>>, sink: Box<dyn Sink>, capacity: usize) {
        let (tx, rx) = mpsc::sync_channel(capacity);
        let dropped = Arc::new(AtomicU64::new(0));
        crate::crash::lock(log).persist = Persist::Queued(Feed { tx, dropped: dropped.clone() });
        // Слабая ссылка: поток не держит журнал живым, а закрытый журнал закрывает очередь, и поток выходит.
        let back: Weak<Mutex<EventLog>> = Arc::downgrade(log);
        crate::crash::spawn_named("events-writer", move || {
            run_writer(rx, sink, &dropped, |notice| {
                if let Some(log) = back.upgrade() {
                    // Только память, без диска: блокировку журнала держат лишь на время этой вставки.
                    crate::crash::lock(&log).remember(notice);
                }
            })
        });
    }

    /// Очередь к потоку записи (если он запущен) — дождаться её разбора, не держа блокировку журнала.
    pub(crate) fn feed(&self) -> Option<Feed> {
        match &self.persist {
            Persist::Queued(feed) => Some(feed.clone()),
            _ => None,
        }
    }

    pub fn instance(&self) -> u64 {
        self.instance
    }

    pub fn loaded(&self) -> u64 {
        self.loaded
    }

    /// Номер последнего события; 0 — событий ещё не было.
    pub fn last_seq(&self) -> u64 {
        self.first_seq + self.items.len() as u64 - 1
    }

    pub fn push(&mut self, event: Event) {
        let notices = match &mut self.persist {
            Persist::Memory => Vec::new(),
            Persist::Direct(sink) => sink.write(&event),
            Persist::Queued(feed) => {
                if feed.send(event.clone()) {
                    Vec::new()
                } else {
                    // Поток записи не выходит сам, пока жив журнал; раз его нет — дальше только память, сказать один раз.
                    self.persist = Persist::Memory;
                    vec![tr("ev.log_writer_gone")]
                }
            }
        };
        // Сначала сбои записи, затем само событие: вызывающие (журнал ошибок окна) берут последнее как своё.
        for text in notices {
            self.remember(failure(text));
        }
        self.remember(event);
    }

    /// Окно: событие ядра, которое уже есть (пришло другим путём — от ядра или от агента), не повторяется.
    /// `false` — повтор, не записано.
    pub fn push_unique(&mut self, event: Event) -> bool {
        if let Some(origin) = event.origin {
            if origin.covered_by(self.covered) || self.items.iter().rev().any(|e| e.origin == Some(origin)) {
                return false;
            }
        }
        self.push(event);
        true
    }

    /// Окно: история от агента — в начало журнала. `covered` — до какого места журнала ядра история полна: события
    /// ядра до него, уже пришедшие от ядра, уходят (в истории они есть, хоть и без номера, если прочитаны из файла).
    pub fn merge_history(&mut self, history: Vec<Event>, covered: Option<Origin>) {
        let known: BTreeSet<Origin> = history.iter().filter_map(|e| e.origin).collect();
        let rest: Vec<Event> =
            self.items.drain(..).filter(|e| e.origin.is_none_or(|o| !o.covered_by(covered) && !known.contains(&o))).collect();
        self.covered = covered;
        for event in history.into_iter().chain(rest) {
            self.remember(event);
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

/// Последняя метка события ядра в файле журнала (если после ротации в нём меток ещё нет — в `.1.log`): с этого места
/// агент продолжает забирать события ядра. Нет файла или меток — `None` (первый запуск: забрать всё, что есть у ядра).
pub fn last_core_mark(path: &std::path::Path) -> Option<Origin> {
    let last_in = |p: &std::path::Path| match std::fs::read_to_string(p) {
        Ok(text) => text.lines().rev().find_map(Origin::parse_mark),
        // Нечитаемый файл: о нём уже сказал `EventLog::open`; без метки агент заберёт всё, что ещё есть у ядра.
        Err(_) => None,
    };
    last_in(path).or_else(|| last_in(&path.with_extension("1.log")))
}

/// Дописать одно событие в файл журнала, не открывая `EventLog`: без чтения, разбора и ротации. Для мест, где
/// живой журнал держит другой владелец (хук паники, остановка службы, обновление): второй `EventLog` на том же
/// файле мог бы провернуть ротацию за спиной первого, а хуку паники нечего разбирать мегабайт.
pub fn append_event(path: &std::path::Path, event: &Event) -> std::io::Result<()> {
    append_events(path, std::slice::from_ref(event))
}

/// То же для нескольких событий — одной записью (хвост журнала ядра на его остановке).
pub fn append_events(path: &std::path::Path, events: &[Event]) -> std::io::Result<()> {
    append(path, &events.iter().map(|e| format!("{}\n", e.line())).collect::<String>())
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
    /// Продолжить с места, до которого события уже забраны (агент после перезапуска — по метке в файле).
    pub fn resume(at: Origin) -> Cursor {
        Cursor { instance: at.instance, seq: at.seq, synced: true }
    }

    /// До какого места забраны события; `None` — ответа ещё не было.
    pub fn position(&self) -> Option<Origin> {
        self.synced.then_some(Origin { instance: self.instance, seq: self.seq })
    }

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

    #[test]
    fn last_seq_is_zero_for_an_empty_log_and_follows_rotation() {
        let mut log = EventLog::open(None);
        assert_eq!(log.last_seq(), 0);
        for i in 0..KEEP + 3 {
            log.push(Event::new(i as u64, "t", Severity::Info, "x", false));
        }
        assert_eq!(log.last_seq(), (KEEP + 3) as u64);
        assert_eq!(log.since(0).last().unwrap().0, log.last_seq());
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

    /// Событие ядра пишется с меткой места; при чтении метка возвращается к своему событию, а старый разбор строк
    /// (версии без меток) её пропускает.
    #[test]
    fn core_mark_follows_its_event_through_the_file() {
        let dir = temp_dir("mark");
        let path = dir.join("events.log");
        let mut log = EventLog::open(Some(path.clone()));
        let origin = Origin { instance: 42, seq: 7 };
        log.push(Event::new(1_790_000_000, "a", Severity::Info, "from core", false).from_core(origin));
        log.push(Event::new(1_790_000_001, "", Severity::Warn, "agent own", false));
        assert_eq!(last_core_mark(&path), Some(origin));
        let back = EventLog::open(Some(path.clone()));
        let read: Vec<(&str, Option<Origin>)> = back.items.iter().map(|e| (e.text.as_str(), e.origin)).collect();
        assert_eq!(read, [("from core", Some(origin)), ("agent own", None)]);
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().filter_map(Event::parse_line).count(), 2, "метка — не событие: {text}");
        assert_eq!(last_core_mark(&dir.join("none.log")), None, "нет файла — забрать всё");
        std::fs::remove_dir_all(&dir).unwrap();
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

    /// Подменный диск: пишет в память; пока ворота закрыты — стоит, как зависший диск или антивирус.
    #[derive(Clone, Default)]
    struct GatedSink {
        open: Arc<(Mutex<bool>, std::sync::Condvar)>,
        written: Arc<Mutex<Vec<Event>>>,
        /// Паника на событии с таким текстом — сбой самого хранилища.
        panic_on: Option<&'static str>,
        /// Сколько записей начато (в том числе стоящих у ворот).
        entered: Arc<AtomicU64>,
    }

    impl GatedSink {
        fn closed() -> GatedSink {
            GatedSink::default()
        }

        fn open(&self) {
            *self.open.0.lock().unwrap() = true;
            self.open.1.notify_all();
        }
    }

    impl Sink for GatedSink {
        fn write(&mut self, event: &Event) -> Vec<String> {
            self.entered.fetch_add(1, Ordering::SeqCst);
            let mut open = self.open.0.lock().unwrap();
            while !*open {
                open = self.open.1.wait(open).unwrap();
            }
            drop(open);
            if self.panic_on == Some(event.text.as_str()) {
                panic!("sink broke on {}", event.text);
            }
            self.written.lock().unwrap().push(event.clone());
            Vec::new()
        }
    }

    fn background(sink: &GatedSink, capacity: usize) -> Arc<Mutex<EventLog>> {
        let log = Arc::new(Mutex::new(EventLog::open(None)));
        EventLog::start_writer(&log, Box::new(sink.clone()), capacity);
        log
    }

    fn flush(log: &Arc<Mutex<EventLog>>) {
        let feed = log.lock().unwrap().feed().expect("журнал с потоком записи");
        assert!(feed.flush(Duration::from_secs(10)), "поток записи разобрал очередь");
    }

    #[test]
    fn stalled_disk_drops_overflow_and_writes_one_counter_record() {
        let sink = GatedSink::closed();
        let log = background(&sink, 4);
        let total = 20;
        // Первое событие поток взял и стоит на диске: дальше очередь заполняется предсказуемо.
        log.lock().unwrap().push(Event::new(0, "t", Severity::Info, "e0", false));
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while sink.entered.load(Ordering::SeqCst) == 0 {
            assert!(std::time::Instant::now() < deadline, "поток записи не взял событие");
            std::thread::yield_now();
        }
        for i in 1..total {
            let started = std::time::Instant::now();
            log.lock().unwrap().push(Event::new(i, "t", Severity::Info, &format!("e{i}"), false));
            assert!(started.elapsed() < Duration::from_millis(10), "запись не ждёт диск: {:?}", started.elapsed());
        }
        assert_eq!(log.lock().unwrap().items.len(), total as usize, "в памяти все события, диск им не помеха");
        sink.open();
        flush(&log);
        let written = sink.written.lock().unwrap().clone();
        let gaps: Vec<&Event> = written.iter().filter(|e| e.tunnel.is_empty()).collect();
        assert_eq!(gaps.len(), 1, "одна запись о пропуске, а не по одной на событие: {written:?}");
        let events = written.len() - 1;
        assert_eq!(events, 1 + 4, "одно у диска и полная очередь; остальные пропущены и посчитаны");
        assert_eq!(gaps[0].text, trf("ev.log_dropped", &[&(total as usize - events).to_string()]));
        assert_eq!(written.last().unwrap().tunnel, "", "запись о пропуске — после записанных до него событий");
        let log = log.lock().unwrap();
        assert!(log.items.iter().any(|e| e.text == gaps[0].text), "о пропуске знает и журнал в памяти");
    }

    #[test]
    fn writer_survives_a_sink_panic_and_keeps_writing() {
        let mut sink = GatedSink::closed();
        sink.panic_on = Some("boom");
        sink.open();
        let log = background(&sink, 16);
        for text in ["before", "boom", "after"] {
            log.lock().unwrap().push(Event::new(1, "t", Severity::Info, text, false));
        }
        flush(&log);
        let written: Vec<String> = sink.written.lock().unwrap().iter().map(|e| e.text.clone()).collect();
        assert_eq!(written, ["before", "after"], "поток записи жив после паники хранилища");
        let log = log.lock().unwrap();
        let failure = log.items.iter().find(|e| e.severity == Severity::Bad).expect("сбой записи — событие в памяти");
        assert!(failure.text.contains("sink broke on boom"), "{}", failure.text);
    }

    #[test]
    fn background_writer_writes_and_rotates_the_real_file() {
        let dir = temp_dir("background");
        let path = dir.join("events.log");
        let log = Arc::new(Mutex::new(EventLog::open(Some(path.clone()))));
        EventLog::write_in_background(&log);
        let text = "x".repeat(4000);
        let lines = (ROTATE_BYTES as usize / 4000) + 20;
        for i in 0..lines {
            log.lock().unwrap().push(Event::new(1_790_000_000 + i as u64, "a", Severity::Info, &text, false));
        }
        flush(&log);
        assert!(path.with_extension("1.log").exists(), "ротация — в потоке записи");
        let kept = |p: PathBuf| std::fs::read_to_string(p).unwrap().lines().count();
        assert_eq!(kept(path.clone()) + kept(path.with_extension("1.log")), lines, "очередь {QUEUE} вместила всё");
        drop(log);
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
