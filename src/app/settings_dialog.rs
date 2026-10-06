//! Окно «Настройки»: все параметры программы в одном окне с разделами вместо флажков в выпадающем меню.
//!
//! Окно правит черновик (`Choices`), а не `Settings`: «Отмена», крестик и Esc его просто бросают, «ОК» и «Применить»
//! переносят всё разом и только с верным узлом пинга — наполовину применённых настроек не бывает. Дальше значения идут
//! прежними путями: `push_options` (агенту — `SetPing`, трей), `Action::Autostart`, `Action::ChooseMode` со своим
//! окном подтверждения смены режима.

use eframe::egui::{self, RichText};

use crate::i18n::tr;
use crate::settings::{Mode, Settings};

use super::dialog::{dialog_buttons, dialog_ok_cancel_apply, dialog_window};
use super::modals::{Outcome, Turn};
use super::theme::{RED, YELLOW};
use super::{Action, App, Modal};

/// Значения параметров в окне — черновик до «ОК» или «Применить».
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Choices {
    multiple: bool,
    tray: bool,
    notify: bool,
    close_to_tray: bool,
    taskbar: bool,
    /// `None` — состояние автозапуска неизвестно (демо-режим): флажка нет.
    autostart: Option<bool>,
    ping_host: String,
    /// Вернуть диалоги, отмеченные «Больше не показывать».
    restore_dialogs: bool,
    mode: Mode,
}

impl Choices {
    pub(super) fn read(s: &Settings, autostart: Option<bool>) -> Choices {
        Choices {
            multiple: s.multiple,
            tray: s.tray,
            notify: s.notify,
            close_to_tray: s.close_to_tray,
            taskbar: s.taskbar,
            autostart,
            ping_host: s.ping_host.clone(),
            restore_dialogs: false,
            mode: s.mode(),
        }
    }

    /// Значения по умолчанию из `Settings::default`. Режим и автозапуск остаются как есть: режим меняется только
    /// через своё подтверждение с перезапуском туннелей, автозапуск — запись в реестре Windows, а не параметр окна.
    fn defaults(&self) -> Choices {
        let d = Settings::default();
        Choices { restore_dialogs: true, autostart: self.autostart, mode: self.mode, ..Choices::read(&d, None) }
    }

    fn ping_host_ok(&self) -> bool {
        crate::ping::valid_host(self.ping_host.trim())
    }

    /// Всё в `Settings` разом или ничего (неверный узел пинга — `false`, `s` не тронут). Автозапуск и режим —
    /// действиями, как раньше из меню: у них свои пути (реестр, окно смены режима).
    fn apply(&self, s: &mut Settings, autostart: Option<bool>, actions: &mut Vec<Action>) -> bool {
        if !self.ping_host_ok() {
            return false;
        }
        s.multiple = self.multiple;
        s.tray = self.tray;
        s.notify = self.notify;
        s.close_to_tray = self.close_to_tray;
        s.taskbar = self.taskbar;
        s.ping_host = self.ping_host.trim().to_string();
        if self.restore_dialogs {
            s.hidden_dialogs.clear();
        }
        if let (Some(want), Some(now)) = (self.autostart, autostart) {
            if want != now {
                actions.push(Action::Autostart(want));
            }
        }
        if self.mode != s.mode() {
            actions.push(Action::ChooseMode(self.mode));
        }
        true
    }
}

/// Кнопка окна (или её клавиша).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Press {
    Ok,
    Apply,
    Cancel,
}

/// Открытое окно «Настройки»: черновик и вопрос «Сбросить?» поверх него.
pub(super) struct SettingsDialog {
    choices: Choices,
    confirm_reset: bool,
}

impl SettingsDialog {
    pub(super) fn new(s: &Settings, autostart: Option<bool>) -> SettingsDialog {
        SettingsDialog { choices: Choices::read(s, autostart), confirm_reset: false }
    }

    /// Решение по кнопке. «Применить» оставляет окно открытым: применённое становится новой точкой отсчёта, флажок
    /// «Снова показывать скрытые диалоги» снимается (уже сделано). При неверном узле окно остаётся, ничего не меняется.
    pub(super) fn press(&mut self, press: Press, s: &mut Settings, autostart: Option<bool>, actions: &mut Vec<Action>) -> Outcome {
        match press {
            Press::Cancel => Outcome::Close,
            Press::Ok if self.choices.apply(s, autostart, actions) => Outcome::Close,
            Press::Apply if self.choices.apply(s, autostart, actions) => {
                self.choices.restore_dialogs = false;
                Outcome::Keep
            }
            Press::Ok | Press::Apply => Outcome::Keep,
        }
    }

