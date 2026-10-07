//! График скорости и полоса пинга под ним: масштаб выпадающим списком над графиком, шкала скоростей справа, кнопка
//! паузы в правом верхнем углу. 2 мин, 10 мин и 1 ч — замеры окна; День, Месяц и Год — история агента
//! (`history_feed`, рисует `history_graph`).

use std::time::Instant;

use eframe::egui::{self, Align2, Color32, FontId, Painter, Pos2, Rect, Response, RichText, Sense, Stroke, Ui, Vec2};

use super::a11y::{self, Painted};
use super::history_feed::{HistoryFeed, Shown};
use super::history_graph;
use super::theme::{mono, palette};
use crate::daemon::agent::proto::HistoryRange;
use crate::fmt;
use crate::i18n::{tr, trf};
use crate::monitor::Live;
use crate::ping::PingState;
use crate::settings::{GraphRange, Settings};

const GRAPH_MIN_H: f32 = 40.0;
const GRAPH_MAX_H: f32 = 600.0;
const PING_STRIP_H: f32 = 50.0;
/// Высота подписи над точками полосы пинга.
const LEGEND_H: f32 = 24.0;
/// Верх графика = пик × запас.
pub(super) const GRAPH_HEADROOM: f64 = 1.15;
/// Полоса над кривыми: подпись пика, метка паузы и кнопка паузы. Кривые туда не заходят — ничего не закрыто.
const TOP_BAND_H: f32 = 20.0;
const PAUSE_BUTTON: f32 = 16.0;
/// Отступ подписей шкалы от правого края и от кривых.
const SCALE_PAD: f32 = 4.0;
const SCALE_FONT: f32 = 11.0;
/// Делений шкалы вместе с нулём — не больше.
const MAX_TICKS: usize = 5;

/// Деление шкалы: значение, байт/с, и подпись в единице шкалы.
#[derive(Debug, PartialEq)]
pub(super) struct Tick {
    pub(super) value: f64,
    pub(super) label: String,
}

/// Круглые деления шкалы от 0 до `top` байт/с: шаг 1, 2 или 5 × 10ⁿ в одной единице (`fmt::unit_of(top)`, как у
/// значений рядом), самый мелкий, при котором делений вместе с нулём не больше `MAX_TICKS`. В байтах шаг не меньше 1:
/// долей байта шкала не показывает. Не число или не больше нуля — делений нет.
pub(super) fn nice_ticks(top: f64) -> Vec<Tick> {
    if !(top.is_finite() && top > 0.0) {
        return Vec::new();
    }
    let unit = fmt::unit_of(top);
    let size = fmt::unit_size(unit);
    let top_u = top / size;
    // Начать на порядок мельче верха: шаг 5 × 10ⁿ⁻² даёт больше 20 делений, мельче искать незачем.
    let mut exp = top_u.log10().floor() as i32 - 1;
    let (step, exp) = loop {
        let fits = |s: &f64| ((top_u / s).floor() as usize) < MAX_TICKS && (unit > 0 || *s >= 1.0);
        if let Some(s) = [1.0, 2.0, 5.0].iter().map(|m| m * 10f64.powi(exp)).find(fits) {
            break (s, exp);
        }
        exp += 1;
    };
    let decimals = (-exp).max(0) as usize;
    let count = (top_u / step).floor() as usize;
    (0..=count)
        .map(|k| {
            let value = k as f64 * step * size;
            Tick { value, label: fmt::rate_in(value, unit, decimals) }
        })
        .collect()
}

/// Откуда график берёт данные: замеры окна за последние секунды или история агента.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Source {
    /// Замеры окна в памяти (раз в секунду, не дольше часа), за столько секунд.
    Live(u32),
    /// История, которую копит агент (`AgentApi::history`), и при закрытом окне.
    History(HistoryRange),
}

/// Источник масштаба: 2 мин, 10 мин и 1 ч — живые замеры окна; День, Месяц и Год — история агента.
pub(super) fn source(range: GraphRange) -> Source {
    match range {
        GraphRange::Min2 => Source::Live(120),
        GraphRange::Min10 => Source::Live(600),
        GraphRange::Hour1 => Source::Live(3600),
        GraphRange::Day => Source::History(HistoryRange::Day),
        GraphRange::Month => Source::History(HistoryRange::Month),
        GraphRange::Year => Source::History(HistoryRange::Year),
    }
}

