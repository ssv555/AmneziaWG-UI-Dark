//! Файлы-источники туннелей и подтверждения действий над ними: правка во внешнем редакторе,
//! синхронизация с AmneziaWG, удаление туннеля.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use eframe::egui::{self, RichText};

use crate::daemon::agent::client::AgentApi;
use crate::daemon::agent::proto::AgentRequest;
use crate::daemon::CoreApi;
use crate::events::Severity;
use crate::groups;
use crate::i18n::{tr, trf};
use crate::monitor::Shared;
use crate::settings::Mode;
use crate::{tray, win};

use super::dialog::{dialog_buttons, dialog_window};
use super::modals::{Modal, Outcome, Turn};
use super::theme::YELLOW;
use super::{Action, App, ErrorSink};

/// Действия, которые перезаписывают или удаляют данные, — только после подтверждения.
#[derive(Clone)]
pub(super) enum Confirm {
    /// AmneziaWG → файл-источник.
    ToSource(String),
    /// Файл-источник → AmneziaWG.
    ToNative(String),
    /// Удалить группу (содержимое переходит уровнем выше).
    DeleteGroup(String),
    /// Удалить туннель в AmneziaWG.
    DeleteTunnel(String),
    /// Удалить службу ядра.
    UninstallCore,
}

impl App {
    /// Родной импорт с выделенным файлом; «Открыть» нажимает пользователь. Напоминание про дубликат.
    pub(super) fn import_source(&self, path: PathBuf) {
        *self.notice.lock().unwrap() = Some(tr("src.import_hint"));
        let (core, error) = (self.core.clone(), self.action_error.clone());
        std::thread::spawn(move || {
            if let Err(e) = core.import_in_native(Some(&path)) {
                error.push(e);
            }
        });
    }

    /// Раз в секунду: сохранён ли файл-источник, открытый на правку, — тогда спросить про импорт.
    pub(super) fn check_watched(&mut self) {
        let Some(saved) = self.sources.poll(Instant::now()) else { return };
        for file in saved {
            if self.modals.open_if_absent(Modal::AskImport { tunnel: file.tunnel, path: file.path }) {
                tray::show_window();
            }
        }
        self.ctx.request_repaint_after(Duration::from_secs(1));
    }

    /// Подтверждение перезаписи (синхронизация) и удаления (группа, туннель).
    pub(super) fn show_confirm(&mut self, ctx: &egui::Context, c: &Confirm, turn: Turn) -> Outcome {
        let (title, text, primary, warning) = self.confirm_texts(c);
        // Туннель без источника: перед удалением можно сохранить копию конфига в файл.
        let offer_copy = matches!(c, Confirm::DeleteTunnel(t) if self.s.book.source(t).is_none());
        let (mut yes, mut no, mut copy) = (false, false, false);
        let mut open = true;
        dialog_window(ctx, title, "confirm", &mut open)
            .show(ctx, |ui| {
                ui.set_width(500.0);
                ui.add(egui::Label::new(text).wrap());
                if let Some(w) = &warning {
                    ui.add_space(6.0);
                    ui.add(egui::Label::new(RichText::new(w).color(YELLOW)).wrap());
                }
                ui.add_space(10.0);
                if offer_copy {
                    copy = ui.button(tr("del.save_copy")).on_hover_text(tr("del.save_copy_hint")).clicked();
                    ui.add_space(4.0);
                }
                (yes, no) = dialog_buttons(ui, &primary, true, Some(&tr("btn.cancel")));
            });
        let (enter, escape) = turn.keys(ctx);
        if copy {
            if let Confirm::DeleteTunnel(t) = c {
                // Отмена выбора файла — диалог остаётся.
                if let Some(path) = win::pick_conf(true, Some(&PathBuf::from(format!("{t}.conf")))) {
                    self.run_delete_tunnel(t.clone(), Some(path));
                    return Outcome::Close;
                }
            }
            return Outcome::Keep;
        }
        if yes || enter {
            match c.clone() {
                c @ (Confirm::ToSource(_) | Confirm::ToNative(_)) => self.run_sync(c),
                Confirm::DeleteGroup(path) => self.apply(Action::DeleteGroup(path)),
                Confirm::DeleteTunnel(t) => self.run_delete_tunnel(t, None),
                Confirm::UninstallCore => self.run_core_setup(crate::daemon::install::UNINSTALL_FLAG),
            }
        }
        if yes || no || enter || escape || !open {
            Outcome::Close
        } else {
            Outcome::Keep
        }
    }

