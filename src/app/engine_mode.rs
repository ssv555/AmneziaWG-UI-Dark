//! Режим 2 в окне: смена режима со справкой, импорт туннелей, «забрать всё из AmneziaWG», резервная копия
//! с паролем, новый туннель и переименование. Хранилище и службы туннелей — в ядре; окно шлёт ему запросы.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use eframe::egui::{self, RichText};

use super::modals::{Modal, Outcome, Turn};
use super::sources::{follow_stats, StatsChange};
use super::dialog::dialog_window;
use super::{dialog_buttons, App, Editor, RED, YELLOW};
use crate::archive::{self, ReadError};
use crate::conf::TunnelInfo;
use crate::daemon::proto::Request;
use crate::events::Severity;
use crate::i18n::{tr, trf};
use crate::monitor::Snapshot;
use crate::settings::{DialogId, Mode};
use crate::{store, win};

/// Минимальная длина пароля резервной копии: zip AES считает ключ всего 1000 раундами PBKDF2,
/// поэтому от подбора защищает только длина пароля.
const MIN_PASSWORD: usize = 12;

/// Пауза перед повторным запросом сведений после ошибки: ядро может перезапускаться, а запрос на каждом кадре — шторм.
const INFO_RETRY: Duration = Duration::from_secs(5);

/// Запросить сведения туннеля у ядра и положить в кэш. Имя остаётся в `loading` на время запроса и (при ошибке)
/// ещё `retry`, чтобы `refresh_store_info` не спрашивал заново на каждом кадре; освобождается всегда, иначе туннель
/// остался бы без сведений до перезапуска окна.
fn fetch_store_info(
    tunnel: &str,
    fetch: impl FnOnce(&str) -> Result<TunnelInfo, String>,
    infos: &Mutex<BTreeMap<String, (TunnelInfo, String)>>,
    loading: &Mutex<BTreeSet<String>>,
    retry: Duration,
) -> Result<(), String> {
    let result = fetch(tunnel);
    match &result {
        Ok(info) => {
            infos.lock().unwrap().insert(tunnel.to_string(), (info.clone(), tr("det.from_store")));
        }
        Err(_) => std::thread::sleep(retry),
    }
    loading.lock().unwrap().remove(tunnel);
    result.map(|_| ())
}

/// Окна режима 2 (поверх основного окна, по одному за раз).
pub(super) enum EngineDialog {
    /// Пароль резервной копии: для сохранения — дважды, для восстановления — один раз.
    Password { purpose: Purpose, pass: String, repeat: String, error: Option<String> },
    /// Имя туннеля: новый (`old` = None) или переименование.
    TunnelName { old: Option<String>, name: String },
}

pub(super) enum Purpose {
    Backup,
    /// Импорт этих файлов: среди них есть зашифрованный архив.
    Restore(Vec<PathBuf>),
}

impl App {
    pub(super) fn engine(&self) -> bool {
        self.s.mode() == Mode::Engine
    }

    /// Выбранный неподключённый туннель хранилища: сведения из его конфига — у ядра, в фоне, один раз
    /// (после правки сведения сбрасываются и запрашиваются снова).
    pub(super) fn refresh_store_info(&mut self, snap: &Snapshot) {
        let Some(t) = self.s.book.tunnel().map(str::to_string) else { return };
        if !self.engine() || snap.running.contains_key(&t) || !snap.tunnels.contains(&t) || self.sources.has_info(&t) {
            return;
        }
        if !self.sources.begin_load(&t) {
            return;
        }
        let (core, infos, loading, ctx, shared) = (self.core.clone(), self.sources.infos_handle(), self.sources.loading_handle(), self.ctx.clone(), self.shared.clone());
        std::thread::spawn(move || {
            if let Err(e) = fetch_store_info(&t, |t| core.details(t), &infos, &loading, INFO_RETRY) {
                shared.log(&t, Severity::Warn, &trf("det.store_failed", &[&e]));
            }
            ctx.request_repaint();
        });
    }

