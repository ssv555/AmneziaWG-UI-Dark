//! Фоновый опрос раз в секунду: список туннелей, состояние запущенных, история для графика,
//! накопительная статистика, события, значок в трее. Работает и при скрытом окне.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::backend::Backend;
use crate::events::{self, Event, EventLog, Severity};
use crate::health::{self, Health, Level};
use crate::ping::PingState;
use crate::stats::{self, Stats};
use crate::tray;
use crate::uapi::Status;

const PERIOD: Duration = Duration::from_secs(1);
const STATS_SAVE_EVERY: Duration = Duration::from_secs(15);
/// Час истории при замере раз в секунду — максимум периода графика.
pub const HISTORY_SECS: usize = 3600;

#[derive(Clone, Copy)]
pub struct Sample {
    pub at: Instant,
    pub rx: u64,
    pub tx: u64,
}

#[derive(Default)]
pub struct Live {
    pub status: Option<Status>,
    pub error: Option<String>,
    pub history: VecDeque<Sample>,
}

impl Live {
    /// Средняя скорость (байт/с) приёма и передачи за последние `secs` секунд.
    pub fn rate(&self, secs: f64) -> (f64, f64) {
        let Some(last) = self.history.back() else { return (0.0, 0.0) };
        let first = self
            .history
            .iter()
            .find(|s| last.at.duration_since(s.at).as_secs_f64() <= secs)
            .unwrap_or(last);
        let dt = last.at.duration_since(first.at).as_secs_f64();
        if dt <= 0.0 {
            return (0.0, 0.0);
        }
        (last.rx.saturating_sub(first.rx) as f64 / dt, last.tx.saturating_sub(first.tx) as f64 / dt)
    }

    /// Скорости по соседним замерам за последние `secs` секунд — точки графика.
    pub fn rate_series(&self, secs: f64) -> Vec<(f64, f64)> {
        let Some(last) = self.history.back() else { return Vec::new() };
        let recent: Vec<&Sample> =
            self.history.iter().filter(|s| last.at.duration_since(s.at).as_secs_f64() <= secs).collect();
        recent
            .windows(2)
            .map(|w| {
                let dt = w[1].at.duration_since(w[0].at).as_secs_f64().max(0.001);
                (w[1].rx.saturating_sub(w[0].rx) as f64 / dt, w[1].tx.saturating_sub(w[0].tx) as f64 / dt)
            })
            .collect()
    }

    /// Сколько секунд туннель наблюдается в этом запуске программы.
    pub fn observed_secs(&self) -> f64 {
        self.history.front().map(|s| s.at.elapsed().as_secs_f64()).unwrap_or(0.0)
    }
}

#[derive(Default)]
pub struct Snapshot {
    /// Все известные туннели: конфиги менеджера плюс запущенные не из них.
    pub tunnels: BTreeSet<String>,
    pub running: BTreeMap<String, Live>,
    pub error: Option<String>,
}

/// Настройки, нужные фоновым потокам; окно обновляет их при изменении.
#[derive(Clone)]
pub struct Options {
    pub ping: bool,
    pub ping_host: String,
    pub notify: bool,
    pub tray: bool,
}

/// Общее состояние окна и фоновых потоков. Порядок захвата: snapshot → pending → ping → остальное.
pub struct Shared {
    pub backend: Backend,
    pub snapshot: Mutex<Snapshot>,
    /// туннель → «Подключение…»/«Отключение…», пока идёт команда пользователя
    pub pending: Mutex<BTreeMap<String, &'static str>>,
    pub ping: Mutex<PingState>,
    pub options: Mutex<Options>,
    pub events: Mutex<EventLog>,
    pub stats: Mutex<Stats>,
    stats_path: Option<PathBuf>,
    /// Состояние службы менеджера AmneziaWG — для строки состояния.
    pub service: Mutex<String>,
}

impl Shared {
    pub fn new(backend: Backend, options: Options, stats_path: Option<PathBuf>, events_path: Option<PathBuf>) -> Shared {
        let stats = stats_path.as_deref().map(stats::load).unwrap_or_default();
        Shared {
            backend,
            snapshot: Default::default(),
            pending: Default::default(),
            ping: Mutex::new(PingState { host: options.ping_host.clone(), ..Default::default() }),
            options: Mutex::new(options),
            events: Mutex::new(EventLog::open(events_path)),
            stats: Mutex::new(stats),
            stats_path,
            service: Default::default(),
        }
    }

    pub fn save_stats(&self) {
        if let Some(path) = &self.stats_path {
            // Копия — чтобы не держать stats, пока берём snapshot (poll захватывает их в обратном порядке).
            let stats = self.stats.lock().unwrap().clone();
            if let Err(e) = stats::save(path, &stats) {
                self.snapshot.lock().unwrap().error = Some(format!("{}: {e}", path.display()));
            }
        }
    }

    pub fn log(&self, tunnel: &str, severity: Severity, text: &str) {
        self.events.lock().unwrap().push(Event::new(unix_now(), tunnel, severity, text, false));
    }

