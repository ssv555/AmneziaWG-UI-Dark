//! История скорости для графика (масштабы День, Месяц, Год): запрос к агенту в фоновом потоке — сразу при смене
//! туннеля или масштаба и потом раз в минуту, пока график её показывает. Кадр только читает последний ответ: канал
//! агента отвечает до 5 с, ждать его в кадре — замёрзшее окно.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::crash::lock;
use crate::daemon::agent::client::AgentApi;
use crate::daemon::agent::proto::{History, HistoryRange};
use crate::i18n::tr;

/// Как часто обновлять показанную историю: самый мелкий её интервал — минута.
pub(super) const REFRESH: Duration = Duration::from_secs(60);

/// Что показать на месте кривых истории.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Shown {
    /// Первый ответ ещё не пришёл.
    Loading,
    /// Агента нет, он прежней версии или ответил не тем; текст — причина (подсказка у надписи).
    Unavailable(String),
    /// Ответ агента и момент его получения (unix-секунды): правый край оси времени.
    Ready { history: Arc<History>, at_unix: u64 },
}

/// Запуск фоновой работы: в окне — поток, в тестах — сразу на месте.
type Spawn = Box<dyn Fn(Box<dyn FnOnce() + Send>) + Send + Sync>;
type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;
type Repaint = Arc<dyn Fn() + Send + Sync>;

/// Источник истории для графика: один туннель и один масштаб за раз (показан один график).
pub(super) struct HistoryFeed {
    /// `None` — демо-режим: агента нет, история недоступна.
    agent: Option<Arc<dyn AgentApi>>,
    spawn: Spawn,
    clock: Clock,
    repaint: Repaint,
    slot: Arc<Mutex<Slot>>,
}

#[derive(Default)]
struct Slot {
    key: Option<(String, HistoryRange)>,
    /// Растёт со сменой ключа: ответ на прежний запрос (другой туннель или масштаб) отбрасывается.
    generation: u64,
    shown: Option<Shown>,
    asked: Option<Instant>,
    in_flight: bool,
}

impl HistoryFeed {
    pub(super) fn real(agent: Option<Arc<dyn AgentApi>>, ctx: eframe::egui::Context) -> HistoryFeed {
        HistoryFeed {
            agent,
            spawn: Box::new(|job| crate::crash::spawn_named("graph-history", job)),
            clock: Arc::new(crate::monitor::unix_now),
            repaint: Arc::new(move || ctx.request_repaint()),
            slot: Arc::default(),
        }
    }

    /// История `tunnel` за `range` на момент `now`. Новый туннель или масштаб — «загрузка» и запрос; ответ старше
    /// `REFRESH` — запрос, а до ответа показан прежний.
    pub(super) fn shown(&self, tunnel: &str, range: HistoryRange, now: Instant) -> Shown {
        let mut slot = lock(&self.slot);
        if slot.key.as_ref().is_none_or(|(t, r)| t.as_str() != tunnel || *r != range) {
            slot.generation += 1;
            slot.key = Some((tunnel.to_string(), range));
            slot.shown = None;
            slot.asked = None;
            slot.in_flight = false;
        }
        let due = slot.asked.is_none_or(|at| now.saturating_duration_since(at) >= REFRESH);
        if due && !slot.in_flight {
            slot.asked = Some(now);
            match &self.agent {
                Some(agent) => {
                    slot.in_flight = true;
                    let job = self.fetch(agent.clone(), tunnel.to_string(), range, slot.generation);
                    // Тестовый запуск выполняет работу сразу, а она берёт ту же блокировку.
                    drop(slot);
                    (self.spawn)(job);
                    slot = lock(&self.slot);
                }
                None => slot.shown = Some(Shown::Unavailable(tr("agent.unavailable"))),
            }
        }
        slot.shown.clone().unwrap_or(Shown::Loading)
    }