    /// Окно смены режима: что это за режим, что даёт, какие туннели будут отключены.
    pub(super) fn show_mode_switch(&mut self, ctx: &egui::Context, target: Mode, turn: Turn) -> Outcome {
        let running = self.shared.running_names();
        // Встроенный режим — только с файлами движка, которые установлены вместе с ядром и чьи суммы вшиты в сборку.
        let problem = if target == Mode::Engine { crate::engine::installed_files_ok().err() } else { None };
        let dialog = DialogId::mode_help(target);
        let (title, help) = match target {
            Mode::Engine => (tr("mode.engine"), tr("mode.engine_help")),
            Mode::Overlay => (tr("mode.overlay"), tr("mode.overlay_help")),
        };
        let help_hidden = self.s.hidden_dialogs.contains(&dialog);
        if mode_switch_silent(help_hidden, !running.is_empty(), problem.is_some()) {
            self.switch_mode(target);
            return Outcome::Close;
        }
        let (mut yes, mut no) = (false, false);
        let mut open = true;
        dialog_window(ctx, title, "mode-switch", &mut open)
            .show(ctx, |ui| {
                ui.set_width(560.0);
                if !help_hidden {
                    ui.add(egui::Label::new(help).wrap());
                    ui.add_space(8.0);
                }
                if !running.is_empty() {
                    ui.add(egui::Label::new(RichText::new(trf("mode.will_disconnect", &[&running.join(", ")])).color(YELLOW)).wrap());
                    ui.add_space(8.0);
                }
                if let Some(p) = &problem {
                    ui.add(egui::Label::new(RichText::new(trf("mode.missing", &[p])).color(RED)).wrap());
                    ui.add_space(8.0);
                }
                ui.add_space(2.0);
                if !help_hidden {
                    ui.checkbox(turn.remember, tr("dlg.dont_show"));
                    ui.add_space(6.0);
                }
                (yes, no) = dialog_buttons(ui, &tr("mode.switch"), problem.is_none(), Some(&tr("btn.cancel")));
            });
        let (enter, escape) = turn.keys(ctx);
        if (yes || enter) && problem.is_none() {
            if *turn.remember {
                self.s.hidden_dialogs.insert(dialog);
            }
            self.switch_mode(target);
            Outcome::Close
        } else if no || escape || !open {
            Outcome::Close
        } else {
            Outcome::Keep
        }
    }

    /// Сменить режим: ядро отключает туннели прежнего режима и переходит на другой — окно не перезапускается,
    /// новый список придёт со следующим опросом.
    fn switch_mode(&mut self, target: Mode) {
        let (core, error, notice, ctx) = (self.core.clone(), self.action_error.clone(), self.notice.clone(), self.ctx.clone());
        *notice.lock().unwrap() = Some(tr("mode.switching"));
        std::thread::spawn(move || {
            let result = core.ok(Request::SetMode(target));
            *notice.lock().unwrap() = None;
            if let Err(e) = result {
                error.push(e);
            }
            ctx.request_repaint();
        });
    }

    /// Файл → «Импорт туннелей…»: `.conf` и `.zip`, несколько сразу.
    pub(super) fn engine_import(&mut self) {
        let files = win::pick_files(win::Files::ConfOrZip, false, true, None);
        if !files.is_empty() {
            self.import_files(files, None);
        }
    }

    /// Файл → «Восстановить из резервной копии…».
    pub(super) fn engine_restore(&mut self) {
        let files = win::pick_files(win::Files::Zip, false, false, None);
        if !files.is_empty() {
            self.import_files(files, None);
        }
    }

