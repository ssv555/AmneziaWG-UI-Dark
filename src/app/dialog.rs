//! Общие части окон: единственный конструктор окна, ряд кнопок и клавиши Enter/Esc.
//!
//! Правило окон (классика Windows): у каждого окна заголовок, за который его перетаскивают, крестик в заголовке
//! и Esc, закрывающий его. Поэтому окна строятся только через `dialog_window` — тест `windows_follow_the_standard`
//! не пускает окно egui в обход него и запрещает закреплять окно на месте или прятать заголовок.

use eframe::egui::{self, Align2, Rect, RichText, Ui, Vec2};

use super::theme::palette;

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
pub(super) fn window_bounds(content: Rect) -> Rect {
    let margin = WINDOW_MARGIN.min(content.width() / 2.0).min(content.height() / 2.0).max(0.0);
    content.shrink(margin)
}

/// Высота кнопки ряда (`button_row`); столько же `dialog_body` оставляет под рядом.
const BUTTON_H: f32 = 26.0;
/// Запас под рядом кнопок: отступ перед ним у вызывающих (до 10), интервалы раскладки и рамка окна (тест ловит нехватку).
const BUTTON_ROW_RESERVE: f32 = BUTTON_H + 28.0;

/// Тело окна по размеру содержимого, которое всегда умещается в главное окно. Ширина — `width`, но не шире области
/// окон (при масштабе 200 % в окне 760 px области всего 356 pt); высота — сколько остаётся в области после заголовка
/// окна и ряда кнопок, длиннее — прокрутка. Ряд кнопок вызывающий рисует сразу за телом: он виден всегда, иначе
/// «Настройки» при масштабе больше 100 % обрезались бы снизу вместе с «ОК» (`max_size` окна режет, не прокручивает).
/// Тест `dialog_body_keeps_the_buttons_reachable`; правило «ширина диалога только отсюда» — `dialog_widths_go_through_dialog_body`.
pub(super) fn dialog_body(ui: &mut Ui, width: f32, body: impl FnOnce(&mut Ui)) {
    let bounds = window_bounds(ui.ctx().content_rect());
    // Поля рамки окна и её обводка (по 1 px с каждой стороны) не входят в ширину содержимого.
    let frame = ui.spacing().window_margin.sum() + Vec2::splat(2.0);
    ui.set_width((width).min(bounds.width() - frame.x).max(0.0));
    // Заголовок окна egui: строка шрифта заголовка и два интервала (как в `egui::Window`).
    let title = ui.text_style_height(&egui::TextStyle::Heading) + 2.0 * ui.spacing().item_spacing.y;
    let max_height = (bounds.height() - frame.y - title - BUTTON_ROW_RESERVE).max(ui.spacing().interact_size.y);
    egui::ScrollArea::vertical().id_salt("dialog-body").max_height(max_height).auto_shrink([false, true]).show(ui, body);
}

/// Строка ошибки ввода под полем: мелко, цветом ошибки, с переносом. Пустая занимает одну строку, чтобы окно не
/// прыгало при наборе; длинная растёт, а не рисуется поверх кнопок (как делал ящик постоянной высоты).
pub(super) fn error_line(ui: &mut Ui, text: &str) {
    let line = ui.text_style_height(&egui::TextStyle::Small) + ui.spacing().item_spacing.y;
    ui.allocate_ui_with_layout(Vec2::new(ui.available_width(), line), egui::Layout::top_down(egui::Align::Min), |ui| {
        ui.add(egui::Label::new(egui::RichText::new(text).color(super::theme::palette().error).small()).wrap());
    });
}

/// Esc для немодального окна `id`: забирается из ввода, только если это окно верхнее (его последним щёлкнули
/// или оно последним появилось) — как в Windows, где Esc получает активное окно, а не первое по порядку отрисовки.
/// Модальные диалоги рисуются раньше и забирают Esc сами (`Turn::keys`). Открытый список или меню забирает Esc себе
/// (как в `dialog_keys`).
pub(super) fn window_escape(ctx: &egui::Context, id: &str) -> bool {
    !popup_open_at_frame_start(ctx)
        && is_top_window(top_window(ctx), id)
        && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
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

/// Кнопка ряда: текст, включена ли, подсказка с клавишей (Enter у главной, Esc у отмены), главная ли (кнопка
/// по умолчанию: её нажимает Enter, когда фокус не на кнопке, и она залита акцентом).
struct RowButton<'a> {
    text: &'a str,
    enabled: bool,
    key_hint: Option<&'static str>,
    primary: bool,
}

