//! Строки таблицы туннелей: группа, туннель, контекстные меню и перетаскивание.

use std::sync::Arc;

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2};

use crate::app::a11y::{self, Painted};
use crate::app::theme::{level_color, palette, Palette, NUM_FONT};
use crate::app::{menu, Action, Confirm, Dialog};
use crate::daemon::proto::Plan;
use crate::groups::{self, Agg, Verdict, UNGROUPED};
use crate::health::{Health, Level};
use crate::i18n::{tr, trf};
use crate::settings::{Mode, Settings, SortKey};
use crate::stats::{self, TunnelStats};

use super::{cell_value, cells, scroll_flag, Drag, List, Primary, CELL_PAD, INDENT, ROW_H};

/// Строка — цель Shift+F10 и клавиши меню: в таблице клавиши — у выделенной строки (как стрелки и Enter), если таблица
/// их сейчас принимает и фокус egui не стоит на другом элементе (значение в карточке, строка журнала).
fn keyboard_target(ui: &Ui, l: &List, selected: bool, resp: &egui::Response) -> bool {
    selected && l.keys.list && ui.memory(|m| m.focused().is_none_or(|f| f == resp.id))
}

/// Подсветка цели под перетаскиваемым и сам перенос при отпускании. `target` — группа (`None` — верхний
/// уровень / «Без группы»). Недопустимое (группа в своего потомка, занятое имя) — красная рамка и запрещающий курсор.
pub(super) fn drop_target(ui: &Ui, resp: &egui::Response, rect: Rect, target: Option<&str>, s: &Settings, actions: &mut Vec<Action>) {
    let Some(drag) = resp.dnd_hover_payload::<Drag>() else { return };
    let verdict = match &*drag {
        Drag::Tunnel(t) => s.book.check_assign(t, target),
        Drag::Group(g) => s.book.check_reparent(g, target),
    };
    let area = rect.shrink(1.0);
    match verdict {
        Verdict::Noop => {}
        Verdict::Invalid => {
            ui.painter().rect_stroke(area, 3.0, Stroke::new(1.5_f32, palette().error), egui::StrokeKind::Inside);
            ui.ctx().set_cursor_icon(egui::CursorIcon::NotAllowed);
        }
        Verdict::Valid => {
            ui.painter().rect_filled(area, 3.0, palette().accent.gamma_multiply(0.15));
            ui.painter().rect_stroke(area, 3.0, Stroke::new(1.5_f32, palette().accent), egui::StrokeKind::Inside);
            if resp.dnd_release_payload::<Drag>().is_some() {
                let to = target.map(str::to_string);
                actions.push(match &*drag {
                    Drag::Tunnel(t) => Action::Assign(t.clone(), to),
                    Drag::Group(g) => Action::Reparent(g.clone(), to),
                });
            }
        }
    }
}

/// Что тащим — подпись у курсора.
pub(super) fn drag_preview(ctx: &egui::Context) {
    let Some(drag) = egui::DragAndDrop::payload::<Drag>(ctx) else { return };
    let Some(pos) = ctx.pointer_latest_pos() else { return };
    let text = match &*drag {
        Drag::Tunnel(t) => format!("● {t}"),
        Drag::Group(g) => format!("▶ {}", groups::leaf(g)),
    };
    egui::Area::new(egui::Id::new("drag-preview"))
        .order(egui::Order::Tooltip)
        .fixed_pos(pos + Vec2::new(16.0, 8.0))
        .interactable(false)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.label(RichText::new(text).strong());
            });
        });
    if ctx.output(|o| o.cursor_icon) == egui::CursorIcon::Default {
        ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
    }
}

/// Треугольник: вершиной вверх (`up`) или вниз; вправо — для свёрнутой группы.
pub(super) fn triangle(painter: &egui::Painter, c: Pos2, r: f32, up: bool, color: Color32) {
    let pts = if up {
        vec![Pos2::new(c.x - r, c.y + r * 0.6), Pos2::new(c.x + r, c.y + r * 0.6), Pos2::new(c.x, c.y - r * 0.7)]
    } else {
        vec![Pos2::new(c.x - r, c.y - r * 0.6), Pos2::new(c.x + r, c.y - r * 0.6), Pos2::new(c.x, c.y + r * 0.7)]
    };
    painter.add(egui::Shape::convex_polygon(pts, color, Stroke::NONE));
}

