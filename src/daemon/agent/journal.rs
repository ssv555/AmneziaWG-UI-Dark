//! Журнал событий агента — единственный писатель `events.log` (запись и ротация, поток `events-writer`). Ядро держит
//! события только в памяти; агент забирает их по курсору из `State` ядра (тот же опрос, что у статистики, —
//! `core_poll`: отдельный запрос `Events` к ядру добавил бы второй вызов в секунду, а события в `State` и так уже идут
//! только новее заданного номера) и дописывает вместе со своими. Место в журнале ядра пишется в файл меткой рядом
//! с событием (`events::Origin`): перезапущенный агент продолжает с последней метки без повторов, а перезапущенное
//! ядро (новый код отсчёта) курсор переставляет на начало (`events::Cursor`).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::core_poll::CoreFeed;
use super::proto::AgentEvents;
use crate::crash::lock;
use crate::daemon::proto::CoreState;
use crate::events::{self, Cursor, Event, EventLog, Origin, Severity};
use crate::i18n::{tr, trf};

/// Сколько ждать поток записи перед выходом агента.
const FLUSH_WAIT: Duration = Duration::from_secs(3);

pub(super) struct AgentJournal {
    /// Общий с потоком `events-writer`: он кладёт сюда сбои записи файла.
    log: Arc<Mutex<EventLog>>,
    /// До какого места забран журнал ядра. Замок — после `log`, в этом порядке везде.
    core: Mutex<Cursor>,
}

impl AgentJournal {
    /// Журнал на файле: история и последняя метка ядра — из файла, дальше файл пишет поток `events-writer`.
    pub(super) fn open(file: PathBuf) -> AgentJournal {
        let mark = events::last_core_mark(&file);
        let journal = AgentJournal::new(EventLog::open(Some(file)), mark);
        EventLog::write_in_background(&journal.log);
        journal
    }

    fn new(log: EventLog, mark: Option<Origin>) -> AgentJournal {
        AgentJournal { log: Arc::new(Mutex::new(log)), core: Mutex::new(mark.map_or_else(Cursor::default, Cursor::resume)) }
    }

    /// Только память — для проверок запросов агента.
    #[cfg(test)]
    pub(super) fn memory() -> AgentJournal {
        AgentJournal::new(EventLog::open(None), None)
    }

    /// Событие самого агента.
    pub(super) fn log(&self, severity: Severity, text: &str) {
        self.push(Event::new(crate::monitor::unix_now(), "", severity, text, false));
    }

    pub(super) fn push(&self, event: Event) {
        lock(&self.log).push(event);
    }

    /// Перед выходом процесса: дождаться, пока поток записи допишет очередь. Не успел (диск стоит) — одна строка об
    /// этом напрямую в `file`: процесс всё равно выходит.
    pub(super) fn flush_before_exit(&self, file: PathBuf) {
        let feed = lock(&self.log).feed();
        if feed.is_none_or(|feed| feed.flush(FLUSH_WAIT)) {
            return;
        }
        let event = Event::new(crate::monitor::unix_now(), "", Severity::Warn, &tr("ev.log_flush_timeout"), false);
        if let Err(e) = events::append_event(&file, &event) {
            eprintln!("agent: cannot write {}: {e}", file.display());
        }
    }

    /// Ответ окну на `Events`: события и место в журнале ядра — из одного захвата, пара согласована.
    pub(super) fn since(&self, after: u64) -> AgentEvents {
        let log = lock(&self.log);
        let core = lock(&self.core).position();
        AgentEvents { instance: log.instance(), loaded: log.loaded(), events: log.since(after), core }
    }
}

impl CoreFeed for AgentJournal {
    fn events_after(&self) -> u64 {
        lock(&self.core).after()
    }

