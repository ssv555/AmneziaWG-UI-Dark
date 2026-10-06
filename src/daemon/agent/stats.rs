//! Накопительная статистика трафика в агенте: счётчики — из `State` ядра (`core_poll`), файл `Stats.ini` в папке
//! данных ведёт только агент. Правила сессий и чистки — `crate::stats`; здесь только откуда берутся замеры, когда
//! файл сохраняется и запросы окна (переименование, удаление туннеля).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::core_poll::{CoreFeed, Log};
use crate::crash::lock;
use crate::daemon::proto::CoreState;
use crate::events::Severity;
use crate::i18n::trf;
use crate::stats::{self, Stats};

/// Так часто статистика чистится и пишется на диск: чаще незачем, а при падении агента теряется не больше этого.
pub(super) const SAVE_EVERY: Duration = Duration::from_secs(15);

type Save = dyn Fn(&Path, &Stats) -> std::io::Result<()> + Send + Sync;

/// Последнее чтение счётчиков туннеля в этом запуске агента.
#[derive(Clone, Copy)]
struct Seen {
    at: Instant,
    /// Когда счётчики менялись в последний раз: от него считается пик (`TunnelStats::observe_sampled`).
    changed: Instant,
    rx: u64,
    tx: u64,
    port: u16,
}

#[derive(Default)]
struct Inner {
    stats: Stats,
    /// Подключённые туннели с последнего ответа ядра.
    seen: BTreeMap<String, Seen>,
    /// Настоящий список туннелей из последнего ответа ядра без ошибки; `None` — чистить не по чему.
    listed: Option<BTreeSet<String>>,
    /// Ошибка записи файла, о которой уже есть запись в журнале.
    save_failing: Option<String>,
}

pub(crate) struct AgentStats {
    path: PathBuf,
    inner: Mutex<Inner>,
    save: Box<Save>,
    log: Box<Log>,
}

impl AgentStats {
    pub(super) fn new(path: PathBuf, save: Box<Save>, log: Box<Log>) -> AgentStats {
        let inner = Inner { stats: stats::load(&path), ..Default::default() };
        AgentStats { path, inner: Mutex::new(inner), save, log }
    }

    /// Настоящая: `Stats.ini` в папке данных — там, где его вело ядро до переноса.
    pub(super) fn real(log: Box<Log>) -> AgentStats {
        AgentStats::new(crate::daemon::data_dir().join("Stats.ini"), Box::new(stats::save), log)
    }

    /// Без файла: демо-режим окна (`app::demo_core`), где статистику держит окно, а не агент.
    pub(crate) fn in_memory() -> AgentStats {
        AgentStats { path: PathBuf::new(), inner: Mutex::default(), save: Box::new(|_, _| Ok(())), log: Box::new(|_, _| {}) }
    }

    /// Демо-окно: учесть замер его выдуманного ядра поверх статистики окна (её правят переименование и удаление в
    /// демо) — по тем же правилам сессий и пиков, что у агента, а не второй их копией.
    pub(crate) fn observe_into(&self, stats: &mut Stats, state: &CoreState, at: Instant) {
        lock(&self.inner).stats = std::mem::take(stats);
        self.accept(state, at);
        *stats = self.snapshot();
    }

    pub(super) fn snapshot(&self) -> Stats {
        lock(&self.inner).stats.clone()
    }

    /// Окно: туннель переименован (ядро подтвердило). Сохраняется сразу — новое имя не должно ждать шага записи.
    pub(super) fn rename(&self, old: &str, new: &str) -> Result<(), String> {
        let mut inner = lock(&self.inner);
        stats::rename(&mut inner.stats, old, new);
        if let Some(seen) = inner.seen.remove(old) {
            inner.seen.insert(new.to_string(), seen);
        }
        self.save_now(&mut inner)
    }