/// Заливка и цвет текста кнопки: у главной — акцент темы (как у кнопки по умолчанию в Windows 11), остальные —
/// обычные кнопки egui (`None`). Пара проверена на контраст в каждой теме (`every_palette_meets_wcag_contrast_targets`).
fn button_look(b: &RowButton) -> Option<(egui::Color32, egui::Color32)> {
    let p = palette();
    b.primary.then_some((p.accent, p.on_accent))
}

/// Единственная раскладка кнопок диалога: полоса по правому краю, кнопки слева направо в порядке `buttons`.
/// Ответы — в том же порядке. Порядок задают вызывающие ниже, по правилу Windows: главная первой, «Отмена» за ней,
/// «Применить» последней.
fn button_row(ui: &mut Ui, buttons: &[RowButton]) -> Vec<egui::Response> {
    let size = Vec2::new(88.0, BUTTON_H);
    // Полоса высотой в кнопку: `with_layout` занял бы всю высоту окна.
    let layout = egui::Layout::right_to_left(egui::Align::Center);
    let mut responses = ui
        .allocate_ui_with_layout(Vec2::new(ui.available_width(), size.y), layout, |ui| {
            // Раскладка справа налево: кнопки добавляются с конца, чтобы на экране стоять в порядке `buttons`.
            buttons
                .iter()
                .rev()
                .map(|b| {
                    let button = match button_look(b) {
                        Some((fill, text)) => egui::Button::new(RichText::new(b.text).color(text)).fill(fill),
                        None => egui::Button::new(b.text),
                    };
                    let resp = ui.add_enabled(b.enabled, button.min_size(size));
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
    RowButton { text, enabled, key_hint: Some("Enter"), primary: true }
}

fn cancel(text: &str) -> RowButton<'_> {
    RowButton { text, enabled: true, key_hint: Some("Esc"), primary: false }
}

fn plain(text: &str, enabled: bool) -> RowButton<'_> {
    RowButton { text, enabled, key_hint: None, primary: false }
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
    let r = button_row(ui, &[primary(primary_text, true), plain(other, true), cancel(cancel_text)]);
    (r[0].clicked(), r[1].clicked(), r[2].clicked())
}

/// Кнопки окна свойств в порядке Windows: «ОК» (Enter), «Отмена» (Esc), «Применить». «ОК» и «Применить» выключаются
/// (неверный ввод, нечего применять); «Отмена» есть всегда. Возвращает (ОК, отмена, применить).
pub(super) fn dialog_ok_cancel_apply(ui: &mut Ui, ok: (&str, bool), cancel_text: &str, apply: (&str, bool)) -> (bool, bool, bool) {
    let r = button_row(ui, &ok_cancel_apply_row(ok, cancel_text, apply));
    (r[0].clicked(), r[1].clicked(), r[2].clicked())
}

fn ok_cancel_apply_row<'a>(ok: (&'a str, bool), cancel_text: &'a str, apply: (&'a str, bool)) -> [RowButton<'a>; 3] {
    [primary(ok.0, ok.1), cancel(cancel_text), plain(apply.0, apply.1)]
}

