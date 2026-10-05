//! Накопительная статистика по туннелям с первого запуска — `Stats.ini` рядом с exe.
//!
//! Счётчики WireGuard обнуляются при каждом старте службы туннеля. Чтобы не терять трафик, пока окно
//! закрыто, храним последние значения счётчиков и порт: тот же порт и счётчики не меньше — та же сессия,
//! добираем разницу; иначе сессия новая и её счётчики считаются с нуля.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::Path;

use crate::ini::Ini;

/// Больше этого разрыв между замерами — не считаем его временем подключения (сон, пауза опроса).
const MAX_GAP_SECS: f64 = 5.0;

/// Статистика туннеля, которого больше нет (удалён или переименован, в том числе в окне самого AmneziaWG, где ядро
/// об этом не узнаёт), хранится столько дней — на случай, если он вернётся, — и потом убирается.
pub const KEEP_ABSENT_DAYS: u64 = 90;

#[derive(Clone, Default, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TunnelStats {
    pub rx: u64,
    pub tx: u64,
    /// Пиковая скорость, байт/с.
    pub peak_rx: f64,
    pub peak_tx: f64,
    /// Сколько секунд туннель был подключён, пока работал awg-ui.
    pub seconds: f64,
    /// Первый замер, unix-секунды.
    pub since: u64,
    last_rx: u64,
    last_tx: u64,
    last_port: u16,
    /// Когда туннель последний раз был в списке туннелей, unix-секунды (0 — неизвестно: файл прежней версии).
    #[serde(default)]
    last_seen: u64,
}

pub type Stats = BTreeMap<String, TunnelStats>;

impl TunnelStats {
    /// Учесть замер счётчиков. `dt` — секунды с прошлого замера в этом запуске программы (None — первый).
    pub fn observe(&mut self, rx: u64, tx: u64, port: u16, dt: Option<f64>, now_unix: u64) {
        if self.since == 0 {
            self.since = now_unix;
        }
        let same_session = self.last_port != 0 && port == self.last_port && rx >= self.last_rx && tx >= self.last_tx;
        let (drx, dtx) = if same_session { (rx - self.last_rx, tx - self.last_tx) } else { (rx, tx) };
        self.rx += drx;
        self.tx += dtx;
        if let Some(dt) = dt.filter(|dt| *dt > 0.0 && *dt <= MAX_GAP_SECS) {
            self.seconds += dt;
            if same_session {
                self.peak_rx = self.peak_rx.max(drx as f64 / dt);
                self.peak_tx = self.peak_tx.max(dtx as f64 / dt);
            }
        }
        (self.last_rx, self.last_tx, self.last_port) = (rx, tx, port);
    }
}

/// Доля времени туннеля среди всех, 0..=1.
pub fn share(stats: &Stats, tunnel: &str) -> f64 {
    let total: f64 = stats.values().map(|s| s.seconds).sum();
    match stats.get(tunnel) {
        Some(s) if total > 0.0 => s.seconds / total,
        _ => 0.0,
    }
}

/// Убрать статистику туннелей, которых нет в `existing` дольше `KEEP_ABSENT_DAYS`. Есть в списке — отметка
/// `last_seen` обновляется; отметки нет (файл прежней версии) — ставится сейчас, и срок идёт с этого момента, а не
/// с «никогда». Вызывать только по настоящему списку туннелей: при ошибке чтения списка все туннели выглядели бы
/// отсутствующими. Возвращает имена убранных.
pub fn prune(stats: &mut Stats, existing: &BTreeSet<String>, now: u64) -> Vec<String> {
    let keep_secs = KEEP_ABSENT_DAYS * 24 * 3600;
    let mut removed = Vec::new();
    stats.retain(|name, s| {
        if existing.contains(name) || s.last_seen == 0 {
            s.last_seen = now;
            return true;
        }
        let stale = now.saturating_sub(s.last_seen) > keep_secs;
        if stale {
            removed.push(name.clone());
        }
        !stale
    });
    removed
}

pub fn load(path: &Path) -> Stats {
    let ini = Ini::load(path);
    ini.section_names()
        .map(|name| {
            let s = TunnelStats {
                rx: ini.get_or(name, "rx", 0),
                tx: ini.get_or(name, "tx", 0),
                peak_rx: ini.get_or(name, "peak_rx", 0.0),
                peak_tx: ini.get_or(name, "peak_tx", 0.0),
                seconds: ini.get_or(name, "seconds", 0.0),
                since: ini.get_or(name, "since", 0),
                last_rx: ini.get_or(name, "last_rx", 0),
                last_tx: ini.get_or(name, "last_tx", 0),
                last_port: ini.get_or(name, "last_port", 0),
                last_seen: ini.get_or(name, "last_seen", 0),
            };
            (name.to_string(), s)
        })
        .collect()
}

