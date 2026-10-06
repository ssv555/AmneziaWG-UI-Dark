//! Ошибки действий окна: из любого потока — в журнал событий сразу, а не при следующем кадре.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use eframe::egui;

use crate::crash::lock;
use crate::events::{Event, EventLog, Severity};
use crate::monitor::{unix_now, Shared};

/// Файл ошибок окна в каталоге журналов (рядом с `crash.log`); ротация по размеру — как у `events.log`.
pub(super) const FILE_NAME: &str = "window-errors.log";

/// Журнал событий — сама очередь: каждая ошибка попадает в него в момент появления, поэтому две ошибки за кадр
/// не затирают друг друга, а скрытое в трее окно (кадры не рисуются) ничего не теряет. Флаг «новая ошибка» нужен
/// только строке состояния — ссылке «Ошибка — открыть журнал событий».
#[derive(Clone)]
pub(super) struct ErrorSink {
    shared: Arc<Shared>,
    fresh: Arc<AtomicBool>,
    ctx: egui::Context,
    /// Файл на диске: журнал в памяти живёт, пока открыто окно, а ошибку нужно уметь разобрать и после него.
    file: Option<Arc<Mutex<EventLog>>>,
}

impl ErrorSink {
    pub(super) fn new(shared: Arc<Shared>, ctx: egui::Context) -> ErrorSink {
        ErrorSink { shared, fresh: Default::default(), ctx, file: None }
    }

    /// Писать ошибки ещё и в файл (ротация — у `EventLog`).
    pub(super) fn persisted_to(mut self, path: PathBuf) -> ErrorSink {
        self.file = Some(Arc::new(Mutex::new(EventLog::open(Some(path)))));
        self
    }

    /// Ошибка действия без привязки к туннелю.
    pub(super) fn push(&self, text: impl AsRef<str>) {
        self.push_for("", text);
    }

    /// Ошибка действия над туннелем.
    pub(super) fn push_for(&self, tunnel: &str, text: impl AsRef<str>) {
        self.shared.log(tunnel, Severity::Bad, text.as_ref());
        self.persist(tunnel, text.as_ref());
        self.fresh.store(true, Ordering::Release);
        self.ctx.request_repaint();
    }

    /// Запись в файл. Сбой записи `EventLog` сообщает событием в собственной памяти — переносим его в журнал окна,
    /// иначе о том, что ошибки больше не сохраняются, никто бы не узнал.
    fn persist(&self, tunnel: &str, text: &str) {
        let Some(file) = &self.file else { return };
        let mut log = lock(file);
        let before = log.since(0).last().map_or(0, |(n, _)| *n);
        log.push(Event::new(unix_now(), tunnel, Severity::Bad, text, false));
        let mut added = log.since(before);
        // Последнее — наше событие (оно запоминается после записи); перед ним может стоять сообщение о сбое.
        added.pop();
        for (_, notice) in added {
            self.shared.log(&notice.tunnel, notice.severity, &notice.text);
        }
    }

    /// Появились ли ошибки с прошлого вопроса.
    pub(super) fn take_fresh(&self) -> bool {
        self.fresh.swap(false, Ordering::AcqRel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::Options;

    fn sink() -> ErrorSink {
        let shared = Arc::new(Shared::new(None, Options { ping: false, ping_host: String::new(), notify: false, tray: false, taskbar: false }, None));
        ErrorSink::new(shared, egui::Context::default())
    }

    fn texts(s: &ErrorSink) -> Vec<String> {
        s.shared.events_since(0).into_iter().map(|(_, e)| e.text).collect()
    }

    #[test]
    fn two_errors_in_one_frame_both_reach_the_log() {
        let s = sink();
        s.push("first");
        s.push(String::from("second"));
        assert_eq!(texts(&s), ["first", "second"]);
        assert!(s.take_fresh());
        assert!(!s.take_fresh(), "флаг сбрасывается после чтения");
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("awg-ui-errors-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn errors_reach_the_file_and_it_rotates_by_size() {
        let dir = temp_dir("rotate");
        let path = dir.join(FILE_NAME);
        let s = sink().persisted_to(path.clone());
        s.push_for("home", "first");
        let text = "x".repeat(4000);
        let lines = 400; // 1.6 МБ — больше предела ротации
        for _ in 1..lines {
            s.push(&text);
        }
        assert!(path.with_extension("1.log").exists(), "ротация на ходу");
        let live = std::fs::metadata(&path).unwrap().len();
        assert!(live < 1024 * 1024, "текущий файл не растёт без границы: {live}");
        let kept = |p: PathBuf| std::fs::read_to_string(p).unwrap().lines().count();
        assert_eq!(kept(path.clone()) + kept(path.with_extension("1.log")), lines, "ничего не потеряно при ротации");
        assert!(std::fs::read_to_string(path.with_extension("1.log")).unwrap().contains("home	first"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unwritable_file_is_reported_once_in_the_window_log() {
        let dir = temp_dir("blocked");
        std::fs::create_dir_all(&dir).unwrap();
        let blocker = dir.join("not-a-dir");
        std::fs::write(&blocker, "x").unwrap();
        let s = sink().persisted_to(blocker.join(FILE_NAME));
        for t in ["a", "b", "c"] {
            s.push(t);
        }
        let log = texts(&s);
        assert_eq!(log.iter().filter(|t| t.as_str() == "a" || t.as_str() == "b" || t.as_str() == "c").count(), 3);
        assert_eq!(log.len(), 4, "три ошибки и одно сообщение, что файл недоступен: {log:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn error_is_logged_without_any_frame() {
        // Окно скрыто в трее: update не вызывается, но ошибка уже в журнале.
        let s = sink();
        s.push_for("home", "boom");
        let log = s.shared.events_since(0);
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].1.tunnel, "home");
        assert_eq!(log[0].1.severity, Severity::Bad);
    }
}