/// (Enter, Esc) для верхнего диалога, по правилам Windows:
/// 1. Enter нажимает элемент в фокусе — кнопку (в том числе «Отмена»), флажок, кнопку списка; его нажимает сам egui,
///    поэтому Enter ему и оставляется, а диалог Enter не получает. Фокус не на таком элементе (нигде, в поле ввода) —
///    Enter означает кнопку по умолчанию (главную) и забирается из ввода.
/// 2. Открыт выпадающий список или меню — Enter и Esc его: Esc закрывает только список, диалог не видит ни того,
///    ни другого. Открыт ли он, смотрится на начало кадра: список закрывается сам, пока рисуется.
///
/// Esc забирается из ввода, чтобы его не увидели окна под диалогом. Enter считается только настоящим нажатием:
/// автоповтор удержанной клавиши подтвердил бы только что открытый диалог, который ещё не прочитан.
/// Порядок вызова любой — до рисования диалога (как в «Настройках») или после: итог один (тесты — в обоих порядках).
pub(super) fn dialog_keys(ctx: &egui::Context) -> (bool, bool) {
    if popup_open_at_frame_start(ctx) {
        return (false, false);
    }
    let to_focused = focused_control(ctx);
    ctx.input_mut(|i| (take_enter(&mut i.events, to_focused), i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)))
}

/// Фокус на элементе, который Enter нажимает сам (кнопка, флажок, кнопка списка — всё, что откликается на щелчок),
/// а не на поле ввода (оно, как и ползунок, ещё и перетаскивается). Элемент окна под модальным диалогом не в счёт:
/// egui его не нажмёт, и Enter остался бы ничьим.
fn focused_control(ctx: &egui::Context) -> bool {
    let Some(id) = ctx.memory(|m| m.focused()) else {
        return false;
    };
    if egui::TextEdit::load_state(ctx, id).is_some() {
        return false;
    }
    ctx.read_response(id).is_some_and(|r| {
        r.enabled() && r.sense.senses_click() && !r.sense.senses_drag() && ctx.memory(|m| m.allows_interaction(r.layer_id))
    })
}

/// Забирает из событий автоповторы Enter без модификаторов (чтобы не ушли окнам ниже) и настоящие нажатия — кроме
/// случая `to_focused`: тогда нажатие остаётся элементу в фокусе. `true` — было настоящее нажатие, и оно забрано.
fn take_enter(events: &mut Vec<egui::Event>, to_focused: bool) -> bool {
    let mut pressed = false;
    events.retain(|e| match e {
        egui::Event::Key { key: egui::Key::Enter, pressed: true, repeat, modifiers, .. }
            if modifiers.matches_logically(egui::Modifiers::NONE) =>
        {
            if *repeat {
                return false;
            }
            pressed = !to_focused;
            to_focused
        }
        _ => true,
    });
    pressed
}

/// Был ли открыт выпадающий список или меню в начале этого кадра. Список закрывается сам, пока рисуется (Esc,
/// щелчок мимо), и вызывающий после рисования увидел бы «закрыт» — тот же Esc закрыл бы и диалог. Снимок делает
/// `note_popups` первым делом в кадре; кадр без снимка (его не сделали) отвечает по текущему состоянию.
fn popup_open_at_frame_start(ctx: &egui::Context) -> bool {
    let pass = ctx.cumulative_pass_nr();
    match ctx.data(|d| d.get_temp::<(u64, bool)>(popup_snapshot_id())) {
        Some((at, open)) if at == pass => open,
        _ => egui::Popup::is_any_open(ctx),
    }
}

fn popup_snapshot_id() -> egui::Id {
    egui::Id::new("awg-ui/popup-open-at-frame-start")
}