pub fn save(path: &Path, stats: &Stats) -> io::Result<()> {
    let mut ini = Ini::default();
    for (name, s) in stats {
        ini.set(name, "rx", s.rx);
        ini.set(name, "tx", s.tx);
        ini.set(name, "peak_rx", s.peak_rx);
        ini.set(name, "peak_tx", s.peak_tx);
        ini.set(name, "seconds", s.seconds);
        ini.set(name, "since", s.since);
        ini.set(name, "last_rx", s.last_rx);
        ini.set(name, "last_tx", s.last_tx);
        ini.set(name, "last_port", s.last_port);
        ini.set(name, "last_seen", s.last_seen);
    }
    ini.save(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_session_adds_delta_and_peak() {
        let mut s = TunnelStats::default();
        s.observe(1000, 100, 5000, None, 1);
        s.observe(3000, 300, 5000, Some(1.0), 2);
        assert_eq!((s.rx, s.tx), (3000, 300));
        assert_eq!(s.peak_rx, 2000.0);
        assert_eq!(s.seconds, 1.0);
        assert_eq!(s.since, 1);
    }

    #[test]
    fn restart_while_closed_catches_up_same_session() {
        // Программа закрыта, туннель жил дальше: после перезапуска добираем разницу, без пика.
        let mut s = TunnelStats::default();
        s.observe(1000, 0, 5000, None, 1);
        s.observe(9000, 0, 5000, None, 100);
        assert_eq!(s.rx, 9000);
        assert_eq!(s.peak_rx, 0.0);
    }

    #[test]
    fn new_session_counts_from_zero() {
        let mut s = TunnelStats::default();
        s.observe(5000, 0, 5000, None, 1);
        s.observe(200, 0, 6000, Some(1.0), 2);
        assert_eq!(s.rx, 5200);
    }

    #[test]
    fn long_gap_is_not_connected_time() {
        let mut s = TunnelStats::default();
        s.observe(0, 0, 1, None, 1);
        s.observe(10, 0, 1, Some(60.0), 2);
        assert_eq!(s.seconds, 0.0);
    }

    const DAY: u64 = 24 * 3600;

    fn with_stats(names: &[&str]) -> Stats {
        names.iter().map(|n| (n.to_string(), TunnelStats::default())).collect()
    }

    #[test]
    fn stats_of_a_tunnel_gone_for_long_are_pruned_and_present_ones_kept() {
        let mut stats = with_stats(&["live", "gone"]);
        let live: BTreeSet<String> = ["live".to_string()].into();
        let t0 = 1_000_000;
        // Первый проход: «gone» уже нет, но срок начинается только сейчас (отметки не было).
        assert!(prune(&mut stats, &live, t0).is_empty());
        assert_eq!(stats["gone"].last_seen, t0, "отметка ставится при первом замеченном отсутствии");
        assert!(prune(&mut stats, &live, t0 + KEEP_ABSENT_DAYS * DAY).is_empty(), "ровно срок — ещё хранится");
        assert_eq!(prune(&mut stats, &live, t0 + KEEP_ABSENT_DAYS * DAY + 1), vec!["gone".to_string()]);
        assert!(stats.contains_key("live") && !stats.contains_key("gone"));
        assert_eq!(stats["live"].last_seen, t0 + KEEP_ABSENT_DAYS * DAY + 1, "существующий туннель обновляет отметку");
    }

    #[test]
    fn tunnel_that_returns_keeps_its_stats() {
        let mut stats = with_stats(&["t"]);
        let (none, back): (BTreeSet<String>, BTreeSet<String>) = (BTreeSet::new(), ["t".to_string()].into());
        prune(&mut stats, &none, 100);
        prune(&mut stats, &back, 100 + (KEEP_ABSENT_DAYS - 1) * DAY);
        assert!(prune(&mut stats, &none, 100 + (KEEP_ABSENT_DAYS + 50) * DAY).is_empty(), "срок считается с последней встречи");
        assert!(stats.contains_key("t"));
    }

    #[test]
    fn last_seen_survives_the_file_and_old_files_without_it_load() {
        let dir = std::env::temp_dir().join(format!("awg-ui-test-seen-{}", std::process::id()));
        let path = dir.join("Stats.ini");
        let mut stats = with_stats(&["a"]);
        prune(&mut stats, &["a".to_string()].into(), 777);
        save(&path, &stats).unwrap();
        assert_eq!(load(&path)["a"].last_seen, 777);
        std::fs::write(&path, "[old]\r\nrx=5\r\n").unwrap();
        let old = load(&path);
        assert_eq!((old["old"].rx, old["old"].last_seen), (5, 0));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn share_and_file_roundtrip() {
        let mut stats = Stats::new();
        let mut a = TunnelStats::default();
        a.observe(0, 0, 1, None, 1);
        a.observe(10, 5, 1, Some(3.0), 2);
        let mut b = TunnelStats::default();
        b.observe(0, 0, 2, None, 1);
        b.observe(1, 1, 2, Some(1.0), 2);
        stats.insert("a".into(), a);
        stats.insert("b".into(), b);
        assert_eq!(share(&stats, "a"), 0.75);
        let dir = std::env::temp_dir().join(format!("awg-ui-test-{}", std::process::id()));
        let path = dir.join("Stats.ini");
        save(&path, &stats).unwrap();
        assert_eq!(load(&path), stats);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
