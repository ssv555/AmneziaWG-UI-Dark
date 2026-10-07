//! Общие части окон: единственный конструктор окна, ряд кнопок и клавиши Enter/Esc.
//!
//! Правило окон (классика Windows): у каждого окна заголовок, за который его перетаскивают, крестик в заголовке
//! и Esc, закрывающий его. Поэтому окна строятся только через `dialog_window` — тест `windows_follow_the_standard`
//! не пускает окно egui в обход него и запрещает закреплять окно на месте или прятать заголовок.

use eframe::egui::{self, Align2, Rect, Ui, Vec2};

/// Отступ окна от краёв главного окна: окно не прилипает к границе и не уходит за неё.
const WINDOW_MARGIN: f32 = 12.0;

/// Окно программы: заголовок, перетаскивается, крестик в заголовке. Крестик сбрасывает `open` — вызывающий
/// обязан понимать `!open` как свою отмену («Отмена», «Позже», «Закрыть»), так же как Esc.
/// При открытии по центру; позицию egui дальше помнит по `id`, поэтому он постоянный, а не из заголовка (тот меняется
/// с языком и содержимым). Не сворачивается и не меняет размер, пока вызывающий не разрешит: ширину задаёт содержимое.
/// Начальное место можно сменить (`pivot` + `default_pos`), закрепить — нельзя.
/// Окно всегда внутри главного (`window_bounds`): и положение, и наибольший размер — в том числе после того, как
/// главное окно уменьшили. Предел размера держит окно с изменяемым размером; окно по размеру содержимого egui растит
/// по содержимому, поэтому такое окно обязано само умещать его в главное окно (текст переносится по ширине, длинное —
/// в свою `ScrollArea`, как история в окне обновлений). Общий `vscroll` не годится: он растягивает маленькие диалоги
/// до размера по умолчанию, а в окне со своей `ScrollArea` делает остаток высоты бесконечным.
pub(super) fn dialog_window<'a>(ctx: &egui::Context, title: impl Into<egui::WidgetText>, id: &str, open: &'a mut bool) -> egui::Window<'a> {
    let bounds = window_bounds(ctx.content_rect());
    egui::Window::new(title)
        .id(egui::Id::new(id))
        .open(open)
        .collapsible(false)
        .resizable(false)
        .pivot(Align2::CENTER_CENTER)
        .default_pos(bounds.center())
        .constrain_to(bounds)
        .max_size(bounds.size())
}

/// Где может быть окно: содержимое главного окна без отступа `WINDOW_MARGIN` с каждой стороны. В слишком маленьком
/// главном окне отступ уменьшается, чтобы область не вывернулась (egui не терпит прямоугольник с min > max).
fn window_bounds(content: Rect) -> Rect {
    let margin = WINDOW_MARGIN.min(content.width() / 2.0).min(content.height() / 2.0).max(0.0);
    content.shrink(margin)
}

/// Esc для немодального окна `id`: забирается из ввода, только если это окно верхнее (его последним щёлкнули
/// или оно последним появилось) — как в Windows, где Esc получает активное окно, а не первое по порядку отрисовки.
/// Модальные диалоги рисуются раньше и забирают Esc сами (`Turn::keys`).
pub(super) fn window_escape(ctx: &egui::Context, id: &str) -> bool {
    is_top_window(top_window(ctx), id) && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
}

/// Enter и Esc для немодального окна `id`: забираются, только если окно верхнее (как в `window_escape`).
pub(super) fn window_keys(ctx: &egui::Context, id: &str) -> (bool, bool) {
    if is_top_window(top_window(ctx), id) {
        dialog_keys(ctx)
    } else {
        (false, false)
    }
}

/// Верхнее из окон, показанных в прошлом кадре. `Context::top_layer_id` помнит и закрытые: egui не убирает слой из
/// порядка, пока другое окно не вышло наверх, и после закрытия верхнего окна Esc не доставался бы никому.
fn top_window(ctx: &egui::Context) -> Option<egui::LayerId> {
    ctx.memory(|m| m.layer_ids().filter(|l| l.order == egui::Order::Middle && m.areas().visible_last_frame(l)).last())
}

/// Верхний слой окон — окно с этим `id` (слой окна egui — `Order::Middle` с `id` окна).
fn is_top_window(top: Option<egui::LayerId>, id: &str) -> bool {
    top == Some(egui::LayerId::new(egui::Order::Middle, egui::Id::new(id)))
}

