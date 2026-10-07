//! Тест умещаемости (без GPU): главное окно и диалоги раскладываются в окне наименьшего размера (`MIN_WINDOW`,
//! 760x480 pt) и в области масштаба 200 % (вдвое меньше точек), и проверяется, что ни одна нарисованная фигура не
//! вышла за окно, а текст — за свой прямоугольник отсечения (так ловятся кнопка за краем и обрезанная надпись), и
//! что ряд кнопок диалога остаётся внутри окна. Раскладка панелей повторяет `App::ui` теми свободными функциями, из
//! которых оно собрано; шрифты и отступы — `base_style`.

use std::collections::BTreeMap;
use std::sync::Arc;

use eframe::egui::{self, epaint::ClippedShape, Pos2, Rect, Shape, Ui, Vec2};

use super::core_ui::{banner_row, Banner};
use super::dialog::{dialog_body, dialog_ok_cancel_apply, dialog_window, window_bounds};
use super::engine_mode::MODE_SWITCH_WIDTH;
use super::settings_dialog::{sections, Choices, SETTINGS_WIDTH};
use super::*;
use crate::events::{Event, Severity};
use crate::i18n::{tr, trf};
use crate::monitor::{Live, Options, Snapshot};
use crate::ping::PingState;
use crate::settings::MIN_WINDOW;
use crate::stats::Stats;

/// Окно 760x480 px при 100 % и то же окно при 200 %: область в точках вдвое меньше.
const SCREENS: [Vec2; 2] = [Vec2::new(MIN_WINDOW[0], MIN_WINDOW[1]), Vec2::new(MIN_WINDOW[0] / 2.0, MIN_WINDOW[1] / 2.0)];
/// Допуск на округление раскладки и выступ глифов за строку.
const TOL: f32 = 2.0;
const LONG_NAME: &str = "office-amsterdam-backup-2";

fn context() -> egui::Context {
    let ctx = egui::Context::default();
    base_style(&ctx);
    ctx
}

/// Три кадра с одним содержимым (размер и положение egui уточняет по прошлому кадру); фигуры последнего.
fn frames(ctx: &egui::Context, screen: Vec2, mut build: impl FnMut(&mut Ui)) -> Vec<ClippedShape> {
    let mut shapes = Vec::new();
    for _ in 0..3 {
        let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, screen)), ..Default::default() };
        shapes = ctx.run_ui(input, |ui| build(ui)).shapes;
    }
    shapes
}

fn describe(shape: &Shape) -> String {
    match shape {
        Shape::Text(t) => format!("text {:?}", t.galley.text()),
        other => crate::explain::variant_name(other),
    }
}

/// Фигуры за окном (`screen`); текст — ещё и за левым или правым краем своего прямоугольника отсечения (обрезанная
/// надпись). Внутри `scroll` (панель с вертикальной прокруткой) низ и верх не проверяются: фигуру на краю
/// прокрутки режут по замыслу, до неё докручивают. Тень окна (размытый прямоугольник) выступает за него по замыслу.
fn escaped(shapes: &[ClippedShape], screen: Rect, scroll: Option<Rect>) -> Vec<String> {
    let mut out = Vec::new();
    for cs in shapes {
        if has_blur(&cs.shape) {
            continue;
        }
        let b = cs.shape.visual_bounding_rect();
        // Пустые фигуры и фигуры целиком за своим отсечением (прокрученное содержимое) не видны — не дефект раскладки.
        if !b.is_finite() || b.is_negative() || !b.intersects(cs.clip_rect) {
            continue;
        }
        let mut within = screen.expand(TOL);
        if matches!(cs.shape, Shape::Text(_)) {
            let c = cs.clip_rect.expand(TOL);
            within = Rect::from_x_y_ranges(within.x_range().intersection(c.x_range()), within.y_range());
        }
        if scroll.is_some_and(|s| s.expand(TOL).contains_rect(cs.clip_rect)) {
            within = Rect::from_x_y_ranges(within.x_range(), egui::Rangef::EVERYTHING);
        }
        if !within.contains_rect(b) {
            out.push(format!("{} at {b:?} outside {within:?}", describe(&cs.shape)));
        }
    }
    out
}

/// Первые находки одной строкой: сборочный скрипт показывает у упавшего теста одну строку после «panicked».
fn one_line(bad: &[String]) -> String {
    bad.iter().take(6).cloned().collect::<Vec<_>>().join(" | ")
}

/// Тень окна: размытый прямоугольник, сам или в составной фигуре (egui рисует тень и рамку окна одной `Shape::Vec`).
fn has_blur(shape: &Shape) -> bool {
    match shape {
        Shape::Rect(r) => r.blur_width > 0.0,
        Shape::Vec(v) => v.iter().any(has_blur),
        _ => false,
    }
}

