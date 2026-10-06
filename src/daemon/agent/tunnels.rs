//! Конфиги туннелей в агенте: помощник в родном окне AmneziaWG (режим 1, UI Automation до 120 с) и хранилище
//! режима 2 (импорт, резервная копия, новый туннель). Раньше это обслуживало ядро; долгий помощник и файловый ввод-вывод
//! там занимали места канала и потоки рядом с VPN. Ядру агент сообщает только итог: `Forget` (туннель удалён в родном
//! окне — убрать из желаемого набора) и `Footprint` (сведения из родного окна — для поиска конфликтов при подключении).
//! Удаление и переименование туннеля режима 2 остаются в ядре: они трогают службу туннеля и желаемый набор. С записью
//! агента они разведены замком хранилища (`store`): правка переименованного или удалённого туннеля — ошибка, а не воскрешение.

use std::sync::Mutex;
use std::time::Duration;

use super::proto::{AgentResponse, TunnelRequest};
use crate::conf::TunnelInfo;
use crate::daemon::helper::{Op, Out};
use crate::daemon::pipe::{self, Timeouts};
use crate::daemon::proto::{NativeOp, Request, Response};
use crate::events::Severity;
use crate::i18n::{tr, trf};
use crate::settings::Mode;
use crate::store;

/// Ответ ядра на `Hello`/`Forget`/`Footprint`: ядро отвечает сразу, без переключений.
const CORE_REPLY: Duration = Duration::from_secs(15);

/// Что агенту нужно от ядра. Ошибка — готовый текст.
pub(super) trait CoreSide: Send + Sync {
    /// Режим работы ядра: от него зависит, куда идёт запрос — в хранилище или в родное окно.
    fn mode(&self) -> Result<Mode, String>;
    /// Туннель удалён в родном окне: убрать его из желаемого набора.
    fn forget(&self, tunnel: &str) -> Result<(), String>;
    /// Сведения туннеля из родного окна (`None` — конфиг изменён, прежние устарели).
    fn footprint(&self, tunnel: &str, info: Option<TunnelInfo>) -> Result<(), String>;
}

/// Запуск помощника в сеансе пользователя (`daemon::helper`); в тестах — подделка.
pub(super) trait Helper: Send + Sync {
    fn run(&self, session: u32, caller: &str, op: &Op) -> Result<Out, String>;
}

/// Кто прислал запрос по каналу агента.
pub(super) struct Caller {
    pub(super) session: u32,
    /// SID учётной записи клиента; не узнан — помощник не запускается (действует от имени клиента).
    pub(super) sid: Option<String>,
    /// Клиент с правами администратора (подтверждение UAC).
    pub(super) elevated: bool,
}

pub(super) struct Tunnels {
    core: Box<dyn CoreSide>,
    helper: Box<dyn Helper>,
    /// Действия в родном окне — по одному: два помощника сразу мешали бы друг другу в одном окне.
    helper_lock: Mutex<()>,
    log: Box<dyn Fn(Severity, &str) + Send + Sync>,
}

impl Tunnels {
    pub(super) fn new(core: Box<dyn CoreSide>, helper: Box<dyn Helper>, log: Box<dyn Fn(Severity, &str) + Send + Sync>) -> Tunnels {
        Tunnels { core, helper, helper_lock: Mutex::new(()), log }
    }

    /// Над каналом ядра и настоящим помощником.
    pub(super) fn real(log: Box<dyn Fn(Severity, &str) + Send + Sync>) -> Tunnels {
        Tunnels::new(Box::new(PipeCoreSide), Box::new(SessionHelper), log)
    }