    /// Заголовок, текст, надпись главной кнопки и предупреждение для подтверждения.
    fn confirm_texts(&self, c: &Confirm) -> (String, String, String, Option<String>) {
        let source = |t: &str| self.s.book.source(t).unwrap_or_default().to_string();
        match c {
            Confirm::ToSource(t) => (tr("sync.title"), trf("sync.confirm_to_source", &[t, &source(t)]), tr("btn.yes"), None),
            Confirm::ToNative(t) => (tr("sync.title"), trf("sync.confirm_to_native", &[t, &source(t)]), tr("btn.yes"), None),
            Confirm::DeleteGroup(path) => {
                let (subgroups, tunnels) = self.s.book.count_below(path);
                let target = groups::parent(path).map(str::to_string).unwrap_or_else(|| tr("app.ungrouped"));
                let text = trf("del.group_text", &[path, &subgroups.to_string(), &tunnels.to_string(), &target]);
                (tr("del.group_title"), text, tr("del.delete"), None)
            }
            Confirm::UninstallCore => (tr("core.uninstall_title"), tr("core.uninstall_text"), tr("del.delete"), None),
            Confirm::DeleteTunnel(t) if self.s.mode() == Mode::Engine => {
                let running = self.shared.is_running(t);
                let warning = running.then(|| tr("eng.delete_running"));
                (tr("del.tunnel_title"), trf("eng.delete_text", &[t]), tr("del.delete"), warning)
            }
            Confirm::DeleteTunnel(t) => {
                let running = self.shared.is_running(t);
                let mut warning = Vec::new();
                if running {
                    warning.push(tr("del.tunnel_running"));
                }
                match self.s.book.source(t) {
                    Some(src) => warning.push(trf("del.tunnel_has_source", &[src])),
                    None => warning.push(tr("del.tunnel_no_source")),
                }
                (tr("del.tunnel_title"), trf("del.tunnel_text", &[t]), tr("del.delete"), Some(warning.join("\n")))
            }
        }
    }

    /// Удаление туннеля в AmneziaWG в фоне; с `copy` — сначала сохранить его конфиг в файл.
    fn run_delete_tunnel(&self, tunnel: String, copy: Option<PathBuf>) {
        let (core, shared, error, notice, deleted) =
            (self.core.clone(), self.shared.clone(), self.action_error.clone(), self.notice.clone(), self.deleted.clone());
        let agent = self.agent.clone();
        let (running_key, done_key, copy_key) = match self.s.mode() {
            Mode::Engine => ("eng.deleting", "eng.deleted", "eng.deleted_copy"),
            Mode::Overlay => ("del.running", "del.done", "del.done_copy"),
        };
        *notice.lock().unwrap() = Some(trf(running_key, &[&tunnel]));
        std::thread::spawn(move || {
            let result = delete_tunnel(core.as_ref(), &tunnel, copy.as_deref()).map(|()| match &copy {
                Some(p) => trf(copy_key, &[&tunnel, &p.display().to_string()]),
                None => trf(done_key, &[&tunnel]),
            });
            if report_outcome(result, &tunnel, Severity::Warn, &shared, &notice, &error) {
                follow_stats(agent.as_deref(), &shared, StatsChange::Forget(tunnel.clone()));
                deleted.lock().unwrap().push(tunnel);
            }
        });
    }

