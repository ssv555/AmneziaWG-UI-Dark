//! Строки таблицы туннелей: группа, туннель, контекстные меню и перетаскивание.

use std::sync::Arc;

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2};

use crate::app::theme::{level_color, BLUE, NUM_FONT, RED};
use crate::app::{Action, Confirm, Dialog};
use crate::daemon::proto::Plan;
use crate::groups::{self, Agg, Verdict, UNGROUPED};
use crate::health::{Health, Level};
use crate::i18n::{tr, trf};
use crate::settings::{Mode, Settings, SortKey};
use crate::stats::{self, TunnelStats};

use super::{activation_plan, cell_value, cells, scroll_flag, Drag, List, INDENT, ROW_H};

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
            ui.painter().rect_stroke(area, 3.0, Stroke::new(1.5_f32, RED), egui::StrokeKind::Inside);
            ui.ctx().set_cursor_icon(egui::CursorIcon::NotAllowed);
        }
        Verdict::Valid => {
            ui.painter().rect_filled(area, 3.0, BLUE.gamma_multiply(0.15));
            ui.painter().rect_stroke(area, 3.0, Stroke::new(1.5_f32, BLUE), egui::StrokeKind::Inside);
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

fn truncated(ui: &Ui, text: &str, font: FontId, color: Color32, width: f32) -> Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::single_section(text.to_string(), egui::TextFormat::simple(font, color));
    job.wrap = egui::text::TextWrapping::truncate_at_width(width.max(10.0));
    ui.fonts(|f| f.layout_job(job))
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
        painter.rect_filled(row, 2.0, ui.visuals().selection.bg_fill);
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
        let active = l.healths.get(t).is_some_and(|h| h.level != Level::Off);
        agg.add(active, st.rx, st.tx, st.peak_rx, stats::share(l.stats, t));
    }
    let shown = if real { groups::leaf(g.key).to_string() } else { tr("app.ungrouped") };
    let count = format!("{}/{}", agg.active, agg.total);
    let count_w = 12.0 + 9.0 * count.chars().count() as f32;
    let name_left = x0 + 22.0;
    let galley = truncated(ui, &shown, FontId::proportional(15.0), strong, c.name.right() - name_left - count_w - 8.0);
    let name_w = galley.size().x;
    painter.galley(Pos2::new(name_left, row.center().y - galley.size().y / 2.0), galley, strong);
    painter.text(Pos2::new(name_left + name_w + 12.0, row.center().y), Align2::LEFT_CENTER, count, FontId::monospace(12.5), weak);
    let mut sum = TunnelStats::default();
    sum.rx = agg.rx;
    sum.tx = agg.tx;
    sum.peak_rx = agg.peak;
    for (key, rect) in &c.nums {
        let text = cell_value(*key, &sum, agg.share);
        painter.text(rect.right_center() - Vec2::new(8.0, 0.0), Align2::RIGHT_CENTER, text, FontId::monospace(NUM_FONT), strong);
    }

    if real {
        resp.dnd_set_drag_payload(Drag::Group(g.key.to_string()));
    }
    drop_target(ui, &resp, row, real.then_some(g.key), s, actions);
    if resp.clicked() {
        actions.push(Action::ToggleCollapse(g.key.to_string()));
        actions.push(Action::SelectGroup(g.key.to_string()));
    }
    if !real {
        return;
    }
    let path = g.key;
    resp.context_menu(|ui| {
        if ui.button(tr("grp.new_sub")).clicked() {
            actions.push(Action::Open(Dialog::NewGroup { parent: Some(path.to_string()), name: String::new(), assign: None }));
            ui.close_menu();
        }
        if ui.add(egui::Button::new(tr("grp.rename")).shortcut_text("F2")).clicked() {
            actions.push(Action::Open(Dialog::Rename { old: path.to_string(), name: groups::leaf(path).to_string() }));
            ui.close_menu();
        }
        ui.separator();
        if ui.add_enabled(s.book.can_move(path, -1), egui::Button::new(tr("grp.up"))).clicked() {
            actions.push(Action::MoveGroup(path.to_string(), -1));
            ui.close_menu();
        }
        if ui.add_enabled(s.book.can_move(path, 1), egui::Button::new(tr("grp.down"))).clicked() {
            actions.push(Action::MoveGroup(path.to_string(), 1));
            ui.close_menu();
        }
        if g.depth > 0 {
            let verdict = s.book.check_reparent(path, None);
            let top = ui.add_enabled(verdict == Verdict::Valid, egui::Button::new(tr("grp.to_top")));
            if top.on_disabled_hover_text(tr("grp.to_top_taken")).clicked() {
                actions.push(Action::Reparent(path.to_string(), None));
                ui.close_menu();
            }
        }
        ui.separator();
        if ui.add(egui::Button::new(tr("grp.delete")).shortcut_text("Del")).clicked() {
            actions.push(Action::Confirm(Confirm::DeleteGroup(path.to_string())));
            ui.close_menu();
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
            ui.close_menu();
        }
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
    let h = l.healths.get(name).cloned().unwrap_or(Health { level: Level::Off, text: String::new() });
    let sense = if s.view.groups { Sense::click_and_drag() } else { Sense::click() };
    let (row, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H), sense);
    let selected = s.book.is_tunnel_highlighted(name);
    if selected {
        scroll_if_flagged(ui, row);
    }
    let painter = ui.painter_at(row);
    if selected {
        painter.rect_filled(row, 2.0, ui.visuals().selection.bg_fill);
    } else if resp.hovered() {
        painter.rect_filled(row, 2.0, ui.visuals().widgets.hovered.weak_bg_fill);
    }
    let text_color = if selected { ui.visuals().selection.stroke.color } else { ui.visuals().text_color() };
    guides(ui, &painter, row, depth);
    let indent = depth as f32 * INDENT + 6.0;
    let c = cells(row, cols);
    painter.circle_filled(Pos2::new(c.name.left() + indent + 5.0, row.center().y), 5.0, level_color(h.level));
    let galley = truncated(ui, name, FontId::proportional(15.0), text_color, c.name.width() - indent - 22.0);
    painter.galley(Pos2::new(c.name.left() + indent + 16.0, row.center().y - galley.size().y / 2.0), galley, text_color);
    let st = l.stats.get(name).cloned().unwrap_or_default();
    let share = stats::share(l.stats, name);
    for (key, rect) in &c.nums {
        painter.text(
            rect.right_center() - Vec2::new(8.0, 0.0),
            Align2::RIGHT_CENTER,
            cell_value(*key, &st, share),
            FontId::monospace(NUM_FONT),
            text_color,
        );
    }
    if s.view.groups {
        resp.dnd_set_drag_payload(Drag::Tunnel(name.clone()));
        // Брошенное на туннель попадает в его группу.
        drop_target(ui, &resp, row, group, s, actions);
    }
    let resp = resp.on_hover_text(format!("{name}\n{}", h.text));
    if resp.clicked() {
        actions.push(Action::Select(name.clone()));
    }
    let running = h.level != Level::Off;
    let busy = h.level == Level::Busy;
    let plan = if running { Plan::Disconnect } else { Plan::Connect };
    if let Some(plan) = activation_plan(h.level).filter(|_| resp.double_clicked()) {
        actions.push(Action::Switch(name.clone(), plan));
    }
    resp.context_menu(|ui| {
        let main = egui::Button::new(tr(if running { "act.disconnect" } else { "act.connect" }));
        let main = if running { main } else { main.shortcut_text("Enter") };
        if ui.add_enabled(!busy, main).clicked() {
            actions.push(Action::Switch(name.clone(), plan));
            ui.close_menu();
        }
        if running && ui.add_enabled(!busy, egui::Button::new(tr("act.reconnect"))).clicked() {
            actions.push(Action::Switch(name.clone(), Plan::Reconnect));
            ui.close_menu();
        }
        ui.menu_button(tr("act.to_group"), |ui| {
            group_menu(ui, s, None, name, actions);
            if s.book.has_groups() {
                ui.separator();
            }
            let loose = s.book.group_of(name).is_none();
            if ui.add(egui::Button::new(tr("app.ungrouped")).selected(loose)).clicked() {
                actions.push(Action::Assign(name.clone(), None));
                ui.close_menu();
            }
            if ui.button(tr("act.new_group")).clicked() {
                actions.push(Action::Open(Dialog::NewGroup { parent: None, name: String::new(), assign: Some(name.clone()) }));
                ui.close_menu();
            }
        });
        if s.mode() == Mode::Engine {
            // Встроенный движок: туннели в своём хранилище, правка — во встроенном редакторе.
            if ui.button(tr("eng.edit")).clicked() {
                actions.push(Action::EngineEdit(name.clone()));
                ui.close_menu();
            }
            if ui.add_enabled(!running, egui::Button::new(tr("eng.rename"))).on_disabled_hover_text(tr("eng.rename_hint")).clicked() {
                actions.push(Action::EngineRename(name.clone()));
                ui.close_menu();
            }
            ui.separator();
            let delete = egui::Button::new(RichText::new(tr("del.tunnel_menu")).color(RED)).shortcut_text("Del");
            if ui.add(delete).clicked() {
                actions.push(Action::Confirm(Confirm::DeleteTunnel(name.clone())));
                ui.close_menu();
            }
            return;
        }
        if ui.button(tr("act.edit_native")).clicked() {
            actions.push(Action::EditNative(name.clone()));
            ui.close_menu();
        }
        ui.separator();
        // Источники — незашифрованные .conf пользователя, из которых туннели импортированы в AmneziaWG.
        if ui.button(tr("act.add_conf")).clicked() {
            actions.push(Action::AddConf);
            ui.close_menu();
        }
        let source = s.book.source(name);
        let edit = ui.add_enabled(source.is_some(), egui::Button::new(tr("act.edit_source")));
        let edit = match source {
            Some(path) => edit.on_hover_text(path),
            None => edit.on_disabled_hover_text(tr("src.none")),
        };
        if edit.clicked() {
            actions.push(Action::EditSource(name.clone()));
            ui.close_menu();
        }
        if source.is_none() {
            if ui.button(tr("act.set_source")).clicked() {
                actions.push(Action::SetSource(name.clone()));
                ui.close_menu();
            }
        } else {
            // Источник привязан — пункт становится подменю синхронизации.
            ui.menu_button(tr("act.set_source"), |ui| {
                if ui.button(tr("sync.to_source")).on_hover_text(tr("sync.to_source_hint")).clicked() {
                    actions.push(Action::Confirm(Confirm::ToSource(name.clone())));
                    ui.close_menu();
                }
                if ui.button(tr("sync.to_native")).on_hover_text(tr("sync.to_native_hint")).clicked() {
                    actions.push(Action::Confirm(Confirm::ToNative(name.clone())));
                    ui.close_menu();
                }
                ui.separator();
                if ui.button(tr("sync.other_file")).clicked() {
                    actions.push(Action::SetSource(name.clone()));
                    ui.close_menu();
                }
            });
        }
        ui.separator();
        let delete = egui::Button::new(RichText::new(tr("del.tunnel_menu")).color(RED)).shortcut_text("Del");
        if ui.add(delete).clicked() {
            actions.push(Action::Confirm(Confirm::DeleteTunnel(name.clone())));
            ui.close_menu();
        }
    });
}