    pub(super) fn handle(&self, req: TunnelRequest, caller: &Caller) -> AgentResponse {
        // Имя туннеля становится частью путей и командных строк — только допустимые имена.
        if let Some(bad) = names_in(&req).into_iter().find(|n| !store::valid_name(n)) {
            return AgentResponse::Refused(trf("eng.bad_name", &[bad]));
        }
        // Команды PreUp/PostUp/… служба туннеля выполнила бы от SYSTEM — из окна их не принимаем.
        if texts_in(&req).into_iter().any(crate::conf::has_scripts) {
            return AgentResponse::Refused(tr("core.no_scripts"));
        }
        let engine = match self.core.mode() {
            Ok(mode) => mode == Mode::Engine,
            Err(e) => return AgentResponse::Err(trf("agent.mode_unknown", &[&e])),
        };
        let need_engine = || if engine { Ok(()) } else { Err(tr("core.only_engine")) };
        match req {
            TunnelRequest::Read(t) if engine => reply(store::read(&store::path(&t)), AgentResponse::Text),
            TunnelRequest::Read(t) => self.helper_reply(caller, Op::ReadConfig(t)),
            TunnelRequest::Write { tunnel, text } if engine => done(store::update(&tunnel, &text)),
            TunnelRequest::Write { tunnel, text } => {
                self.tell_footprint(&tunnel, None);
                self.helper_reply(caller, Op::WriteConfig(tunnel, text))
            }
            // Окно шлёт удаление режима 2 ядру; сюда оно попадает, только если режим сменился по дороге.
            TunnelRequest::Delete(_) if engine => AgentResponse::Refused(tr("agent.delete_in_core")),
            TunnelRequest::Delete(t) => self.delete_native(caller, &t),
            TunnelRequest::Details(t) if engine => reply(store::read(&store::path(&t)).map(|text| crate::conf::parse(&text)), AgentResponse::Info),
            TunnelRequest::Details(t) => {
                let r = self.helper_reply(caller, Op::Details(t.clone()));
                if let AgentResponse::Info(info) = &r {
                    self.tell_footprint(&t, Some(info.clone()));
                }
                r
            }
            TunnelRequest::Import(entries) => reply(need_engine().and_then(|()| self.import(&entries)), AgentResponse::Report),
            TunnelRequest::ExportAll => reply(need_engine().and_then(|()| store::export_all()), AgentResponse::Entries),
            TunnelRequest::NewTunnel(name) => reply(need_engine().and_then(|()| store::new_tunnel(&name)), AgentResponse::Text),
            TunnelRequest::TakeNative => reply(self.take_native(caller), AgentResponse::Report),
            TunnelRequest::Native(_) if engine => AgentResponse::Err(tr("core.only_overlay")),
            TunnelRequest::Native(op) => self.helper_reply(caller, native_op(op)),
        }
    }

    /// Удаление в родном окне; удалённый туннель ядро убирает из желаемого набора (`Forget`). Не дошло до ядра —
    /// ошибка с объяснением: иначе надзор так и пытался бы поднять несуществующий туннель, а пользователь не знал бы.
    fn delete_native(&self, caller: &Caller, tunnel: &str) -> AgentResponse {
        let r = self.helper_reply(caller, Op::Delete(tunnel.to_string()));
        if !matches!(r, AgentResponse::Ok) {
            return r;
        }
        match self.core.forget(tunnel) {
            Ok(()) => AgentResponse::Ok,
            Err(e) => {
                let text = trf("agent.forget_failed", &[tunnel, &e]);
                (self.log)(Severity::Bad, &text);
                AgentResponse::Err(text)
            }
        }
    }

    /// Сведения для ядра. Не дошли — действие пользователя всё равно выполнено, но ядро решает о конфликтах по
    /// прежним сведениям: об этом запись в журнал.
    fn tell_footprint(&self, tunnel: &str, info: Option<TunnelInfo>) {
        if let Err(e) = self.core.footprint(tunnel, info) {
            (self.log)(Severity::Warn, &trf("agent.footprint_failed", &[tunnel, &e]));
        }
    }

    fn import(&self, entries: &[crate::archive::Entry]) -> Result<store::ImportReport, String> {
        let report = store::import(entries)?;
        self.log_imported(report.added.len());
        Ok(report)
    }

    /// «Забрать всё из AmneziaWG»: родной экспорт (помощник в сеансе пользователя) во временный архив в защищённой
    /// папке хранилища, импорт, архив удаляется сразу (остаток после сбоя уберёт `store::startup`).
    fn take_native(&self, caller: &Caller) -> Result<store::ImportReport, String> {
        self.take_native_with(|| self.export_native(caller))
    }

