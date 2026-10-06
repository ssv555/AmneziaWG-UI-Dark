//! График скорости и полоса пинга под ним.

use eframe::egui::{self, Align2, FontId, Pos2, RichText, Sense, Stroke, Ui, Vec2};

use super::a11y::{self, Painted};
use super::theme::{mono, palette};
use crate::fmt;
use crate::i18n::{tr, trf};
use crate::monitor::Live;
use crate::ping::PingState;
use crate::settings::Settings;

const GRAPH_MIN_H: f32 = 40.0;
const GRAPH_MAX_H: f32 = 600.0;
const PING_STRIP_H: f32 = 50.0;
/// Высота подписи над точками полосы пинга.
const LEGEND_H: f32 = 24.0;
/// Верх графика = пик × запас.
const GRAPH_HEADROOM: f64 = 1.15;
const PERIODS: [(u32, &str); 3] = [(120, "period.2m"), (600, "period.10m"), (3600, "period.1h")];

/// Максимум по корзинам — пики не теряются при сжатии часа в ширину графика.
pub(super) fn bucket_max(series: &[(f64, f64)], buckets: usize) -> Vec<(f64, f64)> {
    if buckets == 0 || series.len() <= buckets {
        return series.to_vec();
    }
    let k = series.len().div_ceil(buckets);
    series.chunks(k).map(|c| c.iter().fold((0.0f64, 0.0f64), |a, s| (a.0.max(s.0), a.1.max(s.1)))).collect()
}

/// График скорости (вход — зелёный, выход — синий), под ним полоса пинга. Высота тянется за нижний край.
pub(super) fn graph(ui: &mut Ui, live: &Live, ping: Option<&PingState>, s: &mut Settings) {
    ui.horizontal(|ui| {
        ui.weak(tr("gr.speed"));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            for (secs, title) in PERIODS.iter().rev() {
                ui.selectable_value(&mut s.graph_period, *secs, tr(title));
            }
        });
    });
    let period = s.graph_period.max(60) as f64;
    let height = s.graph_height.clamp(GRAPH_MIN_H, GRAPH_MAX_H);
    let (rect, hover) = ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, egui::CornerRadius::same(4), ui.visuals().extreme_bg_color);
    painter.hline(rect.x_range(), rect.center().y, Stroke::new(1.0_f32, ui.visuals().faint_bg_color));
    let series = live.rate_series(period);
    let points = bucket_max(&series, rect.width() as usize);
    let max = points.iter().map(|(r, t)| r.max(*t)).fold(1.0, f64::max);
    // Запас сверху, чтобы пик не упирался в подпись.
    let scale = max * GRAPH_HEADROOM;
    // Замер раз в секунду; после сжатия одна точка покрывает `per_point` секунд.
    let per_point = if series.len() > points.len() { series.len().div_ceil(points.len().max(1)) } else { 1 };
    let step = rect.width() / period as f32 * per_point as f32;
    let line = |pick: fn(&(f64, f64)) -> f64| -> Vec<Pos2> {
        points
            .iter()
            .rev()
            .enumerate()
            .map(|(i, p)| Pos2::new(rect.right() - i as f32 * step, rect.bottom() - 4.0 - (pick(p) / scale) as f32 * (rect.height() - 8.0)))
            .collect()
    };
    if points.len() >= 2 {
        painter.add(egui::Shape::line(line(|p| p.1), Stroke::new(1.5_f32, palette().graph_tx)));
        painter.add(egui::Shape::line(line(|p| p.0), Stroke::new(1.5_f32, palette().graph_rx)));
    }
    painter.text(
        rect.left_top() + Vec2::new(6.0, 4.0),
        Align2::LEFT_TOP,
        trf("gr.peak", &[&fmt::rate(max)]),
        FontId::proportional(12.0),
        ui.visuals().weak_text_color(),
    );
    // Диктору — то, что зрячий видит с одного взгляда: текущие приём и передача (как в карточке, за 3 с) и пик.
    let (rx_now, tx_now) = live.rate(3.0);
    let (rx, tx, peak) = (fmt::rate(rx_now), fmt::rate(tx_now), fmt::rate(max));
    a11y::describe(&hover, Painted::Graph { rx: &rx, tx: &tx, peak: &peak });

    // Наведение: ближайшая точка — вертикальная линия, точки на кривых, подсказка со значениями.
    if let Some(pos) = hover.hover_pos() {
        if let Some(i) = nearest_from_right(rect.right() - pos.x, step, points.len()) {
            let p = points[points.len() - 1 - i];
            let x = rect.right() - i as f32 * step;
            let y = |v: f64| rect.bottom() - 4.0 - (v / scale) as f32 * (rect.height() - 8.0);
            painter.vline(x, rect.y_range(), Stroke::new(1.0_f32, ui.visuals().weak_text_color()));
            painter.circle_filled(Pos2::new(x, y(p.0)), 3.5, palette().graph_rx);
            painter.circle_filled(Pos2::new(x, y(p.1)), 3.5, palette().graph_tx);
            let when = fmt::ago((i * per_point) as u64);
            hover.on_hover_ui_at_pointer(|ui| {
                ui.label(RichText::new(when).weak());
                if per_point > 1 {
                    ui.label(RichText::new(trf("gr.bucket", &[&fmt::duration(per_point as f64)])).weak());
                }
                egui::Grid::new("graph-tip").num_columns(2).spacing([16.0, 2.0]).show(ui, |ui| {
                    ui.label(&tr("st.rx_rate"));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| ui.label(mono(fmt::rate(p.0), palette().graph_rx)));
                    ui.end_row();
                    ui.label(&tr("st.tx_rate"));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| ui.label(mono(fmt::rate(p.1), palette().graph_tx)));
                    ui.end_row();
                });
            });
        }
    }

    if let Some(ping) = ping {
        ui.add_space(4.0);
        ping_strip(ui, ping, period);
    }

    // Ручка изменения высоты под графиком.
    let (handle, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 8.0), Sense::drag());
    if resp.hovered() || resp.dragged() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
        ui.painter().hline(handle.x_range(), handle.center().y, Stroke::new(2.0_f32, ui.visuals().weak_text_color()));
    }
    if resp.dragged() {
        s.graph_height = (s.graph_height + resp.drag_delta().y).clamp(GRAPH_MIN_H, GRAPH_MAX_H);
    }
}

