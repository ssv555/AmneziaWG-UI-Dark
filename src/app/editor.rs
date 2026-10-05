//! Редактор исходного .conf пользователя.

use std::path::PathBuf;

use eframe::egui;

use crate::i18n::{tr, trf};
use crate::settings::Mode;
use crate::win;

use super::dialog::{dialog_choice, dialog_window, window_escape};
use super::modals::{Modal, Outcome, Turn};
use super::theme::{GREEN, RED, YELLOW};
use super::watcher::Infos;
use super::App;
use crate::daemon::CoreApi;

/// Окно редактора: `id` и для позиции, и для Esc верхнему окну.
const EDITOR_ID: &str = "conf-editor";

/// Открытый в редакторе файл .conf пользователя.
pub(super) struct Editor {
    pub(super) path: PathBuf,
    pub(super) text: String,
    /// Содержимое на диске — для признака «изменён».
    pub(super) saved: String,
    /// Сообщение под кнопками: текст и признак ошибки.
    pub(super) note: Option<(String, bool)>,
    /// Туннель хранилища встроенного движка: сохранение идёт в хранилище, а не в `path`.
    pub(super) tunnel: Option<String>,
}

impl Editor {
    pub(super) fn new(path: PathBuf, text: String, tunnel: Option<String>) -> Self {
        Editor { path, saved: text.clone(), text, note: None, tunnel }
    }

    fn dirty(&self) -> bool {
        self.text != self.saved
    }

    fn file_name(&self) -> String {
        self.path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default()
    }

    /// Записать текст туда, откуда он: в хранилище ядра или в файл. Итог — в строке под кнопками; `true` — записан.
    fn save(&mut self, core: &dyn CoreApi, infos: &Infos) -> bool {
        let written = match &self.tunnel {
            Some(t) => core.write_config(t, &self.text),
            None => std::fs::write(&self.path, &self.text).map_err(|e| crate::fsutil::io_ctx(&self.path, e)),
        };
        match written {
            Ok(()) => {
                self.saved = self.text.clone();
                self.note = Some(match &self.tunnel {
                    Some(t) => {
                        infos.lock().unwrap().remove(t);
                        (trf("eng.saved", &[t]), false)
                    }
                    None => (trf("ed.saved", &[&self.path.display().to_string()]), false),
                });
                true
            }
            Err(e) => {
                self.note = Some((e, true));
                false
            }
        }
    }
}

