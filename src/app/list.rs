//! Таблица туннелей: колонки, заголовок, клавиатура и сама прокручиваемая область.
//! Отрисовка строк (группа, туннель, перетаскивание) — в `rows`.

use std::collections::BTreeMap;

use eframe::egui::{self, Align2, FontId, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2};

use crate::daemon::proto::Plan;
use crate::fmt;
use crate::groups::{self, Row, UNGROUPED};
use crate::health::{Health, Level};
use crate::i18n::tr;
use crate::monitor::Snapshot;
use crate::settings::{Mode, Settings, SortKey};
use crate::stats::{self, Stats, TunnelStats};

mod rows;
use rows::{drag_preview, drop_target, group_row, triangle, tunnel_row, GroupLine};

use super::{Action, Confirm, Dialog};

pub(super) const ROW_H: f32 = 24.0;
/// Сдвиг строки дерева на каждый уровень вложенности.
const INDENT: f32 = 16.0;

/// Что перетаскивают мышью в таблице туннелей.
#[derive(Clone)]
enum Drag {
    Tunnel(String),
    Group(String),
}

/// Числовые колонки слева направо: ключ сортировки, заголовок, ширина из настроек.
fn numeric_columns(s: &Settings) -> Vec<(SortKey, &'static str, f32)> {
    let v = &s.view;
    let c = &s.columns;
    [
        (v.col_rx, SortKey::Rx, "col.rx", c.rx),
        (v.col_tx, SortKey::Tx, "col.tx", c.tx),
        (v.col_peak, SortKey::Peak, "col.peak", c.peak),
        (v.col_share, SortKey::Share, "col.share", c.share),
    ]
    .into_iter()
    .filter(|(on, ..)| *on)
    .map(|(_, k, t, w)| (k, t, w))
    .collect()
}

fn column_width(s: &mut Settings, key: SortKey) -> Option<&mut f32> {
    match key {
        SortKey::Rx => Some(&mut s.columns.rx),
        SortKey::Tx => Some(&mut s.columns.tx),
        SortKey::Peak => Some(&mut s.columns.peak),
        SortKey::Share => Some(&mut s.columns.share),
        SortKey::Name => None,
    }
}

/// Границы ячеек строки: имя занимает остаток ширины.
struct Cells {
    name: Rect,
    nums: Vec<(SortKey, Rect)>,
}

fn cells(row: Rect, cols: &[(SortKey, &str, f32)]) -> Cells {
    let mut right = row.right();
    let mut nums = Vec::new();
    for (key, _, w) in cols.iter().rev() {
        nums.push((*key, Rect::from_x_y_ranges(right - w..=right, row.y_range())));
        right -= w;
    }
    nums.reverse();
    Cells { name: Rect::from_x_y_ranges(row.left()..=right, row.y_range()), nums }
}

fn cell_value(key: SortKey, st: &TunnelStats, share: f64) -> String {
    match key {
        SortKey::Rx => fmt::bytes(st.rx as f64),
        SortKey::Tx => fmt::bytes(st.tx as f64),
        SortKey::Peak => fmt::rate(st.peak_rx),
        SortKey::Share => fmt::percent(share),
        SortKey::Name => String::new(),
    }
}

fn sort_value(key: SortKey, st: Option<&TunnelStats>, share: f64) -> f64 {
    let st = st.cloned().unwrap_or_default();
    match key {
        SortKey::Rx => st.rx as f64,
        SortKey::Tx => st.tx as f64,
        SortKey::Peak => st.peak_rx,
        SortKey::Share => share,
        SortKey::Name => 0.0,
    }
}

/// Какие клавиши таблица может забрать в этом кадре.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Keys {
    /// Стрелки, Enter, F2, Delete — нет окон поверх таблицы, меню и ввода текста.
    pub(super) list: bool,
    /// Ctrl+F — нет окон поверх таблицы.
    pub(super) search: bool,
}