    /// Состояние каждого известного туннеля.
    fn levels(&self) -> (BTreeMap<String, Health>, BTreeSet<String>) {
        let snap = self.snapshot.lock().unwrap();
        let pending = self.pending.lock().unwrap().clone();
        let ping = self.options.lock().unwrap().ping.then(|| self.ping.lock().unwrap().clone());
        let map = snap
            .tunnels
            .iter()
            .map(|t| (t.clone(), health::health(snap.running.get(t), pending.get(t).copied(), ping.as_ref())))
            .collect();
        (map, pending.into_keys().collect())
    }
}

pub fn spawn(shared: Arc<Shared>, ctx: eframe::egui::Context) {
    std::thread::spawn(move || {
        let mut prev: Option<BTreeMap<String, Level>> = None;
        let mut last_save = Instant::now();
        loop {
            poll(&shared);
            let (levels, user) = shared.levels();
            react(&shared, &mut prev, &levels, &user);
            if last_save.elapsed() >= STATS_SAVE_EVERY {
                shared.save_stats();
                last_save = Instant::now();
            }
            ctx.request_repaint();
            std::thread::sleep(PERIOD);
        }
    });
}

/// События, уведомления и значок в трее по новым уровням.
fn react(shared: &Shared, prev: &mut Option<BTreeMap<String, Level>>, levels: &BTreeMap<String, Health>, user: &BTreeSet<String>) {
    // «Занят» (переключение, ожидание рукопожатия) — промежуточное состояние, событий не даёт.
    let settled: BTreeMap<String, (Level, String)> = levels
        .iter()
        .filter(|(_, h)| h.level != Level::Busy)
        .map(|(n, h)| (n.clone(), (h.level, h.text.clone())))
        .collect();
    let options = shared.options.lock().unwrap().clone();
    // Первый опрос — точка отсчёта: туннель, уже поднятый при старте, не событие.
    if let Some(prev) = prev.as_ref() {
        for e in events::transitions(prev, &settled, user, unix_now()) {
            if e.notify && options.notify && options.tray {
                let title = format!("{} — {}", e.tunnel, e.text);
                tray::notify(crate::APP_TITLE, &title, e.severity != Severity::Info);
            }
            shared.events.lock().unwrap().push(e);
        }
    }
    let p = prev.get_or_insert_with(BTreeMap::new);
    for (n, (level, _)) in settled {
        p.insert(n, level);
    }

    if options.tray {
        let active: Vec<(&String, &Health)> = levels.iter().filter(|(_, h)| h.level != Level::Off).collect();
        let worst = active.iter().map(|(_, h)| h.level).max_by_key(|l| severity_rank(*l)).unwrap_or(Level::Off);
        let tip = if active.is_empty() {
            crate::i18n::tr("tray.none")
        } else {
            active.iter().map(|(n, h)| format!("{n}: {}", h.text)).collect::<Vec<_>>().join("\n")
        };
        tray::set_state(worst, &tip);
    }
}

fn severity_rank(level: Level) -> u8 {
    match level {
        Level::Off => 0,
        Level::Ok => 1,
        Level::Busy => 2,
        Level::Warn => 3,
        Level::Bad => 4,
    }
}

pub fn poll(shared: &Shared) {
    let backend = &shared.backend;
    let configs = backend.configs();
    let running = backend.running();
    let queried: Vec<(String, Result<Status, String>)> = running
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|name| (name.clone(), backend.query(name).map_err(|e| e.to_string())))
        .collect();

    let now = Instant::now();
    let mut snap = shared.snapshot.lock().unwrap();
    let mut stats = shared.stats.lock().unwrap();
    snap.error = [configs.as_ref().err(), running.as_ref().err()]
        .into_iter()
        .flatten()
        .map(|e| e.to_string())
        .reduce(|a, b| format!("{a}; {b}"));
    if let Ok(configs) = configs {
        snap.tunnels = configs.into_iter().collect();
    }
    let mut next = BTreeMap::new();
    for (name, result) in queried {
        snap.tunnels.insert(name.clone());
        let mut live = snap.running.remove(&name).unwrap_or_default();
        match result {
            Ok(st) => {
                let dt = live.history.back().map(|s| now.duration_since(s.at).as_secs_f64());
                stats.entry(name.clone()).or_default().observe(st.rx_bytes(), st.tx_bytes(), st.listen_port, dt, unix_now());
                live.history.push_back(Sample { at: now, rx: st.rx_bytes(), tx: st.tx_bytes() });
                while live.history.len() > HISTORY_SECS + 1 {
                    live.history.pop_front();
                }
                live.status = Some(st);
                live.error = None;
            }
            Err(e) => live.error = Some(e),
        }
        next.insert(name, live);
    }
    snap.running = next;
}

pub fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_over_window() {
        let t0 = Instant::now();
        let mut live = Live::default();
        for i in 0..5u64 {
            live.history.push_back(Sample { at: t0 + Duration::from_secs(i), rx: i * 1000, tx: i * 10 });
        }
        let (rx, tx) = live.rate(2.0);
        assert!((rx - 1000.0).abs() < 1e-6 && (tx - 10.0).abs() < 1e-6);
        assert_eq!(live.rate_series(120.0).len(), 4);
        assert_eq!(live.rate_series(2.0).len(), 2);
    }

    #[test]
    fn rate_empty_is_zero() {
        assert_eq!(Live::default().rate(5.0), (0.0, 0.0));
    }
}