fn triangle_right(painter: &egui::Painter, c: Pos2, r: f32, color: Color32) {
    let pts = vec![Pos2::new(c.x - r * 0.6, c.y - r), Pos2::new(c.x - r * 0.6, c.y + r), Pos2::new(c.x + r * 0.7, c.y)];
    painter.add(egui::Shape::convex_polygon(pts, color, Stroke::NONE));
}

/// Фон выделенной строки. В светлой теме бледная заливка почти сливается с фоном, поэтому слева ещё полоса акцента.
fn paint_selected(ui: &Ui, painter: &egui::Painter, row: Rect) {
    painter.rect_filled(row, 2.0, ui.visuals().selection.bg_fill);
    if palette().selection_bar {
        painter.rect_filled(Rect::from_min_size(row.min, Vec2::new(3.0, row.height())), 1.0, palette().accent);
    }
}

fn truncated(ui: &Ui, text: &str, font: FontId, color: Color32, width: f32) -> Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::single_section(text.to_string(), egui::TextFormat::simple(font, color));
    job.wrap = egui::text::TextWrapping::truncate_at_width(width.max(10.0));
    ui.painter().layout_job(job)
}

/// Промежуток между именем группы и счётом «активные/всего».
const COUNT_GAP: f32 = 12.0;

/// Числа строки: прижаты вправо с полем `CELL_PAD`, как заголовок колонки, и рисуются только в своей ячейке.
fn numbers(painter: &egui::Painter, nums: &[(SortKey, Rect)], st: &TunnelStats, share: f64, color: Color32) {
    for (key, rect) in nums {
        let pos = rect.right_center() - Vec2::new(CELL_PAD, 0.0);
        painter.with_clip_rect(*rect).text(pos, Align2::RIGHT_CENTER, cell_value(*key, st, share), FontId::monospace(NUM_FONT), color);
    }
}

/// Строка группы (или «Без группы») в дереве.
pub(super) struct GroupLine<'a> {
    pub(super) key: &'a str,
    pub(super) depth: usize,
    pub(super) collapsed: bool,
    /// Видимые туннели всего поддерева — для итогов.
    pub(super) members: &'a [String],
    pub(super) selected: bool,
}

/// Тонкие вертикальные линии дерева под треугольниками предков.
fn guides(ui: &Ui, painter: &egui::Painter, row: Rect, depth: usize) {
    let color = ui.visuals().widgets.noninteractive.bg_stroke.color;
    for k in 0..depth {
        let x = row.left() + k as f32 * INDENT + 10.0;
        painter.vline(x, row.y_range(), Stroke::new(1.0_f32, color));
    }
}

/// Строка стала выделенной клавишами — прокрутить к ней.
fn scroll_if_flagged(ui: &Ui, row: Rect) {
    if ui.ctx().data_mut(|d| d.remove_temp::<bool>(scroll_flag())).is_some() {
        ui.scroll_to_rect(row, None);
    }
}