    /// События ядра — в журнал с их местом. Ядро вытеснило из памяти часть событий, которых агент ещё не забрал
    /// (агент долго не работал), — запись о пропуске с числом, а не тихая дыра.
    fn accept(&self, state: &CoreState, _: Instant) {
        let mut log = lock(&self.log);
        let mut core = lock(&self.core);
        let expected = core.after() + 1;
        let batch = core.accept(state.events_instance, state.events_loaded, state.events.clone());
        if let Some((first, _)) = batch.events.first() {
            if *first > expected {
                log.push(Event::new(crate::monitor::unix_now(), "", Severity::Warn, &trf("ev.core_events_missed", &[&(first - expected).to_string()]), false));
            }
        }
        for (seq, event) in batch.events {
            log.push(event.with_origin(Origin { instance: state.events_instance, seq }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("awg-agent-journal-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Ответ ядра: события с номерами `seqs` кода отсчёта `instance`.
    fn core(instance: u64, seqs: std::ops::RangeInclusive<u64>) -> CoreState {
        let events = seqs.map(|n| (n, Event::new(1_790_000_000 + n, "t", Severity::Info, &format!("core {instance}/{n}"), false))).collect();
        CoreState { events, events_instance: instance, ..Default::default() }
    }

    /// Журнал на файле без потока записи: строки уходят в файл сразу, проверка не ждёт поток.
    fn direct(file: &std::path::Path) -> AgentJournal {
        AgentJournal::new(EventLog::open(Some(file.to_path_buf())), events::last_core_mark(file))
    }

    fn texts(file: &std::path::Path) -> Vec<String> {
        EventLog::open(Some(file.to_path_buf())).items.iter().map(|e| e.text.clone()).collect()
    }

    #[test]
    fn agent_restart_resumes_from_the_persisted_mark_without_duplicates() {
        let dir = temp_dir("restart");
        let file = dir.join("events.log");
        let first = direct(&file);
        assert_eq!(first.events_after(), 0, "первый запуск — всё, что есть у ядра");
        first.accept(&core(7, 1..=3), Instant::now());
        first.log(Severity::Info, "agent own");
        assert_eq!(first.events_after(), 3);
        drop(first);

        let second = direct(&file);
        assert_eq!(second.events_after(), 3, "после перезапуска — с последней метки в файле");
        // Ядро ещё не знает, что агент перезапущен, и шлёт с номера, который у него спросили.
        second.accept(&core(7, 4..=5), Instant::now());
        assert_eq!(texts(&file), ["core 7/1", "core 7/2", "core 7/3", "agent own", "core 7/4", "core 7/5"]);
        let answer = second.since(0);
        assert_eq!(answer.core, Some(Origin { instance: 7, seq: 5 }));
        let origins: Vec<Option<u64>> = answer.events.iter().map(|(_, e)| e.origin.map(|o| o.seq)).collect();
        assert_eq!(origins, [Some(1), Some(2), Some(3), None, Some(4), Some(5)], "место в ядре прочитано из меток файла");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Обновление с 0.4.0: `events.log` писало ядро 0.4.0 — строки событий без меток места в журнале ядра. Новое ядро
    /// держит события только в памяти и нумерует с единицы. Ловит: строки 0.4.0 не читаются (сменился формат строки
    /// или имена важности) — история в окне пропала бы; файл без меток принимается за «всё уже забрано» и первые
    /// события нового ядра теряются; перезапуск агента после этого пишет их второй раз.
    #[test]
    fn events_log_of_0_4_0_without_marks_is_kept_and_continued() {
        let dir = temp_dir("upgrade");
        let file = dir.join("events.log");
        let v040 = "2026.10.01 12:00:00\tINFO\toffice\tconnected\n\
                    2026.10.01 12:05:00\tWARN\toffice\thandshake late\n\
                    2026-09-30 08:00:00\tFAIL\t\tolder dashed line\n";
        std::fs::write(&file, v040).unwrap();

        let journal = direct(&file);
        assert_eq!(journal.events_after(), 0, "меток нет — у нового ядра забирается всё");
        let history = journal.since(0).events;
        let sev: Vec<(Severity, &str)> = history.iter().map(|(_, e)| (e.severity, e.text.as_str())).collect();
        assert_eq!(sev, [(Severity::Info, "connected"), (Severity::Warn, "handshake late"), (Severity::Bad, "older dashed line")]);
        journal.accept(&core(5, 1..=2), Instant::now());
        drop(journal);

        let again = direct(&file);
        assert_eq!(again.events_after(), 2, "после перезапуска агента — с метки нового ядра");
        again.accept(&core(5, 3..=3), Instant::now());
        assert_eq!(texts(&file), ["connected", "handshake late", "older dashed line", "core 5/1", "core 5/2", "core 5/3"]);
        assert!(std::fs::read_to_string(&file).unwrap().starts_with(v040), "строки 0.4.0 не переписаны");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn core_restart_with_a_new_instance_resyncs_from_the_start() {
        let dir = temp_dir("core-restart");
        let file = dir.join("events.log");
        let journal = direct(&file);
        journal.accept(&core(7, 1..=900), Instant::now());
        // Новое ядро отвечает на старый номер: выборка от чужого номера не годится, курсор — на начало.
        journal.accept(&core(8, 901..=905), Instant::now());
        assert_eq!(journal.events_after(), 0);
        journal.accept(&core(8, 1..=2), Instant::now());
        let tail: Vec<String> = texts(&file).into_iter().rev().take(3).collect();
        assert_eq!(tail, ["core 8/2", "core 8/1", "core 7/900"]);
        assert_eq!(events::last_core_mark(&file), Some(Origin { instance: 8, seq: 2 }));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Ядро вытеснило события, которых агент не забрал: запись о пропуске с числом.
    #[test]
    fn events_lost_in_the_core_ring_are_counted() {
        let journal = AgentJournal::memory();
        journal.accept(&core(7, 1..=2), Instant::now());
        journal.accept(&core(7, 10..=10), Instant::now());
        let texts: Vec<String> = journal.since(0).events.into_iter().map(|(_, e)| e.text).collect();
        let missed = trf("ev.core_events_missed", &["7"]);
        assert_eq!(texts, ["core 7/1", "core 7/2", missed.as_str(), "core 7/10"]);
    }

    /// Свои события агента (обновления, связь с ядром) попадают в файл через поток записи, как события ядра.
    #[test]
    fn agents_own_events_land_in_the_file() {
        let dir = temp_dir("own");
        let file = dir.join("events.log");
        let journal = AgentJournal::open(file.clone());
        journal.log(Severity::Warn, "update check failed");
        journal.accept(&core(3, 1..=1), Instant::now());
        let feed = lock(&journal.log).feed().expect("журнал агента с потоком записи");
        assert!(feed.flush(std::time::Duration::from_secs(10)), "поток записи дописал очередь");
        assert_eq!(texts(&file), ["update check failed", "core 3/1"]);
        assert_eq!(events::last_core_mark(&file), Some(Origin { instance: 3, seq: 1 }));
        drop(journal);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