/// Состояние для таблицы туннелей на этот кадр.
pub(super) struct List<'a> {
    pub(super) snap: &'a Snapshot,
    pub(super) healths: &'a BTreeMap<String, Health>,
    pub(super) stats: &'a Stats,
    pub(super) keys: Keys,
}

/// Метка в памяти egui: выделение сменили клавишами или снаружи (щелчок по уведомлению) — прокрутить к нему.
fn scroll_flag() -> egui::Id {
    egui::Id::new("tunnel-list-scroll")
}

/// Прокрутить список к выделенной строке на ближайшем кадре.
pub(super) fn reveal_selection(ctx: &egui::Context) {
    ctx.data_mut(|d| d.insert_temp(scroll_flag(), true));
}

pub(super) fn tunnel_list(ui: &mut Ui, s: &mut Settings, search: &mut String, l: &List, actions: &mut Vec<Action>) {
    // Режим 2 без туннелей: своё хранилище пусто — сказать, откуда их взять (сами они из оригинала не копируются).
    if s.mode() == Mode::Engine && l.snap.polls > 0 && l.snap.tunnels.is_empty() {
        ui.add_space(12.0);
        ui.add(egui::Label::new(RichText::new(tr("eng.empty_title")).strong()).wrap());
        ui.add_space(4.0);
        ui.add(egui::Label::new(tr("eng.empty_text")).wrap());
        ui.add_space(10.0);
        if crate::backend::native_exe().exists() && ui.button(tr("eng.take_native")).clicked() {
            actions.push(Action::EngineTakeNative);
        }
        if ui.button(tr("eng.import")).clicked() {
            actions.push(Action::EngineImport);
        }
        if ui.button(tr("eng.restore")).clicked() {
            actions.push(Action::EngineRestore);
        }
        return;
    }
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        ui.checkbox(&mut s.view.groups, tr("view.groups"));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if s.view.groups && ui.button(tr("list.add_group")).on_hover_text(tr("list.add_group_tip")).clicked() {
                actions.push(Action::Open(Dialog::NewGroup { parent: None, name: String::new(), assign: None }));
            }
        });
    });
    if s.view.search {
        let id = egui::Id::new("tunnel-search");
        if l.keys.search && ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::F)) {
            ui.memory_mut(|m| m.request_focus(id));
        }
        let resp = ui.add(egui::TextEdit::singleline(search).id(id).hint_text(tr("list.search_hint")).desired_width(f32::INFINITY));
        // Esc в поле поиска — очистить.
        if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            search.clear();
        }
    }
    ui.add_space(2.0);

    let needle = search.trim().to_lowercase();
    let stats = l.stats;
    let mut visible: Vec<&String> =
        l.snap.tunnels.iter().filter(|t| needle.is_empty() || t.to_lowercase().contains(&needle)).collect();
    let share = |t: &str| stats::share(stats, t);
    let (key, desc) = (s.sort, s.sort_desc);
    visible.sort_by(|a, b| {
        let ord = match key {
            SortKey::Name => a.to_lowercase().cmp(&b.to_lowercase()),
            _ => sort_value(key, stats.get(*a), share(a)).total_cmp(&sort_value(key, stats.get(*b), share(b))),
        };
        if desc { ord.reverse() } else { ord }
    });

    let dragging = egui::DragAndDrop::has_payload_of_type::<Drag>(ui.ctx());
    let rows: Vec<Row> = if s.view.groups {
        s.book.rows(&visible, !needle.is_empty(), dragging)
    } else {
        visible.iter().map(|t| Row::Tunnel { name: t.to_string(), depth: 0, group: None }).collect()
    };
    if l.keys.list && !dragging {
        list_keys(ui, &rows, s, l, actions);
    }

    header(ui, s, actions);
    let cols = numeric_columns(s);
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        for row in &rows {
            match row {
                Row::Group { path, depth, collapsed, members } => {
                    let g = GroupLine { key: path, depth: *depth, collapsed: *collapsed, members, selected: s.book.selected_group() == Some(path.as_str()) };
                    group_row(ui, &g, &cols, s, l, actions);
                }
                Row::Ungrouped { collapsed, members } => {
                    let g = GroupLine { key: UNGROUPED, depth: 0, collapsed: *collapsed, members, selected: s.book.selected_group() == Some(UNGROUPED) };
                    group_row(ui, &g, &cols, s, l, actions);
                }
                Row::Tunnel { name, depth, group } => {
                    tunnel_row(ui, name, *depth, group.as_deref(), &cols, s, l, actions);
                }
            }
        }
        // Пустое место под списком — тоже цель перетаскивания: группа на верхний уровень, туннель — из группы.
        if s.view.groups {
            let height = ui.available_height().max(ROW_H * 2.0);
            let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::hover());
            drop_target(ui, &resp, rect, None, s, actions);
        }
    });
    drag_preview(ui.ctx());
}

