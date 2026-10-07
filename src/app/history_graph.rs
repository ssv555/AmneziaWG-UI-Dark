//! График истории скорости (День, Месяц, Год) из ответа агента: средние за интервал — линии, пики — тонкие линии тех
//! же цветов. Интервала без данных агент не хранит — линия там рвётся, к нулю не спускается. Ось времени: часы для
//! суток, дни для месяца, месяцы для года. Шкала скоростей и пауза — общие с живым графиком (`graph`).

use std::ops::Range;

use eframe::egui::{self, Align2, FontId, Painter, Pos2, Rect, Response, RichText, Stroke, Ui};

use super::a11y::{self, Painted};
use super::graph::{scale_frame, top_note, y_of};
use super::history_feed::{Shown, REFRESH};
use super::theme::{mono, palette};
use crate::daemon::agent::proto::{HistoryBucket, HistoryRange};
use crate::fmt;
use crate::i18n::{tr, trf};

/// Полоса под кривыми для подписей оси времени.
const AXIS_H: f32 = 14.0;
const AXIS_FONT: f32 = 11.0;
/// Просвет между подписями оси времени, точек.
const AXIS_GAP: f32 = 16.0;
/// Яркость тонкой линии пиков относительно линии средних того же цвета.
const PEAK_FADE: f32 = 0.45;

/// Кадр графика истории.
pub(super) struct Plot<'a> {
    pub(super) rect: Rect,
    pub(super) range: HistoryRange,
    pub(super) shown: &'a Shown,
    pub(super) paused: bool,
    /// Курсор над кнопкой паузы — её подсказка, а не подсказка точки.
    pub(super) over_button: bool,
}

pub(super) fn plot(ui: &Ui, painter: &Painter, hover: &Response, p: &Plot) {
    let (history, at_unix) = match p.shown {
        Shown::Ready { history, at_unix } if !history.buckets.is_empty() => (history, *at_unix),
        other => return note(ui, painter, hover, p.rect, other),
    };
    // Ответ обновляется раз в минуту — кадр нужен и тогда, когда больше ничего не меняется.
    if !p.paused {
        ui.ctx().request_repaint_after(REFRESH);
    }
    let weak = ui.visuals().weak_text_color();
    let bucket_s = p.range.bucket_secs();
    let to = at_unix;
    let from = to.saturating_sub(span(p.range));
    let buckets: Vec<HistoryBucket> = history.buckets.iter().filter(|b| b.start + bucket_s > from && b.start <= to).cloned().collect();
    let max = buckets.iter().map(|b| b.peak_rx.max(b.peak_tx).max(b.rx).max(b.tx)).fold(1.0, f64::max);
    let (plot, scale) = scale_frame(painter, p.rect, max, AXIS_H, weak);
    let x_of = |t: u64| x_at(plot, from, to, t);

    paint_axis(painter, plot, p.range, from, to, weak);
    let mid = |b: &HistoryBucket| b.start + bucket_s / 2;
    let lines: [(fn(&HistoryBucket) -> f64, _, f32); 4] = [
        (|b| b.peak_tx, palette().graph_tx.gamma_multiply(PEAK_FADE), 1.0),
        (|b| b.peak_rx, palette().graph_rx.gamma_multiply(PEAK_FADE), 1.0),
        (|b| b.tx, palette().graph_tx, 1.5),
        (|b| b.rx, palette().graph_rx, 1.5),
    ];
    for run in runs(&buckets, bucket_s) {
        for (pick, color, width) in &lines {
            let points: Vec<Pos2> = buckets[run.clone()].iter().map(|b| Pos2::new(x_of(mid(b)), y_of(plot, scale, pick(b)))).collect();
            // Один интервал между пропусками — точка: линии из одной точки не видно.
            if points.len() == 1 {
                painter.circle_filled(points[0], *width, *color);
            } else {
                painter.add(egui::Shape::line(points, Stroke::new(*width, *color)));
            }
        }
    }
    top_note(painter, p.rect, trf("gr.hist_legend", &[&fmt::rate(max)]), weak);
    // Диктору — последний интервал и пик за показанный масштаб.
    let last = buckets.last().cloned().unwrap_or_default();
    let (rx, tx, peak) = (fmt::rate(last.rx), fmt::rate(last.tx), fmt::rate(max));
    a11y::describe(hover, Painted::Graph { rx: &rx, tx: &tx, peak: &peak, paused: p.paused });

    let Some(pos) = hover.hover_pos().filter(|pos| !p.over_button && plot.x_range().contains(pos.x)) else { return };
    let at_cursor = to as f64 - (plot.right() - pos.x) as f64 / plot.width().max(1.0) as f64 * (to - from) as f64;
    let Some(i) = nearest_bucket(&buckets, bucket_s, at_cursor) else { return };
    let b = &buckets[i];
    let x = x_of(mid(b));
    painter.vline(x, plot.y_range(), Stroke::new(1.0_f32, weak));
    painter.circle_filled(Pos2::new(x, y_of(plot, scale, b.rx)), 3.5, palette().graph_rx);
    painter.circle_filled(Pos2::new(x, y_of(plot, scale, b.tx)), 3.5, palette().graph_tx);
    let range = p.range;
    hover.clone().on_hover_ui_at_pointer(|ui| tooltip(ui, range, b));
}

