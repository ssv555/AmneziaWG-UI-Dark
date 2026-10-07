//! Строка состояния под таблицей (журнал событий — `event_log`).

use eframe::egui::{self, Color32, RichText, Ui};

use crate::i18n::tr;
use crate::settings::Mode;

use super::notice::{Notice, NoticeKind};
use super::theme::{palette, Palette};
use super::Action;

pub(super) struct StatusBar<'a> {
    pub(super) mode: Mode,
    pub(super) service: &'a str,
    pub(super) poll_error: Option<&'a str>,
    pub(super) unseen_error: bool,
    pub(super) notice: Option<&'a Notice>,
}

/// Ссылка на журнал событий: (текст, подсказка). Ошибка опроса важнее — она ещё длится, её текст — в подсказке
/// (в журнал он записан при появлении, `monitor::set_poll_error`). Иначе — новая ошибка действия.
fn log_link(bar: &StatusBar) -> Option<(String, Option<String>)> {
    match (bar.poll_error, bar.unseen_error) {
        (Some(e), _) => Some((tr("status.poll_error"), Some(e.to_string()))),
        (None, true) => Some((tr("status.error_in_log"), None)),
        (None, false) => None,
    }
}

/// Цвет подсказки: успех и предупреждение различаются не только словами.
fn notice_color(p: &Palette, kind: NoticeKind) -> Color32 {
    match kind {
        NoticeKind::Progress => p.text,
        NoticeKind::Done => p.connected,
        NoticeKind::Warning => p.warning,
    }
}

/// Строка состояния: режим работы, состояние службы, ход текущей операции. Ошибки — в журнале событий; здесь только
/// ссылка на него, без текста ошибки (docs/ui-guidelines.md, «Errors and text»). Ссылка стоит справа первой: длинная
/// подсказка укорачивается («…», полный текст в подсказке при наведении), но не выталкивает ссылку за край окна.
/// Возвращает место ссылки (для теста, что она видна).
pub(super) fn status_bar(ui: &mut Ui, bar: &StatusBar, actions: &mut Vec<Action>) -> Option<egui::Rect> {
    let mut link_rect = None;
    ui.horizontal(|ui| {
        match bar.mode {
            Mode::Engine => ui.label(RichText::new(tr("status.mode_engine")).color(palette().mode2_text).strong()),
            Mode::Overlay => ui.weak(tr("status.mode_overlay")),
        };
        if !bar.service.is_empty() {
            ui.separator();
            ui.weak(bar.service);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if let Some((text, tip)) = log_link(bar) {
                let link = ui.add(egui::Button::new(RichText::new(text).color(palette().error)).small());
                link_rect = Some(link.rect);
                let link = match tip {
                    Some(tip) => link.on_hover_text(tip),
                    None => link,
                };
                if link.clicked() {
                    actions.push(Action::ShowLog);
                }
            }
            if let Some(n) = bar.notice {
                // Справа налево: крестик добавлен первым и стоит после текста, как в Windows.
                if ui.small_button("×").on_hover_text(tr("btn.dismiss")).clicked() {
                    actions.push(Action::ClearNotice);
                }
                ui.add(egui::Label::new(RichText::new(&n.text).color(notice_color(palette(), n.kind))).truncate());
            }
        });
    });
    link_rect
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::theme::{DAYLIGHT, GRAPHITE, SLATE};

    const PALETTES: [(&str, &Palette); 3] = [("graphite", &GRAPHITE), ("slate", &SLATE), ("daylight", &DAYLIGHT)];

    fn bar<'a>(poll_error: Option<&'a str>, unseen_error: bool, notice: Option<&'a Notice>) -> StatusBar<'a> {
        StatusBar { mode: Mode::Overlay, service: "service AmneziaWGTunnel is running", poll_error, unseen_error, notice }
    }

    #[test]
    fn errors_show_only_a_link_never_their_text() {
        let raw = "core: Refused(\"busy\")";
        let (text, tip) = log_link(&bar(Some(raw), false, None)).unwrap();
        assert_eq!(text, tr("status.poll_error"));
        assert!(!text.contains("Refused"), "текст ошибки — не в строке состояния");
        assert_eq!(tip.as_deref(), Some(raw), "подробности — в подсказке");
        assert_eq!(log_link(&bar(None, true, None)), Some((tr("status.error_in_log"), None)));
        assert_eq!(log_link(&bar(None, false, None)), None);
    }

    #[test]
    fn success_and_warning_notices_look_different() {
        for (name, p) in PALETTES {
            assert_eq!(notice_color(p, NoticeKind::Done), p.connected, "{name}");
            assert_eq!(notice_color(p, NoticeKind::Warning), p.warning, "{name}");
            assert_ne!(notice_color(p, NoticeKind::Done), notice_color(p, NoticeKind::Warning), "{name}");
            assert_ne!(notice_color(p, NoticeKind::Progress), notice_color(p, NoticeKind::Done), "{name}");
        }
    }

    /// Окно минимальной ширины, длинная подсказка с путём: ссылка на журнал остаётся внутри окна.
    #[test]
    fn long_notice_does_not_push_the_log_link_out() {
        let notice = Notice {
            kind: NoticeKind::Done,
            text: format!("Event log saved: C:\\Users\\someone\\Documents\\{}\\awg-ui-events-20261007-101500.log", "very-long-folder-name".repeat(4)),
        };
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::Vec2::new(760.0, 480.0));
        let ctx = egui::Context::default();
        let mut link = None;
        for _ in 0..2 {
            let input = egui::RawInput { screen_rect: Some(screen), ..Default::default() };
            let _ = ctx.run_ui(input, |ui| {
                link = status_bar(ui, &bar(Some("pipe: closed"), true, Some(&notice)), &mut Vec::new());
            });
        }
        let link = link.expect("ссылка показана");
        assert!(screen.contains_rect(link), "ссылка {link:?} за краем окна {screen:?}");
    }
}
