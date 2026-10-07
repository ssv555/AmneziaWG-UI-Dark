//! Выход из программы. VPN живёт в службе ядра и без окна, поэтому при подключённых туннелях спрашиваем: отключить
//! их или оставить. Выбор можно запомнить («Настройки → Снова показывать скрытые диалоги» его сбрасывает).

use std::collections::BTreeSet;
use std::sync::atomic::Ordering;

use eframe::egui;

use super::dialog::{dialog_body, dialog_choice};

use super::modals::{Modal, Outcome, Turn};
use super::App;
use crate::daemon::CoreApi;
use crate::daemon::proto::{NativeOp, Plan, Request};
use crate::i18n::{tr, trf};
use crate::settings::DialogId;
use crate::tray;

impl App {
    /// Выход по меню, из трея или крестиком окна; `native` — закрыть и родное окно AmneziaWG.
    pub(super) fn request_exit(&mut self, native: bool) {
        let running = self.running_tunnels();
        match exit_plan(!running.is_empty(), &self.s.hidden_dialogs) {
            ExitPlan::Now => self.exit_now(native),
            ExitPlan::Disconnect => self.disconnect_and_exit(running, native),
            ExitPlan::Ask => {
                self.modals.open(Modal::Exit { native });
                tray::show_window();
            }
        }
    }

    /// «Выход» из меню трея приходит из оконной процедуры — разбираем его в кадре.
    pub(super) fn check_exit_request(&mut self) {
        if self.exit_request.swap(false, Ordering::SeqCst) {
            self.request_exit(false);
        }
    }

    pub(super) fn show_exit_confirm(&mut self, ctx: &egui::Context, native: bool, mut turn: Turn) -> Outcome {
        let running = self.running_tunnels();
        // Туннели успели отключиться сами — спрашивать не о чем.
        if running.is_empty() {
            self.exit_now(native);
            return Outcome::Close;
        }
        let (mut disconnect, mut keep, mut cancel) = (false, false, false);
        let mut open = true;
        turn.window(ctx, tr("exit.title"), "exit-confirm", &mut open)
            .show(ctx, |ui| {
                dialog_body(ui, 520.0, |ui| {
                    ui.add(egui::Label::new(trf("exit.text", &[&running.join(", ")])).wrap());
                    ui.add_space(10.0);
                    ui.checkbox(turn.remember, tr("dlg.remember_choice"));
                });
                ui.add_space(6.0);
                (disconnect, keep, cancel) = dialog_choice(ui, &tr("exit.disconnect"), &tr("exit.keep"), &tr("btn.cancel"));
            });
        let (enter, escape) = turn.keys(ctx);
        if disconnect || keep || enter {
            if *turn.remember {
                self.s.hidden_dialogs.insert(if keep { DialogId::ExitKeep } else { DialogId::ExitDisconnect });
            }
            if keep {
                self.exit_now(native);
            } else {
                self.disconnect_and_exit(running, native);
            }
            Outcome::Close
        } else if cancel || escape || !open {
            Outcome::Close
        } else {
            Outcome::Keep
        }
    }

    pub(super) fn exit_now(&mut self, native: bool) {
        self.save_before_exit();
        finish_exit(self.core.as_ref(), native, &self.action_error);
    }

    /// Отключить туннели через ядро и выйти; не вышло — окно остаётся, ошибка в журнале.
    fn disconnect_and_exit(&mut self, running: Vec<String>, native: bool) {
        self.save_before_exit();
        self.notice.progress(tr("exit.disconnecting"));
        let (core, error, notice, ctx) = (self.core.clone(), self.action_error.clone(), self.notice.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            match running.iter().try_for_each(|t| core.ok(Request::Switch { tunnel: t.clone(), plan: Plan::Disconnect, multiple: true })) {
                Ok(()) => finish_exit(core.as_ref(), native, &error),
                Err(e) => {
                    notice.clear();
                    error.push(e);
                    ctx.request_repaint();
                }
            }
        });
    }

    /// Подключённые и те, что ядро переподключает: «выйти с отключением» снимает и желание держать их подключёнными,
    /// иначе ядро продолжало бы поднимать их без окна.
    fn running_tunnels(&self) -> Vec<String> {
        let mut names = self.shared.running_names();
        let retrying: Vec<String> = self.shared.retrying_names().into_iter().filter(|t| !names.contains(t)).collect();
        names.extend(retrying);
        names
    }

    /// Настройки — на диск перед выходом. Сбой записи настроек уходит в журнал окна: `ErrorSink` пишет
    /// и в `window-errors.log` сразу, так что запись переживёт выход.
    pub(super) fn save_before_exit(&self) {
        if let Err(e) = self.s.to_ini().save(self.window.settings_path()) {
            self.action_error.push(crate::fsutil::io_ctx(self.window.settings_path(), e));
        }
    }
}

#[derive(PartialEq, Eq, Debug)]
enum ExitPlan {
    Now,
    Disconnect,
    Ask,
}

/// Без подключённых туннелей — выйти сразу; запомненный выбор — выполнить его без диалога; иначе спросить.
fn exit_plan(running: bool, hidden: &BTreeSet<DialogId>) -> ExitPlan {
    if !running || hidden.contains(&DialogId::ExitKeep) {
        ExitPlan::Now
    } else if hidden.contains(&DialogId::ExitDisconnect) {
        ExitPlan::Disconnect
    } else {
        ExitPlan::Ask
    }
}

fn finish_exit(core: &dyn CoreApi, native: bool, error: &super::errors::ErrorSink) {
    // Родное окно запущено с правами администратора — закрывает его помощник ядра.
    if native {
        if let Err(e) = core.native(NativeOp::Close) {
            error.push(e);
        }
    }
    tray::remove();
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remembered_exit_choice_skips_the_dialog() {
        let none = BTreeSet::new();
        assert_eq!(exit_plan(true, &none), ExitPlan::Ask);
        assert_eq!(exit_plan(false, &none), ExitPlan::Now);
        assert_eq!(exit_plan(true, &BTreeSet::from([DialogId::ExitKeep])), ExitPlan::Now);
        assert_eq!(exit_plan(true, &BTreeSet::from([DialogId::ExitDisconnect])), ExitPlan::Disconnect);
        // Справка о режиме к выходу отношения не имеет.
        assert_eq!(exit_plan(true, &BTreeSet::from([DialogId::ModeEngine])), ExitPlan::Ask);
    }
}
