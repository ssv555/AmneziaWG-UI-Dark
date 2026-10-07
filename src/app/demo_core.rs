//! Ядро демо-режима: окно говорит с ним так же, как с настоящим (`CoreApi`), а оно отвечает выдуманными
//! туннелями `Demo`. Так у окна один путь к туннелям, а не ветка «ядро или демо» в каждом действии.

use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::backend::{Demo, TunnelHost};
use crate::daemon::proto::{Request, Response};
use crate::daemon::server::{run_switch, to_replace};
use crate::daemon::agent::AgentStats;
use crate::daemon::CoreApi;
use crate::monitor::Shared;
use crate::settings::Mode;

/// Демо-статистика пересчитывается так же часто, как агент опрашивает ядро.
const STATS_PERIOD: Duration = Duration::from_secs(1);

pub(super) struct DemoCore(pub(super) Arc<Demo>);

impl CoreApi for DemoCore {
    fn call(&self, req: Request) -> Result<Response, String> {
        let d = &self.0;
        let done = |r: Result<(), String>| r.map(|()| Response::Ok);
        match req {
            Request::Switch { tunnel, plan, multiple } => {
                // Те же шаги, что у ядра. Сведений о маршрутах у выдуманных туннелей нет: с `multiple` конфликтов нет,
                // без него заменяются все остальные — как в ядре.
                let running = d.running().map_err(|e| e.to_string())?;
                let others = to_replace(&tunnel, plan, multiple, &running, |_| None);
                done(run_switch(d.as_ref(), &tunnel, plan, &others))
            }
            Request::Delete(t) => done(d.disconnect(&t)),
            Request::Details(t) => Ok(Response::Info(d.details(&t))),
            Request::Read(t) => Ok(Response::Text(d.config_text(&t))),
            // Родного окна и настроек ядра в демо нет: действия в окне, запись конфига и настройки ничего не делают.
            Request::Write { .. } | Request::Native(_) | Request::SetLanguage(_) | Request::SetPing { .. } => Ok(Response::Ok),
            // Хранилища режима 2, обновлений и смены режима в демо нет — как и без ядра.
            other => Err(format!("demo: no core for {}", crate::explain::variant_name(&other))),
        }
    }
}

/// Демо: статистика трафика растёт по счётчикам выдуманных туннелей тем же кодом, что у агента (`AgentStats`), только
/// без файла. Паника шага — в журнал окна, цикл идёт дальше (как пинг демо).
pub(super) fn spawn_stats(shared: Arc<Shared>) {
    crate::crash::spawn_named("demo-stats", move || {
        let stats = AgentStats::in_memory();
        let report = |panic: &str, wait: Duration| shared.report_secondary_panic(panic, wait);
        crate::crash::nonfatal_loop(STATS_PERIOD, &std::thread::sleep, &report, || {
            stats_step(&shared, &stats, Instant::now());
            ControlFlow::Continue(())
        });
    });
}

fn stats_step(shared: &Shared, stats: &AgentStats, at: Instant) {
    // Режим статистике не важен: из `State` она берёт только туннели и счётчики.
    let state = shared.core_state(Mode::Overlay, u64::MAX);
    shared.update_stats(|s| stats.observe_into(s, &state, at));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::proto::Plan;

    #[test]
    fn demo_switch_without_multiple_replaces_the_running_tunnel() {
        let d = Arc::new(Demo::new());
        let core = DemoCore(d.clone());
        let names = d.configs().unwrap();
        let (a, c) = (&names[0], &names[1]);
        core.ok(Request::Switch { tunnel: a.clone(), plan: Plan::Connect, multiple: false }).unwrap();
        core.ok(Request::Switch { tunnel: c.clone(), plan: Plan::Connect, multiple: false }).unwrap();
        assert_eq!(d.running().unwrap(), vec![c.clone()]);
        core.ok(Request::Switch { tunnel: a.clone(), plan: Plan::Connect, multiple: true }).unwrap();
        assert_eq!(d.running().unwrap().len(), 2);
        core.ok(Request::Switch { tunnel: a.clone(), plan: Plan::Disconnect, multiple: false }).unwrap();
        assert_eq!(d.running().unwrap(), vec![c.clone()]);
    }

    /// Демо: статистика растёт по счётчикам опроса (до шага 10 её считал опрос окна; после переноса в агента — стояла).
    /// Правка окна (удаление туннеля в демо) не затирается следующим шагом.
    #[test]
    fn demo_stats_grow_and_keep_window_edits() {
        use crate::monitor::{poll, Options};
        use crate::uapi::{Peer, Status};
        struct Counters(u64);
        impl TunnelHost for Counters {
            fn configs(&self) -> std::io::Result<Vec<String>> {
                Ok(vec!["t".into(), "gone".into()])
            }
            fn running(&self) -> std::io::Result<Vec<String>> {
                Ok(vec!["t".into()])
            }
            fn query(&self, _: &str) -> std::io::Result<Status> {
                Ok(Status { listen_port: 7, peers: vec![Peer { rx_bytes: self.0, tx_bytes: self.0 / 2, ..Default::default() }], ..Default::default() })
            }
            fn connect(&self, _: &str) -> Result<(), String> {
                Ok(())
            }
            fn disconnect(&self, _: &str) -> Result<(), String> {
                Ok(())
            }
        }
        let options = Options { ping: false, ping_host: String::new(), notify: false, tray: false, taskbar: false };
        let shared = Shared::new(None, options, None);
        shared.update_stats(|s| s.insert("gone".into(), Default::default()));
        let stats = AgentStats::in_memory();
        let t0 = Instant::now();
        for (i, rx) in [1_000u64, 5_000, 9_000].into_iter().enumerate() {
            poll(&shared, &Counters(rx));
            stats_step(&shared, &stats, t0 + Duration::from_secs(i as u64));
            if i == 0 {
                shared.update_stats(|s| s.remove("gone"));
            }
        }
        let (rx, keys) = shared.update_stats(|s| (s["t"].rx, s.keys().cloned().collect::<Vec<_>>()));
        assert_eq!(rx, 9_000, "новая сессия: первый замер целиком, дальше прирост");
        assert_eq!(keys, ["t"], "удалённое окном не вернулось");
    }
}