    /// Режим проверяется до экспорта (в режиме 1 помощника запускать незачем) и после: за время экспорта (до 120 с)
    /// режим могли сменить, и импорт в хранилище режима 2 тогда уже не нужен.
    fn take_native_with(&self, export: impl FnOnce() -> Result<Vec<crate::archive::Entry>, String>) -> Result<store::ImportReport, String> {
        let only_engine = || match self.core.mode()? {
            Mode::Engine => Ok(()),
            Mode::Overlay => Err(tr("core.only_engine")),
        };
        only_engine()?;
        let entries = export()?;
        only_engine()?;
        self.import(&entries)
    }

    /// Родной экспорт во временный архив и его чтение; архив удаляется в любом случае.
    fn export_native(&self, caller: &Caller) -> Result<Vec<crate::archive::Entry>, String> {
        let zip = store::export_temp()?;
        let exported = match self.run_helper(caller, &Op::Export(zip.display().to_string())) {
            Ok(Out::Ok) => crate::archive::read(&zip, None).map_err(|e| format!("{}: {e:?}", zip.display())),
            Ok(Out::Err(e)) | Err(e) => Err(e),
            Ok(other) => Err(format!("helper: {other:?}")),
        };
        let removed = match std::fs::remove_file(&zip) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(crate::fsutil::io_ctx(&zip, e)),
            _ => Ok(()),
        };
        removed.and(exported)
    }

    fn log_imported(&self, n: usize) {
        if n > 0 {
            (self.log)(Severity::Info, &trf("eng.imported", &[&n.to_string()]));
        }
    }

    /// Действие в родном окне — через помощника в сеансе того, кто прислал запрос, и от его имени.
    fn helper_reply(&self, caller: &Caller, op: Op) -> AgentResponse {
        match self.run_helper(caller, &op) {
            Ok(Out::Ok) => AgentResponse::Ok,
            Ok(Out::Text(t)) => AgentResponse::Text(t),
            Ok(Out::Info(i)) => AgentResponse::Info(i),
            Ok(Out::Err(e)) | Err(e) => AgentResponse::Err(e),
        }
    }

    fn run_helper(&self, caller: &Caller, op: &Op) -> Result<Out, String> {
        let sid = caller.sid.as_deref().ok_or_else(|| tr("core.other_session"))?;
        let _one = crate::crash::lock(&self.helper_lock);
        self.helper.run(caller.session, sid, op)
    }
}

fn reply<T>(result: Result<T, String>, ok: impl FnOnce(T) -> AgentResponse) -> AgentResponse {
    result.map_or_else(AgentResponse::Err, ok)
}

fn done(result: Result<(), String>) -> AgentResponse {
    reply(result, |()| AgentResponse::Ok)
}

fn native_op(op: NativeOp) -> Op {
    match op {
        NativeOp::Open => Op::Open,
        NativeOp::Edit(t) => Op::Edit(t),
        NativeOp::Import(f) => Op::Import(f),
        NativeOp::Close => Op::Close,
    }
}

/// Имена туннелей из запроса (имена из архива импорта проверяет сам импорт: неподходящие — в `bad_name` отчёта).
fn names_in(req: &TunnelRequest) -> Vec<&str> {
    match req {
        TunnelRequest::Read(t) | TunnelRequest::Delete(t) | TunnelRequest::Details(t) | TunnelRequest::NewTunnel(t) => vec![t],
        TunnelRequest::Write { tunnel, .. } => vec![tunnel],
        TunnelRequest::Native(NativeOp::Edit(t)) => vec![t],
        _ => vec![],
    }
}

/// Тексты конфигов из запроса.
fn texts_in(req: &TunnelRequest) -> Vec<&str> {
    match req {
        TunnelRequest::Write { text, .. } => vec![text],
        TunnelRequest::Import(entries) => entries.iter().map(|e| e.text.as_str()).collect(),
        _ => vec![],
    }
}