    /// Убрать удалённые туннели из настроек: группа, источник, выбор, кэш сведений. Файл-источник не трогаем.
    pub(super) fn apply_deleted(&mut self) {
        let gone: Vec<String> = std::mem::take(&mut *self.deleted.lock().unwrap());
        for t in gone {
            self.s.book.forget_tunnel(&t);
            self.sources.forget(&t);
        }
    }

    /// Выбранный неподключённый туннель с источником: разобрать файл заново, если он изменился.
    pub(super) fn refresh_source_info(&mut self) {
        let Some(t) = self.s.book.tunnel().map(str::to_string) else { return };
        let Some(path) = self.s.book.source(&t).map(PathBuf::from) else { return };
        // Предупреждение, а не ошибка действия: пользователь ничего не делал, а файл-источник мог просто исчезнуть.
        if let Err(e) = self.sources.refresh_source(&t, &path) {
            self.shared.log(&t, Severity::Warn, &crate::fsutil::io_ctx(&path, e));
        }
    }

    /// Синхронизация в фоне: родное окно управляется автоматически, итог — в строке состояния.
    fn run_sync(&self, c: Confirm) {
        let (core, shared, error, notice) = (self.core.clone(), self.shared.clone(), self.action_error.clone(), self.notice.clone());
        let (tunnel, to_source) = match c {
            Confirm::ToSource(t) => (t, true),
            Confirm::ToNative(t) => (t, false),
            Confirm::DeleteGroup(_) | Confirm::DeleteTunnel(_) | Confirm::UninstallCore => return,
        };
        let Some(path) = self.s.book.source(&tunnel).map(PathBuf::from) else { return };
        *notice.lock().unwrap() = Some(trf("sync.running", &[&tunnel]));
        std::thread::spawn(move || {
            let result = if to_source {
                save_config_copy(core.as_ref(), &tunnel, &path)
            } else {
                std::fs::read_to_string(&path)
                    .map_err(|e| crate::fsutil::io_ctx(&path, e))
                    .and_then(|text| core.write_config(&tunnel, &text))
            };
            let done = if to_source { "sync.done_to_source" } else { "sync.done_to_native" };
            report_outcome(result.map(|()| trf(done, &[&path.display().to_string()])), &tunnel, Severity::Info, &shared, &notice, &error);
        });
    }

    pub(super) fn show_ask_import(&mut self, ctx: &egui::Context, tunnel: &str, path: &Path, turn: Turn) -> Outcome {
        let (mut yes, mut no) = (false, false);
        let mut open = true;
        dialog_window(ctx, tr("src.saved_title"), "ask-import", &mut open)
            .show(ctx, |ui| {
                ui.set_width(460.0);
                ui.add(egui::Label::new(trf("src.ask", &[&path.display().to_string()])).wrap());
                ui.add_space(10.0);
                (yes, no) = dialog_buttons(ui, &tr("btn.yes"), true, Some(&tr("btn.no")));
            });
        let (enter, escape) = turn.keys(ctx);
        yes |= enter;
        no |= escape || !open;
        if yes {
            self.apply(Action::ImportSource(tunnel.to_string()));
        }
        if yes || no {
            Outcome::Close
        } else {
            Outcome::Keep
        }
    }
}

/// Итог фонового действия над туннелем: успех — в журнал событий и строку состояния; ошибка — один раз в журнал
/// (через приёмник ошибок окна, под именем туннеля), строка состояния очищается. Возвращает «удалось».
fn report_outcome(
    result: Result<String, String>,
    tunnel: &str,
    done: Severity,
    shared: &Shared,
    notice: &Mutex<Option<String>>,
    error: &ErrorSink,
) -> bool {
    match result {
        Ok(text) => {
            shared.log(tunnel, done, &text);
            *notice.lock().unwrap() = Some(text);
            true
        }
        Err(e) => {
            *notice.lock().unwrap() = None;
            error.push_for(tunnel, e);
            false
        }
    }
}