pub(super) fn group_row(ui: &mut Ui, g: &GroupLine, cols: &[(SortKey, &str, f32)], s: &Settings, l: &List, actions: &mut Vec<Action>) {
    let real = g.key != UNGROUPED;
    let sense = if real { Sense::click_and_drag() } else { Sense::click() };
    let (row, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H + 2.0), sense);
    if g.selected {
        scroll_if_flagged(ui, row);
    }
    let painter = ui.painter_at(row);
    if g.selected {
        paint_selected(ui, &painter, row);
    } else if resp.hovered() {
        painter.rect_filled(row, 2.0, ui.visuals().widgets.hovered.weak_bg_fill);
    }
    let strong = if g.selected { ui.visuals().selection.stroke.color } else { ui.visuals().strong_text_color() };
    let weak = if g.selected { strong } else { ui.visuals().weak_text_color() };
    guides(ui, &painter, row, g.depth);
    let c = cells(row, cols);
    let x0 = row.left() + g.depth as f32 * INDENT;
    let arrow = Pos2::new(x0 + 10.0, row.center().y);
    if g.collapsed {
        triangle_right(&painter, arrow, 4.5, strong);
    } else {
        triangle(&painter, arrow, 4.5, false, strong);
    }

    // Итоги по всему поддереву: активные/всего, сумма скачанного и отданного, пик — максимум, доля — сумма.
    let mut agg = Agg::default();
    for t in g.members {
        let st = l.stats.get(t).cloned().unwrap_or_default();
        let active = l.healths.get(t).is_some_and(|h| Primary::of(h.level, l.snap.core_lost).is_on());
        agg.add(active, st.rx, st.tx, st.peak_rx, stats::share(l.stats, t));
    }
    let shown = if real { groups::leaf(g.key).to_string() } else { tr("app.ungrouped") };
    // Счёт «активные/всего» измерен, имя обрезается по остатку ячейки; обрезанное — целиком в подсказке.
    let count = painter.layout_no_wrap(format!("{}/{}", agg.active, agg.total), FontId::monospace(12.5), weak);
    let name_left = x0 + 22.0;
    let galley = truncated(ui, &shown, FontId::proportional(15.0), strong, c.name.right() - name_left - count.size().x - COUNT_GAP - CELL_PAD);
    let elided = galley.elided;
    let name_w = galley.size().x;
    let name_cell = painter.with_clip_rect(c.name);
    name_cell.galley(Pos2::new(name_left, row.center().y - galley.size().y / 2.0), galley, strong);
    name_cell.galley(Pos2::new(name_left + name_w + COUNT_GAP, row.center().y - count.size().y / 2.0), count, weak);
    let mut sum = TunnelStats::default();
    sum.rx = agg.rx;
    sum.tx = agg.tx;
    sum.peak_rx = agg.peak;
    numbers(&painter, &c.nums, &sum, agg.share, strong);
    let resp = if elided { resp.on_hover_text(shown.as_str()) } else { resp };

    if real {
        resp.dnd_set_drag_payload(Drag::Group(g.key.to_string()));
    }
    drop_target(ui, &resp, row, real.then_some(g.key), s, actions);
    if resp.clicked() {
        actions.push(Action::ToggleCollapse(g.key.to_string()));
        actions.push(Action::SelectGroup(g.key.to_string()));
    }
    a11y::describe(
        &resp,
        Painted::Group { name: &shown, expanded: !g.collapsed, active: agg.active, total: agg.total, selected: g.selected },
    );
    if !real {
        return;
    }
    let path = g.key;
    menu::context_menu(&resp, keyboard_target(ui, l, g.selected, &resp), |ui| {
        if ui.button(tr("grp.new_sub")).clicked() {
            actions.push(Action::Open(Dialog::NewGroup { parent: Some(path.to_string()), name: String::new(), assign: None }));
            ui.close();
        }
        if ui.add(egui::Button::new(tr("grp.rename")).shortcut_text("F2")).clicked() {
            actions.push(Action::Open(Dialog::Rename { old: path.to_string(), name: groups::leaf(path).to_string() }));
            ui.close();
        }
        ui.separator();
        if ui.add_enabled(s.book.can_move(path, -1), egui::Button::new(tr("grp.up"))).clicked() {
            actions.push(Action::MoveGroup(path.to_string(), -1));
            ui.close();
        }
        if ui.add_enabled(s.book.can_move(path, 1), egui::Button::new(tr("grp.down"))).clicked() {
            actions.push(Action::MoveGroup(path.to_string(), 1));
            ui.close();
        }
        if g.depth > 0 {
            let verdict = s.book.check_reparent(path, None);
            let top = ui.add_enabled(verdict == Verdict::Valid, egui::Button::new(tr("grp.to_top")));
            if top.on_disabled_hover_text(tr("grp.to_top_taken")).clicked() {
                actions.push(Action::Reparent(path.to_string(), None));
                ui.close();
            }
        }
        ui.separator();
        if ui.add(egui::Button::new(tr("grp.delete")).shortcut_text("Del")).clicked() {
            actions.push(Action::Confirm(Confirm::DeleteGroup(path.to_string())));
            ui.close();
        }
    });
}