/// Что делает «активация» строки (двойной клик, Enter): только подключает. Отключение — кнопкой или пунктом меню:
/// промах двойным кликом или Enter по подключённому туннелю иначе роняет VPN и пускает трафик в открытую сеть.
fn activation_plan(level: Level) -> Option<Plan> {
    (level == Level::Off).then_some(Plan::Connect)
}

/// Главное действие с туннелем — одно решение для кнопки карточки, меню строки таблицы и меню трея. Карточка
/// раньше решала сама, по живому интерфейсу: туннель, который ядро переподключает (интерфейса нет, ядро повторяет),
/// там был «Подключить», а в меню — «Отключить».
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Primary {
    /// Отключён: «Подключить».
    Connect,
    /// Подключён или его держит и переподключает ядро: «Отключить» (останавливает и повторы).
    Disconnect,
    /// Уже переключается: «Отключить», но недоступно.
    Busy,
    /// Нет связи с ядром — состояние неизвестно: не показан подключённым, действие недоступно (подсказка — почему).
    Unknown,
}

impl Primary {
    /// `core_lost` — нет связи с ядром (`Snapshot::core_lost`); уровень тогда «неизвестно», а не состояние туннеля.
    pub(super) fn of(level: Level, core_lost: bool) -> Primary {
        if core_lost {
            return Primary::Unknown;
        }
        match level {
            Level::Off => Primary::Connect,
            Level::Busy => Primary::Busy,
            Level::Ok | Level::Warn | Level::Bad => Primary::Disconnect,
        }
    }

    /// Запрос ядру; `None` — действие сейчас недоступно (пункт и кнопка серые).
    pub(super) fn plan(self) -> Option<Plan> {
        match self {
            Primary::Connect => Some(Plan::Connect),
            Primary::Disconnect => Some(Plan::Disconnect),
            Primary::Busy | Primary::Unknown => None,
        }
    }

    /// Туннель показан включённым: отметка в трее, «Отключить» на кнопке и в меню, счёт активных у группы.
    pub(super) fn is_on(self) -> bool {
        matches!(self, Primary::Disconnect | Primary::Busy)
    }

    /// Подсказка к недоступному действию, если серость надо объяснить.
    pub(super) fn hint(self) -> Option<&'static str> {
        (self == Primary::Unknown).then_some("health.core_lost")
    }

    /// Ключ подписи кнопки и пункта меню.
    pub(super) fn label(self) -> &'static str {
        if self.is_on() {
            "act.disconnect"
        } else {
            "act.connect"
        }
    }
}