fn range_title(range: GraphRange) -> String {
    tr(match range {
        GraphRange::Min2 => "period.2m",
        GraphRange::Min10 => "period.10m",
        GraphRange::Hour1 => "period.1h",
        GraphRange::Day => "period.day",
        GraphRange::Month => "period.month",
        GraphRange::Year => "period.year",
    })
}

/// Что показывает кадр: живые замеры за столько секунд или историю масштаба (ответ агента или снимок паузы).
enum Shows {
    Live(u32),
    History(HistoryRange, Shown),
}

/// Состояние графика на время работы окна (в настройки не пишется): пауза и история от агента.
pub(super) struct GraphState {
    pub(super) pause: GraphPause,
    pub(super) feed: HistoryFeed,
}

/// Пауза графика — снимок данных на момент нажатия, чтобы навести мышь на пик, пока он не уехал. Живёт, пока открыто
/// окно (в настройки не пишется); показан другой туннель — пауза снята. Опрос идёт дальше: снимок просто не обновляется.
/// История (День, Месяц, Год) замирает тоже — снимком того масштаба, на котором нажали: выбран масштаб с другим
/// источником — пауза снята; между 2 мин, 10 мин и 1 ч пауза стоит (снимок замеров покрывает час).
#[derive(Default)]
pub(super) struct GraphPause {
    frozen: Option<Frozen>,
}

struct Frozen {
    tunnel: String,
    range: GraphRange,
    live: Live,
    ping: Option<PingState>,
    history: Option<Shown>,
    at: Instant,
}

/// Что рисует кадр: живые данные или снимок паузы.
pub(super) struct GraphData<'a> {
    pub(super) live: &'a Live,
    pub(super) ping: Option<&'a PingState>,
    /// Момент, к которому отсчитаны возрасты точек: сейчас или момент паузы.
    pub(super) at: Instant,
    /// На сколько секунд снимок отстаёт от сейчас: подсказки называют настоящий возраст точки.
    pub(super) lag_secs: u64,
    pub(super) paused: bool,
}

impl GraphPause {
    /// Показан другой туннель или выбран масштаб с другим источником — пауза снята: снимок одного туннеля под именем
    /// другого или снимок суток под заголовком «Год» не показывается.
    pub(super) fn follow(&mut self, tunnel: &str, range: GraphRange) {
        let history = |r: GraphRange| matches!(source(r), Source::History(_));
        if self.frozen.as_ref().is_some_and(|f| f.tunnel != tunnel || (f.range != range && (history(f.range) || history(range)))) {
            self.frozen = None;
        }
    }

    /// На паузу — снимок `live`, `ping` и показанной истории на момент `now`; с паузы — снова живые данные.
    pub(super) fn toggle(&mut self, tunnel: &str, range: GraphRange, live: &Live, ping: Option<&PingState>, history: Option<&Shown>, now: Instant) {
        self.frozen = match self.frozen.take() {
            Some(_) => None,
            None => Some(Frozen {
                tunnel: tunnel.to_string(),
                range,
                live: live.clone(),
                ping: ping.cloned(),
                history: history.cloned(),
                at: now,
            }),
        };
    }

    /// Снимок истории на паузе; `None` — не на паузе или нажали на живом масштабе.
    pub(super) fn frozen_history(&self) -> Option<&Shown> {
        self.frozen.as_ref().and_then(|f| f.history.as_ref())
    }

    /// Данные кадра. Полоса пинга скрыта сейчас (`ping` — `None`) — её нет и на паузе.
    pub(super) fn data<'a>(&'a self, live: &'a Live, ping: Option<&'a PingState>, now: Instant) -> GraphData<'a> {
        match &self.frozen {
            Some(f) => GraphData {
                live: &f.live,
                ping: ping.and(f.ping.as_ref()),
                at: f.at,
                lag_secs: now.saturating_duration_since(f.at).as_secs(),
                paused: true,
            },
            None => GraphData { live, ping, at: now, lag_secs: 0, paused: false },
        }
    }
}

