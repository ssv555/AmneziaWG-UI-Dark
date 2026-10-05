//! Ядро демо-режима: окно говорит с ним так же, как с настоящим (`CoreApi`), а оно отвечает выдуманными
//! туннелями `Demo`. Так у окна один путь к туннелям, а не ветка «ядро или демо» в каждом действии.

use std::sync::Arc;

use crate::backend::{Demo, TunnelHost};
use crate::daemon::proto::{Request, Response};
use crate::daemon::server::{run_switch, to_replace};
use crate::daemon::CoreApi;

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
            other => Err(format!("demo: no core for {other:?}")),
        }
    }
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
}