    /// Прочитать файлы (это файлы пользователя — читает окно) и отдать туннели ядру. Зашифрованный архив без
    /// пароля — спросить пароль и повторить; неверный пароль — то же окно с ошибкой.
    fn import_files(&mut self, files: Vec<PathBuf>, password: Option<String>) {
        let mut entries = Vec::new();
        for f in &files {
            let is_zip = f.extension().is_some_and(|e| e.eq_ignore_ascii_case("zip"));
            if !is_zip {
                match archive::read_conf(f) {
                    Ok(e) => entries.push(e),
                    Err(e) => return self.fail(e),
                }
                continue;
            }
            match archive::read(f, password.as_deref()) {
                Ok(e) => entries.extend(e),
                Err(ReadError::NeedPassword) => return self.ask_password(Purpose::Restore(files), None),
                Err(ReadError::WrongPassword) => return self.ask_password(Purpose::Restore(files), Some(tr("eng.wrong_password"))),
                Err(ReadError::Other(e)) => return self.fail(e),
            }
        }
        match self.core.report(Request::Import(entries)) {
            Ok(r) => *self.notice.lock().unwrap() = Some(report_text(&r)),
            Err(e) => self.fail(e),
        }
    }

    /// «Забрать всё из AmneziaWG»: ядро запускает родной экспорт в вашем сеансе и импортирует туннели.
    pub(super) fn engine_take_native(&self) {
        let (core, error, notice, ctx) = (self.core.clone(), self.action_error.clone(), self.notice.clone(), self.ctx.clone());
        *notice.lock().unwrap() = Some(tr("eng.taking"));
        std::thread::spawn(move || {
            match core.report(Request::TakeNative) {
                Ok(r) => *notice.lock().unwrap() = Some(report_text(&r)),
                Err(e) => {
                    *notice.lock().unwrap() = None;
                    error.push(e);
                }
            }
            ctx.request_repaint();
        });
    }

    /// Файл → «Резервная копия…»: сначала пароль, потом куда сохранить.
    pub(super) fn engine_backup(&mut self) {
        self.ask_password(Purpose::Backup, None);
    }

    fn ask_password(&mut self, purpose: Purpose, error: Option<String>) {
        self.modals.open(Modal::Engine(EngineDialog::Password { purpose, pass: String::new(), repeat: String::new(), error }));
    }

    /// Туннели — у ядра, архив с паролем пишет окно туда, куда укажет пользователь.
    fn write_backup(&mut self, password: &str) {
        let name = format!("AmneziaWG-UI-Dark-backup-{}.zip", crate::fmt::date(crate::monitor::unix_now()));
        let Some(file) = win::pick_files(win::Files::Zip, true, false, Some(&PathBuf::from(name))).into_iter().next() else { return };
        match self.core.entries(Request::ExportAll).and_then(|entries| archive::write_backup(&file, password, &entries).map(|()| entries.len())) {
            Ok(n) => *self.notice.lock().unwrap() = Some(trf("eng.backup_done", &[&n.to_string(), &file.display().to_string()])),
            Err(e) => self.fail(e),
        }
    }

    /// Файл → «Новый туннель…» / меню туннеля → «Переименовать…».
    pub(super) fn ask_tunnel_name(&mut self, old: Option<String>) {
        let name = old.clone().unwrap_or_default();
        self.modals.open(Modal::Engine(EngineDialog::TunnelName { old, name }));
    }

    /// Правка туннеля хранилища во встроенном редакторе (сохранение — снова в хранилище ядра, зашифрованным).
    pub(super) fn engine_edit(&mut self, tunnel: String) {
        match self.core.read_config(&tunnel) {
            Ok(text) => self.open_store_editor(tunnel, text),
            Err(e) => self.fail(e),
        }
    }

    fn open_store_editor(&mut self, tunnel: String, text: String) {
        let path = PathBuf::from(format!("{tunnel}.conf"));
        self.editor = Some(Editor::new(path, text, Some(tunnel)));
    }

    fn create_tunnel(&mut self, name: String) {
        match self.core.text(Request::NewTunnel(name.clone())) {
            Ok(text) => {
                self.s.book.select_tunnel(&name);
                self.open_store_editor(name, text);
            }
            Err(e) => self.fail(e),
        }
    }

