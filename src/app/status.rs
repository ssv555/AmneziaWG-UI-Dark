//! Строка состояния под таблицей (журнал событий — `event_log`).

use eframe::egui::{self, RichText, Ui};

use crate::i18n::{tr, trf};
use crate::settings::Mode;

use super::theme::palette;
use super::Action;

pub(super) struct StatusBar<'a> {
    pub(super) mode: Mode,
    pub(super) service: &'a str,
    pub(super) poll_error: Option<&'a str>,
    pub(super) unseen_error: bool,
    pub(super) notice: Option<&'a str>,
}

/// Строка состояния: режим работы, состояние службы, ход текущей операции. Ошибки — в журнале событий; когда
/// журнал скрыт, здесь только ссылка на него.
pub(super) fn status_bar(ui: &mut Ui, bar: &StatusBar, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        match bar.mode {
            Mode::Engine => ui.label(RichText::new(tr("status.mode_engine")).color(palette().mode2_text).strong()),
            Mode::Overlay => ui.weak(tr("status.mode_overlay")),
        };
        if !bar.service.is_empty() {
            ui.separator();
            ui.weak(bar.service);
        }
        if let Some(e) = bar.poll_error {
            ui.colored_label(palette().error, trf("status.poll_error", &[e]));
        }
        if let Some(n) = bar.notice {
            if ui.small_button("×").on_hover_text(tr("btn.dismiss")).clicked() {
                actions.push(Action::ClearNotice);
            }
            ui.colored_label(palette().connected, n);
        }
        if bar.unseen_error && ui.add(egui::Button::new(RichText::new(tr("status.error_in_log")).color(palette().error)).small()).clicked() {
            actions.push(Action::ShowLog);
        }
    });
}

