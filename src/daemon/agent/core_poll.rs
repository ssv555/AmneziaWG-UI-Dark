//! Опрос `State` ядра раз в секунду — общий источник для частей агента, которым нужно состояние туннелей
//! (статистика и журнал событий по курсору — `journal`). Каждая часть — `CoreFeed`: говорит, с какого события ей нужен
//! журнал, и получает каждый ответ ядра. Ядро недоступно — части ничего не получают и держат последнее, что знали;
//! в журнал — одна запись о потере связи и одна о её возвращении.

use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::daemon::pipe::Timeouts;
use crate::daemon::proto::{CoreState, Request, Response};
use crate::events::Severity;
use crate::i18n::{tr, trf};

/// Шаг опроса: так часто ядро само снимает счётчики туннелей.
pub(super) const PERIOD: Duration = Duration::from_secs(1);
/// Ядро отвечает на `State` сразу; зависшее ядро не должно надолго останавливать опрос.
const TIMEOUTS: Timeouts = Timeouts { send: Duration::from_secs(2), reply: Duration::from_secs(3) };

pub(super) type Fetch = dyn Fn(u64) -> Result<CoreState, String> + Send + Sync;
pub(super) type Log = dyn Fn(Severity, &str) + Send + Sync;

/// Потребитель состояния ядра.
pub(super) trait CoreFeed: Send + Sync {
    /// С какого события нужен журнал ядра; `u64::MAX` — события не нужны.
    fn events_after(&self) -> u64 {
        u64::MAX
    }

    /// Очередной ответ ядра, снятый в `at`.
    fn accept(&self, state: &CoreState, at: Instant);
}

/// Опрос: запрос к ядру (функцией — так он проверяется без канала) и состояние связи для журнала.
pub(super) struct CorePoll {
    fetch: Box<Fetch>,
    log: Box<Log>,
    /// Ошибка, о которой уже есть запись; `None` — связь есть.
    failing: Option<String>,
}

impl CorePoll {
    pub(super) fn new(fetch: Box<Fetch>, log: Box<Log>) -> CorePoll {
        CorePoll { fetch, log, failing: None }
    }

    /// Настоящий: канал ядра.
    pub(super) fn real(log: Box<Log>) -> CorePoll {
        CorePoll::new(Box::new(fetch_core_state), log)
    }

    /// Один шаг: спросить ядро и раздать ответ. Нет ответа — частям ничего, запись в журнал только при потере связи.
    pub(super) fn step(&mut self, feeds: &[Arc<dyn CoreFeed>], at: Instant) {
        let after = feeds.iter().map(|f| f.events_after()).min().unwrap_or(u64::MAX);
        match (self.fetch)(after) {
            Ok(state) => {
                if self.failing.take().is_some() {
                    (self.log)(Severity::Info, &tr("agent.core_back"));
                }
                for feed in feeds {
                    feed.accept(&state, at);
                }
            }
            Err(e) => {
                if self.failing.is_none() {
                    (self.log)(Severity::Warn, &trf("agent.core_lost", &[&e]));
                }
                self.failing = Some(e);
            }
        }
    }
}

/// Поток опроса. Вторичный (`crash::nonfatal_loop`): паника части — запись в журнал и пауза, а не остановка агента.
pub(super) fn spawn(mut poll: CorePoll, feeds: Vec<Arc<dyn CoreFeed>>, report: Box<dyn Fn(&str, Duration) + Send>) {
    crate::crash::spawn_named("agent-core-poll", move || {
        crate::crash::nonfatal_loop(PERIOD, &std::thread::sleep, &*report, || {
            poll.step(&feeds, Instant::now());
            ControlFlow::Continue(())
        });
    });
}

fn fetch_core_state(events_after: u64) -> Result<CoreState, String> {
    match crate::daemon::pipe::call_to(crate::daemon::pipe::NAME, &Request::State { events_after }, TIMEOUTS)? {
        Response::State(state) => Ok(*state),
        Response::Err(e) | Response::Refused(e) => Err(e),
        other => Err(crate::i18n::trf("err.core_unexpected", &[&crate::explain::variant_name(&other)])),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recorder {
        seen: Mutex<Vec<Option<String>>>,
        after: u64,
    }

    impl CoreFeed for Recorder {
        fn events_after(&self) -> u64 {
            self.after
        }
        fn accept(&self, state: &CoreState, _: Instant) {
            self.seen.lock().unwrap().push(state.error.clone());
        }
    }

    #[test]
    fn lost_core_is_logged_once_feeds_keep_last_and_return_is_logged() {
        let answers = Arc::new(Mutex::new(vec![
            Ok(CoreState { error: Some("first".into()), ..Default::default() }),
            Err("pipe: gone".to_string()),
            Err("pipe: still gone".to_string()),
            Ok(CoreState::default()),
        ]));
        let asked = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::new(Mutex::new(Vec::new()));
        let (a, q, l) = (answers.clone(), asked.clone(), log.clone());
        let mut poll = CorePoll::new(
            Box::new(move |after| {
                q.lock().unwrap().push(after);
                a.lock().unwrap().remove(0)
            }),
            Box::new(move |severity, text| l.lock().unwrap().push((severity, text.to_string()))),
        );
        let feed = Arc::new(Recorder { after: 7, ..Default::default() });
        let none = Arc::new(Recorder { after: u64::MAX, ..Default::default() });
        let feeds: Vec<Arc<dyn CoreFeed>> = vec![feed.clone(), none];
        for _ in 0..4 {
            poll.step(&feeds, Instant::now());
        }
        assert_eq!(*feed.seen.lock().unwrap(), vec![Some("first".to_string()), None], "без ответа частям ничего");
        assert_eq!(*asked.lock().unwrap(), vec![7; 4], "журнал — с самого раннего нужного события");
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 2, "{log:?}");
        assert!(log[0].0 == Severity::Warn && log[0].1.contains("pipe: gone"), "{log:?}");
        assert_eq!(log[1], (Severity::Info, tr("agent.core_back")));
    }
}