/// Запомнить, открыт ли сейчас список или меню, — для `dialog_keys` и `window_escape` этого кадра. Вызывается в
/// начале кадра, до рисования чего-либо (`App::ui`).
pub(super) fn note_popups(ctx: &egui::Context) {
    let open = egui::Popup::is_any_open(ctx);
    let pass = ctx.cumulative_pass_nr();
    ctx.data_mut(|d| d.insert_temp(popup_snapshot_id(), (pass, open)));
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
        assert!(!take_enter(&mut ev, false));
        assert!(ev.is_empty());
        // Настоящее нажатие подтверждает; отпускание, Esc и Ctrl+Enter остаются в вводе.
        let events = vec![
            key(egui::Key::Enter, true, false, none),
            key(egui::Key::Enter, false, false, none),
            key(egui::Key::Escape, true, false, none),
            key(egui::Key::Enter, true, false, egui::Modifiers::CTRL),
        ];
        let mut ev = events.clone();
        assert!(take_enter(&mut ev, false));
        assert_eq!(ev.len(), 3);
        // Фокус на кнопке: нажатие остаётся ей, диалог его не получает; автоповтор забирается всё равно.
        let mut ev = events;
        ev.push(key(egui::Key::Enter, true, true, none));
        assert!(!take_enter(&mut ev, true));
        assert_eq!(ev.len(), 4);
        assert!(ev.iter().all(|e| !matches!(e, egui::Event::Key { repeat: true, .. })));
    }

    /// U16: в каждом ряду ровно одна главная кнопка, и только она залита акцентом с его цветом текста.
    #[test]
    fn exactly_one_primary_button_is_accent_filled() {
        let rows: [Vec<RowButton>; 3] = [
            vec![primary("Delete", true), cancel("Cancel")],
            vec![primary("Save", true), plain("Don't save", true), cancel("Cancel")],
            ok_cancel_apply_row(("OK", true), "Cancel", ("Apply", false)).into_iter().collect(),
        ];
        let p = palette();
        for row in &rows {
            let looks: Vec<_> = row.iter().map(button_look).collect();
            assert_eq!(looks.iter().filter(|l| l.is_some()).count(), 1, "one default button per row");
            assert_eq!(looks[0], Some((p.accent, p.on_accent)), "primary comes first and is accent-filled");
            assert_eq!(row.iter().filter(|b| b.key_hint == Some("Enter")).count(), 1);
        }
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

    fn press(key: egui::Key) -> egui::Event {
        egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE }
    }

    fn release(key: egui::Key) -> egui::Event {
        egui::Event::Key { key, physical_key: None, pressed: false, repeat: false, modifiers: egui::Modifiers::NONE }
    }

    /// Что увидел вызывающий в кадре диалога: нажатые кнопки, Enter/Esc от `dialog_keys` и id элементов.
    #[derive(Default, Debug)]
    struct Seen {
        primary: bool,
        cancel: bool,
        enter: bool,
        escape: bool,
        buttons: Vec<egui::Id>,
        field: Option<egui::Id>,
        /// Id списка выпадающего списка (как у egui: id кнопки с "popup").
        combo: Option<egui::Id>,
    }

    /// Кадр диалога «поле ввода, выпадающий список, Удалить / Отмена» с этими событиями. `keys_first` — Enter и Esc
    /// забираются до рисования (как в «Настройках»), иначе после (как в остальных диалогах): правило одно для обоих.
    fn dialog_frame(ctx: &egui::Context, events: Vec<egui::Event>, keys_first: bool) -> Seen {
        let mut seen = Seen::default();
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(800.0, 500.0))),
            events,
            ..Default::default()
        };
        let _ = ctx.run_ui(input, |ui| {
            let ctx = ui.ctx();
            // Как `App::ui`: снимок «открыт ли список» до рисования.
            note_popups(ctx);
            if keys_first {
                (seen.enter, seen.escape) = dialog_keys(ctx);
            }
            let mut open = true;
            dialog_window(ctx, "t", "test-keys", &mut open).show(ctx, |ui| {
                let mut text = String::new();
                seen.field = Some(ui.text_edit_singleline(&mut text).id);
                let mut choice = 0;
                let combo = egui::ComboBox::from_id_salt("test-combo").selected_text("a").show_ui(ui, |ui| {
                    ui.selectable_value(&mut choice, 1, "b");
                });
                seen.combo = Some(egui::Popup::default_response_id(&combo.response));
                let r = button_row(ui, &[primary("Delete", true), cancel("Cancel")]);
                (seen.primary, seen.cancel) = (r[0].clicked(), r[1].clicked());
                seen.buttons = r.iter().map(|r| r.id).collect();
            });
            if !keys_first {
                (seen.enter, seen.escape) = dialog_keys(ctx);
            }
        });
        seen
    }

    /// Диалог показан несколько кадров (egui уточняет раскладку окна по прошлому кадру).
    fn settled(ctx: &egui::Context, keys_first: bool) -> Seen {
        (0..3).map(|_| dialog_frame(ctx, Vec::new(), keys_first)).last().unwrap()
    }

    /// U1: Tab на «Отмена» и Enter — это «Отмена», как в Windows, а не главная кнопка (раньше удалялся туннель).
    #[test]
    fn enter_on_a_focused_button_presses_that_button_only() {
        for keys_first in [false, true] {
            let ctx = egui::Context::default();
            let s = settled(&ctx, keys_first);
            ctx.memory_mut(|m| m.request_focus(s.buttons[1]));
            let s = dialog_frame(&ctx, vec![press(egui::Key::Enter), release(egui::Key::Enter)], keys_first);
            assert!(s.cancel, "keys_first={keys_first}: focused Cancel not pressed: {s:?}");
            assert!(!s.primary && !s.enter, "keys_first={keys_first}: Enter on Cancel confirmed: {s:?}");
            // Фокус на главной кнопке: Enter нажимает её саму, и подтверждение одно.
            ctx.memory_mut(|m| m.request_focus(s.buttons[0]));
            let s = dialog_frame(&ctx, vec![press(egui::Key::Enter), release(egui::Key::Enter)], keys_first);
            assert!(s.primary && !s.cancel && !s.enter, "keys_first={keys_first}: {s:?}");
        }
    }

    /// Фокус не на кнопке (нигде, в поле ввода) — Enter достаётся кнопке по умолчанию.
    #[test]
    fn enter_elsewhere_is_the_default_button() {
        for keys_first in [false, true] {
            let ctx = egui::Context::default();
            let s = settled(&ctx, keys_first);
            let s2 = dialog_frame(&ctx, vec![press(egui::Key::Enter), release(egui::Key::Enter)], keys_first);
            assert!(s2.enter && !s2.cancel, "keys_first={keys_first}: nothing focused: {s2:?}");
            ctx.memory_mut(|m| m.request_focus(s.field.unwrap()));
            let s3 = dialog_frame(&ctx, vec![press(egui::Key::Enter), release(egui::Key::Enter)], keys_first);
            assert!(s3.enter && !s3.cancel, "keys_first={keys_first}: text field focused: {s3:?}");
        }
    }

    /// U17: открыт выпадающий список — Esc закрывает только список, Enter не подтверждает диалог; следующий Esc
    /// закрывает диалог.
    #[test]
    fn keys_with_an_open_list_stay_in_the_list() {
        for keys_first in [false, true] {
            let ctx = egui::Context::default();
            let s = settled(&ctx, keys_first);
            let list = s.combo.unwrap();
            egui::Popup::open_id(&ctx, list);
            dialog_frame(&ctx, Vec::new(), keys_first);
            let s = dialog_frame(&ctx, vec![press(egui::Key::Enter), release(egui::Key::Enter)], keys_first);
            assert!(!s.enter && !s.escape, "keys_first={keys_first}: Enter in an open list reached the dialog: {s:?}");
            egui::Popup::open_id(&ctx, list);
            dialog_frame(&ctx, Vec::new(), keys_first);
            assert!(egui::Popup::is_id_open(&ctx, list));
            let s = dialog_frame(&ctx, vec![press(egui::Key::Escape), release(egui::Key::Escape)], keys_first);
            assert!(!s.escape, "keys_first={keys_first}: Esc in an open list closed the dialog: {s:?}");
            assert!(!egui::Popup::is_any_open(&ctx), "keys_first={keys_first}: Esc did not close the list");
            let s = dialog_frame(&ctx, vec![press(egui::Key::Escape), release(egui::Key::Escape)], keys_first);
            assert!(s.escape, "keys_first={keys_first}: list closed, Esc must close the dialog: {s:?}");
        }
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

    /// Окно с телом из `dialog_body` и рядом кнопок за ним, три кадра; (окно, ряд кнопок, область окон).
    fn body_rects(screen: Vec2, width: f32, lines: usize) -> (Rect, Rect, Rect) {
        let ctx = egui::Context::default();
        let (mut window, mut row) = (Rect::NOTHING, Rect::NOTHING);
        for _ in 0..3 {
            let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, screen)), ..Default::default() };
            let _ = ctx.run_ui(input, |ui| {
                let ctx = ui.ctx();
                let mut open = true;
                let shown = dialog_window(ctx, "t", "test-body", &mut open).show(ctx, |ui| {
                    dialog_body(ui, width, |ui| {
                        for i in 0..lines {
                            ui.label(format!("line {i}"));
                        }
                    });
                    ui.add_space(10.0);
                    let r = button_row(ui, &ok_cancel_apply_row(("OK", true), "Cancel", ("Apply", true)));
                    row = r[0].rect.union(r[2].rect);
                });
                window = shown.expect("window is open").response.rect;
            });
        }
        (window, row, window_bounds(ctx.content_rect()))
    }

    /// Правило 6 стандарта: тело диалога не шире области окон, а ряд кнопок виден всегда — и в окне 760x480 с
    /// телом выше окна («Настройки» при масштабе больше 100 %), и при масштабе 200 % (область 356x216 pt).
    #[test]
    fn dialog_body_keeps_the_buttons_reachable() {
        for (screen, width) in [(Vec2::new(760.0, 480.0), 460.0), (Vec2::new(380.0, 240.0), 560.0)] {
            let (window, row, bounds) = body_rects(screen, width, 40);
            assert!(bounds.expand(0.5).contains_rect(window), "{screen:?}: window {window:?} leaves {bounds:?}");
            assert!(window.contains_rect(row), "{screen:?}: buttons {row:?} outside the window {window:?}");
            assert!(row.height() >= BUTTON_H - 0.5, "{screen:?}: button row clipped: {row:?}");
            // Тело без прокрутки остаётся своего размера: окно не растягивается до области.
            let (small, row, _) = body_rects(screen, width, 2);
            assert!(small.height() < window.height() - 60.0, "{screen:?}: short body {small:?} vs long {window:?}");
            assert!(small.contains_rect(row));
        }
    }

    /// Строка ошибки: пустая и короткая — одна строка (окно не прыгает при наборе), длинная растёт вниз, а не
    /// рисуется поверх кнопок (так делал `add_sized` постоянной высоты в диалоге имени туннеля).
    #[test]
    fn error_line_grows_with_the_text() {
        let ctx = egui::Context::default();
        let heights = |text: &str| {
            let (mut error, mut row) = (Rect::NOTHING, Rect::NOTHING);
            let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
                ui.set_width(300.0);
                let before = ui.cursor().top();
                error_line(ui, text);
                let after = ui.cursor().top();
                error = Rect::from_min_max(egui::pos2(0.0, before), egui::pos2(300.0, after));
                row = button_row(ui, &[primary("OK", true)])[0].rect;
            });
            (error, row)
        };
        let (empty, _) = heights("");
        let (short, _) = heights("Name is taken");
        let (long, row) = heights(&"Tunnel name is not allowed: up to 32 Latin letters, digits and _ = + . - ".repeat(3));
        assert!((empty.height() - short.height()).abs() < 0.5, "empty {empty:?} vs one line {short:?}");
        assert!(long.height() > short.height() * 2.5, "three lines {long:?} vs one {short:?}");
        assert!(row.top() >= long.bottom() - 0.5, "buttons {row:?} start below the error {long:?}");
    }

    /// Ширина диалога числом задаётся только через `dialog_body`: `ui.set_width(460.0)` в самом окне при масштабе
    /// 200 % делало окно шире области (356 pt) — правая часть и кнопки за краем. Иглы собраны из частей.
    #[test]
    fn dialog_widths_go_through_dialog_body() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join("app");
        let mut files = Vec::new();
        collect_rs(&root, &mut files);
        let own = root.join("dialog.rs");
        let needle = concat!("set_wid", "th(");
        let mut bad = Vec::new();
        for path in files.iter().filter(|p| **p != own) {
            let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            for (n, line) in text.lines().enumerate() {
                let numeric = line.match_indices(needle).any(|(i, _)| line[i + needle.len()..].starts_with(|c: char| c.is_ascii_digit()));
                if numeric {
                    bad.push(format!("src/app/{}:{}: {}", path.strip_prefix(&root).unwrap().display(), n + 1, line.trim()));
                }
            }
        }
        assert!(bad.is_empty(), "dialog width outside dialog_body:\n{}", bad.join("\n"));
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