/// Сохранить конфиг туннеля из ядра в файл (копия перед удалением, синхронизация в файл-источник).
fn save_config_copy(core: &dyn CoreApi, tunnel: &str, path: &Path) -> Result<(), String> {
    let text = core.read_config(tunnel)?;
    std::fs::write(path, text).map_err(|e| crate::fsutil::io_ctx(path, e))
}

/// Удалить туннель через ядро; с `copy` — сначала сохранить его конфиг. Не удалось сохранить копию — не удаляем.
fn delete_tunnel(core: &dyn CoreApi, tunnel: &str, copy: Option<&Path>) -> Result<(), String> {
    if let Some(path) = copy {
        save_config_copy(core, tunnel, path)?;
    }
    core.delete_tunnel(tunnel)
}

/// Что ядро сделало с туннелем — то же надо сделать со статистикой.
pub(super) enum StatsChange {
    Rename { old: String, new: String },
    Forget(String),
}

/// Ядро подтвердило переименование или удаление туннеля: статистику ведёт агент, ему сообщается то же
/// (`StatsRename`/`StatsForget`). Агент недоступен — запись в журнал окна; старое имя тогда уберёт чистка статистики
/// через `stats::KEEP_ABSENT_DAYS`. Агента нет (демо) — меняется статистика самого окна.
pub(super) fn follow_stats(agent: Option<&dyn AgentApi>, shared: &Shared, change: StatsChange) {
    let Some(agent) = agent else {
        shared.update_stats(|stats| match &change {
            StatsChange::Rename { old, new } => crate::stats::rename(stats, old, new),
            StatsChange::Forget(tunnel) => drop(stats.remove(tunnel)),
        });
        return;
    };
    let (tunnel, request) = match change {
        StatsChange::Rename { old, new } => (old.clone(), AgentRequest::StatsRename { old, new }),
        StatsChange::Forget(tunnel) => (tunnel.clone(), AgentRequest::StatsForget(tunnel)),
    };
    if let Err(e) = agent.ok(request) {
        shared.log(&tunnel, Severity::Warn, &trf("agent.stats_not_updated", &[&tunnel, &e]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::fake::FakeCore;
    use crate::daemon::proto::{Request, Response};

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("awg-ui-sources-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn store_core() -> FakeCore {
        FakeCore::new(|req| match req {
            Request::Read(t) => Ok(Response::Text(format!("[Interface]\n# {t}\n"))),
            Request::Delete(_) => Ok(Response::Ok),
            other => Err(format!("unexpected {other:?}")),
        })
    }

    #[test]
    fn delete_saves_the_copy_first_then_deletes() {
        let dir = temp_dir("copy");
        let copy = dir.join("office.conf");
        let core = store_core();
        delete_tunnel(&core, "office", Some(&copy)).unwrap();
        assert_eq!(core.requests(), [r#"Read("office")"#, r#"Delete("office")"#]);
        assert_eq!(std::fs::read_to_string(&copy).unwrap(), "[Interface]\n# office\n");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn delete_is_not_sent_when_the_copy_cannot_be_saved() {
        let dir = temp_dir("nocopy");
        // Папки нет — копию не записать, туннель должен остаться.
        let copy = dir.join("missing").join("office.conf");
        let core = store_core();
        assert!(delete_tunnel(&core, "office", Some(&copy)).is_err());
        assert_eq!(core.requests(), [r#"Read("office")"#]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn delete_refused_by_the_core_is_an_error() {
        let core = FakeCore::new(|_| Ok(Response::Err("office: connected".into())));
        assert_eq!(delete_tunnel(&core, "office", None), Err("office: connected".to_string()));
    }

    #[test]
    fn delete_with_unreachable_core_is_an_error() {
        let core = FakeCore::unreachable("core unavailable");
        assert_eq!(delete_tunnel(&core, "office", None), Err("core unavailable".to_string()));
    }

    fn quiet_shared() -> Shared {
        Shared::new(None, crate::monitor::Options { ping: false, ping_host: String::new(), notify: false, tray: false, taskbar: false }, None)
    }

    /// После ответа ядра агенту уходит то же изменение статистики, ровно одним запросом.
    #[test]
    fn stats_change_goes_to_the_agent() {
        use crate::daemon::agent::client::FakeAgent;
        use crate::daemon::agent::proto::AgentResponse;
        let sent = std::sync::Arc::new(Mutex::new(Vec::new()));
        let s = sent.clone();
        let agent = FakeAgent(Box::new(move |req| {
            s.lock().unwrap().push(format!("{req:?}"));
            Ok(AgentResponse::Ok)
        }));
        let shared = quiet_shared();
        follow_stats(Some(&agent), &shared, StatsChange::Rename { old: "a".into(), new: "b".into() });
        follow_stats(Some(&agent), &shared, StatsChange::Forget("c".into()));
        assert_eq!(*sent.lock().unwrap(), [r#"StatsRename { old: "a", new: "b" }"#, r#"StatsForget("c")"#]);
        assert!(shared.events_since(0).is_empty(), "удачно — без записей");
    }

    /// Агент недоступен: туннель уже переименован ядром, окно не мешает — одна запись в журнал с причиной.
    #[test]
    fn stats_change_with_absent_agent_is_logged_not_an_error() {
        use crate::daemon::agent::client::FakeAgent;
        let shared = quiet_shared();
        follow_stats(Some(&FakeAgent::unreachable("pipe: not found")), &shared, StatsChange::Forget("office".into()));
        let events = shared.events_since(0);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].1.severity, Severity::Warn);
        assert!(events[0].1.text.contains("office") && events[0].1.text.contains("pipe: not found"), "{}", events[0].1.text);
    }

    /// Демо (агента нет): меняется статистика самого окна.
    #[test]
    fn stats_change_without_an_agent_edits_the_window_stats() {
        let shared = quiet_shared();
        shared.update_stats(|s| {
            s.insert("a".into(), Default::default());
            s.insert("c".into(), Default::default());
        });
        follow_stats(None, &shared, StatsChange::Rename { old: "a".into(), new: "b".into() });
        follow_stats(None, &shared, StatsChange::Forget("c".into()));
        assert_eq!(shared.update_stats(|s| s.keys().cloned().collect::<Vec<_>>()), ["b"]);
    }

    #[test]
    fn failed_delete_or_sync_reaches_the_log_exactly_once() {
        let (shared, error) = super::super::testkit::sink();
        let notice = Mutex::new(Some("deleting".to_string()));
        assert!(!report_outcome(Err("core unavailable".into()), "office", Severity::Warn, &shared, &notice, &error));
        assert_eq!(super::super::testkit::errors(&shared), [("office".to_string(), "core unavailable".to_string())]);
        assert_eq!(shared.events_since(0).len(), 1, "одна запись, а не две");
        assert_eq!(*notice.lock().unwrap(), None);
        assert!(error.take_fresh());
    }

    #[test]
    fn successful_delete_is_logged_once_and_shown_in_the_status_bar() {
        let (shared, error) = super::super::testkit::sink();
        let notice = Mutex::new(None);
        assert!(report_outcome(Ok("office deleted".into()), "office", Severity::Warn, &shared, &notice, &error));
        let log = shared.events_since(0);
        assert_eq!(log.len(), 1);
        assert_eq!((log[0].1.tunnel.as_str(), log[0].1.severity, log[0].1.text.as_str()), ("office", Severity::Warn, "office deleted"));
        assert_eq!(notice.lock().unwrap().as_deref(), Some("office deleted"));
        assert!(!error.take_fresh());
    }
}