/// Подменю «В группу»: дерево групп вложенными меню; текущая группа отмечена.
fn group_menu(ui: &mut Ui, s: &Settings, of: Option<&str>, tunnel: &str, actions: &mut Vec<Action>) {
    let current = s.book.group_of(tunnel);
    for g in s.book.children(of) {
        let here = current == Some(g.as_str());
        let mut assign = false;
        if s.book.children(Some(g)).is_empty() {
            assign = ui.add(egui::Button::new(groups::leaf(g)).selected(here)).clicked();
        } else {
            let inside = current.is_some_and(|c| groups::within(c, g));
            let title = if inside { RichText::new(groups::leaf(g)).strong() } else { RichText::new(groups::leaf(g)) };
            ui.menu_button(title, |ui| {
                assign = ui.add(egui::Button::new(trf("act.into_group", &[groups::leaf(g)])).selected(here)).clicked();
                ui.separator();
                group_menu(ui, s, Some(g), tunnel, actions);
            });
        }
        if assign {
            actions.push(Action::Assign(tunnel.to_string(), Some(g.clone())));
            ui.close();
        }
    }
}

/// Имя активного туннеля — цветом его кружка, чтобы вся строка читалась включённой: подключён — `connected`,
/// подключается, переподключается или пинг не проходит — `warning`. `None` — обычный текст: отключён, состояние
/// неизвестно (нет связи с ядром, кружок жёлтый, но туннель не показан включённым). Ошибка (`Level::Bad`) — имя `error`,
/// чтобы сбой не отличался от «отключён» одним цветом кружка; в выбранной строке красный нечитаем (2.2 на 005C80
/// Графита) — там имя обычное, а сбой показывает кольцо вместо кружка (`status_mark`).
fn name_color(p: &Palette, level: Level, primary: Primary, selected: bool) -> Option<Color32> {
    match level {
        Level::Bad => (!selected).then_some(p.error),
        _ if !primary.is_on() => None,
        Level::Ok => Some(p.connected),
        Level::Busy | Level::Warn => Some(p.warning),
        Level::Off => None,
    }
}