    fn fetch(&self, agent: Arc<dyn AgentApi>, tunnel: String, range: HistoryRange, generation: u64) -> Box<dyn FnOnce() + Send> {
        let (slot, clock, repaint) = (self.slot.clone(), self.clock.clone(), self.repaint.clone());
        Box::new(move || {
            let answer = agent.history(&tunnel, range);
            let at_unix = clock();
            let mut slot = lock(&slot);
            if slot.generation != generation {
                return;
            }
            slot.in_flight = false;
            slot.shown = Some(match answer {
                Ok(history) => Shown::Ready { history: Arc::new(history), at_unix },
                Err(e) => Shown::Unavailable(e),
            });
            drop(slot);
            repaint();
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::daemon::agent::client::FakeAgent;
    use crate::daemon::agent::proto::{AgentRequest, AgentResponse, HistoryBucket};

    /// Работа откладывается, пока тест не выполнит её сам: так видно, что кадр не ждёт ответа.
    fn feed(agent: Option<FakeAgent>, queue: &Arc<Mutex<Vec<Box<dyn FnOnce() + Send>>>>) -> HistoryFeed {
        let queue = queue.clone();
        HistoryFeed {
            agent: agent.map(|a| Arc::new(a) as Arc<dyn AgentApi>),
            spawn: Box::new(move |job| queue.lock().unwrap().push(job)),
            clock: Arc::new(|| 1_000_000),
            repaint: Arc::new(|| {}),
            slot: Arc::default(),
        }
    }

    fn run_all(queue: &Arc<Mutex<Vec<Box<dyn FnOnce() + Send>>>>) {
        let jobs: Vec<_> = queue.lock().unwrap().drain(..).collect();
        jobs.into_iter().for_each(|job| job());
    }

    fn answering(calls: Arc<AtomicUsize>) -> FakeAgent {
        FakeAgent(Box::new(move |req| match req {
            AgentRequest::History { tunnel, range } => {
                calls.fetch_add(1, Ordering::SeqCst);
                let bucket = HistoryBucket { start: 60, secs: 60.0, rx: tunnel.len() as f64, ..Default::default() };
                Ok(AgentResponse::History(Box::new(History { bucket_s: range.bucket_secs(), buckets: vec![bucket] })))
            }
            other => Err(format!("unexpected {other:?}")),
        }))
    }

    #[test]
    fn history_is_fetched_in_the_background_and_refreshed_once_a_minute() {
        let queue = Arc::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let feed = feed(Some(answering(calls.clone())), &queue);
        let t0 = Instant::now();
        assert_eq!(feed.shown("office", HistoryRange::Day, t0), Shown::Loading, "кадр не ждёт агента");
        assert_eq!(feed.shown("office", HistoryRange::Day, t0 + Duration::from_secs(1)), Shown::Loading);
        assert_eq!(queue.lock().unwrap().len(), 1, "один запрос, пока первый в пути");
        run_all(&queue);
        let Shown::Ready { history, at_unix } = feed.shown("office", HistoryRange::Day, t0 + Duration::from_secs(2)) else { panic!() };
        assert_eq!((history.bucket_s, history.buckets[0].rx, at_unix), (60, 6.0, 1_000_000));
        assert!(queue.lock().unwrap().is_empty(), "свежий ответ — без запроса");

        // Через минуту — новый запрос; до его ответа виден прежний.
        let shown = feed.shown("office", HistoryRange::Day, t0 + REFRESH);
        assert!(matches!(shown, Shown::Ready { .. }));
        assert_eq!(queue.lock().unwrap().len(), 1);
        run_all(&queue);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_new_tunnel_or_range_asks_at_once_and_drops_the_late_answer() {
        let queue = Arc::default();
        let feed = feed(Some(answering(Arc::default())), &queue);
        let t0 = Instant::now();
        feed.shown("office", HistoryRange::Day, t0);
        // Ответ про office ещё в пути — выбран другой масштаб: сразу новый запрос, «загрузка».
        assert_eq!(feed.shown("office", HistoryRange::Year, t0), Shown::Loading);
        assert_eq!(queue.lock().unwrap().len(), 2);
        assert_eq!(feed.shown("home", HistoryRange::Year, t0), Shown::Loading);
        assert_eq!(queue.lock().unwrap().len(), 3);
        run_all(&queue);
        let Shown::Ready { history, .. } = feed.shown("home", HistoryRange::Year, t0) else { panic!() };
        assert_eq!((history.bucket_s, history.buckets[0].rx), (86_400, 4.0), "показан ответ про home за год");
    }

    #[test]
    fn an_old_or_missing_agent_means_history_unavailable() {
        let queue = Arc::default();
        let old = FakeAgent(Box::new(|_| Ok(AgentResponse::Refused("pipe: unknown variant `History`".into()))));
        let feed = feed(Some(old), &queue);
        feed.shown("office", HistoryRange::Month, Instant::now());
        run_all(&queue);
        let Shown::Unavailable(why) = feed.shown("office", HistoryRange::Month, Instant::now()) else { panic!() };
        assert!(why.contains("unknown variant"), "{why}");

        let feed = super::tests::feed(Some(FakeAgent::unreachable("agent pipe: not found")), &queue);
        feed.shown("office", HistoryRange::Day, Instant::now());
        run_all(&queue);
        assert_eq!(feed.shown("office", HistoryRange::Day, Instant::now()), Shown::Unavailable("agent pipe: not found".into()));

        // Демо: агента нет — сразу «недоступна», без потока.
        let feed = super::tests::feed(None, &queue);
        assert!(matches!(feed.shown("office", HistoryRange::Day, Instant::now()), Shown::Unavailable(_)));
        assert!(queue.lock().unwrap().is_empty());
    }
}