/// Высота значения `v` байт/с в области кривых `plot` при верхе шкалы `scale`.
pub(super) fn y_of(plot: Rect, scale: f64, v: f64) -> f32 {
    plot.bottom() - 4.0 - (v / scale) as f32 * (plot.height() - 8.0)
}

/// Шкала скоростей справа и деления поперёк для пика `max` байт/с (верх — с запасом `GRAPH_HEADROOM`). Возвращает
/// область кривых — левее подписей шкалы, ниже полосы паузы, выше `bottom_band` (подписи оси времени) — и верх шкалы.
pub(super) fn scale_frame(painter: &Painter, rect: Rect, max: f64, bottom_band: f32, weak: Color32) -> (Rect, f64) {
    let scale = max * GRAPH_HEADROOM;
    let ticks: Vec<_> = nice_ticks(scale)
        .into_iter()
        .map(|t| (t.value, painter.layout_no_wrap(t.label, FontId::proportional(SCALE_FONT), weak)))
        .collect();
    let scale_w = ticks.iter().map(|(_, g)| g.size().x).fold(0.0, f32::max) + 2.0 * SCALE_PAD;
    let plot = Rect::from_min_max(Pos2::new(rect.left(), rect.top() + TOP_BAND_H), Pos2::new(rect.right() - scale_w, rect.bottom() - bottom_band));
    for (value, galley) in ticks {
        let y = y_of(plot, scale, value);
        painter.hline(plot.x_range(), y, Stroke::new(1.0_f32, palette().border));
        let top = (y - galley.size().y / 2.0).clamp(plot.top(), rect.bottom() - galley.size().y);
        painter.galley(Pos2::new(rect.right() - SCALE_PAD - galley.size().x, top), galley, weak);
    }
    (plot, scale)
}

/// Подпись в полосе над кривыми, слева: пик или легенда истории.
pub(super) fn top_note(painter: &Painter, rect: Rect, text: String, weak: Color32) {
    painter.text(rect.left_top() + Vec2::new(6.0, 4.0), Align2::LEFT_TOP, text, FontId::proportional(12.0), weak);
}

/// Кнопка паузы значком: «пауза» — две полосы, на паузе — нажата, значок «продолжить» (треугольник).
fn paint_pause_button(ui: &Ui, resp: &Response, paused: bool) {
    let v = ui.style().interact_selectable(resp, paused);
    let painter = ui.painter();
    painter.rect(resp.rect, egui::CornerRadius::same(4), v.weak_bg_fill, v.bg_stroke, egui::StrokeKind::Inside);
    let icon = resp.rect.shrink(4.5);
    let color = v.fg_stroke.color;
    if paused {
        painter.add(egui::Shape::convex_polygon(vec![icon.left_top(), icon.right_center(), icon.left_bottom()], color, Stroke::NONE));
    } else {
        let bar = Vec2::new(icon.width() / 3.0, icon.height());
        painter.rect_filled(Rect::from_min_size(icon.min, bar), 0.0, color);
        painter.rect_filled(Rect::from_min_size(Pos2::new(icon.right() - bar.x, icon.top()), bar), 0.0, color);
    }
}

/// Максимум по корзинам — пики не теряются при сжатии часа в ширину графика.
pub(super) fn bucket_max(series: &[(f64, f64)], buckets: usize) -> Vec<(f64, f64)> {
    if buckets == 0 || series.len() <= buckets {
        return series.to_vec();
    }
    let k = series.len().div_ceil(buckets);
    series.chunks(k).map(|c| c.iter().fold((0.0f64, 0.0f64), |a, s| (a.0.max(s.0), a.1.max(s.1)))).collect()
}

/// Заголовок с выбором масштаба справа — выпадающий список. Выбор сразу в настройках: они пишутся сами, как прочий вид.
fn range_picker(ui: &mut Ui, s: &mut Settings) {
    ui.horizontal(|ui| {
        ui.weak(tr("gr.speed"));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            egui::ComboBox::from_id_salt("graph-range").selected_text(range_title(s.graph_range)).show_ui(ui, |ui| {
                for range in GraphRange::ALL {
                    ui.selectable_value(&mut s.graph_range, range, range_title(range));
                }
            });
        });
    });
}

