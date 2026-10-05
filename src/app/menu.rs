//! Строка меню окна.

use eframe::egui::{self, Ui};

use crate::i18n::{self, tr, trf};
use crate::settings::{Mode, Settings};

use super::{updates, Action, Confirm};

/// Масштабы в меню «Вид»; Ctrl+Plus/Minus даёт и промежуточные.
const SCALES: [f32; 8] = [0.8, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0];

/// `updates_new` — у компонента есть обновление: точка у «Справки» и пометка у пункта проверки.
pub(super) fn menu_bar(ui: &mut Ui, s: &mut Settings, autostart: Option<bool>, lang_dir: &std::path::Path, updates_new: bool, actions: &mut Vec<Action>) {
    egui::menu::bar(ui, |ui| {
        let engine = s.mode() == Mode::Engine;
        ui.menu_button(tr("menu.file"), |ui| {
            let mut item = |ui: &mut Ui, key: &str, action: Action| {
                if ui.button(tr(key)).clicked() {
                    actions.push(action);
                    ui.close_menu();
                }
            };
            if engine {
                item(ui, "eng.new", Action::EngineNew);
                item(ui, "eng.import", Action::EngineImport);
                if crate::backend::native_exe().exists() {
                    item(ui, "eng.take_native", Action::EngineTakeNative);
                }
                ui.separator();
                item(ui, "eng.backup", Action::EngineBackup);
                item(ui, "eng.restore", Action::EngineRestore);
                ui.separator();
            }
            item(ui, "file.open_conf", Action::OpenConf);
            ui.separator();
            item(ui, "file.exit", Action::Exit);
            if !engine {
                item(ui, "file.exit_native", Action::ExitWithNative);
            }
        });
        ui.menu_button(tr("menu.view"), |ui| {
            let v = &mut s.view;
            ui.checkbox(&mut v.groups, tr("view.groups"));
            ui.checkbox(&mut v.search, tr("view.search"));
            ui.separator();
            ui.weak(tr("view.columns"));
            ui.checkbox(&mut v.col_rx, tr("view.col_rx"));
            ui.checkbox(&mut v.col_tx, tr("view.col_tx"));
            ui.checkbox(&mut v.col_peak, tr("view.col_peak"));
            ui.checkbox(&mut v.col_share, tr("view.col_share"));
            ui.separator();
            ui.weak(tr("view.right"));
            ui.checkbox(&mut v.totals, tr("view.totals"));
            ui.checkbox(&mut v.graph, tr("view.graph"));
            ui.checkbox(&mut v.ping, tr("view.ping"));
            ui.checkbox(&mut v.reconnect, tr("view.reconnect"));
            ui.checkbox(&mut v.details, tr("view.details"));
            ui.separator();
            ui.checkbox(&mut v.log, tr("view.log"));
            ui.separator();
            ui.menu_button(trf("view.scale", &[&format!("{:.0}", s.ui_scale * 100.0)]), |ui| {
                for scale in SCALES {
                    if ui.radio((s.ui_scale - scale).abs() < 0.01, format!("{:.0} %", scale * 100.0)).clicked() {
                        s.ui_scale = scale;
                        ui.close_menu();
                    }
                }
                ui.separator();
                ui.weak(tr("view.scale_keys"));
            });
        });
        ui.menu_button(tr("menu.settings"), |ui| {
            ui.checkbox(&mut s.multiple, tr("set.multiple"));
            ui.checkbox(&mut s.tray, tr("set.tray"));
            ui.add_enabled_ui(s.tray, |ui| {
                ui.checkbox(&mut s.notify, tr("set.notify"));
                ui.checkbox(&mut s.close_to_tray, tr("set.close_to_tray"));
            });
            ui.checkbox(&mut s.taskbar, tr("set.taskbar"));
            if let Some(on) = autostart {
                let mut want = on;
                if ui.checkbox(&mut want, tr("set.autostart")).changed() {
                    actions.push(Action::Autostart(want));
                }
            }
            ui.horizontal(|ui| {
                ui.label(tr("set.ping_to"));
                ui.add(egui::TextEdit::singleline(&mut s.ping_host).desired_width(140.0));
            });
            if ui.add_enabled(!s.hidden_dialogs.is_empty(), egui::Button::new(tr("set.show_hidden"))).clicked() {
                s.hidden_dialogs.clear();
                ui.close_menu();
            }
            ui.separator();
            ui.menu_button(tr("set.mode"), |ui| {
                for (mode, key) in [(Mode::Overlay, "mode.overlay"), (Mode::Engine, "mode.engine")] {
                    if ui.radio(s.mode() == mode, tr(key)).clicked() {
                        actions.push(Action::ChooseMode(mode));
                        ui.close_menu();
                    }
                }
                // Режим 2: забрать туннели из оригинала — только по команде, когда нужно (своя копия правится отдельно).
                if engine && crate::backend::native_exe().exists() {
                    ui.separator();
                    if ui.button(tr("eng.take_native")).on_hover_text(tr("eng.take_native_hint")).clicked() {
                        actions.push(Action::EngineTakeNative);
                        ui.close_menu();
                    }
                }
            });
            ui.menu_button(tr("core.menu"), |ui| {
                if ui.button(tr("core.reinstall")).on_hover_text(tr("core.uac_hint")).clicked() {
                    actions.push(Action::InstallCore);
                    ui.close_menu();
                }
                if ui.button(tr("core.uninstall_title")).clicked() {
                    actions.push(Action::Confirm(Confirm::UninstallCore));
                    ui.close_menu();
                }
            });
            ui.separator();
            if ui.button(tr("set.shortcut")).clicked() {
                actions.push(Action::DesktopShortcut);
                ui.close_menu();
            }
            if !engine && ui.button(tr("set.original")).clicked() {
                actions.push(Action::OpenOriginal);
                ui.close_menu();
            }
        });
        ui.menu_button(tr("menu.language"), |ui| {
            let current = i18n::current_code();
            for (code, name) in i18n::available(lang_dir) {
                if ui.radio(code == current, format!("{name} ({code})")).clicked() {
                    actions.push(Action::Language(code));
                    ui.close_menu();
                }
            }
            ui.separator();
            if ui.button(tr("lang.add")).clicked() {
                actions.push(Action::AddLanguage);
                ui.close_menu();
            }
            if ui.button(tr("lang.folder")).clicked() {
                actions.push(Action::OpenLangFolder);
                ui.close_menu();
            }
        });
        let help = updates::menu_title(ui, tr("menu.help"), updates_new);
        ui.menu_button(help, |ui| {
            if ui.button(tr(if updates_new { "upd.menu_new" } else { "upd.menu" })).clicked() {
                actions.push(Action::CheckUpdates);
                ui.close_menu();
            }
            ui.separator();
            if ui.button(tr("help.about")).clicked() {
                actions.push(Action::About);
                ui.close_menu();
            }
        });
    });
}