/// Прямоугольники надписей с этим текстом (начало текста: обрезанная надпись хранит исходный текст).
fn text_rects(shapes: &[ClippedShape], text: &str) -> Vec<Rect> {
    shapes
        .iter()
        .filter_map(|cs| match &cs.shape {
            Shape::Text(t) if t.galley.text().starts_with(text) => Some(cs.shape.visual_bounding_rect()),
            _ => None,
        })
        .collect()
}

fn live() -> Live {
    let peer = crate::uapi::Peer { endpoint: "203.0.113.5:51820".into(), rx_bytes: 123_456_789, tx_bytes: 9_876_543, ..Default::default() };
    Live { status: Some(crate::uapi::Status { peers: vec![peer], ..Default::default() }), error: None, history: Default::default() }
}

fn shared_with_events() -> Arc<Shared> {
    let options = Options { ping: false, ping_host: String::new(), notify: false, tray: false, taskbar: false };
    let shared = Arc::new(Shared::new(None, options, None));
    let now = monitor::unix_now();
    // Тексты короткие: длинная строка журнала режется у правого края — отдельный дефект панели, не раскладки окна.
    shared.push_event(Event::new(now - 60, LONG_NAME, Severity::Info, &tr("ev.connected"), false));
    shared.push_event(Event::new(now - 30, LONG_NAME, Severity::Warn, &trf("health.stale", &["3 min"]), false));
    shared.push_event(Event::new(now, "", Severity::Bad, &tr("health.core_lost"), false));
    shared
}

/// Длинный текст полосы ядра: ошибка канала целиком, как её даёт `LinkState::Down`.
fn banner_text() -> String {
    trf("core.down", &["The system cannot find the file specified. (os error 2) while opening \\\\.\\pipe\\awg-ui-core"])
}

/// Главное окно как в `App::ui`: меню, полоса ядра, строка состояния, журнал, таблица (сохранённая ширина 520 —
/// больше, чем остаётся карточке в окне 760) и карточка туннеля с длинным именем и всеми тремя кнопками.
fn main_window(ui: &mut Ui, s: &mut Settings, shared: &Shared, graph: &mut GraphState, filter: &mut LogFilter) -> (Rect, bool) {
    let mut actions = Vec::new();
    let mut nav = menu::MenuNav::default();
    let lang_dir = std::env::temp_dir();
    let bar = menu::MenuBarInput { lang_dir: &lang_dir, updates_new: true, keyboard: true };
    egui::Panel::top("menu").show(ui, |ui| menu_bar(ui, &mut nav, s, &bar, &mut actions));
    let text = banner_text();
    let banner = Banner { text: &text, button: Some(&tr("core.reinstall")), retry: false, installing: false };
    egui::Panel::top("core-banner").show(ui, |ui| banner_row(ui, &banner));
    egui::Panel::bottom("status").show(ui, |ui| {
        let bar = StatusBar { mode: Mode::Overlay, service: "AmneziaWG: running", poll_error: None, unseen_error: true, notice: None };
        status_bar(ui, &bar, &mut actions);
    });
    if s.view.log {
        egui::Panel::bottom("log").resizable(true).default_size(s.log_height).size_range(60.0..=600.0).show(ui, |ui| {
            event_log(ui, shared, filter, Some(LONG_NAME), &mut actions);
        });
    }
    let mut snap = Snapshot::default();
    snap.tunnels.extend([LONG_NAME.to_string(), "home.nl-ams.full".to_string(), "travel.fi-hel.v4".to_string()]);
    snap.running.insert(LONG_NAME.to_string(), live());
    snap.polls = 1;
    let healths: BTreeMap<String, Health> = snap.tunnels.iter().map(|t| (t.clone(), Health { level: Level::Ok, text: tr("health.ok_traffic"), detail: None })).collect();
    let stats = Stats::new();
    let keys = Keys { list: true, search: true };
    let mut search = String::new();
    egui::Panel::left("tunnels").resizable(true).default_size(s.left_width).size_range(LEFT_MIN..=left_panel_max(ui.available_width())).show(ui, |ui| {
        let list = List { snap: &snap, healths: &healths, stats: &stats, keys };
        tunnel_list(ui, s, &mut search, &list, &mut actions);
    });
    let ping = PingState::default();
    let card = egui::CentralPanel::default().show(ui, |ui| {
        let d = Detail {
            name: LONG_NAME,
            live: snap.running.get(LONG_NAME),
            health: healths[LONG_NAME].clone(),
            busy: false,
            ping: &ping,
            ping_unavailable: false,
            stats: &stats,
            info: None,
            info_loading: false,
            // «Повторить» и «Переподключить» вместе не бывают: повтор — у туннеля, который не поднялся.
            retry_slow: false,
            core_lost: false,
            snap: &snap,
        };
        details(ui, &d, s, graph, &mut actions)
    });
    (card.response.rect, card.inner)
}