/// Пинг не прошёл: последнее удачное значение и «н/д», если оно было, иначе только «н/д».
pub(super) fn stale_ping(ping: &PingState) -> String {
    let na = tr("st.na");
    match ping.history.iter().rev().find_map(|(_, ms)| *ms) {
        Some(ms) => format!("{} ({na})", trf("unit.ms", &[&ms.to_string()])),
        None => na,
    }
}

fn ping_strip(ui: &mut Ui, ping: &PingState, period: f64) {
    let (rect, hover) = ui.allocate_exact_size(Vec2::new(ui.available_width(), PING_STRIP_H), Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, egui::CornerRadius::same(4), ui.visuals().extreme_bg_color);
    let recent: Vec<(f64, Option<u32>)> = ping
        .history
        .iter()
        .map(|(at, ms)| (at.elapsed().as_secs_f64(), *ms))
        .filter(|(age, _)| *age <= period)
        .collect();
    let max = recent.iter().filter_map(|(_, ms)| *ms).max().unwrap_or(1).max(1) as f32;
    for (age, ms) in &recent {
        let x = rect.right() - (*age / period) as f32 * rect.width();
        match ms {
            Some(ms) => {
                let y = rect.bottom() - 4.0 - (*ms as f32 / max) * (rect.height() - 4.0 - LEGEND_H);
                painter.circle_filled(Pos2::new(x, y), 2.5, palette().graph_ping);
            }
            None => {
                painter.vline(x, rect.y_range(), Stroke::new(2.0_f32, palette().error));
            }
        }
    }
    painter.text(
        rect.left_top() + Vec2::new(6.0, 3.0),
        Align2::LEFT_TOP,
        trf("gr.ping_legend", &[&trf("unit.ms", &[&format!("{max:.0}")])]),
        FontId::proportional(12.0),
        ui.visuals().weak_text_color(),
    );
    let last = match ping.last {
        Some(Ok(ms)) => trf("unit.ms", &[&ms.to_string()]),
        _ => stale_ping(ping),
    };
    a11y::describe(&hover, Painted::Ping { host: &ping.host, last: &last });

    // Наведение: ближайший замер по времени.
    let Some(pos) = hover.hover_pos() else { return };
    let age_at_cursor = (rect.right() - pos.x) as f64 / rect.width() as f64 * period;
    let Some((age, ms)) = recent.iter().min_by(|a, b| (a.0 - age_at_cursor).abs().total_cmp(&(b.0 - age_at_cursor).abs())) else {
        return;
    };
    let x = rect.right() - (*age / period) as f32 * rect.width();
    painter.vline(x, rect.y_range(), Stroke::new(1.0_f32, ui.visuals().weak_text_color()));
    let (text, color) = match ms {
        Some(ms) => (trf("unit.ms", &[&ms.to_string()]), palette().graph_ping),
        None => (tr("st.no_reply"), palette().error),
    };
    let when = fmt::ago(*age as u64);
    hover.on_hover_ui_at_pointer(|ui| {
        ui.label(RichText::new(when).weak());
        ui.horizontal(|ui| {
            ui.label(trf("st.ping_to", &[&ping.host]));
            ui.label(mono(text, color));
        });
    });
}

/// Индекс точки (отсчёт от правого края), ближайшей к курсору на расстоянии `dx` от правого края.
fn nearest_from_right(dx: f32, step: f32, len: usize) -> Option<usize> {
    if len == 0 || step <= 0.0 || dx < -step / 2.0 {
        return None;
    }
    let i = (dx / step).round().max(0.0) as usize;
    (i < len).then_some(i)
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    #[test]
    fn failed_ping_shows_last_value_and_na() {
        let now = Instant::now();
        let mut ping = PingState::default();
        assert_eq!(stale_ping(&ping), "n/a");
        ping.history.extend([(now, Some(47)), (now, None), (now, None)]);
        assert_eq!(stale_ping(&ping), "47 ms (n/a)");
    }

    #[test]
    fn bucket_keeps_peaks() {
        let series: Vec<(f64, f64)> = (0..100).map(|i| (if i == 37 { 1000.0 } else { 1.0 }, 0.0)).collect();
        let b = bucket_max(&series, 10);
        assert_eq!(b.len(), 10);
        assert_eq!(b.iter().map(|p| p.0).fold(0.0, f64::max), 1000.0);
        assert_eq!(bucket_max(&series[..5], 10).len(), 5);
    }

    #[test]
    fn hover_picks_nearest_point() {
        assert_eq!(nearest_from_right(0.0, 10.0, 5), Some(0));
        assert_eq!(nearest_from_right(14.0, 10.0, 5), Some(1));
        assert_eq!(nearest_from_right(16.0, 10.0, 5), Some(2));
        // Левее самой старой точки — подсказки нет.
        assert_eq!(nearest_from_right(100.0, 10.0, 5), None);
        assert_eq!(nearest_from_right(5.0, 10.0, 0), None);
    }
}