/// Кружок состояния строки. Ошибка — кольцо: форма, а не только цвет, отличает её от серого «отключён» и для тех,
/// кто не различает красный и серый.
fn status_mark(painter: &egui::Painter, center: Pos2, radius: f32, level: Level) {
    if level == Level::Bad {
        painter.circle_stroke(center, radius - 1.0, Stroke::new(2.0, level_color(level)));
    } else {
        painter.circle_filled(center, radius, level_color(level));
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn tunnel_row(
    ui: &mut Ui,
    name: &String,
    depth: usize,
    group: Option<&str>,
    cols: &[(SortKey, &str, f32)],
    s: &Settings,
    l: &List,
    actions: &mut Vec<Action>,
) {
    let h = l.healths.get(name).cloned().unwrap_or(Health::new(Level::Off, String::new()));
    let sense = if s.view.groups { Sense::click_and_drag() } else { Sense::click() };
    let (row, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H), sense);
    let selected = s.book.is_tunnel_highlighted(name);
    if selected {
        scroll_if_flagged(ui, row);
    }
    let painter = ui.painter_at(row);
    if selected {
        paint_selected(ui, &painter, row);
    } else if resp.hovered() {
        painter.rect_filled(row, 2.0, ui.visuals().widgets.hovered.weak_bg_fill);
    }
    let text_color = if selected { ui.visuals().selection.stroke.color } else { ui.visuals().text_color() };
    let primary = Primary::of(h.level, l.snap.core_lost);
    let active = name_color(palette(), h.level, primary, selected);
    guides(ui, &painter, row, depth);
    let indent = depth as f32 * INDENT + 6.0;
    let c = cells(row, cols);
    let dot_r = if active.is_some() { 6.0 } else { 5.0 };
    status_mark(&painter, Pos2::new(c.name.left() + indent + 5.0, row.center().y), dot_r, h.level);
    let name_color = active.unwrap_or(text_color);
    let galley = truncated(ui, name, FontId::proportional(15.0), name_color, c.name.width() - indent - 22.0);
    let name_cell = painter.with_clip_rect(c.name);
    name_cell.galley(Pos2::new(c.name.left() + indent + 16.0, row.center().y - galley.size().y / 2.0), galley, name_color);
    let st = l.stats.get(name).cloned().unwrap_or_default();
    numbers(&painter, &c.nums, &st, stats::share(l.stats, name), text_color);
    if s.view.groups {
        resp.dnd_set_drag_payload(Drag::Tunnel(name.clone()));
        // Брошенное на туннель попадает в его группу.
        drop_target(ui, &resp, row, group, s, actions);
    }
    let resp = resp.on_hover_text(format!("{name}\n{}", h.log_text()));
    if resp.clicked() {
        actions.push(Action::Select(name.clone()));
    }
    let running = primary.is_on();
    let busy = h.level == Level::Busy;
    if let Some(plan) = primary.activation().filter(|_| resp.double_clicked()) {
        actions.push(Action::Switch(name.clone(), plan));
    }
    a11y::describe(&resp, Painted::Tunnel { name, state: primary, group: group.map(groups::leaf), selected });
    menu::context_menu(&resp, keyboard_target(ui, l, selected, &resp), |ui| {
        let main = egui::Button::new(tr(primary.label()));
        let main = if primary == Primary::Connect { main.shortcut_text("Enter") } else { main };
        let toggle = primary.plan();
        let item = ui.add_enabled(toggle.is_some(), main);
        let item = match primary.hint() {
            Some(hint) => item.on_disabled_hover_text(tr(hint)),
            None => item,
        };
        let item = match primary.connect_hint(name, s.multiple, l.snap) {
            Some(hint) => item.on_hover_text(hint),
            None => item,
        };
        if let (true, Some(plan)) = (item.clicked(), toggle) {
            actions.push(Action::Switch(name.clone(), plan));
            ui.close();
        }
        if running && ui.add_enabled(!busy, egui::Button::new(tr("act.reconnect"))).clicked() {
            actions.push(Action::Switch(name.clone(), Plan::Reconnect));
            ui.close();
        }
        // Ядро не подключило туннель за 10 минут и пробует раз в 10 минут — повторить сразу, расписание с начала.
        if l.snap.retries.get(name).is_some_and(|r| r.slow) && ui.add_enabled(!busy, egui::Button::new(tr("act.retry"))).clicked() {
            actions.push(Action::Retry(name.clone()));
            ui.close();
        }
        ui.menu_button(tr("act.to_group"), |ui| {
            group_menu(ui, s, None, name, actions);
            if s.book.has_groups() {
                ui.separator();
            }
            let loose = s.book.group_of(name).is_none();
            if ui.add(egui::Button::new(tr("app.ungrouped")).selected(loose)).clicked() {
                actions.push(Action::Assign(name.clone(), None));
                ui.close();
            }
            if ui.button(tr("act.new_group")).clicked() {
                actions.push(Action::Open(Dialog::NewGroup { parent: None, name: String::new(), assign: Some(name.clone()) }));
                ui.close();
            }
        });
        if s.mode() == Mode::Engine {
            // Встроенный движок: туннели в своём хранилище, правка — во встроенном редакторе.
            if ui.button(tr("eng.edit")).clicked() {
                actions.push(Action::EngineEdit(name.clone()));
                ui.close();
            }
            if ui.add_enabled(!running, egui::Button::new(tr("eng.rename"))).on_disabled_hover_text(tr("eng.rename_hint")).clicked() {
                actions.push(Action::EngineRename(name.clone()));
                ui.close();
            }
            ui.separator();
            let delete = egui::Button::new(RichText::new(tr("del.tunnel_menu")).color(palette().error)).shortcut_text("Del");
            if ui.add(delete).clicked() {
                actions.push(Action::Confirm(Confirm::DeleteTunnel(name.clone())));
                ui.close();
            }
            return;
        }
        if ui.button(tr("act.edit_native")).clicked() {
            actions.push(Action::EditNative(name.clone()));
            ui.close();
        }
        ui.separator();
        // Источники — незашифрованные .conf пользователя, из которых туннели импортированы в AmneziaWG.
        if ui.button(tr("act.add_conf")).clicked() {
            actions.push(Action::AddConf);
            ui.close();
        }
        let source = s.book.source(name);
        let edit = ui.add_enabled(source.is_some(), egui::Button::new(tr("act.edit_source")));
        let edit = match source {
            Some(path) => edit.on_hover_text(path),
            None => edit.on_disabled_hover_text(tr("src.none")),
        };
        if edit.clicked() {
            actions.push(Action::EditSource(name.clone()));
            ui.close();
        }
        if source.is_none() {
            if ui.button(tr("act.set_source")).clicked() {
                actions.push(Action::SetSource(name.clone()));
                ui.close();
            }
        } else {
            // Источник привязан — пункт становится подменю синхронизации.
            ui.menu_button(tr("act.set_source"), |ui| {
                if ui.button(tr("sync.to_source")).on_hover_text(tr("sync.to_source_hint")).clicked() {
                    actions.push(Action::Confirm(Confirm::ToSource(name.clone())));
                    ui.close();
                }
                if ui.button(tr("sync.to_native")).on_hover_text(tr("sync.to_native_hint")).clicked() {
                    actions.push(Action::Confirm(Confirm::ToNative(name.clone())));
                    ui.close();
                }
                ui.separator();
                if ui.button(tr("sync.other_file")).clicked() {
                    actions.push(Action::SetSource(name.clone()));
                    ui.close();
                }
            });
        }
        ui.separator();
        let delete = egui::Button::new(RichText::new(tr("del.tunnel_menu")).color(palette().error)).shortcut_text("Del");
        if ui.add(delete).clicked() {
            actions.push(Action::Confirm(Confirm::DeleteTunnel(name.clone())));
            ui.close();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::theme::{DAYLIGHT, GRAPHITE, SLATE};

    /// Подсказка к имени группы показывается, когда имя обрезано: признак — `elided` того же galley, что рисуется.
    /// Было: длинное имя группы обрезалось многоточием без подсказки (у строк туннелей она есть).
    #[test]
    fn truncated_group_name_is_detected_for_its_tooltip() {
        let ctx = egui::Context::default();
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            let font = FontId::proportional(15.0);
            let long = truncated(ui, "Очень длинное имя группы туннелей", font.clone(), GRAPHITE.text_strong, 60.0);
            assert!(long.elided && long.size().x <= 60.0 + 1.0, "{}", long.size().x);
            assert!(!truncated(ui, "Дом", font, GRAPHITE.text_strong, 200.0).elided);
        });
    }

    #[test]
    fn active_tunnel_name_takes_the_colour_of_its_dot() {
        for p in [&GRAPHITE, &SLATE, &DAYLIGHT] {
            let of = |level| name_color(p, level, Primary::of(level, false), false);
            assert_eq!(of(Level::Ok), Some(p.connected));
            assert_eq!((of(Level::Busy), of(Level::Warn)), (Some(p.warning), Some(p.warning)), "подключается, переподключается");
            assert_eq!(of(Level::Ok), Some(p.level(Level::Ok)), "имя того же цвета, что кружок");
            assert_eq!(of(Level::Warn), Some(p.level(Level::Warn)));
            // Отключён — обычный текст; ошибка — красное имя, не только кружок (в выбранной строке — кольцо).
            assert_eq!(of(Level::Off), None);
            assert_eq!(of(Level::Bad), Some(p.error));
            assert_eq!(name_color(p, Level::Bad, Primary::of(Level::Bad, false), true), None, "красный на выделении нечитаем");
            // Нет связи с ядром: уровень Warn, кружок жёлтый, но туннель не включён — имя обычное.
            assert_eq!(name_color(p, Level::Warn, Primary::of(Level::Warn, true), false), None);
        }
    }
}
