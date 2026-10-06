//! Панель журнала событий: фильтр по важности и туннелю, поиск, копирование строк и сохранение в файл.
//! Отбор строк — чистая функция `visible` над событиями: панель только рисует то, что она вернула.

use eframe::egui::{self, Ui};

use crate::events::{Event, Severity};
use crate::fmt;
use crate::i18n::{tr, trf};
use crate::monitor::Shared;

use super::menu::context_menu;
use super::theme::{dot, mono, severity_color, GRAY};
use super::Action;

/// Какие события показывать по важности.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(super) enum SeverityFilter {
    #[default]
    All,
    WarningsAndErrors,
    Errors,
}

impl SeverityFilter {
    const ALL: [SeverityFilter; 3] = [SeverityFilter::All, SeverityFilter::WarningsAndErrors, SeverityFilter::Errors];

    fn admits(self, s: Severity) -> bool {
        match self {
            SeverityFilter::All => true,
            SeverityFilter::WarningsAndErrors => s != Severity::Info,
            SeverityFilter::Errors => s == Severity::Bad,
        }
    }

    fn label(self) -> String {
        tr(match self {
            SeverityFilter::All => "log.sev_all",
            SeverityFilter::WarningsAndErrors => "log.sev_warn",
            SeverityFilter::Errors => "log.sev_errors",
        })
    }
}

/// Состояние фильтра панели; живёт, пока открыто окно (в настройки не пишется).
#[derive(Clone, Debug, Default)]
pub(super) struct LogFilter {
    pub(super) severity: SeverityFilter,
    /// Только события выбранного туннеля. Туннель не выбран — фильтр по туннелю не действует.
    pub(super) selected_only: bool,
    /// Подстрока в имени туннеля или тексте, без учёта регистра.
    pub(super) query: String,
}

/// События, которые проходят фильтр, в исходном порядке. `selected` — туннель, выбранный в списке.
pub(super) fn visible<'a>(events: impl IntoIterator<Item = &'a Event>, filter: &LogFilter, selected: Option<&str>) -> Vec<&'a Event> {
    let tunnel = selected.filter(|_| filter.selected_only);
    let query = filter.query.trim().to_lowercase();
    events
        .into_iter()
        .filter(|e| filter.severity.admits(e.severity))
        .filter(|e| tunnel.is_none_or(|t| e.tunnel == t))
        .filter(|e| query.is_empty() || e.tunnel.to_lowercase().contains(&query) || e.text.to_lowercase().contains(&query))
        .collect()
}

/// Строки для буфера обмена и файла — в формате `events.log` (`Event::line`), по строке на событие.
pub(super) fn as_text(events: &[&Event]) -> String {
    events.iter().map(|e| e.line() + "\n").collect()
}

/// Имя файла, которое предлагает «Сохранить как…».
pub(super) fn default_file_name(now: u64) -> String {
    format!("awg-ui-events-{}.log", fmt::file_stamp(now))
}

impl super::App {
    /// «Сохранить как…»: стандартный диалог Windows, запись целиком (временный файл и переименование).
    pub(super) fn save_log(&mut self, text: &str) {
        let name = default_file_name(crate::monitor::unix_now());
        let Some(file) = crate::win::pick_files(crate::win::Files::Log, true, false, Some(std::path::Path::new(&name))).into_iter().next() else {
            return;
        };
        match crate::fsutil::write_atomic(&file, text.as_bytes()) {
            Ok(()) => *self.notice.lock().unwrap() = Some(trf("log.saved", &[&file.display().to_string()])),
            Err(e) => self.action_error.push(crate::fsutil::io_ctx(&file, e)),
        }
    }
}

pub(super) fn event_log(ui: &mut Ui, shared: &Shared, filter: &mut LogFilter, selected: Option<&str>, actions: &mut Vec<Action>) {
    shared.with_events(|events| {
        let shown = visible(&events.items, filter, selected);
        ui.add_space(4.0);
        toolbar(ui, filter, selected, &shown, actions);
        // Черта отделяет заголовок от строк, прокрученных наполовину.
        ui.separator();
        egui::ScrollArea::vertical().auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
            if events.items.is_empty() {
                ui.weak(tr("log.empty"));
            } else if shown.is_empty() {
                ui.weak(tr("log.no_match"));
            }
            lines(ui, &shown);
        });
    });
}

fn toolbar(ui: &mut Ui, filter: &mut LogFilter, selected: Option<&str>, shown: &[&Event], actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        ui.strong(tr("log.title"));
        ui.separator();
        egui::ComboBox::from_id_salt("log-severity").selected_text(filter.severity.label()).show_ui(ui, |ui| {
            for s in SeverityFilter::ALL {
                ui.selectable_value(&mut filter.severity, s, s.label());
            }
        });
        let tunnel_label = |only: bool| match selected {
            Some(t) if only => t.to_string(),
            _ => tr("log.tunnel_all"),
        };
        egui::ComboBox::from_id_salt("log-tunnel").selected_text(tunnel_label(filter.selected_only)).show_ui(ui, |ui| {
            ui.selectable_value(&mut filter.selected_only, false, tr("log.tunnel_all"));
            if selected.is_some() {
                ui.selectable_value(&mut filter.selected_only, true, tunnel_label(true));
            }
        });
        ui.add(egui::TextEdit::singleline(&mut filter.query).hint_text(tr("log.search")).desired_width(160.0));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.add_enabled(!shown.is_empty(), egui::Button::new(tr("log.save_as"))).clicked() {
                actions.push(Action::SaveLog(as_text(shown)));
            }
            if ui.add_enabled(!shown.is_empty(), egui::Button::new(tr("log.copy_all"))).clicked() {
                ui.ctx().copy_text(as_text(shown));
            }
        });
    });
}