    /// Черновик изменён относительно `Settings` — «Применить» доступна.
    fn changed(&self, s: &Settings, autostart: Option<bool>) -> bool {
        self.choices != Choices::read(s, autostart)
    }
}

impl App {
    pub(super) fn open_settings(&mut self) {
        self.modals.open(Modal::Settings(Box::new(SettingsDialog::new(&self.s, self.autostart))));
    }

    pub(super) fn show_settings(&mut self, ctx: &egui::Context, dlg: &mut SettingsDialog, turn: Turn) -> Outcome {
        let (enter, escape) = turn.keys(ctx);
        if dlg.confirm_reset {
            return self.show_reset_confirm(ctx, dlg, enter, escape);
        }
        let hidden = self.s.hidden_dialogs.len();
        let changed = dlg.changed(&self.s, self.autostart);
        let mut open = true;
        let (mut ok, mut apply, mut cancel, mut reset) = (false, false, false, false);
        dialog_window(ctx, tr("set.title"), "settings", &mut open).show(ctx, |ui| {
            ui.set_width(460.0);
            sections(ui, &mut dlg.choices, hidden);
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                reset = ui.button(tr("set.reset")).clicked();
            });
            ui.add_space(6.0);
            let valid = dlg.choices.ping_host_ok();
            (ok, cancel, apply) = dialog_ok_cancel_apply(ui, (&tr("btn.ok"), valid), &tr("btn.cancel"), (&tr("set.apply"), valid && changed));
        });
        if reset {
            dlg.confirm_reset = true;
            return Outcome::Keep;
        }
        let press = if ok || enter {
            Press::Ok
        } else if apply {
            Press::Apply
        } else if cancel || escape || !open {
            Press::Cancel
        } else {
            return Outcome::Keep;
        };
        let mut actions = Vec::new();
        let outcome = dlg.press(press, &mut self.s, self.autostart, &mut actions);
        for action in actions {
            self.apply(action);
        }
        outcome
    }

    /// «Сбросить к значениям по умолчанию?» поверх окна настроек: сбрасывается черновик, применяют его «ОК»/«Применить».
    fn show_reset_confirm(&mut self, ctx: &egui::Context, dlg: &mut SettingsDialog, enter: bool, escape: bool) -> Outcome {
        let mut open = true;
        let (mut yes, mut no) = (false, false);
        dialog_window(ctx, tr("set.reset_title"), "settings-reset", &mut open).order(egui::Order::Foreground).show(ctx, |ui| {
            ui.set_width(420.0);
            ui.add(egui::Label::new(tr("set.reset_text")).wrap());
            ui.add_space(10.0);
            (yes, no) = dialog_buttons(ui, &tr("set.reset"), true, Some(&tr("btn.cancel")));
        });
        if yes || enter {
            dlg.choices = dlg.choices.defaults();
        }
        if yes || enter || no || escape || !open {
            dlg.confirm_reset = false;
        }
        Outcome::Keep
    }
}

/// Разделы окна: Общие, Уведомления и трей, Сеть, Режим работы.
fn sections(ui: &mut egui::Ui, c: &mut Choices, hidden: usize) {
    section(ui, "set.sec_general");
    ui.checkbox(&mut c.multiple, tr("set.multiple"));
    ui.checkbox(&mut c.taskbar, tr("set.taskbar"));
    if let Some(on) = &mut c.autostart {
        ui.checkbox(on, tr("set.autostart"));
    }
    ui.add_enabled(hidden > 0 || c.restore_dialogs, egui::Checkbox::new(&mut c.restore_dialogs, tr("set.show_hidden")));

    section(ui, "set.sec_notify");
    ui.checkbox(&mut c.tray, tr("set.tray"));
    ui.add_enabled_ui(c.tray, |ui| {
        ui.checkbox(&mut c.notify, tr("set.notify"));
        ui.checkbox(&mut c.close_to_tray, tr("set.close_to_tray"));
    });

    section(ui, "set.sec_network");
    ui.horizontal(|ui| {
        ui.label(tr("set.ping_to"));
        ui.add(egui::TextEdit::singleline(&mut c.ping_host).desired_width(220.0));
    });
    if !c.ping_host_ok() {
        ui.add(egui::Label::new(RichText::new(tr("set.ping_bad")).color(RED)).wrap());
    }

    section(ui, "set.mode");
    for (mode, key) in [(Mode::Overlay, "mode.overlay"), (Mode::Engine, "mode.engine")] {
        ui.radio_value(&mut c.mode, mode, tr(key));
    }
    ui.add(egui::Label::new(RichText::new(tr("set.mode_note")).color(YELLOW).small()).wrap());
}