/// Ширина масштаба, секунд: длина ряда агента.
fn span(range: HistoryRange) -> u64 {
    range.capacity() as u64 * range.bucket_secs()
}

/// x момента `t` на оси `[from, to]` в области `plot`; правее «сейчас» (текущий интервал) — у правого края.
fn x_at(plot: Rect, from: u64, to: u64, t: u64) -> f32 {
    let part = (t.min(to).saturating_sub(from)) as f64 / (to - from).max(1) as f64;
    plot.left() + part as f32 * plot.width()
}

/// Вместо кривых — одна строка: «загрузка», «истории пока нет» или «недоступна» (причина — в подсказке).
fn note(ui: &Ui, painter: &Painter, hover: &Response, rect: Rect, shown: &Shown) {
    let (text, why) = match shown {
        Shown::Loading => (tr("gr.hist_loading"), None),
        Shown::Unavailable(why) => (tr("gr.hist_unavailable"), Some(why.as_str())),
        Shown::Ready { .. } => (tr("gr.hist_empty"), None),
    };
    let weak = ui.visuals().weak_text_color();
    painter.text(rect.center(), Align2::CENTER_CENTER, &text, FontId::proportional(13.0), weak);
    a11y::describe(hover, Painted::GraphNote { text: &text });
    if let Some(why) = why.filter(|w| !w.is_empty()) {
        hover.clone().on_hover_text(why);
    }
}

/// Отрезки ряда без пропусков: соседние интервалы идут вплотную (`start` через `bucket_s`). Пропуск — разрыв линии,
/// а не спуск к нулю: агент не хранит интервалов без данных.
pub(super) fn runs(buckets: &[HistoryBucket], bucket_s: u64) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut begin = 0;
    for i in 1..buckets.len() {
        if buckets[i].start != buckets[i - 1].start + bucket_s {
            out.push(begin..i);
            begin = i;
        }
    }
    if !buckets.is_empty() {
        out.push(begin..buckets.len());
    }
    out
}

/// Интервал, ближайший к моменту `t`: середина не дальше одного интервала. В пропуске — `None` (подсказки нет).
pub(super) fn nearest_bucket(buckets: &[HistoryBucket], bucket_s: u64, t: f64) -> Option<usize> {
    let distance = |b: &HistoryBucket| (b.start as f64 + bucket_s as f64 / 2.0 - t).abs();
    let (i, b) = buckets.iter().enumerate().min_by(|a, b| distance(a.1).total_cmp(&distance(b.1)))?;
    (distance(b) <= bucket_s as f64).then_some(i)
}

/// Время интервала в подсказке: начало с минутами для суток и месяца; для года — дата суток UTC (такие интервалы
/// у агента).
pub(super) fn bucket_time(range: HistoryRange, start: u64) -> String {
    match range {
        HistoryRange::Day | HistoryRange::Month => fmt::date_time(start),
        HistoryRange::Year => fmt::utc_date(start),
    }
}

/// Подпись деления оси: час (`14:00`), дата (`2026.10.05`) или месяц (`2026.10`).
pub(super) fn axis_label(range: HistoryRange, t: u64) -> String {
    match range {
        HistoryRange::Day => fmt::time(t),
        HistoryRange::Month => fmt::date(t),
        HistoryRange::Year => fmt::utc_month(t),
    }
}

