//! Строка состояния и журнал событий под таблицей.

use eframe::egui::{self, RichText, Ui};

use crate::fmt;
use crate::i18n::{tr, trf};
use crate::monitor::Shared;
use crate::settings::Mode;

use super::theme::{dot, mono, severity_color, GRAY, GREEN, NEON, RED};
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
            Mode::Engine => ui.label(RichText::new(tr("status.mode_engine")).color(NEON).strong()),
            Mode::Overlay => ui.weak(tr("status.mode_overlay")),
        };
        if !bar.service.is_empty() {
            ui.separator();
            ui.weak(bar.service);
        }
        if let Some(e) = bar.poll_error {
            ui.colored_label(RED, trf("status.poll_error", &[e]));
        }
        if let Some(n) = bar.notice {
            if ui.small_button("×").on_hover_text(tr("btn.dismiss")).clicked() {
                actions.push(Action::ClearNotice);
            }
            ui.colored_label(GREEN, n);
        }
        if bar.unseen_error && ui.add(egui::Button::new(RichText::new(tr("status.error_in_log")).color(RED)).small()).clicked() {
            actions.push(Action::ShowLog);
        }
    });
}

pub(super) fn event_log(ui: &mut Ui, shared: &Shared) {
    ui.add_space(4.0);
    ui.strong(tr("log.title"));
    // Черта отделяет заголовок от строк, прокрученных наполовину.
    ui.separator();
    egui::ScrollArea::vertical().auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
        shared.with_events(|events| {
            if events.items.is_empty() {
                ui.weak(tr("log.empty"));
            }
            for e in &events.items {
                ui.horizontal(|ui| {
                    ui.label(mono(fmt::date_time_sec(e.at), GRAY));
                    dot(ui, severity_color(e.severity), 4.0);
                    if !e.tunnel.is_empty() {
                        ui.strong(&e.tunnel);
                    }
                    ui.label(&e.text);
                });
            }
        });
    });
}