/// Клавиши на выделенной строке: ↑/↓ — соседняя строка, ←/→ — свернуть/раскрыть или к родителю,
/// Enter — подключить туннель (отключение — только кнопкой или меню) или свернуть группу, F2 — переименовать группу, Delete — удалить группу.
fn list_keys(ui: &Ui, rows: &[Row], s: &Settings, l: &List, actions: &mut Vec<Action>) {
    use egui::Key;
    let (up, down, left, right, enter, f2, delete) = ui.input_mut(|i| {
        let mut k = |key| i.consume_key(egui::Modifiers::NONE, key);
        (k(Key::ArrowUp), k(Key::ArrowDown), k(Key::ArrowLeft), k(Key::ArrowRight), k(Key::Enter), k(Key::F2), k(Key::Delete))
    });
    if rows.is_empty() || !(up || down || left || right || enter || f2 || delete) {
        return;
    }
    let current = rows.iter().position(|r| match r {
        Row::Group { path, .. } => s.book.selected_group() == Some(path.as_str()),
        Row::Ungrouped { .. } => s.book.selected_group() == Some(UNGROUPED),
        Row::Tunnel { name, .. } => s.book.is_tunnel_highlighted(name),
    });
    let select = |i: usize, actions: &mut Vec<Action>| {
        actions.push(match &rows[i] {
            Row::Group { path, .. } => Action::SelectGroup(path.clone()),
            Row::Ungrouped { .. } => Action::SelectGroup(UNGROUPED.to_string()),
            Row::Tunnel { name, .. } => Action::Select(name.clone()),
        });
        reveal_selection(ui.ctx());
    };
    if up || down {
        let next = match current {
            None => 0,
            Some(c) if up => c.saturating_sub(1),
            Some(c) => (c + 1).min(rows.len() - 1),
        };
        select(next, actions);
        return;
    }
    let Some(c) = current else { return };
    match &rows[c] {
        Row::Group { path, collapsed, .. } => {
            if (left && !collapsed) || (right && *collapsed) || enter {
                actions.push(Action::ToggleCollapse(path.clone()));
            } else if left {
                if let Some(p) = groups::parent(path) {
                    actions.push(Action::SelectGroup(p.to_string()));
                }
            }
            if f2 {
                actions.push(Action::Open(Dialog::Rename { old: path.clone(), name: groups::leaf(path).to_string() }));
            }
            if delete {
                actions.push(Action::Confirm(Confirm::DeleteGroup(path.clone())));
            }
        }
        Row::Ungrouped { collapsed, .. } => {
            if (left && !collapsed) || (right && *collapsed) || enter {
                actions.push(Action::ToggleCollapse(UNGROUPED.to_string()));
            }
        }
        Row::Tunnel { name, group, .. } => {
            let level = l.healths.get(name).map_or(Level::Off, |h| h.level);
            if let Some(plan) = activation_plan(level).filter(|_| enter) {
                actions.push(Action::Switch(name.clone(), plan));
            }
            if delete {
                actions.push(Action::Confirm(Confirm::DeleteTunnel(name.clone())));
            }
            if left && s.view.groups {
                actions.push(Action::SelectGroup(group.clone().unwrap_or_else(|| UNGROUPED.to_string())));
            }
        }
    }
}

