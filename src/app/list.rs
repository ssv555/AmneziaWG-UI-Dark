//! Таблица туннелей: колонки, заголовок, клавиатура и сама прокручиваемая область.
//! Отрисовка строк (группа, туннель, перетаскивание) — в `rows`.

use std::collections::{BTreeMap, BTreeSet};

use eframe::egui::{self, Align2, FontId, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2};

use crate::daemon::proto::Plan;
use crate::fmt;
use crate::groups::{self, Row, UNGROUPED};
use crate::health::{Health, Level};
use crate::i18n::{tr, trf};
use crate::monitor::Snapshot;
use crate::settings::{DialogId, Mode, Settings, SortKey};
use crate::stats::{self, Stats, TunnelStats};

mod rows;
use rows::{drag_preview, drop_target, group_row, triangle, tunnel_row, GroupLine};

use super::theme::NUM_FONT;
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

/// Колонка имени не у́же этого: иначе от имён остаются одни многоточия. Числовые колонки, которым не хватает места
/// рядом с ней, скрываются (`fit_columns`).
const NAME_MIN: f32 = 120.0;
/// Поле ячейки с каждой стороны: числа не прилипают к соседней колонке.
const CELL_PAD: f32 = 8.0;
/// Место под треугольник сортировки слева от заголовка числовой колонки. Отведено всегда, чтобы ширина колонки
/// не прыгала при смене сортировки.
const SORT_MARK: f32 = 12.0;
const HEADER_FONT: f32 = 13.0;
/// Самое широкое значение байт и скоростей: `fmt::bytes` держит число меньше 1024 в своей единице с двумя знаками,
/// а 1048575 B — это «1024.00 KiB» (округление вверх), длиннее не бывает ни в одной единице.
const WIDEST_BYTES: f64 = 1024.0 * 1024.0 - 1.0;

/// Видимые числовые колонки слева направо (меню «Вид»): ключ сортировки и заголовок.
fn numeric_columns(s: &Settings) -> Vec<(SortKey, &'static str)> {
    let v = &s.view;
    [(v.col_rx, SortKey::Rx, "col.rx"), (v.col_tx, SortKey::Tx, "col.tx"), (v.col_peak, SortKey::Peak, "col.peak"), (v.col_share, SortKey::Share, "col.share")]
        .into_iter()
        .filter(|(on, ..)| *on)
        .map(|(_, k, t)| (k, t))
        .collect()
}

/// Самое широкое значение, какое колонка может показать, — тем же форматированием, что ячейки (`cell_value`).
fn widest_value(key: SortKey) -> String {
    match key {
        SortKey::Rx | SortKey::Tx => fmt::bytes(WIDEST_BYTES),
        SortKey::Peak => fmt::rate(WIDEST_BYTES),
        SortKey::Share => fmt::percent(1.0),
        SortKey::Name => String::new(),
    }
}

fn text_width(ui: &Ui, text: String, font: FontId) -> f32 {
    ui.painter().layout_no_wrap(text, font, ui.visuals().text_color()).size().x
}

/// Ширина числовой колонки по содержимому: самое широкое её значение или заголовок с треугольником сортировки,
/// оба измерены шрифтом, которым рисуются. Числа не наезжают на соседей при любом значении и языке.
fn natural_width(ui: &Ui, key: SortKey, title: &str) -> f32 {
    let value = text_width(ui, widest_value(key), FontId::monospace(NUM_FONT));
    let header = text_width(ui, tr(title), FontId::proportional(HEADER_FONT)) + SORT_MARK;
    value.max(header) + 2.0 * CELL_PAD
}