    /// Переименование у ядра; статистика — у агента (в фоне: зависший агент не держит окно); группа и выбор — здесь.
    fn rename_tunnel(&mut self, old: String, new: String) {
        if let Err(e) = self.core.ok(Request::Rename { old: old.clone(), new: new.clone() }) {
            return self.fail(e);
        }
        let (agent, shared, change) = (self.agent.clone(), self.shared.clone(), StatsChange::Rename { old: old.clone(), new: new.clone() });
        std::thread::spawn(move || follow_stats(agent.as_deref(), &shared, change));
        self.s.book.rename_tunnel(&old, &new);
        self.sources.rename(&old, &new);
    }

    fn fail(&self, e: String) {
        *self.notice.lock().unwrap() = None;
        self.action_error.push(e);
    }

    /// Окна пароля и имени туннеля.
    pub(super) fn show_engine_dialog(&mut self, ctx: &egui::Context, dialog: &mut EngineDialog, turn: Turn) -> Outcome {
        let focus = turn.fresh;
        let existing = self.shared.known_tunnels();
        let (mut ok, mut cancel, mut valid, mut open) = (false, false, false, true);
        match dialog {
            EngineDialog::Password { purpose, pass, repeat, error } => {
                let backup = matches!(purpose, Purpose::Backup);
                let title = tr(if backup { "eng.backup_title" } else { "eng.restore_title" });
                dialog_window(ctx, title, "engine-dialog", &mut open).show(ctx, |ui| {
                    ui.set_width(420.0);
                    ui.add(egui::Label::new(tr(if backup { "eng.backup_text" } else { "eng.restore_text" })).wrap());
                    ui.add_space(6.0);
                    ui.label(tr("eng.password"));
                    let out = ui.add(egui::TextEdit::singleline(pass).password(true).desired_width(f32::INFINITY));
                    if focus {
                        out.request_focus();
                    }
                    if backup {
                        ui.label(tr("eng.password_repeat"));
                        ui.add(egui::TextEdit::singleline(repeat).password(true).desired_width(f32::INFINITY));
                    }
                    let problem = if pass.is_empty() {
                        None
                    } else if backup && pass.chars().count() < MIN_PASSWORD {
                        Some(tr("eng.password_short"))
                    } else if backup && pass != repeat {
                        Some(tr("eng.password_mismatch"))
                    } else {
                        None
                    };
                    valid = !pass.is_empty() && problem.is_none();
                    let line = problem.or_else(|| error.clone()).unwrap_or_default();
                    ui.add_sized([ui.available_width(), 18.0], egui::Label::new(RichText::new(line).color(RED).small()).truncate());
                    (ok, cancel) = dialog_buttons(ui, &tr(if backup { "eng.backup_save" } else { "btn.ok" }), valid, Some(&tr("btn.cancel")));
                });
            }
            EngineDialog::TunnelName { old, name } => {
                let title = match old {
                    Some(o) => trf("eng.rename_title", &[o]),
                    None => tr("eng.new_title"),
                };
                dialog_window(ctx, title, "engine-dialog", &mut open).show(ctx, |ui| {
                    ui.set_width(380.0);
                    ui.label(tr("dlg.name"));
                    let out = ui.add(egui::TextEdit::singleline(name).desired_width(f32::INFINITY));
                    if focus {
                        out.request_focus();
                    }
                    let same = old.as_deref() == Some(name.as_str());
                    let error = if name.is_empty() || same {
                        String::new()
                    } else if !crate::engine::valid_name(name) {
                        trf("eng.bad_name", &[name])
                    // NTFS не различает регистр: «Office» занят, если есть «office» (кроме самого переименуемого).
                    } else if store::find(&existing, name).is_some_and(|e| Some(e) != old.as_ref()) {
                        tr("dlg.err_exists")
                    } else {
                        String::new()
                    };
                    valid = !name.is_empty() && !same && error.is_empty();
                    ui.add_sized([ui.available_width(), 34.0], egui::Label::new(RichText::new(error).color(RED).small()).wrap());
                    (ok, cancel) = dialog_buttons(ui, &tr("btn.ok"), valid, Some(&tr("btn.cancel")));
                });
            }
        }
        let (enter, escape) = turn.keys(ctx);
        ok |= enter && valid;
        cancel |= escape || !open;
        if cancel {
            return Outcome::Close;
        }
        if !ok {
            return Outcome::Keep;
        }
        // Диалог закрывается, но действие может открыть его снова (неверный пароль) — `Modals` положит новый поверх.
        match dialog {
            EngineDialog::Password { purpose: Purpose::Backup, pass, .. } => self.write_backup(pass),
            EngineDialog::Password { purpose: Purpose::Restore(files), pass, .. } => {
                let (files, pass) = (std::mem::take(files), std::mem::take(pass));
                self.import_files(files, Some(pass));
            }
            EngineDialog::TunnelName { old: None, name } => self.create_tunnel(std::mem::take(name)),
            EngineDialog::TunnelName { old: Some(old), name } => self.rename_tunnel(std::mem::take(old), std::mem::take(name)),
        }
        Outcome::Close
    }
}