/// Кнопка ряда: текст, включена ли, подсказка с клавишей (Enter у главной, Esc у отмены).
struct RowButton<'a> {
    text: &'a str,
    enabled: bool,
    key_hint: Option<&'static str>,
}

/// Единственная раскладка кнопок диалога: полоса по правому краю, кнопки слева направо в порядке `buttons`.
/// Ответы — в том же порядке. Порядок задают вызывающие ниже, по правилу Windows: главная первой, «Отмена» за ней,
/// «Применить» последней.
fn button_row(ui: &mut Ui, buttons: &[RowButton]) -> Vec<egui::Response> {
    let size = Vec2::new(88.0, 26.0);
    // Полоса высотой в кнопку: `with_layout` занял бы всю высоту окна.
    let layout = egui::Layout::right_to_left(egui::Align::Center);
    let mut responses = ui
        .allocate_ui_with_layout(Vec2::new(ui.available_width(), size.y), layout, |ui| {
            // Раскладка справа налево: кнопки добавляются с конца, чтобы на экране стоять в порядке `buttons`.
            buttons
                .iter()
                .rev()
                .map(|b| {
                    let resp = ui.add_enabled(b.enabled, egui::Button::new(b.text).min_size(size));
                    match b.key_hint {
                        Some(hint) => resp.on_hover_text(hint),
                        None => resp,
                    }
                })
                .collect::<Vec<_>>()
        })
        .inner;
    responses.reverse();
    responses
}

fn primary<'a>(text: &'a str, enabled: bool) -> RowButton<'a> {
    RowButton { text, enabled, key_hint: Some("Enter") }
}

fn cancel(text: &str) -> RowButton<'_> {
    RowButton { text, enabled: true, key_hint: Some("Esc") }
}

/// Кнопки диалога по правому краю в порядке Windows: главная первой, «Отмена» последней.
/// Возвращает (главная нажата, отмена нажата).
pub(super) fn dialog_buttons(ui: &mut Ui, primary_text: &str, enabled: bool, cancel_text: Option<&str>) -> (bool, bool) {
    let mut row = vec![primary(primary_text, enabled)];
    row.extend(cancel_text.map(cancel));
    let r = button_row(ui, &row);
    (r[0].clicked(), r.get(1).is_some_and(|c| c.clicked()))
}

/// Три варианта ответа по правому краю: главный (Enter), второй вариант, «Отмена» (Esc) — как «Сохранить /
/// Не сохранять / Отмена» в Windows. Возвращает (главная, второй вариант, отмена).
pub(super) fn dialog_choice(ui: &mut Ui, primary_text: &str, other: &str, cancel_text: &str) -> (bool, bool, bool) {
    let other = RowButton { text: other, enabled: true, key_hint: None };
    let r = button_row(ui, &[primary(primary_text, true), other, cancel(cancel_text)]);
    (r[0].clicked(), r[1].clicked(), r[2].clicked())
}

/// Кнопки окна свойств в порядке Windows: «ОК» (Enter), «Отмена» (Esc), «Применить». «ОК» и «Применить» выключаются
/// (неверный ввод, нечего применять); «Отмена» есть всегда. Возвращает (ОК, отмена, применить).
pub(super) fn dialog_ok_cancel_apply(ui: &mut Ui, ok: (&str, bool), cancel_text: &str, apply: (&str, bool)) -> (bool, bool, bool) {
    let r = button_row(ui, &ok_cancel_apply_row(ok, cancel_text, apply));
    (r[0].clicked(), r[1].clicked(), r[2].clicked())
}

fn ok_cancel_apply_row<'a>(ok: (&'a str, bool), cancel_text: &'a str, apply: (&'a str, bool)) -> [RowButton<'a>; 3] {
    [primary(ok.0, ok.1), cancel(cancel_text), RowButton { text: apply.0, enabled: apply.1, key_hint: None }]
}

/// Enter и Esc для верхнего диалога: забираются из ввода, чтобы их не увидели окна под ним.
/// Enter считается только настоящим нажатием: автоповтор удержанной клавиши подтвердил бы только что
/// открытый диалог, который ещё не прочитан.
pub(super) fn dialog_keys(ctx: &egui::Context) -> (bool, bool) {
    ctx.input_mut(|i| (take_enter(&mut i.events), i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)))
}