/// Окно 760x480 pt, журнал открыт (140 pt) и закрыт: ничего не выходит за окно, никакая надпись не обрезана по
/// ширине, карточке остаётся не меньше `CARD_MIN`, имя туннеля не лежит под кнопками карточки, кнопка полосы ядра
/// в окне. С открытым журналом карточке с графиком не хватает высоты — колонка прокручивается (`details`): её низ
/// (ручка графика) нарисован, но ниже окна, и его клавиатура и колесо прокрутки достанут. При 200 % окно держит
/// те же 760x480 pt (`WindowState::min_size_due`), поэтому раскладка та же.
#[test]
fn main_window_fits_the_minimum_size() {
    for log_open in [true, false] {
        let ctx = context();
        let mut s = Settings::default();
        s.book.select_tunnel(LONG_NAME);
        assert_eq!(s.left_width, 520.0, "the saved width that used to squeeze the card");
        s.view.log = log_open;
        let shared = shared_with_events();
        let mut graph = GraphState { pause: GraphPause::default(), feed: history_feed::HistoryFeed::real(None, ctx.clone()) };
        let mut filter = LogFilter { selected_only: true, ..Default::default() };
        let (mut card, mut scrolls) = (Rect::NOTHING, false);
        let shapes = frames(&ctx, SCREENS[0], |ui| (card, scrolls) = main_window(ui, &mut s, &shared, &mut graph, &mut filter));
        let screen = Rect::from_min_size(Pos2::ZERO, SCREENS[0]);
        let bad = escaped(&shapes, screen, Some(card));
        assert!(bad.is_empty(), "log {log_open}: shapes leave the window or their clip: {}", one_line(&bad));
        assert!(card.width() >= CARD_MIN - TOL, "log {log_open}: card {card:?} narrower than {CARD_MIN}");
        let name = text_rects(&shapes, LONG_NAME);
        let header = name.iter().copied().max_by(|a, b| a.height().total_cmp(&b.height())).expect("the card header with the tunnel name (18 pt)");
        for label in ["act.disconnect", "act.reconnect"] {
            let buttons = text_rects(&shapes, &tr(label));
            assert!(!buttons.is_empty(), "log {log_open}: button {label} is drawn");
            for b in buttons {
                assert!(!b.intersects(header), "log {log_open}: name {header:?} under button {label} {b:?}");
            }
        }
        // С открытым журналом карточке с графиком не хватает высоты: колонка прокручивается, а не режется (egui не
        // рисует фигуры за окном прокрутки, поэтому признак — из самой `ScrollArea`). Что нарисовано — в ширину карточки.
        if log_open {
            assert!(scrolls, "log {log_open}: the column is taller than the panel and must scroll");
        }
        for r in text_rects(&shapes, &tr("det.not_loaded")) {
            assert!(r.left() >= card.left() && r.right() <= card.right(), "log {log_open}: {r:?} in {card:?}");
        }
        let fix = text_rects(&shapes, &tr("core.reinstall"));
        assert!(fix.iter().any(|r| r.right() <= screen.right()), "log {log_open}: banner button {fix:?} in {screen:?}");
    }
}

/// Полоса ядра на ширине 760: длинная ошибка переносится, кнопка «Переустановить ядро» остаётся в окне.
#[test]
fn core_banner_keeps_the_button_in_the_window() {
    let ctx = context();
    let text = banner_text();
    let shapes = frames(&ctx, SCREENS[0], |ui| {
        let banner = Banner { text: &text, button: Some(&tr("core.reinstall")), retry: true, installing: true };
        egui::Panel::top("core-banner").show(ui, |ui| banner_row(ui, &banner));
    });
    let screen = Rect::from_min_size(Pos2::ZERO, SCREENS[0]);
    let bad = escaped(&shapes, screen, None);
    assert!(bad.is_empty(), "{}", one_line(&bad));
    let fix = text_rects(&shapes, &tr("core.reinstall"));
    let retry = text_rects(&shapes, &tr("core.retry"));
    assert_eq!((fix.len(), retry.len()), (1, 1));
    assert!(retry[0].right() <= fix[0].left(), "Windows order: fix button at the right edge, retry before it");
    let message = text_rects(&shapes, &text[..20]);
    assert!(message[0].height() > 20.0, "the long text wraps: {message:?}");
}

