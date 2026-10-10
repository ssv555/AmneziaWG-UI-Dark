//! Доступность для экранных дикторов (Narrator, NVDA). Виджеты egui сообщают о себе сами; всё, что нарисовано
//! painter-ом поверх `allocate_exact_size` (строки таблицы, график, точки состояния), для дерева AccessKit — пустой
//! прямоугольник. Каждое такое место описывает себя здесь, одним вызовом `describe`: подпись и роль собираются в
//! одном месте, а не копиями по местам рисования.

use eframe::egui::{Response, WidgetInfo, WidgetType};

use crate::i18n::{tr, trf};

use super::list::Primary;

/// Нарисованный элемент и то, что о нём должен услышать пользователь диктора.
pub(super) enum Painted<'a> {
    /// Строка туннеля: имя, состояние, группа (лист пути). `selected` — выделена в таблице.
    Tunnel { name: &'a str, state: Primary, group: Option<&'a str>, selected: bool },
    /// Заголовок группы: развёрнута ли, сколько туннелей поддерева включено.
    Group { name: &'a str, expanded: bool, active: usize, total: usize, selected: bool },
    /// График скорости: текущие приём и передача, пик за показанный период (уже отформатированы); `paused` — на паузе.
    Graph { rx: &'a str, tx: &'a str, peak: &'a str, paused: bool },
    /// Надпись на месте графика (история загружается, её нет или она недоступна): текст как есть.
    GraphNote { text: &'a str },
    /// Кнопка паузы графика (нарисована значком): подпись — что она сделает, `paused` — нажата.
    GraphPause { paused: bool },
    /// Полоса пинга: узел и последний результат.
    Ping { host: &'a str, last: &'a str },
    /// Цветная точка состояния: её смысл словами (цвет диктор не передаёт).
    Dot { meaning: &'a str },
}

/// Слово состояния туннеля — по тому же решению, что кнопка и меню (`Primary`), чтобы диктор не расходился с экраном.
pub(super) fn state_word(state: Primary) -> String {
    tr(match state {
        Primary::Connect => "a11y.disconnected",
        Primary::Disconnect => "a11y.connected",
        Primary::Busy => "a11y.connecting",
        Primary::Unknown => "a11y.unknown",
    })
}

impl Painted<'_> {
    /// Подпись узла — то, что диктор прочтёт.
    pub(super) fn label(&self) -> String {
        match self {
            Painted::Tunnel { name, state, group, .. } => {
                let mut text = format!("{name}, {}", state_word(*state));
                if let Some(g) = group {
                    text.push_str(", ");
                    text.push_str(&trf("a11y.in_group", &[g]));
                }
                text
            }
            Painted::Group { name, active, total, .. } => trf("a11y.group", &[name, &active.to_string(), &total.to_string()]),
            Painted::Graph { rx, tx, peak, paused } => {
                let text = trf("a11y.graph", &[rx, tx, peak]);
                if *paused {
                    format!("{text}, {}", tr("gr.paused"))
                } else {
                    text
                }
            }
            Painted::GraphPause { paused } => tr(if *paused { "gr.resume" } else { "gr.pause" }),
            Painted::GraphNote { text } => text.to_string(),
            Painted::Ping { host, last } => trf("a11y.ping", &[host, last]),
            Painted::Dot { meaning } => meaning.to_string(),
        }
    }

    fn info(&self) -> WidgetInfo {
        match self {
            // Как у selectable_label egui: выделение — состояние «нажата», диктор читает его сам.
            Painted::Tunnel { selected, .. } => WidgetInfo::selected(WidgetType::SelectableLabel, true, *selected, self.label()),
            Painted::Group { selected, .. } => WidgetInfo::selected(WidgetType::CollapsingHeader, true, *selected, self.label()),
            // Как у кнопки-переключателя egui: пауза — состояние «нажата».
            Painted::GraphPause { paused } => WidgetInfo::selected(WidgetType::Button, true, *paused, self.label()),
            Painted::Graph { .. } | Painted::GraphNote { .. } | Painted::Ping { .. } | Painted::Dot { .. } => WidgetInfo::labeled(WidgetType::Image, true, self.label()),
        }
    }

    fn expanded(&self) -> Option<bool> {
        match self {
            Painted::Group { expanded, .. } => Some(*expanded),
            _ => None,
        }
    }
}

/// Описать нарисованный элемент для дерева доступности. Вызывать после всех взаимодействий с `resp` (клик, фокус):
/// egui превращает их в события диктора по этому же описанию.
pub(super) fn describe(resp: &Response, what: Painted) {
    // Подпись собирается, только когда egui её спросит (AccessKit включён или есть событие для диктора).
    resp.widget_info(|| what.info());
    // У WidgetInfo нет «развёрнуто/свёрнуто» — свойство узла ставим сами (Narrator читает «развёрнуто»/«свёрнуто»).
    if let Some(expanded) = what.expanded() {
        resp.ctx.accesskit_node_builder(resp.id, |node| node.set_expanded(expanded));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::{self, accesskit, Sense, Vec2};

    /// Один кадр с включённым AccessKit: нарисовать элемент через `describe`, вернуть узел дерева по его id.
    fn node_of(what: impl Fn() -> Painted<'static>) -> accesskit::Node {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut id = None;
        let out = ctx.run_ui(egui::RawInput::default(), |ui| {
            let (_, resp) = ui.allocate_exact_size(Vec2::new(200.0, 20.0), Sense::click());
            describe(&resp, what());
            id = Some(resp.id);
        });
        let update = out.platform_output.accesskit_update.expect("AccessKit включён — дерево есть");
        let id = accesskit::NodeId::from(id.expect("кадр нарисован").value());
        update.nodes.into_iter().find(|(n, _)| *n == id).map(|(_, node)| node).expect("узел строки в дереве")
    }

    #[test]
    fn tunnel_row_node_has_name_state_and_group() {
        let node = node_of(|| Painted::Tunnel { name: "office", state: Primary::Disconnect, group: Some("work"), selected: true });
        let label = node.label().expect("подпись");
        assert!(label.contains("office"), "{label}");
        assert!(label.contains(&tr("a11y.connected")), "{label}");
        assert!(label.contains(&trf("a11y.in_group", &["work"])), "{label}");
        assert_eq!(node.toggled(), Some(accesskit::Toggled::True), "выделенная строка");
    }

    #[test]
    fn group_header_node_reports_expanded_state() {
        let collapsed = node_of(|| Painted::Group { name: "work", expanded: false, active: 1, total: 3, selected: false });
        assert_eq!(collapsed.is_expanded(), Some(false));
        assert!(collapsed.label().is_some_and(|l| l.contains("work")));
        let open = node_of(|| Painted::Group { name: "work", expanded: true, active: 1, total: 3, selected: false });
        assert_eq!(open.is_expanded(), Some(true));
    }

    #[test]
    fn every_state_has_its_own_word() {
        let words: Vec<String> = [Primary::Connect, Primary::Disconnect, Primary::Busy, Primary::Unknown].into_iter().map(state_word).collect();
        for (i, w) in words.iter().enumerate() {
            assert!(!w.is_empty() && !w.starts_with("a11y."), "нет перевода: {w}");
            assert!(!words[i + 1..].contains(w), "два состояния звучат одинаково: {w}");
        }
    }

    #[test]
    fn ungrouped_tunnel_label_has_no_group_part() {
        let label = Painted::Tunnel { name: "home", state: Primary::Unknown, group: None, selected: false }.label();
        assert_eq!(label, format!("home, {}", tr("a11y.unknown")));
    }

    #[test]
    fn graph_summary_names_the_numbers() {
        let label = Painted::Graph { rx: "1 KB/s", tx: "2 KB/s", peak: "3 KB/s", paused: false }.label();
        for part in ["1 KB/s", "2 KB/s", "3 KB/s"] {
            assert!(label.contains(part), "{label}");
        }
        assert!(!label.contains(&tr("gr.paused")), "{label}");
        let paused = Painted::Graph { rx: "1 KB/s", tx: "2 KB/s", peak: "3 KB/s", paused: true }.label();
        assert!(paused.ends_with(&tr("gr.paused")), "{paused}");
    }

    #[test]
    fn pause_button_says_what_it_will_do_and_is_pressed_while_paused() {
        let live = node_of(|| Painted::GraphPause { paused: false });
        assert_eq!(live.label(), Some(tr("gr.pause").as_str()));
        assert_ne!(live.toggled(), Some(accesskit::Toggled::True));
        let paused = node_of(|| Painted::GraphPause { paused: true });
        assert_eq!(paused.label(), Some(tr("gr.resume").as_str()));
        assert_eq!(paused.toggled(), Some(accesskit::Toggled::True));
    }
}