/// Забирает из событий нажатия Enter без модификаторов (и автоповторы — чтобы не ушли окнам ниже);
/// `true`, если среди них было настоящее нажатие, а не автоповтор.
fn take_enter(events: &mut Vec<egui::Event>) -> bool {
    let mut pressed = false;
    events.retain(|e| match e {
        egui::Event::Key { key: egui::Key::Enter, pressed: true, repeat, modifiers, .. }
            if modifiers.matches_logically(egui::Modifiers::NONE) =>
        {
            pressed |= !*repeat;
            false
        }
        _ => true,
    });
    pressed
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    fn key(key: egui::Key, pressed: bool, repeat: bool, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key { key, physical_key: None, pressed, repeat, modifiers }
    }

    #[test]
    fn enter_repeat_does_not_confirm() {
        let none = egui::Modifiers::NONE;
        // Только автоповтор: не подтверждает, но забирается из ввода.
        let mut ev = vec![key(egui::Key::Enter, true, true, none), key(egui::Key::Enter, true, true, none)];
        assert!(!take_enter(&mut ev));
        assert!(ev.is_empty());
        // Настоящее нажатие подтверждает; отпускание, Esc и Ctrl+Enter остаются в вводе.
        let mut ev = vec![
            key(egui::Key::Enter, true, false, none),
            key(egui::Key::Enter, false, false, none),
            key(egui::Key::Escape, true, false, none),
            key(egui::Key::Enter, true, false, egui::Modifiers::CTRL),
        ];
        assert!(take_enter(&mut ev));
        assert_eq!(ev.len(), 3);
    }

    /// Правило 3 стандарта: в окне свойств «ОК» первой, за ней «Отмена», «Применить» последней (а не «ОК, Применить,
    /// Отмена»). Главная — с подсказкой Enter, отмена — Esc.
    #[test]
    fn property_sheet_buttons_are_ok_cancel_apply() {
        let row = ok_cancel_apply_row(("OK", true), "Cancel", ("Apply", false));
        assert_eq!(row.iter().map(|b| b.text).collect::<Vec<_>>(), ["OK", "Cancel", "Apply"]);
        assert_eq!(row.iter().map(|b| b.key_hint).collect::<Vec<_>>(), [Some("Enter"), Some("Esc"), None]);
        assert!(!row[2].enabled, "nothing to apply - Apply is disabled");
    }

    /// `button_row` ставит кнопки на экране слева направо в порядке списка, прижатыми к правому краю, и отдаёт
    /// ответы в том же порядке: от этого зависит порядок кнопок во всех диалогах.
    #[test]
    fn button_row_lays_out_in_list_order() {
        let ctx = egui::Context::default();
        let mut rects = Vec::new();
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            let row = ok_cancel_apply_row(("OK", true), "Cancel", ("Apply", true));
            rects = button_row(ui, &row).iter().map(|r| r.rect).collect();
        });
        assert_eq!(rects.len(), 3);
        assert!(rects[0].right() <= rects[1].left() && rects[1].right() <= rects[2].left(), "order: {rects:?}");
        let screen = ctx.content_rect();
        assert!(screen.right() - rects[2].right() < 20.0, "row is right-aligned: {rects:?} in {screen:?}");
    }

    #[test]
    fn window_bounds_keep_a_margin_and_never_invert() {
        let content = Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(800.0, 500.0));
        assert_eq!(window_bounds(content), Rect::from_min_max(egui::pos2(12.0, 12.0), egui::pos2(788.0, 488.0)));
        // Крошечное главное окно: отступ не больше половины стороны, область не пустая и не вывернутая.
        let tiny = window_bounds(Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(16.0, 100.0)));
        assert_eq!((tiny.min.x, tiny.max.x), (8.0, 8.0));
        assert!(tiny.width() >= 0.0 && tiny.height() >= 0.0, "{tiny:?}");
    }

    /// Окно с этим содержимым три кадра подряд (положение и размер egui уточняет по прошлому кадру); прямоугольник
    /// окна из последнего кадра и область, где оно обязано быть.
    fn shown_rect(screen: Vec2, build: fn(egui::Window<'_>) -> egui::Window<'_>, content: Vec2) -> (Rect, Rect) {
        let ctx = egui::Context::default();
        let mut rect = Rect::NOTHING;
        for _ in 0..3 {
            let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, screen)), ..Default::default() };
            let _ = ctx.run_ui(input, |ui| {
                let ctx = ui.ctx();
                let mut open = true;
                let shown = build(dialog_window(ctx, "t", "test-bounds", &mut open)).show(ctx, |ui| {
                    ui.allocate_space(content);
                });
                rect = shown.expect("window is open").response.rect;
            });
        }
        (rect, window_bounds(ctx.content_rect()))
    }

    /// Правило: окно с изменяемым размером никогда не выходит за главное окно (с отступом `WINDOW_MARGIN`), даже если
    /// заданный размер больше, — а окно по размеру содержимого остаётся своего размера и по центру.
    #[test]
    fn dialogs_stay_inside_the_main_window() {
        let screen = Vec2::new(800.0, 500.0);
        fn big(w: egui::Window<'_>) -> egui::Window<'_> {
            w.resizable(true).default_size([1000.0, 560.0]).min_size([920.0, 360.0])
        }
        let (resizable, bounds) = shown_rect(screen, big, Vec2::new(10.0, 10.0));
        assert!(bounds.contains_rect(resizable), "resizable window {resizable:?} leaves {bounds:?}");
        // Ширина 1000 срезана до области (776), а не схлопнута; высоту egui у окна с изменяемым размером берёт по
        // содержимому (здесь оно 10 pt), поэтому её здесь не проверить — ограничение то же, `max_size`.
        assert!(resizable.width() > 700.0, "clamped to the bounds, not collapsed: {resizable:?}");
        // Окно по размеру содержимого: ограничение не растягивает и не сдвигает его.
        let (small, bounds) = shown_rect(screen, |w| w, Vec2::new(200.0, 100.0));
        // Ширина не меньше ~320: окно по содержимому egui начинает с размера по умолчанию и не сужает.
        assert!(small.width() < 400.0 && small.height() < 200.0, "{small:?}");
        assert!((small.center() - bounds.center()).length() < 2.0, "centred: {small:?} in {bounds:?}");
        assert!(bounds.contains_rect(small), "{small:?} in {bounds:?}");
    }

    #[test]
    fn escape_goes_to_the_top_window_only() {
        let layer = |id: &str| Some(egui::LayerId::new(egui::Order::Middle, egui::Id::new(id)));
        assert!(is_top_window(layer("conf-editor"), "conf-editor"));
        assert!(!is_top_window(layer("updates"), "conf-editor"));
        assert!(!is_top_window(None, "conf-editor"));
        // Тот же id, но не слой окон (всплывающее меню, подсказка) — не окно.
        assert!(!is_top_window(Some(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("conf-editor"))), "conf-editor"));
    }

    /// Правило окон: в src/ окно egui создаётся только в `dialog_window`, а в src/app ни одно окно не закрепляется
    /// на месте и не теряет заголовок (с ним — перетаскивание и крестик). Иглы собраны из частей, чтобы тест
    /// не находил сам себя.
    #[test]
    fn windows_follow_the_standard() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        collect_rs(&root, &mut files);
        assert!(files.len() > 10, "src/ not scanned: {}", root.display());
        let own = std::path::Path::new("app").join("dialog.rs");
        let banned_in_app = [concat!(".anc", "hor("), concat!(".mov", "able(false)"), concat!(".title", "_bar(false)")];
        let mut bad = Vec::new();
        for path in &files {
            let rel = path.strip_prefix(&root).unwrap();
            let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            let in_app = rel.starts_with("app") || rel == std::path::Path::new("app.rs");
            for (n, line) in text.lines().enumerate() {
                let at = format!("src/{}:{}", rel.display(), n + 1);
                if rel != own && creates_egui_window(line) {
                    bad.push(format!("{at}: egui window outside dialog_window"));
                }
                for b in banned_in_app.iter().filter(|b| in_app && line.contains(*b)) {
                    bad.push(format!("{at}: {b}"));
                }
            }
        }
        assert!(bad.is_empty(), "window standard broken:\n{}", bad.join("\n"));
    }

    #[test]
    fn egui_window_detection() {
        assert!(creates_egui_window(concat!("egui::Win", "dow::new(title)")));
        assert!(creates_egui_window(concat!("    Win", "dow::new(title)")));
        assert!(!creates_egui_window(concat!("UpdatesWin", "dow::new(link)")));
        assert!(!creates_egui_window("dialog_window(ctx, title, id, &mut open)"));
    }

    /// `Window::new(` как самостоятельный путь (`egui::Window::new(`, `Window::new(` после `use`),
    /// а не хвост другого типа (`UpdatesWindow::new(`).
    fn creates_egui_window(line: &str) -> bool {
        let needle = concat!("Win", "dow::new(");
        line.match_indices(needle).any(|(i, _)| !line[..i].ends_with(|c: char| c.is_alphanumeric() || c == '_'))
    }

    pub(in crate::app) fn collect_rs(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
            let path = entry.unwrap_or_else(|e| panic!("{}: {e}", dir.display())).path();
            if path.is_dir() {
                collect_rs(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
}
