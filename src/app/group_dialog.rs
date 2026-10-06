//! Диалог имени группы: новая группа или переименование.

use eframe::egui::{self, RichText};

use crate::groups;
use crate::i18n::{tr, trf};

use super::dialog::{dialog_buttons, dialog_window};
use super::modals::{Outcome, Turn};
use super::theme::palette;
use super::App;

pub(super) enum Dialog {
    /// Новая группа в `parent` (`None` — верхний уровень); `assign` — туннель, который сразу в неё положить.
    NewGroup { parent: Option<String>, name: String, assign: Option<String> },
    /// `old` — полный путь, `name` — новое имя последней части пути.
    Rename { old: String, name: String },
}

impl App {
    pub(super) fn show_group_dialog(&mut self, ctx: &egui::Context, dialog: &mut Dialog, turn: Turn) -> Outcome {
        let (title, text, parent, except) = match dialog {
            Dialog::NewGroup { parent, name, .. } => {
                let title = match parent {
                    Some(p) => trf("dlg.new_subgroup", &[p]),
                    None => tr("dlg.new_group"),
                };
                (title, name, parent.clone(), None)
            }
            Dialog::Rename { old, name } => (tr("dlg.rename_group"), name, groups::parent(old).map(str::to_string), Some(old.clone())),
        };
        let book = &self.s.book;
        // Фокус и выделение текста — только в кадре открытия; дальше поле живёт как обычно.
        let focus = turn.fresh;
        let (mut ok, mut cancel, mut valid) = (false, false, false);
        let mut open = true;
        dialog_window(ctx, title, "group-dialog", &mut open)
            .show(ctx, |ui| {
                ui.set_width(340.0);
                ui.label(tr("dlg.name"));
                let mut out = egui::TextEdit::singleline(text).desired_width(f32::INFINITY).show(ui);
                if focus {
                    out.response.request_focus();
                    let end = egui::text::CCursor::new(text.chars().count());
                    out.state.cursor.set_char_range(Some(egui::text::CCursorRange::two(egui::text::CCursor::new(0), end)));
                    out.state.store(ui.ctx(), out.response.id);
                }
                let check = book.check_name(parent.as_deref(), text, except.as_deref());
                valid = check.is_ok();
                // Пустое имя — не ошибка, пока пользователь ещё ничего не ввёл.
                let error = check.err().filter(|e| *e != groups::NameError::Empty).map(name_error).unwrap_or_default();
                // Строка ошибки занимает место всегда — окно не прыгает при наборе.
                ui.add_sized([ui.available_width(), 18.0], egui::Label::new(RichText::new(error).color(palette().error).small()).truncate());
                ui.add_space(4.0);
                (ok, cancel) = dialog_buttons(ui, &tr("btn.ok"), valid, Some(&tr("btn.cancel")));
            });
        let (enter, escape) = turn.keys(ctx);
        ok |= enter && valid;
        cancel |= escape || !open;
        if cancel {
            return Outcome::Close;
        }
        if !ok {
            return Outcome::Keep;
        }
        match dialog {
            Dialog::NewGroup { parent, name, assign } => match self.s.book.add(parent.as_deref(), name) {
                Err(e) => self.action_error.push(name_error(e)),
                Ok(path) => {
                    // Новую подгруппу видно сразу: родитель раскрывается.
                    if let Some(p) = parent {
                        self.s.book.expand(p);
                    }
                    match assign {
                        Some(tunnel) => {
                            if let Err(groups::NoSuchGroup(g)) = self.s.book.assign(tunnel, Some(&path)) {
                                self.action_error.push(trf("grp.err_missing", &[&g]));
                            }
                        }
                        None => self.s.book.select_group(&path),
                    }
                }
            },
            Dialog::Rename { old, name } => {
                // Выбранная группа переезжает вместе с переименованной (см. `TunnelBook::rename`).
                if let Err(e) = self.s.book.rename(old, name) {
                    self.action_error.push(name_error(e));
                }
            }
        }
        Outcome::Close
    }
}

/// Текст ошибки имени; пустое имя до `show_group_dialog` не доходит (кнопка «ОК» недоступна), но молча не теряется.
fn name_error(e: groups::NameError) -> String {
    match e {
        groups::NameError::Empty => tr("dlg.err_empty"),
        groups::NameError::BadChar => tr("dlg.err_slash"),
        groups::NameError::Exists => tr("dlg.err_exists"),
    }
}