fn section(ui: &mut egui::Ui, key: &str) {
    ui.add_space(8.0);
    ui.label(RichText::new(tr(key)).strong());
    ui.separator();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::DialogId;

    fn edited(s: &Settings) -> SettingsDialog {
        let mut d = SettingsDialog::new(s, Some(false));
        let c = &mut d.choices;
        c.multiple = !c.multiple;
        c.tray = !c.tray;
        c.notify = !c.notify;
        c.close_to_tray = !c.close_to_tray;
        c.taskbar = !c.taskbar;
        c.autostart = Some(true);
        c.ping_host = " 9.9.9.9 ".into();
        c.restore_dialogs = true;
        c.mode = Mode::Engine;
        d
    }

    fn with_hidden() -> Settings {
        let mut s = Settings::default();
        s.hidden_dialogs.insert(DialogId::Disconnect);
        s
    }

    #[test]
    fn ok_applies_every_option_at_once() {
        let mut s = with_hidden();
        let before = s.clone();
        let mut d = edited(&s);
        let mut actions = Vec::new();
        assert_eq!(d.press(Press::Ok, &mut s, Some(false), &mut actions), Outcome::Close);
        assert_eq!(
            (s.multiple, s.tray, s.notify, s.close_to_tray, s.taskbar),
            (!before.multiple, !before.tray, !before.notify, !before.close_to_tray, !before.taskbar)
        );
        assert_eq!(s.ping_host, "9.9.9.9");
        assert!(s.hidden_dialogs.is_empty());
        // Автозапуск и режим — прежними путями: действие автозапуска и окно подтверждения смены режима.
        assert_eq!(actions.len(), 2);
        assert!(matches!(actions[0], Action::Autostart(true)));
        assert!(matches!(actions[1], Action::ChooseMode(Mode::Engine)));
        assert_eq!(s.mode(), Mode::Overlay, "the mode changes only after its own confirmation");
    }

    #[test]
    fn cancel_applies_nothing() {
        let mut s = with_hidden();
        let before = s.to_ini().to_text();
        let mut d = edited(&s);
        let mut actions = Vec::new();
        assert_eq!(d.press(Press::Cancel, &mut s, Some(false), &mut actions), Outcome::Close);
        assert_eq!(s.to_ini().to_text(), before);
        assert!(actions.is_empty());
    }

    #[test]
    fn invalid_ping_host_blocks_ok_and_apply_without_partial_changes() {
        let mut s = with_hidden();
        let before = s.to_ini().to_text();
        let mut d = edited(&s);
        d.choices.ping_host = "not a host".into();
        let mut actions = Vec::new();
        assert_eq!(d.press(Press::Ok, &mut s, Some(false), &mut actions), Outcome::Keep);
        assert_eq!(d.press(Press::Apply, &mut s, Some(false), &mut actions), Outcome::Keep);
        assert_eq!(s.to_ini().to_text(), before);
        assert!(actions.is_empty());
    }

    #[test]
    fn apply_keeps_the_window_and_then_cancel_keeps_what_was_applied() {
        let mut s = Settings::default();
        let mut d = SettingsDialog::new(&s, None);
        d.choices.multiple = true;
        let mut actions = Vec::new();
        assert_eq!(d.press(Press::Apply, &mut s, None, &mut actions), Outcome::Keep);
        assert!(s.multiple);
        assert!(!d.changed(&s, None));
        d.choices.tray = false;
        assert_eq!(d.press(Press::Cancel, &mut s, None, &mut actions), Outcome::Close);
        assert!(s.multiple && s.tray);
        assert!(actions.is_empty(), "autostart unknown (demo) and mode unchanged: no actions");
    }

    #[test]
    fn reset_restores_defaults_but_keeps_mode_and_autostart() {
        let mut s = with_hidden();
        s.multiple = true;
        s.ping_host = "9.9.9.9".into();
        let mut d = SettingsDialog::new(&s, Some(true));
        d.choices.mode = Mode::Engine;
        let reset = d.choices.defaults();
        let def = Choices::read(&Settings::default(), Some(true));
        assert_eq!(reset, Choices { restore_dialogs: true, mode: Mode::Engine, ..def });
    }
}
