//! Строка меню окна.

use eframe::egui::{self, Ui};

use crate::i18n::{self, tr, trf};
use crate::settings::{Mode, Settings};

use super::{updates, Action, Confirm};

/// Масштабы в меню «Вид»; Ctrl+Plus/Minus даёт и промежуточные.
const SCALES: [f32; 8] = [0.8, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0];

/// Меню закрывается щелчком мимо него или явным `ui.close()` у пункта-команды. С egui 0.32 по умолчанию
/// меню закрывает любой щелчок внутри — тогда «Вид» с флажками закрывался бы на каждом флажке.
const CLOSE: egui::PopupCloseBehavior = egui::PopupCloseBehavior::CloseOnClickOutside;

/// Контекстное меню элемента: правый щелчок, а для элемента под фокусом клавиатуры (`keyboard`) — ещё Shift+F10
/// и клавиша меню (её `MenuKey` превращает в Shift+F10), как в Windows. Открытое с клавиатуры меню стоит под
/// элементом, а не у указателя мыши. Подменю наследуют то же правило закрытия. Единственный путь к контекстному меню
/// в окне: тест `context_menus_go_through_the_helper` не пускает правый щелчок в обход него.
pub(super) fn context_menu(resp: &egui::Response, keyboard: bool, add_contents: impl FnOnce(&mut Ui)) {
    let id = egui::Popup::default_response_id(resp);
    let by_key = id.with("by-key");
    if keyboard && resp.ctx.input_mut(|i| i.consume_key(egui::Modifiers::SHIFT, egui::Key::F10)) {
        egui::Popup::open_id(&resp.ctx, id);
        resp.ctx.data_mut(|d| d.insert_temp(by_key, true));
    } else if resp.secondary_clicked() {
        resp.ctx.data_mut(|d| d.remove::<bool>(by_key));
    }
    let popup = egui::Popup::context_menu(resp).close_behavior(CLOSE);
    // Под элементом в координатах экрана (с преобразованием слоя, как у самого egui), а не у указателя.
    let under = || egui::PopupAnchor::from(resp).rect(id, &resp.ctx).unwrap_or(resp.rect).left_bottom();
    let popup = if resp.ctx.data(|d| d.get_temp::<bool>(by_key)).unwrap_or(false) { popup.at_position(under()) } else { popup };
    popup.show(add_contents);
}

/// Клавиша меню (Apps) для egui: egui-winit её не переводит, и событие теряется. Каждое нажатие переключает младший
/// бит `GetKeyState(VK_APPS)` — по его смене кадр узнаёт о нажатии, даже если нажатие и отпускание пришли между
/// кадрами, — и добавляет в ввод Shift+F10, равнозначное ей в Windows.
#[derive(Default)]
pub(super) struct MenuKey {
    /// Бит переключения на прошлом кадре; `None` — точки отсчёта нет (первый кадр, окно без фокуса).
    toggle: Option<bool>,
}

impl MenuKey {
    /// Зовётся из `raw_input_hook` каждый кадр.
    pub(super) fn hook(&mut self, raw: &mut egui::RawInput) {
        self.step(raw, apps_key_toggle());
    }

    /// Без фокуса окна нажатия уходят другим программам, а при возврате фокуса Windows может сверить состояние
    /// клавиш — точка отсчёта берётся заново, без ложного открытия меню.
    fn step(&mut self, raw: &mut egui::RawInput, toggle: bool) {
        let refocused = raw.events.iter().any(|e| matches!(e, egui::Event::WindowFocused(true)));
        let pressed = raw.focused && !refocused && self.toggle.is_some_and(|t| t != toggle);
        self.toggle = raw.focused.then_some(toggle);
        if pressed {
            let modifiers = egui::Modifiers::SHIFT;
            raw.events.push(egui::Event::Key { key: egui::Key::F10, physical_key: None, pressed: true, repeat: false, modifiers });
        }
    }
}

fn apps_key_toggle() -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_APPS};
    // SAFETY: GetKeyState только читает состояние клавиатуры потока и указателей не принимает.
    unsafe { GetKeyState(VK_APPS as i32) & 1 != 0 }
}