/// Деления оси времени в `[from, to]`: часы (День) и местные полночи (Месяц) — с самым мелким шагом, при котором их не
/// больше `max_marks`; начала месяцев UTC (Год, как интервалы агента) — каждый первый, второй, третий или шестой.
/// Смещение пояса берётся на `to`: в сутки перехода на летнее время деления до перехода сдвинуты на час.
pub(super) fn axis_marks(range: HistoryRange, from: u64, to: u64, max_marks: usize) -> Vec<u64> {
    if max_marks == 0 || from >= to {
        return Vec::new();
    }
    match range {
        HistoryRange::Day => local_marks(from, to, &[1, 2, 3, 4, 6, 12].map(|h| h * 3600), max_marks),
        HistoryRange::Month => local_marks(from, to, &[1, 2, 7, 14].map(|d| d * 86_400), max_marks),
        HistoryRange::Year => {
            let starts = fmt::utc_month_starts(from, to);
            let every = |k: u32| starts.iter().filter(|s| (s.1 - 1) % k == 0).map(|s| s.0).collect::<Vec<_>>();
            [1, 2, 3, 6].into_iter().map(&every).find(|marks| marks.len() <= max_marks).unwrap_or_else(|| every(12))
        }
    }
}

/// Кратные `step` по местному времени в `[from, to]` для первого шага из `steps`, при котором их не больше `max`.
fn local_marks(from: u64, to: u64, steps: &[u64], max: usize) -> Vec<u64> {
    let offset = fmt::utc_offset(to);
    let marks = |step: u64| -> Vec<u64> {
        let step = step as i64;
        let first = (from as i64 + offset + step - 1).div_euclid(step) * step - offset;
        (0..).map(|k| first + k * step).take_while(|t| *t <= to as i64).filter(|t| *t >= from as i64).map(|t| t as u64).collect()
    };
    let largest = steps.last().copied().unwrap_or(3600);
    steps.iter().map(|s| marks(*s)).find(|m| m.len() <= max).unwrap_or_else(|| marks(largest))
}

/// Деления и подписи оси времени под кривыми; подпись, что вылезла бы за правый край, не рисуется.
fn paint_axis(painter: &Painter, plot: Rect, range: HistoryRange, from: u64, to: u64, weak: egui::Color32) {
    let font = FontId::proportional(AXIS_FONT);
    let widest = painter.layout_no_wrap(axis_label(range, to), font.clone(), weak).size().x;
    let max_marks = (plot.width() / (widest + AXIS_GAP)).floor().max(0.0) as usize;
    for t in axis_marks(range, from, to, max_marks) {
        let x = x_at(plot, from, to, t);
        painter.vline(x, plot.y_range(), Stroke::new(1.0_f32, palette().border));
        let galley = painter.layout_no_wrap(axis_label(range, t), font.clone(), weak);
        if x + 2.0 + galley.size().x <= plot.right() {
            painter.galley(Pos2::new(x + 2.0, plot.bottom() + 1.0), galley, weak);
        }
    }
}