fn lines(ui: &mut Ui, shown: &[&Event]) {
    // Id строки — от её содержимого, не от места: новые события и смена фильтра не переносят открытое меню на
    // другую строку. Одинаковые события подряд (та же секунда) различаются порядковым номером.
    let mut prev: Option<&Event> = None;
    let mut nth = 0u32;
    for &e in shown {
        nth = if prev.is_some_and(|p| p.at == e.at && p.tunnel == e.tunnel && p.text == e.text) { nth + 1 } else { 0 };
        prev = Some(e);
        let row = ui.horizontal(|ui| {
            ui.label(mono(fmt::date_time_sec(e.at), GRAY));
            dot(ui, severity_color(e.severity), 4.0, e.severity.as_str());
            if !e.tunnel.is_empty() {
                ui.strong(&e.tunnel);
            }
            ui.label(&e.text);
        });
        let id = ui.id().with(("log-line", e.at, e.tunnel.as_str(), e.text.as_str(), nth));
        let resp = ui.interact(row.response.rect, id, egui::Sense::click());
        context_menu(&resp, resp.has_focus(), |ui| {
            if ui.button(tr("log.copy_line")).clicked() {
                ui.ctx().copy_text(as_text(&[e]));
                ui.close();
            }
            if ui.button(tr("log.copy_all")).clicked() {
                ui.ctx().copy_text(as_text(shown));
                ui.close();
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(tunnel: &str, severity: Severity, text: &str) -> Event {
        Event::new(1_760_000_000, tunnel, severity, text, false)
    }

    fn sample() -> Vec<Event> {
        vec![
            ev("home", Severity::Info, "Connected"),
            ev("work", Severity::Warn, "Handshake is late"),
            ev("home", Severity::Bad, "Link lost"),
            ev("", Severity::Bad, "Core unreachable"),
        ]
    }

    fn texts(v: &[&Event]) -> Vec<String> {
        v.iter().map(|e| e.text.clone()).collect()
    }

    #[test]
    fn default_filter_shows_everything_in_order() {
        let all = sample();
        assert_eq!(texts(&visible(&all, &LogFilter::default(), Some("home"))), texts(&all.iter().collect::<Vec<_>>()));
    }

    #[test]
    fn severity_filter_keeps_warnings_and_errors_or_only_errors() {
        let all = sample();
        let warn = LogFilter { severity: SeverityFilter::WarningsAndErrors, ..Default::default() };
        assert_eq!(texts(&visible(&all, &warn, None)), ["Handshake is late", "Link lost", "Core unreachable"]);
        let errors = LogFilter { severity: SeverityFilter::Errors, ..Default::default() };
        assert_eq!(texts(&visible(&all, &errors, None)), ["Link lost", "Core unreachable"]);
    }

    #[test]
    fn tunnel_filter_uses_the_selected_tunnel_and_is_off_without_one() {
        let all = sample();
        let only = LogFilter { selected_only: true, ..Default::default() };
        assert_eq!(texts(&visible(&all, &only, Some("home"))), ["Connected", "Link lost"]);
        // Нет выбранного туннеля — нечего сужать: показывается всё, а не пустой журнал.
        assert_eq!(visible(&all, &only, None).len(), all.len());
    }

    #[test]
    fn search_is_case_insensitive_over_tunnel_and_text_including_cyrillic() {
        let mut all = sample();
        all.push(ev("дом", Severity::Info, "Связь восстановлена"));
        let by_text = LogFilter { query: "  LINK ".into(), ..Default::default() };
        assert_eq!(texts(&visible(&all, &by_text, None)), ["Link lost"]);
        let by_tunnel = LogFilter { query: "WORK".into(), ..Default::default() };
        assert_eq!(texts(&visible(&all, &by_tunnel, None)), ["Handshake is late"]);
        let cyr = LogFilter { query: "СВЯЗЬ".into(), ..Default::default() };
        assert_eq!(texts(&visible(&all, &cyr, None)), ["Связь восстановлена"]);
    }

    #[test]
    fn filters_combine_and_can_leave_nothing() {
        let all = sample();
        let f = LogFilter { severity: SeverityFilter::Errors, selected_only: true, query: "lost".into() };
        assert_eq!(texts(&visible(&all, &f, Some("home"))), ["Link lost"]);
        assert!(visible(&all, &f, Some("work")).is_empty());
        assert!(visible(&[], &LogFilter::default(), None).is_empty());
    }

    #[test]
    fn text_is_one_events_log_line_per_event_with_the_shared_date_format() {
        let all = sample();
        let text = as_text(&visible(&all, &LogFilter { severity: SeverityFilter::Errors, ..Default::default() }, None));
        let date = fmt::date_time_sec(1_760_000_000);
        assert_eq!(text, format!("{date}\tFAIL\thome\tLink lost\n{date}\tFAIL\t\tCore unreachable\n"));
        assert_eq!(as_text(&[]), "");
    }

    #[test]
    fn default_file_name_uses_the_shared_file_stamp() {
        assert_eq!(default_file_name(1_760_000_000), format!("awg-ui-events-{}.log", fmt::file_stamp(1_760_000_000)));
    }
}