/// `updates_new` — у компонента есть обновление: точка у «Справки» и пометка у пункта проверки.
pub(super) fn menu_bar(ui: &mut Ui, s: &mut Settings, autostart: Option<bool>, lang_dir: &std::path::Path, updates_new: bool, actions: &mut Vec<Action>) {
    egui::MenuBar::new().config(egui::containers::menu::MenuConfig::new().close_behavior(CLOSE)).ui(ui, |ui| {
        let engine = s.mode() == Mode::Engine;
        ui.menu_button(tr("menu.file"), |ui| {
            let mut item = |ui: &mut Ui, key: &str, action: Action| {
                if ui.button(tr(key)).clicked() {
                    actions.push(action);
                    ui.close();
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
                        ui.close();
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
                ui.close();
            }
            ui.separator();
            ui.menu_button(tr("set.mode"), |ui| {
                for (mode, key) in [(Mode::Overlay, "mode.overlay"), (Mode::Engine, "mode.engine")] {
                    if ui.radio(s.mode() == mode, tr(key)).clicked() {
                        actions.push(Action::ChooseMode(mode));
                        ui.close();
                    }
                }
                // Режим 2: забрать туннели из оригинала — только по команде, когда нужно (своя копия правится отдельно).
                if engine && crate::backend::native_exe().exists() {
                    ui.separator();
                    if ui.button(tr("eng.take_native")).on_hover_text(tr("eng.take_native_hint")).clicked() {
                        actions.push(Action::EngineTakeNative);
                        ui.close();
                    }
                }
            });
            ui.menu_button(tr("core.menu"), |ui| {
                if ui.button(tr("core.reinstall")).on_hover_text(tr("core.uac_hint")).clicked() {
                    actions.push(Action::InstallCore);
                    ui.close();
                }
                if ui.button(tr("core.uninstall_title")).clicked() {
                    actions.push(Action::Confirm(Confirm::UninstallCore));
                    ui.close();
                }
            });
            ui.separator();
            if ui.button(tr("set.shortcut")).clicked() {
                actions.push(Action::DesktopShortcut);
                ui.close();
            }
            if !engine && ui.button(tr("set.original")).clicked() {
                actions.push(Action::OpenOriginal);
                ui.close();
            }
        });
        ui.menu_button(tr("menu.language"), |ui| {
            let current = i18n::current_code();
            for (code, name) in i18n::available(lang_dir) {
                if ui.radio(code == current, format!("{name} ({code})")).clicked() {
                    actions.push(Action::Language(code));
                    ui.close();
                }
            }
            ui.separator();
            if ui.button(tr("lang.add")).clicked() {
                actions.push(Action::AddLanguage);
                ui.close();
            }
            if ui.button(tr("lang.folder")).clicked() {
                actions.push(Action::OpenLangFolder);
                ui.close();
            }
        });
        let help = updates::menu_title(ui, tr("menu.help"), updates_new);
        ui.menu_button(help, |ui| {
            if ui.button(tr(if updates_new { "upd.menu_new" } else { "upd.menu" })).clicked() {
                actions.push(Action::CheckUpdates);
                ui.close();
            }
            if ui.button(tr("help.diag")).on_hover_text(tr("help.diag_hint")).clicked() {
                actions.push(Action::CopyDiagnostics);
                ui.close();
            }
            ui.separator();
            if ui.button(tr("help.about")).clicked() {
                actions.push(Action::About);
                ui.close();
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(focused: bool, events: Vec<egui::Event>) -> egui::RawInput {
        egui::RawInput { focused, events, ..Default::default() }
    }

    fn opens_menu(raw: &egui::RawInput) -> bool {
        raw.events.iter().any(|e| matches!(e, egui::Event::Key { key: egui::Key::F10, pressed: true, modifiers, .. } if modifiers.shift))
    }

    #[test]
    fn menu_key_press_becomes_shift_f10() {
        let mut k = MenuKey::default();
        let mut raw = input(true, vec![]);
        k.step(&mut raw, false);
        assert!(!opens_menu(&raw), "первый кадр — точка отсчёта");
        for toggle in [true, false] {
            let mut raw = input(true, vec![]);
            k.step(&mut raw, toggle);
            assert!(opens_menu(&raw), "каждое нажатие меняет бит");
        }
        let mut raw = input(true, vec![]);
        k.step(&mut raw, false);
        assert!(!opens_menu(&raw), "бит не менялся — нажатия не было");
    }

    #[test]
    fn menu_key_outside_the_window_is_ignored() {
        let mut k = MenuKey::default();
        k.step(&mut input(true, vec![]), false);
        let mut away = input(false, vec![]);
        k.step(&mut away, true);
        assert!(!opens_menu(&away), "окно без фокуса");
        let mut back = input(true, vec![egui::Event::WindowFocused(true)]);
        k.step(&mut back, false);
        assert!(!opens_menu(&back), "возврат фокуса — новая точка отсчёта");
        let mut raw = input(true, vec![]);
        k.step(&mut raw, true);
        assert!(opens_menu(&raw));
    }

    /// Правило 5 стандарта: контекстное меню открывается и с клавиатуры, поэтому в src/app оно строится только через
    /// `context_menu` (там Shift+F10 и клавиша меню). Правый щелчок напрямую (`Popup::context_menu`,
    /// `Response::context_menu`, `secondary_clicked`) вне этого файла — ошибка. Иглы собраны из частей.
    #[test]
    fn context_menus_go_through_the_helper() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join("app");
        let mut files = Vec::new();
        super::super::dialog::tests::collect_rs(&root, &mut files);
        assert!(files.len() > 10, "src/app not scanned: {}", root.display());
        let own = root.join("menu.rs");
        let banned = [concat!("Popup::context", "_menu("), concat!(".context", "_menu("), concat!("secondary", "_clicked(")];
        let mut bad = Vec::new();
        for path in files.iter().filter(|p| **p != own) {
            let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            for (n, line) in text.lines().enumerate() {
                for b in banned.iter().filter(|b| line.contains(*b)) {
                    bad.push(format!("{}:{}: {b}", path.display(), n + 1));
                }
            }
        }
        assert!(bad.is_empty(), "context menu outside menu::context_menu:\n{}", bad.join("\n"));
    }
}
