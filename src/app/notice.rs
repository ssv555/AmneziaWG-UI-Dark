//! Подсказка в строке состояния: ход операции, успех или предупреждение. Ошибки сюда не попадают — они идут
//! в журнал событий (`errors::ErrorSink`), а строка состояния показывает ссылку на него.

use std::sync::{Arc, Mutex};

use crate::crash::lock;
use crate::events::Severity;
use crate::monitor::Shared;

/// Вид подсказки: от него цвет в строке состояния и запись в журнал событий.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum NoticeKind {
    /// Идёт операция или подсказка, что делать дальше: обычный текст, в журнал не пишется (итог операции напишется).
    Progress,
    /// Сделано: зелёный, в журнал — «информация».
    Done,
    /// Сделано не всё или отказ пользователя (UAC, часть файлов не принята): жёлтый, в журнал — «предупреждение».
    Warning,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Notice {
    pub(super) kind: NoticeKind,
    pub(super) text: String,
}

/// Одна подсказка на окно; пишут её и фоновые потоки. Правило «подсказка тоже в журнале событий» живёт здесь,
/// а не у каждого вызова: успех и предупреждение после закрытия подсказки остаются в журнале.
#[derive(Clone)]
pub(super) struct Notices {
    current: Arc<Mutex<Option<Notice>>>,
    shared: Arc<Shared>,
}

impl Notices {
    pub(super) fn new(shared: Arc<Shared>) -> Notices {
        Notices { current: Default::default(), shared }
    }

    pub(super) fn progress(&self, text: impl Into<String>) {
        self.set(NoticeKind::Progress, text.into());
    }

    pub(super) fn done(&self, text: impl Into<String>) {
        self.post("", NoticeKind::Done, Severity::Info, text.into());
    }

    pub(super) fn warn(&self, text: impl Into<String>) {
        self.post("", NoticeKind::Warning, Severity::Warn, text.into());
    }

    /// Итог действия над туннелем: в журнал под именем туннеля с важностью `severity` и в строку состояния.
    pub(super) fn post(&self, tunnel: &str, kind: NoticeKind, severity: Severity, text: String) {
        self.shared.log(tunnel, severity, &text);
        self.set(kind, text);
    }

    pub(super) fn clear(&self) {
        *lock(&self.current) = None;
    }

    pub(super) fn current(&self) -> Option<Notice> {
        lock(&self.current).clone()
    }

    fn set(&self, kind: NoticeKind, text: String) {
        *lock(&self.current) = Some(Notice { kind, text });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::Options;

    fn notices() -> Notices {
        Notices::new(Arc::new(Shared::new(None, Options { ping: false, ping_host: String::new(), notify: false, tray: false, taskbar: false }, None)))
    }

    fn logged(n: &Notices) -> Vec<(Severity, String)> {
        n.shared.events_since(0).into_iter().map(|(_, e)| (e.severity, e.text)).collect()
    }

    #[test]
    fn success_and_warning_reach_the_log_progress_does_not() {
        let n = notices();
        n.progress("Importing…");
        assert_eq!(n.current().map(|c| c.kind), Some(NoticeKind::Progress));
        assert!(logged(&n).is_empty(), "ход операции — не событие");
        n.done("Imported: 2");
        assert_eq!(n.current(), Some(Notice { kind: NoticeKind::Done, text: "Imported: 2".into() }));
        n.warn("Administrator rights were not granted");
        assert_eq!(n.current().map(|c| c.kind), Some(NoticeKind::Warning));
        n.clear();
        assert_eq!(n.current(), None);
        assert_eq!(
            logged(&n),
            [(Severity::Info, "Imported: 2".to_string()), (Severity::Warn, "Administrator rights were not granted".to_string())],
            "закрытая подсказка остаётся в журнале"
        );
    }
}