/// Ядро за его каналом. Агент работает от SYSTEM — ядро принимает от него внутренние запросы.
struct PipeCoreSide;

impl PipeCoreSide {
    fn ask(what: &str, request: Request) -> Result<Response, String> {
        pipe::call_to(pipe::NAME, &request, Timeouts::with_reply(CORE_REPLY)).map_err(|e| format!("core {what}: {e}"))
    }

    fn ok(what: &str, request: Request) -> Result<(), String> {
        match Self::ask(what, request)? {
            Response::Ok => Ok(()),
            Response::Err(e) | Response::Refused(e) => Err(format!("core {what}: {e}")),
            _ => Err(format!("core {what}: unexpected reply")),
        }
    }
}

impl CoreSide for PipeCoreSide {
    fn mode(&self) -> Result<Mode, String> {
        match Self::ask("Hello", Request::Hello)? {
            Response::Hello { mode, .. } => Ok(mode),
            Response::Err(e) | Response::Refused(e) => Err(format!("core Hello: {e}")),
            _ => Err("core Hello: unexpected reply".into()),
        }
    }
    fn forget(&self, tunnel: &str) -> Result<(), String> {
        Self::ok("Forget", Request::Forget(tunnel.into()))
    }
    fn footprint(&self, tunnel: &str, info: Option<TunnelInfo>) -> Result<(), String> {
        Self::ok("Footprint", Request::Footprint { tunnel: tunnel.into(), info })
    }
}

/// Настоящий помощник: `awg-ui.exe --native-op` в сеансе пользователя с его повышенным токеном.
struct SessionHelper;