    /// Окно: туннель удалён (ядро подтвердило). Иначе `[имя]` осталось бы в файле на 90 дней и искажало доли времени.
    pub(super) fn forget(&self, tunnel: &str) -> Result<(), String> {
        let mut inner = lock(&self.inner);
        inner.stats.remove(tunnel);
        inner.seen.remove(tunnel);
        self.save_now(&mut inner)
    }

    /// Шаг записи: чистка по настоящему списку туннелей и сохранение. Ошибка записи — в журнал один раз, пока запись
    /// снова не пройдёт.
    pub(super) fn tick(&self, now_unix: u64) {
        let mut inner = lock(&self.inner);
        let pruned = match inner.listed.take() {
            Some(listed) => stats::prune(&mut inner.stats, &listed, now_unix),
            None => Vec::new(),
        };
        for name in pruned {
            (self.log)(Severity::Info, &trf("stats.pruned", &[&name, &stats::KEEP_ABSENT_DAYS.to_string()]));
        }
        let result = self.save_now(&mut inner);
        match (result, inner.save_failing.is_some()) {
            (Ok(()), _) => inner.save_failing = None,
            (Err(e), false) => {
                (self.log)(Severity::Bad, &e);
                inner.save_failing = Some(e);
            }
            (Err(e), true) => inner.save_failing = Some(e),
        }
    }

    /// Запись без журнала: вызвавший решает, кому сообщить об ошибке (окну или в журнал).
    fn save_now(&self, inner: &mut Inner) -> Result<(), String> {
        (self.save)(&self.path, &inner.stats).map_err(|e| crate::fsutil::io_ctx(&self.path, e))
    }
}