/// График скорости (вход — зелёный, выход — синий), под ним полоса пинга. Высота тянется за нижний край.
/// `live` — замеры подключённого туннеля; `None` — не подключён (тогда график зовут только ради истории).
/// `view.pause` — пауза окна: на паузе график и полоса пинга показывают снимок, шкала идёт за снимком.
pub(super) fn graph(ui: &mut Ui, tunnel: &str, live: Option<&Live>, ping: Option<&PingState>, s: &mut Settings, view: &mut GraphState) {
    range_picker(ui, s);
    let height = s.graph_height.clamp(GRAPH_MIN_H, GRAPH_MAX_H);
    let (rect, hover) = ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::hover());

    // Кнопка — раньше данных: нажатие в этом кадре уже показывает снимок (или живые данные).
    let now = Instant::now();
    view.pause.follow(tunnel, s.graph_range);
    let no_live = Live::default();
    let live = live.unwrap_or(&no_live);
    // На паузе история не запрашивается: показан снимок.
    let shows = match source(s.graph_range) {
        Source::Live(secs) => Shows::Live(secs),
        Source::History(range) => Shows::History(range, match view.pause.frozen_history() {
            Some(frozen) => frozen.clone(),
            None => view.feed.shown(tunnel, range, now),
        }),
    };
    let button_rect = Rect::from_min_size(Pos2::new(rect.right() - 2.0 - PAUSE_BUTTON, rect.top() + 2.0), Vec2::splat(PAUSE_BUTTON));
    let button = ui.interact(button_rect, hover.id.with("pause"), Sense::click());
    if button.clicked() {
        let history = match &shows {
            Shows::History(_, shown) => Some(shown),
            Shows::Live(_) => None,
        };
        view.pause.toggle(tunnel, s.graph_range, live, ping, history, now);
    }
    let data = view.pause.data(live, ping, now);

    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, egui::CornerRadius::same(4), ui.visuals().extreme_bg_color);
    let over_button = button.hovered();
    match &shows {
        Shows::Live(secs) => live_plot(ui, &painter, &hover, rect, *secs as f64, &data, over_button),
        Shows::History(range, shown) => {
            history_graph::plot(ui, &painter, &hover, &history_graph::Plot { rect, range: *range, shown, paused: data.paused, over_button });
        }
    }
    if data.paused {
        let weak = ui.visuals().weak_text_color();
        painter.text(Pos2::new(button_rect.left() - 6.0, button_rect.center().y), Align2::RIGHT_CENTER, tr("gr.paused"), FontId::proportional(12.0), weak);
    }
    paint_pause_button(ui, &button, data.paused);
    a11y::describe(&button, Painted::GraphPause { paused: data.paused });
    button.on_hover_text(tr(if data.paused { "gr.resume" } else { "gr.pause" }));

    match (&shows, data.ping) {
        (Shows::Live(secs), Some(ping)) => {
            ui.add_space(4.0);
            ping_strip(ui, ping, *secs as f64, data.at, data.lag_secs);
        }
        // Агент пингует один узел и истории пинга не хранит.
        (Shows::History(..), Some(_)) => {
            ui.add_space(4.0);
            ui.weak(tr("gr.ping_live_only"));
        }
        (_, None) => {}
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

/// Живые замеры за `period` секунд: кривые по секундам (сжатые до ширины максимумом), подсказка с возрастом точки.
fn live_plot(ui: &Ui, painter: &Painter, hover: &Response, rect: Rect, period: f64, data: &GraphData, over_button: bool) {
    let weak = ui.visuals().weak_text_color();
    let series = data.live.rate_series(period);
    // Пик по всем замерам — тот же, что по корзинам (`bucket_max` пиков не теряет), а нужен раньше: от него шкала,
    // от ширины её подписей — ширина области кривых, от неё — число корзин.
    let max = series.iter().map(|(r, t)| r.max(*t)).fold(1.0, f64::max);
    let (plot, scale) = scale_frame(painter, rect, max, 0.0, weak);

    let points = bucket_max(&series, plot.width().max(1.0) as usize);
    // Замер раз в секунду; после сжатия одна точка покрывает `per_point` секунд.
    let per_point = if series.len() > points.len() { series.len().div_ceil(points.len().max(1)) } else { 1 };
    let step = plot.width() / period as f32 * per_point as f32;
    let line = |pick: fn(&(f64, f64)) -> f64| -> Vec<Pos2> {
        points.iter().rev().enumerate().map(|(i, p)| Pos2::new(plot.right() - i as f32 * step, y_of(plot, scale, pick(p)))).collect()
    };
    if points.len() >= 2 {
        painter.add(egui::Shape::line(line(|p| p.1), Stroke::new(1.5_f32, palette().graph_tx)));
        painter.add(egui::Shape::line(line(|p| p.0), Stroke::new(1.5_f32, palette().graph_rx)));
    }
    top_note(painter, rect, trf("gr.peak", &[&fmt::rate(max)]), weak);
    // Диктору — то, что зрячий видит с одного взгляда: текущие приём и передача (как в карточке, за 3 с) и пик.
    let (rx_now, tx_now) = data.live.rate(3.0);
    let (rx, tx, peak) = (fmt::rate(rx_now), fmt::rate(tx_now), fmt::rate(max));
    a11y::describe(hover, Painted::Graph { rx: &rx, tx: &tx, peak: &peak, paused: data.paused });

    // Наведение: ближайшая точка — вертикальная линия, точки на кривых, подсказка со значениями. Над кнопкой — её подсказка.
    let Some(pos) = hover.hover_pos().filter(|_| !over_button) else { return };
    let Some(i) = nearest_from_right(plot.right() - pos.x, step, points.len()) else { return };
    let p = points[points.len() - 1 - i];
    let x = plot.right() - i as f32 * step;
    painter.vline(x, plot.y_range(), Stroke::new(1.0_f32, weak));
    painter.circle_filled(Pos2::new(x, y_of(plot, scale, p.0)), 3.5, palette().graph_rx);
    painter.circle_filled(Pos2::new(x, y_of(plot, scale, p.1)), 3.5, palette().graph_tx);
    let when = fmt::ago((i * per_point) as u64 + data.lag_secs);
    hover.clone().on_hover_ui_at_pointer(|ui| {
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

/// Пинг не прошёл: последнее удачное значение и «н/д», если оно было, иначе только «н/д».
pub(super) fn stale_ping(ping: &PingState) -> String {
    let na = tr("st.na");
    match ping.history.iter().rev().find_map(|(_, ms)| *ms) {
        Some(ms) => format!("{} ({na})", trf("unit.ms", &[&ms.to_string()])),
        None => na,
    }
}

/// Полоса пинга. Возрасты замеров — от `at` (сейчас или момент паузы); `lag_secs` — отставание снимка для подсказки.
fn ping_strip(ui: &mut Ui, ping: &PingState, period: f64, at: Instant, lag_secs: u64) {
    let (rect, hover) = ui.allocate_exact_size(Vec2::new(ui.available_width(), PING_STRIP_H), Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, egui::CornerRadius::same(4), ui.visuals().extreme_bg_color);
    let recent: Vec<(f64, Option<u32>)> = ping
        .history
        .iter()
        .map(|(when, ms)| (at.saturating_duration_since(*when).as_secs_f64(), *ms))
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
    let when = fmt::ago(*age as u64 + lag_secs);
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

    fn labels(top: f64) -> Vec<String> {
        nice_ticks(top).into_iter().map(|t| t.label).collect()
    }

    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;

    #[test]
    fn scale_ticks_are_round_numbers_in_one_unit() {
        // Тишина: пик не меньше 1 B/s, верх 1.15 — долей байта нет.
        assert_eq!(labels(1.15), ["0 B/s", "1 B/s"]);
        assert_eq!(labels(9.0), ["0 B/s", "2 B/s", "4 B/s", "6 B/s", "8 B/s"]);
        assert_eq!(labels(1000.0), ["0 B/s", "500 B/s", "1000 B/s"]);
        // Верх перешёл 1024 B — подписи в KiB, как у значений рядом; деления в байтах.
        assert_eq!(labels(1150.0), ["0.0 KiB/s", "0.5 KiB/s", "1.0 KiB/s"]);
        assert_eq!(nice_ticks(1150.0).iter().map(|t| t.value).collect::<Vec<_>>(), [0.0, 512.0, KIB]);
        assert_eq!(labels(3.5 * MIB), ["0 MiB/s", "1 MiB/s", "2 MiB/s", "3 MiB/s"]);
        assert_eq!(labels(1023.0 * KIB), ["0 KiB/s", "500 KiB/s", "1000 KiB/s"]);
        assert_eq!(labels(0.25 * MIB * 1.15), ["0 KiB/s", "100 KiB/s", "200 KiB/s"]);
    }

    #[test]
    fn scale_has_no_ticks_without_a_range() {
        assert!(nice_ticks(0.0).is_empty());
        assert!(nice_ticks(-5.0).is_empty());
        assert!(nice_ticks(f64::NAN).is_empty());
        assert!(nice_ticks(f64::INFINITY).is_empty());
    }

    /// По всему диапазону скоростей: от нуля, по возрастанию, не выше верха, 3–5 делений (в байтах ниже 2 B/s — меньше:
    /// долей байта нет), шаг ровный.
    #[test]
    fn scale_ticks_fit_any_range() {
        let mut top = 1.15;
        while top < 5e12 {
            let ticks = nice_ticks(top);
            let n = ticks.len();
            assert!((2..=MAX_TICKS).contains(&n), "{top}: {n}");
            assert!(n >= 3 || top < 2.0, "{top}: {n}");
            assert_eq!(ticks[0].value, 0.0);
            assert!(ticks[n - 1].value <= top * (1.0 + 1e-9), "{top}");
            let step = ticks[1].value;
            for (k, t) in ticks.iter().enumerate() {
                assert!((t.value - k as f64 * step).abs() <= step * 1e-9, "{top}: {:?}", ticks);
            }
            top *= 1.37;
        }
    }

    fn live_with(rates: &[u64], start: Instant) -> Live {
        let mut live = Live::default();
        let mut rx = 0;
        for (i, r) in rates.iter().enumerate() {
            rx += r;
            live.history.push_back(crate::monitor::Sample { at: start + std::time::Duration::from_secs(i as u64), rx, tx: 0 });
        }
        live
    }

    #[test]
    fn pause_freezes_the_snapshot_while_samples_keep_coming() {
        let t0 = Instant::now();
        let mut live = live_with(&[0, 100, 200], t0);
        let mut ping = PingState { host: "10.0.0.1".into(), ..Default::default() };
        ping.history.push_back((t0, Some(40)));
        let mut pause = GraphPause::default();
        let at_pause = t0 + std::time::Duration::from_secs(2);
        pause.toggle("office", GraphRange::Min2, &live, Some(&ping), None, at_pause);
        let frozen = live.rate_series(120.0);

        // Опрос идёт дальше: новые замеры и пинги в живых данных.
        live = live_with(&[0, 100, 200, 5000, 9000], t0);
        ping.history.push_back((t0 + std::time::Duration::from_secs(4), Some(900)));
        let later = t0 + std::time::Duration::from_secs(14);
        pause.follow("office", GraphRange::Min2);
        let data = pause.data(&live, Some(&ping), later);
        assert!(data.paused && pause.frozen.is_some());
        assert_eq!(data.live.rate_series(120.0), frozen, "на паузе — снимок");
        assert_eq!(data.ping.map(|p| p.history.len()), Some(1), "пинг тоже снимок");
        assert_eq!(data.at, at_pause, "возрасты точек — от момента паузы");
        assert_eq!(data.lag_secs, 12, "подсказка называет настоящий возраст");
        // Полосу пинга выключили на паузе — её нет.
        assert!(pause.data(&live, None, later).ping.is_none());

        // Продолжить — снова живые данные.
        pause.toggle("office", GraphRange::Min2, &live, Some(&ping), None, later);
        let data = pause.data(&live, Some(&ping), later);
        assert!(!data.paused && pause.frozen.is_none());
        assert_eq!(data.live.rate_series(120.0), live.rate_series(120.0));
        assert_eq!(data.ping.map(|p| p.history.len()), Some(2));
        assert_eq!((data.at, data.lag_secs), (later, 0));
    }

    #[test]
    fn switching_tunnel_resumes_live_view() {
        let t0 = Instant::now();
        let office = live_with(&[0, 100, 200], t0);
        let home = live_with(&[0, 7, 7, 7], t0);
        let mut pause = GraphPause::default();
        pause.toggle("office", GraphRange::Min2, &office, None, None, t0);
        pause.follow("office", GraphRange::Min2);
        assert!(pause.frozen.is_some(), "тот же туннель — пауза стоит");
        pause.follow("home", GraphRange::Min2);
        assert!(pause.frozen.is_none());
        let data = pause.data(&home, None, t0);
        assert!(!data.paused);
        assert_eq!(data.live.rate_series(120.0), home.rate_series(120.0), "снимок чужого туннеля не показан");
        // Назад к первому туннелю — живые данные, пауза не вернулась.
        pause.follow("office", GraphRange::Min2);
        assert!(!pause.data(&office, None, t0).paused);
    }

    #[test]
    fn live_ranges_use_window_samples_and_long_ones_the_agents_history() {
        let sources: Vec<_> = GraphRange::ALL.into_iter().map(source).collect();
        assert_eq!(
            sources,
            [
                Source::Live(120),
                Source::Live(600),
                Source::Live(3600),
                Source::History(HistoryRange::Day),
                Source::History(HistoryRange::Month),
                Source::History(HistoryRange::Year),
            ]
        );
        // Живой масштаб не длиннее того, что окно держит в памяти.
        for s in sources {
            if let Source::Live(secs) = s {
                assert!(secs as usize <= crate::monitor::HISTORY_SECS, "{secs}");
            }
        }
    }

    fn history_of(rx: f64) -> Shown {
        let bucket = crate::daemon::agent::proto::HistoryBucket { start: 0, secs: 60.0, rx, ..Default::default() };
        let history = crate::daemon::agent::proto::History { bucket_s: 60, buckets: vec![bucket] };
        Shown::Ready { history: std::sync::Arc::new(history), at_unix: 600 }
    }

    #[test]
    fn pause_freezes_the_shown_history_until_the_source_changes() {
        let t0 = Instant::now();
        let live = Live::default();
        let mut pause = GraphPause::default();
        assert!(pause.frozen_history().is_none());
        pause.toggle("office", GraphRange::Day, &live, None, Some(&history_of(1.0)), t0);
        pause.follow("office", GraphRange::Day);
        assert_eq!(pause.frozen_history(), Some(&history_of(1.0)), "на паузе — снимок, а не новый ответ агента");
        assert!(pause.data(&live, None, t0).paused);

        // Другой масштаб истории — снимок суток под заголовком «Месяц» не показывается.
        pause.follow("office", GraphRange::Month);
        assert!(pause.frozen_history().is_none() && !pause.data(&live, None, t0).paused);

        // С истории на живой масштаб и обратно — тоже другой источник.
        pause.toggle("office", GraphRange::Year, &live, None, Some(&history_of(2.0)), t0);
        pause.follow("office", GraphRange::Min10);
        assert!(!pause.data(&live, None, t0).paused);
        pause.toggle("office", GraphRange::Min2, &live, None, None, t0);
        pause.follow("office", GraphRange::Year);
        assert!(!pause.data(&live, None, t0).paused);

        // Между живыми масштабами пауза стоит, как в 0.5.3; другой туннель — снята и на истории.
        pause.toggle("office", GraphRange::Min2, &live, None, None, t0);
        pause.follow("office", GraphRange::Hour1);
        assert!(pause.data(&live, None, t0).paused);
        pause.toggle("office", GraphRange::Hour1, &live, None, None, t0);
        pause.toggle("office", GraphRange::Day, &live, None, Some(&history_of(3.0)), t0);
        pause.follow("home", GraphRange::Day);
        assert!(pause.frozen_history().is_none());

        // Снова на паузу и с паузы — история опять от агента.
        pause.toggle("home", GraphRange::Day, &live, None, Some(&history_of(4.0)), t0);
        pause.toggle("home", GraphRange::Day, &live, None, Some(&history_of(4.0)), t0);
        assert!(pause.frozen_history().is_none());
    }
}