impl Helper for SessionHelper {
    fn run(&self, session: u32, caller: &str, op: &Op) -> Result<Out, String> {
        crate::daemon::helper::run(session, caller, op)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Подделка ядра: режимы по очереди (последний остаётся), просьбы записываются; `forget_fails` — ядро не ответило.
    #[derive(Default)]
    struct FakeCore {
        modes: Mutex<Vec<Mode>>,
        calls: Mutex<Vec<String>>,
        unreachable: bool,
        forget_fails: bool,
    }

    impl FakeCore {
        fn in_mode(mode: Mode) -> FakeCore {
            FakeCore { modes: Mutex::new(vec![mode]), ..Default::default() }
        }
    }

    impl CoreSide for Arc<FakeCore> {
        fn mode(&self) -> Result<Mode, String> {
            if self.unreachable {
                return Err("pipe: not found".into());
            }
            let mut modes = self.modes.lock().unwrap();
            Ok(if modes.len() > 1 { modes.remove(0) } else { modes[0] })
        }
        fn forget(&self, tunnel: &str) -> Result<(), String> {
            self.calls.lock().unwrap().push(format!("forget {tunnel}"));
            if self.forget_fails {
                return Err("core Forget: pipe: not found".into());
            }
            Ok(())
        }
        fn footprint(&self, tunnel: &str, info: Option<TunnelInfo>) -> Result<(), String> {
            self.calls.lock().unwrap().push(format!("footprint {tunnel} {}", info.map_or("none", |_| "info")));
            Ok(())
        }
    }

    /// Подделка помощника: записывает задания, ответ задаёт тест.
    struct FakeHelper {
        ops: Arc<Mutex<Vec<String>>>,
        reply: Box<dyn Fn(&Op) -> Out + Send + Sync>,
    }

    impl Helper for FakeHelper {
        fn run(&self, session: u32, caller: &str, op: &Op) -> Result<Out, String> {
            self.ops.lock().unwrap().push(format!("{session} {caller} {op:?}"));
            Ok((self.reply)(op))
        }
    }

    struct Rig {
        tunnels: Tunnels,
        core: Arc<FakeCore>,
        ops: Arc<Mutex<Vec<String>>>,
        logged: Arc<Mutex<Vec<(Severity, String)>>>,
    }

    impl Rig {
        fn ops(&self) -> Vec<String> {
            self.ops.lock().unwrap().clone()
        }
        fn core_calls(&self) -> Vec<String> {
            self.core.calls.lock().unwrap().clone()
        }
    }

    fn rig(core: FakeCore, reply: impl Fn(&Op) -> Out + Send + Sync + 'static) -> Rig {
        let core = Arc::new(core);
        let ops = Arc::new(Mutex::new(Vec::new()));
        let logged = Arc::new(Mutex::new(Vec::new()));
        let sink = logged.clone();
        let helper = FakeHelper { ops: ops.clone(), reply: Box::new(reply) };
        let tunnels = Tunnels::new(Box::new(core.clone()), Box::new(helper), Box::new(move |s, t| sink.lock().unwrap().push((s, t.to_string()))));
        Rig { tunnels, core, ops, logged }
    }

    fn user() -> Caller {
        Caller { session: 3, sid: Some("S-1-5-21-1-2-3-1001".into()), elevated: false }
    }

    #[test]
    fn native_delete_tells_the_core_to_forget_the_tunnel() {
        let r = rig(FakeCore::in_mode(Mode::Overlay), |_| Out::Ok);
        assert_eq!(r.tunnels.handle(TunnelRequest::Delete("office".into()), &user()), AgentResponse::Ok);
        assert_eq!(r.ops(), [r#"3 S-1-5-21-1-2-3-1001 Delete("office")"#]);
        assert_eq!(r.core_calls(), ["forget office"]);
    }

    #[test]
    fn failed_native_delete_does_not_touch_the_desired_set() {
        let r = rig(FakeCore::in_mode(Mode::Overlay), |_| Out::Err("no such tunnel".into()));
        assert_eq!(r.tunnels.handle(TunnelRequest::Delete("office".into()), &user()), AgentResponse::Err("no such tunnel".into()));
        assert!(r.core_calls().is_empty());
    }

    /// Туннель удалён, а ядро об этом не узнало: пользователь видит ошибку, в журнале — запись.
    #[test]
    fn delete_that_the_core_did_not_take_is_an_error_and_logged() {
        let r = rig(FakeCore { forget_fails: true, ..FakeCore::in_mode(Mode::Overlay) }, |_| Out::Ok);
        let expected = trf("agent.forget_failed", &["office", "core Forget: pipe: not found"]);
        assert_eq!(r.tunnels.handle(TunnelRequest::Delete("office".into()), &user()), AgentResponse::Err(expected.clone()));
        assert_eq!(*r.logged.lock().unwrap(), [(Severity::Bad, expected)]);
    }

    /// Сведения из родного окна уходят ядру; запись конфига сначала забывает прежние.
    #[test]
    fn details_and_writes_feed_the_core_footprint() {
        let r = rig(FakeCore::in_mode(Mode::Overlay), |op| match op {
            Op::Details(_) => Out::Info(TunnelInfo::default()),
            _ => Out::Ok,
        });
        assert_eq!(r.tunnels.handle(TunnelRequest::Details("a".into()), &user()), AgentResponse::Info(TunnelInfo::default()));
        let write = TunnelRequest::Write { tunnel: "a".into(), text: "[Interface]\nAddress = 10.0.0.2/32\n".into() };
        assert_eq!(r.tunnels.handle(write, &user()), AgentResponse::Ok);
        assert_eq!(r.core_calls(), ["footprint a info", "footprint a none"]);
        assert_eq!(r.ops().len(), 2);
    }

    #[test]
    fn bad_names_and_scripts_are_refused_before_the_helper_runs() {
        let r = rig(FakeCore::in_mode(Mode::Overlay), |_| Out::Ok);
        let bad = r.tunnels.handle(TunnelRequest::Read(r"..\x".into()), &user());
        assert_eq!(bad, AgentResponse::Refused(trf("eng.bad_name", &[r"..\x"])));
        let script = TunnelRequest::Write { tunnel: "a".into(), text: "[Interface]\nPostUp = cmd /c calc\n".into() };
        assert_eq!(r.tunnels.handle(script, &user()), AgentResponse::Refused(tr("core.no_scripts")));
        let import = TunnelRequest::Import(vec![crate::archive::Entry { name: "a".into(), text: "PreUp = x\n".into() }]);
        assert_eq!(r.tunnels.handle(import, &user()), AgentResponse::Refused(tr("core.no_scripts")));
        assert!(r.ops().is_empty() && r.core_calls().is_empty());
    }

    /// Клиент без известного SID (или ядро недоступно) — помощник не запускается, ответ объясняет почему.
    #[test]
    fn helper_needs_a_known_caller_and_a_known_mode() {
        let r = rig(FakeCore::in_mode(Mode::Overlay), |_| Out::Ok);
        let anonymous = Caller { session: 0, sid: None, elevated: true };
        assert_eq!(r.tunnels.handle(TunnelRequest::Native(NativeOp::Open), &anonymous), AgentResponse::Err(tr("core.other_session")));
        let r2 = rig(FakeCore { unreachable: true, ..Default::default() }, |_| Out::Ok);
        let answer = r2.tunnels.handle(TunnelRequest::Native(NativeOp::Open), &user());
        assert_eq!(answer, AgentResponse::Err(trf("agent.mode_unknown", &["pipe: not found"])));
        assert!(r.ops().is_empty() && r2.ops().is_empty());
    }

    #[test]
    fn requests_of_the_other_mode_are_refused() {
        let overlay = rig(FakeCore::in_mode(Mode::Overlay), |_| Out::Ok);
        for req in [TunnelRequest::ExportAll, TunnelRequest::NewTunnel("a".into()), TunnelRequest::Import(vec![])] {
            assert_eq!(overlay.tunnels.handle(req, &user()), AgentResponse::Err(tr("core.only_engine")));
        }
        let engine = rig(FakeCore::in_mode(Mode::Engine), |_| Out::Ok);
        assert_eq!(engine.tunnels.handle(TunnelRequest::Native(NativeOp::Close), &user()), AgentResponse::Err(tr("core.only_overlay")));
        assert_eq!(engine.tunnels.handle(TunnelRequest::Delete("a".into()), &user()), AgentResponse::Refused(tr("agent.delete_in_core")));
        assert!(overlay.ops().is_empty() && engine.ops().is_empty());
    }

    #[test]
    fn take_native_refuses_overlay_mode_without_running_the_helper() {
        let r = rig(FakeCore::in_mode(Mode::Overlay), |_| Out::Ok);
        let mut helper_ran = false;
        let result = r.tunnels.take_native_with(|| {
            helper_ran = true;
            Ok(Vec::new())
        });
        assert_eq!(result, Err(tr("core.only_engine")));
        assert!(!helper_ran);
    }

    /// Режим сменили, пока шёл экспорт: импорт в хранилище режима 2 уже не нужен.
    #[test]
    fn take_native_import_is_refused_if_mode_changed_during_export() {
        let r = rig(FakeCore { modes: Mutex::new(vec![Mode::Engine, Mode::Overlay]), ..Default::default() }, |_| Out::Ok);
        let mut exported = false;
        let result = r.tunnels.take_native_with(|| {
            exported = true;
            Ok(Vec::new())
        });
        assert_eq!(result, Err(tr("core.only_engine")));
        assert!(exported);
    }

    /// «Забрать всё из AmneziaWG» обходит проверку запроса (`texts_in`): конфиг с командой отклоняется самим импортом
    /// и назван в отчёте. В хранилище ничего не пишется (только отклонённые записи), тест его не меняет.
    #[test]
    fn take_native_refuses_configs_with_scripts_and_reports_them() {
        let r = rig(FakeCore { modes: Mutex::new(vec![Mode::Engine, Mode::Engine]), ..Default::default() }, |_| Out::Ok);
        let evil = crate::archive::Entry { name: "evil-take-native-test".into(), text: "[Interface]\nPostUp = cmd /c calc\n".into() };
        let report = r.tunnels.take_native_with(|| Ok(vec![evil])).unwrap();
        assert_eq!(report.scripts, vec!["evil-take-native-test".to_string()]);
        assert!(report.added.is_empty());
    }
}
