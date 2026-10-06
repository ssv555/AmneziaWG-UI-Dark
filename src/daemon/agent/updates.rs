//! Обновления в агенте: менеджер (`update::manager`) с его журналом событий. С ядром менеджер говорит только
//! внутренними запросами по каналу ядра (`PipeCore`). Записи менеджера раз в секунду переходят в журнал агента
//! (`journal`): оттуда они попадают в файл и в живой журнал окна (`Events`) без перезапуска ядра.

use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::Duration;

use super::journal::AgentJournal;
use crate::monitor::{Options, Shared};
use crate::update::core_link::pipe_core::PipeCore;
use crate::update::manager::Manager;

/// Как часто записи менеджера уходят в журнал агента.
const FORWARD_EVERY: Duration = Duration::from_secs(1);

/// Менеджер обновлений агента и поток, переносящий его записи в журнал агента.
pub(super) fn start(journal: Arc<AgentJournal>) -> Arc<Manager> {
    let options = Options { ping: false, ping_host: String::new(), notify: false, tray: false, taskbar: false };
    let shared = Arc::new(Shared::new(None, options, None));
    let log = shared.clone();
    crate::crash::spawn_named("agent-events", move || {
        let report = |panic: &str, wait: Duration| log.report_secondary_panic(panic, wait);
        let mut forwarded = 0;
        crate::crash::nonfatal_loop(FORWARD_EVERY, &std::thread::sleep, &report, || {
            forwarded = forward(&log, &journal, forwarded);
            ControlFlow::Continue(())
        });
    });
    Manager::new(shared, Arc::new(PipeCore::new()))
}

/// Перенести в журнал агента записи менеджера после номера `after`; вернуть номер последней перенесённой.
fn forward(shared: &Shared, journal: &AgentJournal, after: u64) -> u64 {
    let mut done = after;
    for (seq, event) in shared.with_events(|log| log.since(after)) {
        journal.push(event);
        done = seq;
    }
    done
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::Severity;

    fn shared() -> Shared {
        let options = Options { ping: false, ping_host: String::new(), notify: false, tray: false, taskbar: false };
        Shared::new(None, options, None)
    }

    /// Записи менеджера доходят до журнала агента (а с ним — до окна и файла) по одному разу.
    #[test]
    fn manager_events_reach_the_agent_journal_once() {
        let s = shared();
        let journal = AgentJournal::memory();
        s.log("", Severity::Info, "first");
        let after = forward(&s, &journal, 0);
        s.log("", Severity::Bad, "second");
        let after = forward(&s, &journal, after);
        assert_eq!(forward(&s, &journal, after), after, "нового нет — ничего не перенесено");
        let texts: Vec<String> = journal.since(0).events.into_iter().map(|(_, e)| e.text).collect();
        assert_eq!(texts, ["first", "second"]);
    }
}