/// Подсказка интервала: время, средние и пики приёма и передачи, сколько туннель был подключён внутри интервала.
fn tooltip(ui: &mut Ui, range: HistoryRange, b: &HistoryBucket) {
    ui.label(RichText::new(bucket_time(range, b.start)).weak());
    egui::Grid::new("history-tip").num_columns(3).spacing([16.0, 2.0]).show(ui, |ui| {
        ui.label("");
        ui.label(RichText::new(tr("gr.avg")).weak());
        ui.label(RichText::new(tr("gr.peak_col")).weak());
        ui.end_row();
        for (title, avg, peak, color) in [("st.rx_rate", b.rx, b.peak_rx, palette().graph_rx), ("st.tx_rate", b.tx, b.peak_tx, palette().graph_tx)] {
            ui.label(&tr(title));
            ui.label(mono(fmt::rate(avg), color));
            ui.label(mono(fmt::rate(peak), color));
            ui.end_row();
        }
    });
    ui.label(RichText::new(trf("gr.connected", &[&fmt::duration(b.secs)])).weak());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(starts: &[u64]) -> Vec<HistoryBucket> {
        starts.iter().map(|s| HistoryBucket { start: *s, secs: 60.0, rx: 1.0, tx: 1.0, peak_rx: 2.0, peak_tx: 2.0 }).collect()
    }

    #[test]
    fn gaps_split_the_lines() {
        assert!(runs(&[], 60).is_empty());
        assert_eq!(runs(&at(&[0, 60, 120]), 60), [0..3]);
        // Пропуск в две минуты и одиночный интервал между пропусками — отдельные отрезки.
        assert_eq!(runs(&at(&[0, 60, 240, 360, 420]), 60), [0..2, 2..3, 3..5]);
        assert_eq!(runs(&at(&[0, 86_400, 3 * 86_400]), 86_400), [0..2, 2..3]);
    }

    #[test]
    fn tooltip_finds_the_nearest_bucket_but_not_inside_a_gap() {
        let b = at(&[0, 60, 600]);
        assert_eq!(nearest_bucket(&b, 60, 35.0), Some(0));
        assert_eq!(nearest_bucket(&b, 60, 95.0), Some(1));
        assert_eq!(nearest_bucket(&b, 60, 620.0), Some(2));
        assert_eq!(nearest_bucket(&b, 60, 350.0), None, "середина пропуска");
        assert_eq!(nearest_bucket(&[], 60, 0.0), None);
    }

    /// Форма подписей, а не цифры: часовой пояс машины сдвигает час и дату.
    fn shape(s: &str, pattern: &str) {
        assert_eq!(s.len(), pattern.len(), "{s}");
        for (c, p) in s.chars().zip(pattern.chars()) {
            assert!(if p == '9' { c.is_ascii_digit() } else { c == p }, "{s} против {pattern}");
        }
    }

    #[test]
    fn axis_labels_fit_the_range() {
        let t = 1_791_209_253; // 2026-10-05 14:07:33 UTC
        shape(&axis_label(HistoryRange::Day, t), "99:99");
        shape(&axis_label(HistoryRange::Month, t), "9999.99.99");
        assert_eq!(axis_label(HistoryRange::Year, t), "2026.10");
        shape(&bucket_time(HistoryRange::Day, t), "9999.99.99 99:99");
        shape(&bucket_time(HistoryRange::Month, t), "9999.99.99 99:99");
        assert_eq!(bucket_time(HistoryRange::Year, t - t % 86_400), "2026.10.05", "сутки UTC");
    }

    #[test]
    fn axis_marks_are_hours_days_and_months() {
        let to = 1_791_209_253;
        let day = axis_marks(HistoryRange::Day, to - span(HistoryRange::Day), to, 8);
        assert!((6..=8).contains(&day.len()), "{day:?}");
        let step = day[1] - day[0];
        assert_eq!(step % 3600, 0);
        for t in &day {
            assert_eq!((*t as i64 + fmt::utc_offset(to)).rem_euclid(step as i64), 0, "ровный местный час");
            assert!(axis_label(HistoryRange::Day, *t).ends_with(":00"), "{}", axis_label(HistoryRange::Day, *t));
        }
        // Тесно — шаг крупнее; совсем некуда — делений нет.
        assert!(axis_marks(HistoryRange::Day, to - 86_400, to, 2).len() <= 2);
        assert!(axis_marks(HistoryRange::Day, to - 86_400, to, 0).is_empty());

        let month = axis_marks(HistoryRange::Month, to - span(HistoryRange::Month), to, 6);
        assert!((3..=6).contains(&month.len()), "{month:?}");
        for t in &month {
            assert_eq!((*t as i64 + fmt::utc_offset(to)).rem_euclid(86_400), 0, "местная полночь");
        }

        let year = axis_marks(HistoryRange::Year, to - span(HistoryRange::Year), to, 12);
        assert_eq!(year.len(), 12, "каждый месяц");
        assert!(year.iter().all(|t| fmt::utc_date(*t).ends_with(".01")));
        let quarters = axis_marks(HistoryRange::Year, to - span(HistoryRange::Year), to, 4);
        let names: Vec<_> = quarters.iter().map(|t| axis_label(HistoryRange::Year, *t)).collect();
        assert_eq!(names, ["2026.01", "2026.04", "2026.07", "2026.10"], "шаг три месяца, от января");
    }

    #[test]
    fn time_axis_runs_left_to_right_and_keeps_the_current_bucket_inside() {
        let plot = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(100.0, 50.0));
        assert_eq!(x_at(plot, 1000, 2000, 1000), 0.0);
        assert_eq!(x_at(plot, 1000, 2000, 1500), 50.0);
        assert_eq!(x_at(plot, 1000, 2000, 2030), 100.0, "середина текущего интервала правее «сейчас»");
    }
}