/// Панель журнала на ширине 760 с длинным именем туннеля в списке: ни одна надпись не обрезана, кнопки не лежат на
/// поле поиска и друг на друге.
#[test]
fn event_log_toolbar_fits_the_minimum_window() {
    let ctx = context();
    let shared = shared_with_events();
    let mut filter = LogFilter { selected_only: true, ..Default::default() };
    let long = "office-amsterdam-backup-2-very-long-tunnel-name";
    let shapes = frames(&ctx, SCREENS[0], |ui| {
        let mut actions = Vec::new();
        egui::Panel::bottom("log").default_size(140.0).show(ui, |ui| event_log(ui, &shared, &mut filter, Some(long), &mut actions));
    });
    let screen = Rect::from_min_size(Pos2::ZERO, SCREENS[0]);
    let bad = escaped(&shapes, screen, None);
    assert!(bad.is_empty(), "{}", one_line(&bad));
    let texts = ["log.title", "log.sev_all", "log.copy_all", "log.save_as", "log.search"].map(|k| text_rects(&shapes, &tr(k)));
    let mut rects: Vec<Rect> = texts.iter().map(|r| r[0]).collect();
    rects.extend(text_rects(&shapes, long).into_iter().filter(|r| r.width() < 200.0));
    for (i, a) in rects.iter().enumerate() {
        for b in &rects[i + 1..] {
            assert!(!a.intersects(*b), "toolbar texts overlap: {a:?} and {b:?}");
        }
    }
}

/// Диалог с этим телом в окне `screen`: (окно, ряд кнопок, область окон).
fn dialog_rects(ctx: &egui::Context, screen: Vec2, width: f32, mut body: impl FnMut(&mut Ui)) -> (Rect, Rect, Rect) {
    let (mut window, mut row) = (Rect::NOTHING, Rect::NOTHING);
    let shapes = frames(ctx, screen, |ui| {
        let ctx = ui.ctx();
        let mut open = true;
        let shown = dialog_window(ctx, "t", "fit-dialog", &mut open).show(ctx, |ui| {
            dialog_body(ui, width, &mut body);
            ui.add_space(6.0);
            let top = ui.cursor().top();
            dialog_ok_cancel_apply(ui, (&tr("btn.ok"), true), &tr("btn.cancel"), (&tr("set.apply"), true));
            row = Rect::from_min_max(Pos2::new(ui.min_rect().left(), top), ui.min_rect().right_bottom());
        });
        window = shown.expect("dialog is open").response.rect;
    });
    // Фигуры не проверяются: прокрученное тело рисует строки за окном по замыслу; окно и кнопки — ниже.
    drop(shapes);
    (window, row, window_bounds(ctx.content_rect()))
}

fn assert_dialog_fits(name: &str, screen: Vec2, (window, row, bounds): (Rect, Rect, Rect)) {
    assert!(bounds.expand(0.5).contains_rect(window), "{name} {screen:?}: window {window:?} leaves {bounds:?}");
    assert!(window.contains_rect(row), "{name} {screen:?}: buttons {row:?} outside {window:?}");
    assert!(row.height() >= 26.0 - 0.5, "{name} {screen:?}: button row clipped {row:?}");
}

/// «Настройки» (самое высокое окно) и смена режима со справкой (самое широкое) в окне 760x480 и при 200 %: окно в
/// области, «ОК / Отмена / Применить» видны. Остальные диалоги идут тем же `dialog_body` (`dialog_widths_go_through_dialog_body`).
#[test]
fn dialogs_fit_the_minimum_window_and_zoom_200() {
    let ctx = context();
    for screen in SCREENS {
        let mut choices = Choices::read(&Settings::default(), Some(true));
        let settings = dialog_rects(&ctx, screen, SETTINGS_WIDTH, |ui| {
            sections(ui, &mut choices, 1);
            ui.add_space(10.0);
            let _ = ui.button(tr("set.reset"));
        });
        assert_dialog_fits("settings", screen, settings);
        let mut remember = false;
        let mode = dialog_rects(&ctx, screen, MODE_SWITCH_WIDTH, |ui| {
            ui.add(egui::Label::new(tr("mode.engine_help")).wrap());
            ui.add_space(8.0);
            ui.add(egui::Label::new(trf("mode.will_disconnect", &[LONG_NAME])).wrap());
            ui.add_space(8.0);
            ui.add(egui::Label::new(trf("mode.missing", &["awg.dll"])).wrap());
            ui.add_space(10.0);
            ui.checkbox(&mut remember, tr("dlg.dont_show"));
        });
        assert_dialog_fits("mode-switch", screen, mode);
    }
}