impl App {
    /// Редактор исходного .conf пользователя: сохранить, сохранить как, импортировать в родной клиент.
    pub(super) fn show_editor(&mut self, ctx: &egui::Context) {
        let (core, error, infos) = (self.core.clone(), self.action_error.clone(), self.sources.infos_handle());
        let engine = self.s.mode() == Mode::Engine;
        let Some(ed) = &mut self.editor else { return };
        let dirty = ed.dirty();
        let file = ed.file_name();
        let marker = if dirty { format!(" ({})", tr("ed.modified")) } else { String::new() };
        let (mut open, mut save, mut save_as, mut import, mut close) = (true, false, false, false, false);
        dialog_window(ctx, format!("{} — {file}{marker}", tr("ed.title")), EDITOR_ID, &mut open)
            .default_size([720.0, 540.0])
            .resizable(true)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    save = ui.add_enabled(dirty, egui::Button::new(tr("ed.save")).shortcut_text("Ctrl+S")).clicked();
                    save_as = ui.button(tr("ed.save_as")).clicked();
                    // Туннель хранилища уже «в программе»; файл с диска — импорт туда, куда смотрит режим.
                    if ed.tunnel.is_none() {
                        import = ui.button(tr(if engine { "ed.import_engine" } else { "ed.import" })).clicked();
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        close = ui.button(tr("btn.close")).on_hover_text("Esc").clicked();
                    });
                });
                if !ed.text.contains("[Interface]") {
                    ui.colored_label(YELLOW, tr("ed.no_interface"));
                }
                if let Some((note, is_error)) = &ed.note {
                    ui.colored_label(if *is_error { RED } else { GREEN }, note);
                }
                ui.separator();
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    ui.add(egui::TextEdit::multiline(&mut ed.text).code_editor().desired_width(f32::INFINITY).desired_rows(24));
                });
            });
        // Ctrl+S — сохранить, Esc — закрыть, если редактор верхнее окно (Enter в редакторе — перевод строки, не действие окна).
        let ctrl_s = ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::S));
        save |= ctrl_s && dirty;
        close |= window_escape(ctx, EDITOR_ID);
        if save_as {
            match (win::pick_conf(true, Some(&ed.path)), &ed.tunnel) {
                // Туннель хранилища: «Сохранить как» — копия в обычный файл, правка остаётся в хранилище.
                (Some(path), Some(_)) => {
                    ed.note = Some(match std::fs::write(&path, &ed.text) {
                        Ok(()) => (trf("ed.saved", &[&path.display().to_string()]), false),
                        Err(e) => (crate::fsutil::io_ctx(&path, e), true),
                    });
                }
                (Some(path), None) => {
                    ed.path = path;
                    save = true;
                }
                (None, _) => save = false,
            }
        }
        // Импорт берёт файл с диска — несохранённое сначала сохраняем.
        if (save || (import && dirty)) && !ed.save(core.as_ref(), &infos) {
            import = false;
        }
        if import && engine {
            ed.note = Some(import_into_store(core.as_ref(), &ed.path));
            import = false;
        }
        if import {
            let path = ed.path.display().to_string();
            ctx.copy_text(path.clone());
            ed.note = Some((trf("ed.import_hint", &[&path]), false));
            let file = ed.path.clone();
            std::thread::spawn(move || {
                if let Err(e) = core.import_in_native(Some(&file)) {
                    error.push(e);
                }
            });
        }
        if !open || close {
            // Несохранённое молча не теряем: спросить, сохранить ли (диалог рисуется со следующего кадра).
            if ed.dirty() {
                self.modals.open_if_absent(Modal::EditorUnsaved);
                ctx.request_repaint();
            } else {
                self.editor = None;
            }
        }
    }

    /// Редактор закрывают с несохранёнными изменениями: сохранить и закрыть, закрыть без сохранения или остаться.
    pub(super) fn show_editor_unsaved(&mut self, ctx: &egui::Context, turn: Turn) -> Outcome {
        // Сохранили (Ctrl+S) или закрыли, пока висел вопрос, — спрашивать не о чем.
        let Some(file) = self.editor.as_ref().filter(|ed| ed.dirty()).map(Editor::file_name) else { return Outcome::Close };
        let (mut save, mut discard, mut cancel) = (false, false, false);
        let mut open = true;
        dialog_window(ctx, tr("ed.unsaved_title"), "editor-unsaved", &mut open).show(ctx, |ui| {
            ui.set_width(440.0);
            ui.add(egui::Label::new(trf("ed.unsaved_text", &[&file])).wrap());
            ui.add_space(10.0);
            (save, discard, cancel) = dialog_choice(ui, &tr("ed.save"), &tr("ed.discard"), &tr("btn.cancel"));
        });
        let (enter, escape) = turn.keys(ctx);
        if save || enter {
            // Не записалось — редактор остаётся открытым с ошибкой под кнопками, текст не теряется.
            let (core, infos) = (self.core.clone(), self.sources.infos_handle());
            if self.editor.as_mut().is_some_and(|ed| ed.save(core.as_ref(), &infos)) {
                self.editor = None;
            }
            Outcome::Close
        } else if discard {
            self.editor = None;
            Outcome::Close
        } else if cancel || escape || !open {
            Outcome::Close
        } else {
            Outcome::Keep
        }
    }
}