/// Смена режима без окна: справка о режиме скрыта («Больше не показывать»), подключений нет и движок на месте.
/// С подключениями — короткое подтверждение (туннели будут отключены); ошибка (нет файлов движка) — всегда.
fn mode_switch_silent(help_hidden: bool, running: bool, problem: bool) -> bool {
    help_hidden && !running && !problem
}

/// «Импортировано: N» + какие уже были, какие с неподходящим именем и какие отклонены из-за команд.
fn report_text(r: &store::ImportReport) -> String {
    let mut text = trf("eng.imported", &[&r.added.len().to_string()]);
    if !r.existing.is_empty() {
        text += " · ";
        text += &trf("eng.import_existing", &[&r.existing.join(", ")]);
    }
    if !r.bad_name.is_empty() {
        text += " · ";
        text += &trf("eng.import_bad_name", &[&r.bad_name.join(", ")]);
    }
    if !r.scripts.is_empty() {
        text += " · ";
        text += &trf("eng.import_scripts", &[&r.scripts.join(", ")]);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remembered_mode_help_skips_the_dialog_only_when_nothing_else_to_say() {
        assert!(mode_switch_silent(true, false, false));
        assert!(!mode_switch_silent(false, false, false), "справка не скрыта");
        assert!(!mode_switch_silent(true, true, false), "туннели будут отключены — спросить");
        assert!(!mode_switch_silent(true, false, true), "нет файлов движка — показать ошибку");
    }

    #[test]
    fn failed_info_fetch_releases_the_tunnel_for_a_retry() {
        let (infos, loading) = (Mutex::new(BTreeMap::new()), Mutex::new(BTreeSet::from(["t".to_string()])));
        let r = fetch_store_info("t", |_| Err("pipe busy".into()), &infos, &loading, Duration::ZERO);
        assert_eq!(r, Err("pipe busy".to_string()));
        assert!(loading.lock().unwrap().is_empty(), "имя осталось в info_loading — сведения больше не запросятся");
        assert!(infos.lock().unwrap().is_empty());
    }

    #[test]
    fn successful_info_fetch_fills_the_cache() {
        let (infos, loading) = (Mutex::new(BTreeMap::new()), Mutex::new(BTreeSet::from(["t".to_string()])));
        fetch_store_info("t", |_| Ok(TunnelInfo::default()), &infos, &loading, Duration::ZERO).unwrap();
        assert!(loading.lock().unwrap().is_empty());
        assert!(infos.lock().unwrap().contains_key("t"));
    }
}