/// Какие колонки помещаются в строку шириной `row`: имя не у́же `NAME_MIN`, числовые колонки, которым не хватает
/// места, скрываются справа налево (порядок меню «Вид» — от важной к наименее важной). Наложения нет никогда.
fn fit_columns(row: f32, mut cols: Vec<(SortKey, &'static str, f32)>) -> Vec<(SortKey, &'static str, f32)> {
    while !cols.is_empty() && row - cols.iter().map(|c| c.2).sum::<f32>() < NAME_MIN {
        cols.pop();
    }
    cols
}

/// Колонки таблицы на этот кадр: ширины по содержимому, лишние для ширины `row` скрыты.
fn layout_columns(ui: &Ui, s: &Settings, row: f32) -> Vec<(SortKey, &'static str, f32)> {
    let natural = numeric_columns(s).into_iter().map(|(k, t)| (k, t, natural_width(ui, k, t))).collect();
    fit_columns(row, natural)
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

    let cols = layout_columns(ui, s, ui.available_width());
    header(ui, &cols, s, actions);
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

    /// Что делает «активация» строки (двойной клик, Enter): только подключает. Отключение — кнопкой, пунктом меню
    /// или из трея: промах двойным кликом или Enter по подключённому туннелю иначе роняет VPN и пускает трафик в
    /// открытую сеть. Неизвестное состояние (нет связи с ядром) не активируется, как и серая кнопка.
    pub(super) fn activation(self) -> Option<Plan> {
        self.plan().filter(|p| *p == Plan::Connect)
    }

    /// Подсказка к «Подключить»: какие туннели подключение снимет. Без «несколько сразу» ядро отключает все остальные
    /// работающие (`to_replace` в ядре) — их окно знает из снимка и называет. С «несколько сразу» ядро снимает только
    /// конфликтующие по адресам и маршрутам; сравнивает оно конфиги, которых у окна нет, — тогда общая подсказка.
    pub(super) fn connect_hint(self, name: &str, multiple: bool, snap: &Snapshot) -> Option<String> {
        if self != Primary::Connect {
            return None;
        }
        let others: Vec<&str> = snap.running.keys().map(String::as_str).filter(|t| *t != name).collect();
        match (others.is_empty(), multiple) {
            (true, _) => None,
            (false, false) => Some(trf("act.connect_replaces", &[&others.join(", ")])),
            (false, true) => Some(tr("act.connect_conflicts")),
        }
    }
}

/// Спросить подтверждение до запроса ядру. Отключение рвёт VPN — по кнопке, меню строки и трея сначала вопрос,
/// пока пользователь не ответил «Больше не спрашивать» (`DialogId::Disconnect`; «Снова показывать скрытые диалоги»
/// возвращает вопрос). Одно решение для окна и трея.
pub(super) fn asks_first(plan: Plan, hidden: &BTreeSet<DialogId>) -> bool {
    plan == Plan::Disconnect && !hidden.contains(&DialogId::Disconnect)
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
            if let Some(plan) = Primary::of(level, l.snap.core_lost).activation().filter(|_| enter) {
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

/// Заголовок таблицы: клик — сортировка. Ширины колонок — по содержимому (`layout_columns`), каждый заголовок
/// рисуется только в своей ячейке.
fn header(ui: &mut Ui, cols: &[(SortKey, &str, f32)], s: &Settings, actions: &mut Vec<Action>) {
    let (row, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H), Sense::hover());
    let painter = ui.painter_at(row);
    let weak = ui.visuals().weak_text_color();
    painter.line_segment([row.left_bottom(), row.right_bottom()], Stroke::new(1.0_f32, ui.visuals().widgets.noninteractive.bg_stroke.color));
    let c = cells(row, cols);
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
            (Align2::RIGHT_CENTER, rect.right_center() - Vec2::new(CELL_PAD, 0.0))
        };
        let cell = painter.with_clip_rect(rect);
        let text_rect = cell.text(pos, align, tr(title), FontId::proportional(HEADER_FONT), color);
        if s.sort == key {
            let x = if key == SortKey::Name { text_rect.right() + 8.0 } else { text_rect.left() - 8.0 };
            triangle(&cell, Pos2::new(x, row.center().y), 4.0, !s.sort_desc, color);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activation_only_connects() {
        assert_eq!(Primary::of(Level::Off, false).activation(), Some(Plan::Connect));
        for running in [Level::Ok, Level::Warn, Level::Bad, Level::Busy] {
            assert_eq!(Primary::of(running, false).activation(), None, "{running:?}: двойной клик и Enter не отключают");
        }
        // Нет связи с ядром: «отключён» в снимке ничего не значит — как и серая кнопка, активация ничего не шлёт.
        assert_eq!(Primary::of(Level::Off, true).activation(), None);
    }

    #[test]
    fn disconnect_asks_until_the_answer_is_remembered() {
        let none = BTreeSet::new();
        assert!(asks_first(Plan::Disconnect, &none));
        assert!(!asks_first(Plan::Connect, &none));
        assert!(!asks_first(Plan::Reconnect, &none), "переподключение VPN не оставляет выключенным");
        assert!(!asks_first(Plan::Disconnect, &BTreeSet::from([DialogId::Disconnect])), "«Больше не спрашивать»");
        assert!(asks_first(Plan::Disconnect, &BTreeSet::from([DialogId::ExitKeep])), "чужой скрытый диалог не в счёт");
    }

    fn running(names: &[&str]) -> Snapshot {
        let live = |t: &&str| (t.to_string(), crate::monitor::Live::default());
        Snapshot { tunnels: names.iter().map(|t| t.to_string()).collect(), running: names.iter().map(live).collect(), polls: 1, ..Default::default() }
    }

    #[test]
    fn connect_hint_names_the_tunnels_the_core_will_drop() {
        let snap = running(&["home", "office"]);
        let connect = Primary::Connect;
        assert_eq!(connect.connect_hint("lab", false, &snap), Some(trf("act.connect_replaces", &["home, office"])));
        // С «несколько сразу» ядро снимает только конфликтующие — окно их не знает, подсказка общая.
        assert_eq!(connect.connect_hint("lab", true, &snap), Some(tr("act.connect_conflicts")));
        // Сам туннель в список не входит; других подключённых нет — подсказки нет.
        assert_eq!(connect.connect_hint("home", false, &running(&["home"])), None);
        assert_eq!(connect.connect_hint("lab", false, &running(&[])), None);
        // Подсказка — только к «Подключить».
        assert_eq!(Primary::Disconnect.connect_hint("home", false, &snap), None);
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

    fn all_columns() -> Settings {
        let mut s = Settings::default();
        (s.view.col_rx, s.view.col_tx, s.view.col_peak, s.view.col_share) = (true, true, true, true);
        s
    }

    /// Узкая панель: числовые колонки скрываются справа налево, имя не у́же минимума; было — колонки складывались
    /// с конца строки и при всех четырёх (360 pt) на панели 260 pt левая граница имени уходила правее правой.
    #[test]
    fn columns_that_do_not_fit_hide_from_the_right() {
        let natural = vec![(SortKey::Rx, "col.rx", 100.0), (SortKey::Tx, "col.tx", 100.0), (SortKey::Peak, "col.peak", 120.0), (SortKey::Share, "col.share", 70.0)];
        let keys = |row: f32| fit_columns(row, natural.clone()).iter().map(|c| c.0).collect::<Vec<_>>();
        // Широкая панель: всё на месте, имени остаток.
        assert_eq!(keys(600.0), vec![SortKey::Rx, SortKey::Tx, SortKey::Peak, SortKey::Share]);
        assert_eq!(keys(390.0 + NAME_MIN), vec![SortKey::Rx, SortKey::Tx, SortKey::Peak, SortKey::Share], "ровно впритык");
        // Чуть уже — первой уходит последняя (доля), потом пик.
        assert_eq!(keys(389.0 + NAME_MIN), vec![SortKey::Rx, SortKey::Tx, SortKey::Peak]);
        assert_eq!(keys(320.0 + NAME_MIN), vec![SortKey::Rx, SortKey::Tx, SortKey::Peak], "без доли — впритык");
        assert_eq!(keys(319.0 + NAME_MIN), vec![SortKey::Rx, SortKey::Tx]);
        assert_eq!(keys(260.0), vec![SortKey::Rx]);
        // Уже имени с одной колонкой: только имя.
        assert_eq!(keys(NAME_MIN + 99.0), Vec::<SortKey>::new());
        assert_eq!(keys(50.0), Vec::<SortKey>::new());
        for row in [50.0, 200.0, 260.0, 300.0, 400.0, 520.0, 900.0] {
            let cols = fit_columns(row, natural.clone());
            let c = cells(Rect::from_min_size(Pos2::ZERO, Vec2::new(row, ROW_H)), &cols);
            assert!(c.name.width() >= NAME_MIN.min(row), "{row}: имя {}", c.name.width());
            assert!(c.name.left() <= c.name.right(), "{row}");
        }
    }

    /// `widest_value` и правда самое длинное, что показывает колонка: цифры моноширинные, так что длина в символах —
    /// это ширина. Перебор границ всех единиц, включая округление вверх до «1024.00».
    #[test]
    fn widest_value_is_the_longest_cell() {
        let mut bytes = vec![0.0, 1.0, 999.0, 1023.0, 1024.0, 1_000_000.0];
        for k in 1..=4 {
            let unit = 1024.0_f64.powi(k);
            bytes.extend([unit - 1.0, unit, unit * 999.99, unit * 1023.994, unit * 1023.996]);
        }
        // Выше «1024.00 TiB» (петабайт за сеанс) число растёт без новой единицы — такое значение обрезает ячейка.
        bytes.push(1024.0_f64.powi(5) - 1.0);
        let len = |s: String| s.chars().count();
        for b in bytes {
            assert!(len(fmt::bytes(b)) <= len(widest_value(SortKey::Rx)), "{}", fmt::bytes(b));
            assert!(len(fmt::rate(b)) <= len(widest_value(SortKey::Peak)), "{}", fmt::rate(b));
        }
        for share in [0.0, 0.05, 0.999, 1.0] {
            assert!(len(fmt::percent(share)) <= len(widest_value(SortKey::Share)), "{share}");
        }
    }

    /// Ширины измерены шрифтом в настоящем контексте egui: самое широкое значение и заголовок с треугольником влезают
    /// в ячейку с полями; при всех четырёх колонках на панели минимальной ширины (260 pt) имя остаётся не у́же минимума,
    /// на широкой — видны все колонки. Было: пик 104 pt при «1023.99 KiB/s» шире 110 pt — наезд на «Отдано».
    #[test]
    fn measured_columns_hold_their_widest_value() {
        let ctx = egui::Context::default();
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            let s = all_columns();
            for (key, title) in numeric_columns(&s) {
                let w = natural_width(ui, key, title);
                let value = text_width(ui, widest_value(key), FontId::monospace(NUM_FONT));
                let header = text_width(ui, tr(title), FontId::proportional(HEADER_FONT));
                assert!(value > 40.0, "{key:?}: {value}");
                assert!(w >= value + 2.0 * CELL_PAD && w >= header + SORT_MARK + 2.0 * CELL_PAD, "{key:?}: {w}");
            }
            let wide = layout_columns(ui, &s, 900.0);
            assert_eq!(wide.len(), 4);
            let narrow = layout_columns(ui, &s, 260.0);
            assert!(narrow.len() < 4, "{narrow:?}");
            assert!(260.0 - narrow.iter().map(|c| c.2).sum::<f32>() >= NAME_MIN);
            assert!(layout_columns(ui, &Settings::default(), 900.0).len() == numeric_columns(&Settings::default()).len());
        });
    }
}