/// Импорт файла из редактора в хранилище режима 2 -> сообщение под кнопками: текст и признак ошибки.
fn import_into_store(core: &dyn CoreApi, path: &std::path::Path) -> (String, bool) {
    let report = crate::archive::read_conf(path).and_then(|e| core.report(crate::daemon::proto::Request::Import(vec![e])));
    match report {
        Ok(r) if !r.added.is_empty() => (trf("eng.imported", &[&r.added.len().to_string()]), false),
        Ok(r) if !r.existing.is_empty() => (trf("eng.import_existing", &[&r.existing.join(", ")]), true),
        Ok(r) => (trf("eng.import_bad_name", &[&r.bad_name.join(", ")]), true),
        Err(e) => (e, true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conf::TunnelInfo;
    use crate::daemon::fake::FakeCore;
    use crate::daemon::proto::{Request, Response};
    use crate::store::ImportReport;

    fn conf(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("awg-ui-editor-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("office.conf");
        std::fs::write(&path, "[Interface]\nAddress = 10.0.0.2/32\n").unwrap();
        path
    }

    #[test]
    fn save_writes_the_file_and_clears_the_modified_mark() {
        let path = conf("save");
        let mut ed = Editor::new(path.clone(), "[Interface]\n".into(), None);
        ed.text.push_str("Address = 10.0.0.3/32\n");
        assert!(ed.dirty());
        let core = FakeCore::unreachable("core not used for files");
        assert!(ed.save(&core, &Infos::default()));
        assert!(!ed.dirty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), ed.text);
        assert_eq!(ed.note, Some((trf("ed.saved", &[&path.display().to_string()]), false)));
        assert!(core.requests().is_empty());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn failed_save_keeps_the_text_modified_and_shows_the_error() {
        // Туннель хранилища, ядро недоступно: закрытие «с сохранением» не должно потерять правку.
        let mut ed = Editor::new(PathBuf::from("office.conf"), "[Interface]\n".into(), Some("office".into()));
        ed.text.push_str("MTU = 1280\n");
        let core = FakeCore::unreachable("core unavailable");
        assert!(!ed.save(&core, &Infos::default()));
        assert!(ed.dirty());
        assert_eq!(ed.note, Some(("core unavailable".to_string(), true)));
    }

    #[test]
    fn saving_a_store_tunnel_drops_its_stale_details() {
        let mut ed = Editor::new(PathBuf::from("office.conf"), "[Interface]\n".into(), Some("office".into()));
        ed.text.push_str("MTU = 1280\n");
        let core = FakeCore::new(|_| Ok(Response::Ok));
        let infos = Infos::default();
        infos.lock().unwrap().insert("office".into(), (TunnelInfo::default(), "store".into()));
        assert!(ed.save(&core, &infos));
        assert!(infos.lock().unwrap().is_empty());
        assert_eq!(ed.note, Some((trf("eng.saved", &["office"]), false)));
    }

    #[test]
    fn import_sends_the_file_as_one_entry_and_reports_what_was_added() {
        let path = conf("added");
        let core = FakeCore::new(|req| match req {
            Request::Import(entries) => Ok(Response::Report(ImportReport { added: entries.iter().map(|e| e.name.clone()).collect(), ..Default::default() })),
            other => Err(format!("unexpected {other:?}")),
        });
        let (text, is_error) = import_into_store(&core, &path);
        assert!(!is_error, "{text}");
        assert_eq!(text, trf("eng.imported", &["1"]));
        let seen = core.requests();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].starts_with(r#"Import([Entry { name: "office""#), "{seen:?}");
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn import_of_an_existing_tunnel_is_an_error_note() {
        let path = conf("existing");
        let core = FakeCore::new(|_| Ok(Response::Report(ImportReport { existing: vec!["office".into()], ..Default::default() })));
        assert_eq!(import_into_store(&core, &path), (trf("eng.import_existing", &["office"]), true));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn import_with_unreachable_core_shows_the_error() {
        let path = conf("down");
        let core = FakeCore::unreachable("core unavailable");
        assert_eq!(import_into_store(&core, &path), ("core unavailable".to_string(), true));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
