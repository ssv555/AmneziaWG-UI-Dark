//! Общие части окон: единственный конструктор окна, ряд кнопок и клавиши Enter/Esc.
//!
//! Правило окон (классика Windows): у каждого окна заголовок, за который его перетаскивают, крестик в заголовке
//! и Esc, закрывающий его. Поэтому окна строятся только через `dialog_window` — тест `windows_follow_the_standard`
//! не пускает окно egui в обход него и запрещает закреплять окно на месте или прятать заголовок.

use eframe::egui::{self, Align2, Ui, Vec2};

/// Окно программы: заголовок, перетаскивается, крестик в заголовке. Крестик сбрасывает `open` — вызывающий
/// обязан понимать `!open` как свою отмену («Отмена», «Позже», «Закрыть»), так же как Esc.
/// При открытии по центру; позицию egui дальше помнит по `id`, поэтому он постоянный, а не из заголовка (тот меняется
/// с языком и содержимым). Не сворачивается и не меняет размер, пока вызывающий не разрешит: ширину задаёт содержимое.
/// Начальное место можно сменить (`pivot` + `default_pos`), закрепить — нельзя.
pub(super) fn dialog_window<'a>(ctx: &egui::Context, title: impl Into<egui::WidgetText>, id: &str, open: &'a mut bool) -> egui::Window<'a> {
    egui::Window::new(title)
        .id(egui::Id::new(id))
        .open(open)
        .collapsible(false)
        .resizable(false)
        .pivot(Align2::CENTER_CENTER)
        .default_pos(ctx.screen_rect().center())
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

/// Кнопки диалога по правому краю в порядке Windows: главная первой, «Отмена» последней.
/// Возвращает (главная нажата, отмена нажата).
pub(super) fn dialog_buttons(ui: &mut Ui, primary: &str, enabled: bool, cancel: Option<&str>) -> (bool, bool) {
    let (mut ok, mut no) = (false, false);
    let size = Vec2::new(88.0, 26.0);
    // Полоса высотой в кнопку: `with_layout` занял бы всю высоту окна.
    let layout = egui::Layout::right_to_left(egui::Align::Center);
    ui.allocate_ui_with_layout(Vec2::new(ui.available_width(), size.y), layout, |ui| {
        // Справа налево: сначала «Отмена», левее — главная.
        if let Some(c) = cancel {
            no = ui.add(egui::Button::new(c).min_size(size)).on_hover_text("Esc").clicked();
        }
        ok = ui.add_enabled(enabled, egui::Button::new(primary).min_size(size)).on_hover_text("Enter").clicked();
    });
    (ok, no)
}

/// Три кнопки по правому краю: главная (Enter), второй вариант, «Отмена» (Esc).
/// Возвращает (главная, второй вариант, отмена).
pub(super) fn dialog_choice(ui: &mut Ui, primary: &str, other: &str, cancel: &str) -> (bool, bool, bool) {
    let (mut ok, mut alt, mut no) = (false, false, false);
    let size = Vec2::new(88.0, 26.0);
    let layout = egui::Layout::right_to_left(egui::Align::Center);
    ui.allocate_ui_with_layout(Vec2::new(ui.available_width(), size.y), layout, |ui| {
        no = ui.add(egui::Button::new(cancel).min_size(size)).on_hover_text("Esc").clicked();
        alt = ui.add(egui::Button::new(other).min_size(size)).clicked();
        ok = ui.add(egui::Button::new(primary).min_size(size)).on_hover_text("Enter").clicked();
    });
    (ok, alt, no)
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
mod tests {
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

    fn collect_rs(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
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