/// Заголовок таблицы: клик — сортировка, перетаскивание левой границы числовой колонки — её ширина.
fn header(ui: &mut Ui, s: &mut Settings, actions: &mut Vec<Action>) {
    let (row, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H), Sense::hover());
    let painter = ui.painter_at(row);
    let weak = ui.visuals().weak_text_color();
    painter.line_segment([row.left_bottom(), row.right_bottom()], Stroke::new(1.0_f32, ui.visuals().widgets.noninteractive.bg_stroke.color));
    let cols = numeric_columns(s);
    let c = cells(row, &cols);
    let mut titled: Vec<(SortKey, &str, Rect)> = vec![(SortKey::Name, "col.tunnel", c.name)];
    titled.extend(cols.iter().zip(&c.nums).map(|((k, t, _), (_, r))| (*k, *t, *r)));
    for (key, title, rect) in titled {
        let id = ui.id().with(("hdr", title));
        let resp = ui.interact(rect, id, Sense::click()).on_hover_text(tr("col.sort_hint"));
        if resp.clicked() {
            actions.push(Action::Sort(key));
        }
        let color = if resp.hovered() { ui.visuals().strong_text_color() } else { weak };
        let (align, pos) = if key == SortKey::Name {
            (Align2::LEFT_CENTER, rect.left_center() + Vec2::new(22.0, 0.0))
        } else {
            (Align2::RIGHT_CENTER, rect.right_center() - Vec2::new(8.0, 0.0))
        };
        let text_rect = painter.text(pos, align, tr(title), FontId::proportional(13.0), color);
        if s.sort == key {
            let x = if key == SortKey::Name { text_rect.right() + 8.0 } else { text_rect.left() - 8.0 };
            triangle(&painter, Pos2::new(x, row.center().y), 4.0, !s.sort_desc, color);
        }
        if key != SortKey::Name {
            let handle = Rect::from_x_y_ranges(rect.left() - 3.0..=rect.left() + 3.0, row.y_range());
            let drag = ui.interact(handle, id.with("resize"), Sense::drag());
            if drag.hovered() || drag.dragged() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                painter.vline(rect.left(), row.y_range(), Stroke::new(1.0_f32, weak));
            }
            if let Some(w) = column_width(s, key) {
                *w = (*w - drag.drag_delta().x).clamp(50.0, 320.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activation_only_connects() {
        assert_eq!(activation_plan(Level::Off), Some(Plan::Connect));
        for running in [Level::Ok, Level::Warn, Level::Bad, Level::Busy] {
            assert_eq!(activation_plan(running), None, "{running:?}");
        }
    }

    #[test]
    fn toggle_connects_off_disconnects_the_rest_and_waits_while_busy() {
        assert_eq!(Primary::of(Level::Off, false).plan(), Some(Plan::Connect));
        for on in [Level::Ok, Level::Warn, Level::Bad] {
            assert_eq!(Primary::of(on, false).plan(), Some(Plan::Disconnect), "{on:?}: подключён или переподключается");
        }
        assert_eq!(Primary::of(Level::Busy, false).plan(), None);
        assert_eq!(Primary::of(Level::Busy, false).label(), "act.disconnect");
    }

    /// Ядро держит туннель и повторяет подключение (живого интерфейса нет): главное действие — «Отключить»,
    /// оно останавливает повторы. Карточка раньше предлагала здесь «Подключить».
    #[test]
    fn tunnel_the_core_retries_offers_disconnect() {
        use crate::daemon::proto::RetryState;
        let mut snap = crate::monitor::Snapshot { tunnels: ["a".to_string()].into_iter().collect(), polls: 1, ..Default::default() };
        for slow in [false, true] {
            snap.retries.insert("a".into(), RetryState { attempt: 3, next_in_s: 5, last_error: "timeout".into(), slow });
            let primary = Primary::of(snap.health("a", None, None).level, snap.core_lost);
            assert_eq!((primary, primary.label(), primary.plan()), (Primary::Disconnect, "act.disconnect", Some(Plan::Disconnect)), "slow={slow}");
            assert!(primary.is_on(), "в трее отмечен");
        }
    }

    /// Нет связи с ядром: состояние неизвестно — ни подключённым, ни отключённым туннель не показан, действие
    /// недоступно и объяснено. Раньше такой туннель выглядел подключённым и предлагал «Отключить».
    #[test]
    fn unknown_state_does_not_look_connected() {
        let mut snap = crate::monitor::Snapshot { tunnels: ["a".to_string()].into_iter().collect(), polls: 1, core_lost: true, ..Default::default() };
        let retry = crate::daemon::proto::RetryState { attempt: 1, next_in_s: 5, last_error: String::new(), slow: false };
        snap.retries.insert("a".into(), retry);
        let primary = Primary::of(snap.health("a", None, None).level, snap.core_lost);
        assert_eq!(primary, Primary::Unknown);
        assert!(!primary.is_on());
        assert_eq!((primary.plan(), primary.hint()), (None, Some("health.core_lost")));
        assert_eq!(Primary::of(Level::Off, false).hint(), None);
    }


    #[test]
    fn cells_fill_row() {
        let row = Rect::from_min_size(Pos2::ZERO, Vec2::new(500.0, 24.0));
        let cols = [(SortKey::Rx, "a", 100.0), (SortKey::Share, "b", 60.0)];
        let c = cells(row, &cols);
        assert_eq!(c.name.width(), 340.0);
        assert_eq!(c.nums[0].1.left(), 340.0);
        assert_eq!(c.nums[1].1.right(), 500.0);
    }
}