impl CoreFeed for AgentStats {
    /// Учесть счётчики подключённых туннелей. Один и тот же замер ядра, прочитанный дважды, ничего не добавляет;
    /// разрыв (ядро не отвечало) не идёт во время подключения, а байты за него добираются по правилу сессий.
    fn accept(&self, state: &CoreState, at: Instant) {
        let now_unix = crate::monitor::unix_now();
        let mut inner = lock(&self.inner);
        let mut next = BTreeMap::new();
        for (name, result) in &state.running {
            let prev = inner.seen.remove(name);
            let Ok(st) = result else {
                // Ядро не прочитало счётчики: замера нет, следующий считается от прошлого.
                if let Some(prev) = prev {
                    next.insert(name.clone(), prev);
                }
                continue;
            };
            let (rx, tx, port) = (st.rx_bytes(), st.tx_bytes(), st.listen_port);
            let unchanged = prev.is_some_and(|p| (p.rx, p.tx, p.port) == (rx, tx, port));
            let changed = prev.filter(|_| unchanged).map_or(at, |p| p.changed);
            let dt = prev.map(|p| at.duration_since(p.at).as_secs_f64());
            let rate_dt = prev.map(|p| at.duration_since(p.changed).as_secs_f64());
            inner.stats.entry(name.clone()).or_default().observe_sampled(rx, tx, port, dt, rate_dt, now_unix);
            next.insert(name.clone(), Seen { at, changed, rx, tx, port });
        }
        inner.seen = next;
        // Ошибка в ответе ядра может значить, что список конфигов не прочитан: тогда все туннели выглядели бы удалёнными.
        if state.error.is_none() {
            inner.listed = Some(state.tunnels.iter().cloned().collect());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::uapi::{Peer, Status};
    use std::sync::Arc;

    fn status(rx: u64, tx: u64, port: u16) -> Status {
        Status { listen_port: port, peers: vec![Peer { rx_bytes: rx, tx_bytes: tx, ..Default::default() }], ..Default::default() }
    }

    fn state(running: &[(&str, Result<Status, String>)]) -> CoreState {
        CoreState {
            tunnels: running.iter().map(|(n, _)| n.to_string()).collect(),
            running: running.iter().map(|(n, r)| (n.to_string(), r.clone())).collect(),
            ..Default::default()
        }
    }

    type Logged = Arc<Mutex<Vec<(Severity, String)>>>;

    fn agent_stats(dir: &Path, save: Box<Save>) -> (AgentStats, Logged) {
        let logged = Logged::default();
        let l = logged.clone();
        let stats = AgentStats::new(dir.join("Stats.ini"), save, Box::new(move |s, t| l.lock().unwrap().push((s, t.to_string()))));
        (stats, logged)
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("awg-agent-stats-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Приёмка шага 10: ядро не отвечало 30 секунд, порт тот же — байты за разрыв не теряются и не задваиваются,
    /// а сам разрыв не идёт во время подключения.
    #[test]
    fn core_state_gap_of_30_s_with_the_same_port_keeps_bytes_continuous() {
        let dir = temp_dir("gap");
        let (s, _) = agent_stats(&dir, Box::new(stats::save));
        let t0 = Instant::now();
        let sec = |n: u64| t0 + Duration::from_secs(n);
        s.accept(&state(&[("t", Ok(status(1_000, 100, 5000)))]), sec(0));
        s.accept(&state(&[("t", Ok(status(2_000, 200, 5000)))]), sec(1));
        // 30 секунд без ответа ядра: туннель жил, счётчики росли.
        s.accept(&state(&[("t", Ok(status(90_000, 9_000, 5000)))]), sec(31));
        s.accept(&state(&[("t", Ok(status(91_000, 9_100, 5000)))]), sec(32));
        let st = &s.snapshot()["t"];
        assert_eq!((st.rx, st.tx), (91_000, 9_100), "байты непрерывны");
        assert_eq!(st.seconds, 2.0, "разрыв не считается временем подключения");
        assert_eq!(st.peak_rx, 1_000.0, "прирост за разрыв — не пик");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Тот же замер ядра прочитан дважды: прирост следующего чтения делится на два шага, а не на один.
    #[test]
    fn repeated_core_sample_does_not_double_the_peak() {
        let dir = temp_dir("peak");
        let (s, _) = agent_stats(&dir, Box::new(stats::save));
        let t0 = Instant::now();
        let sec = |n: u64| t0 + Duration::from_secs(n);
        s.accept(&state(&[("t", Ok(status(0, 0, 1)))]), sec(0));
        s.accept(&state(&[("t", Ok(status(0, 0, 1)))]), sec(1));
        s.accept(&state(&[("t", Ok(status(2_000, 0, 1)))]), sec(2));
        let st = &s.snapshot()["t"];
        assert_eq!((st.rx, st.peak_rx, st.seconds), (2_000, 1_000.0, 2.0));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn new_port_is_a_new_session_and_unread_counters_are_skipped() {
        let dir = temp_dir("session");
        let (s, _) = agent_stats(&dir, Box::new(stats::save));
        let t0 = Instant::now();
        s.accept(&state(&[("t", Ok(status(5_000, 0, 1)))]), t0);
        s.accept(&state(&[("t", Err("uapi: timeout".into()))]), t0 + Duration::from_secs(1));
        s.accept(&state(&[("t", Ok(status(300, 0, 2)))]), t0 + Duration::from_secs(2));
        assert_eq!(s.snapshot()["t"].rx, 5_300);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rename_and_forget_change_the_file_at_once() {
        let dir = temp_dir("rename");
        let (s, _) = agent_stats(&dir, Box::new(stats::save));
        s.accept(&state(&[("old", Ok(status(10, 1, 1))), ("gone", Ok(status(5, 5, 2)))]), Instant::now());
        s.rename("old", "new").unwrap();
        s.forget("gone").unwrap();
        let saved = stats::load(&dir.join("Stats.ini"));
        assert_eq!(saved.keys().collect::<Vec<_>>(), ["new"]);
        assert_eq!(saved["new"].rx, 10);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn stats_survive_an_agent_restart() {
        let dir = temp_dir("restart");
        let (s, _) = agent_stats(&dir, Box::new(stats::save));
        s.accept(&state(&[("t", Ok(status(700, 70, 9)))]), Instant::now());
        s.tick(1_000);
        let (again, _) = agent_stats(&dir, Box::new(stats::save));
        again.accept(&state(&[("t", Ok(status(900, 90, 9)))]), Instant::now());
        assert_eq!((again.snapshot()["t"].rx, again.snapshot()["t"].tx), (900, 90), "та же сессия после перезапуска агента");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Обновление с 0.4.0: `Stats.ini` записан ядром 0.4.0 (формат его `stats::save`), агент стартует впервые, туннель
    /// так и работает на том же порту. Ловит: агент начинает с пустой статистики и первой записью затирает историю;
    /// первый замер после обновления считается новой сессией (счётчики службы прибавились бы целиком, задвоив трафик);
    /// туннель, который сейчас не подключён, пропадает из файла.
    #[test]
    fn stats_ini_of_0_4_0_is_continued_without_loss_or_double_count() {
        let dir = temp_dir("upgrade");
        let v040 = "[office]\r\nrx=5000\r\ntx=600\r\npeak_rx=1500.5\r\npeak_tx=200\r\nseconds=3600\r\nsince=1700000000\r\n\
                    last_rx=4000\r\nlast_tx=500\r\nlast_port=51820\r\nlast_seen=1790000000\r\n\
                    [home]\r\nrx=777\r\ntx=77\r\npeak_rx=10\r\npeak_tx=1\r\nseconds=60\r\nsince=1600000000\r\n\
                    last_rx=777\r\nlast_tx=77\r\nlast_port=40000\r\nlast_seen=1790000000\r\n";
        std::fs::write(dir.join("Stats.ini"), v040).unwrap();
        let (s, logged) = agent_stats(&dir, Box::new(stats::save));
        let mut core = state(&[("office", Ok(status(4_500, 550, 51820)))]);
        core.tunnels.push("home".into());
        s.accept(&core, Instant::now());
        s.tick(1_790_000_100);
        assert!(logged.lock().unwrap().is_empty(), "{:?}", logged.lock().unwrap());

        let saved = stats::load(&dir.join("Stats.ini"));
        let office = &saved["office"];
        assert_eq!((office.rx, office.tx), (5_500, 650), "прибавился только прирост с последнего замера 0.4.0");
        assert_eq!((office.peak_rx, office.seconds, office.since), (1500.5, 3600.0, 1_700_000_000));
        let home = &saved["home"];
        assert_eq!((home.rx, home.tx, home.since), (777, 77, 1_600_000_000), "неподключённый туннель не потерян");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Чистка — только по ответу ядра без ошибки: при ошибке список туннелей мог не прочитаться.
    #[test]
    fn pruning_needs_a_clean_core_answer() {
        let dir = temp_dir("prune");
        let (s, logged) = agent_stats(&dir, Box::new(stats::save));
        s.accept(&state(&[("t", Ok(status(1, 1, 1)))]), Instant::now());
        s.tick(1);
        let far = 2 + (stats::KEEP_ABSENT_DAYS + 1) * 24 * 3600;
        s.accept(&CoreState { error: Some("configs: denied".into()), ..Default::default() }, Instant::now());
        s.tick(far);
        assert!(s.snapshot().contains_key("t"), "список не прочитан — не чистим");
        s.accept(&CoreState::default(), Instant::now());
        s.tick(far);
        assert!(s.snapshot().is_empty());
        assert!(logged.lock().unwrap().iter().any(|(sev, text)| *sev == Severity::Info && text.contains('t')));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn save_failure_is_logged_once_and_returned_to_the_window() {
        let dir = temp_dir("savefail");
        let (s, logged) = agent_stats(&dir, Box::new(|_, _| Err(std::io::Error::other("disk full"))));
        s.tick(1);
        s.tick(2);
        assert_eq!(logged.lock().unwrap().len(), 1, "одна запись, пока запись не пройдёт");
        let e = s.forget("x").unwrap_err();
        assert!(e.contains("Stats.ini") && e.contains("disk full"), "{e}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
