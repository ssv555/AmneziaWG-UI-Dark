//! Модальные диалоги окна — один стек вместо отдельного поля `App` на каждый диалог.
//! Enter и Esc получает только верхний (последний открытый: egui рисует новое окно поверх прежних);
//! фокус поля ввода — только в кадре открытия; галочка «Больше не показывать» — у каждого диалога своя.
//!
//! Перечисление, а не `Box<dyn Modal>`: каждый диалог действует через `App` (настройки, ядро, строка состояния),
//! набор диалогов закрыт, и одна `match` в `App::show_modal` проще, чем трейт с окружением из половины `App`.

use std::mem::discriminant;
use std::path::PathBuf;

use eframe::egui;

use super::dialog::{dialog_keys, dialog_window};
use super::engine_mode::EngineDialog;
use super::group_dialog::Dialog;
use super::sources::Confirm;
use super::updates::Confirm as UpdateConfirm;
use crate::settings::Mode;

/// Открытый модальный диалог. Каждого вида — не больше одного: повторное открытие заменяет прежний.
pub(super) enum Modal {
    /// Имя группы: новая или переименование.
    Group(Dialog),
    /// Окна режима 2: пароль резервной копии, имя туннеля.
    Engine(EngineDialog),
    /// Смена режима работы со справкой.
    ModeSwitch(Mode),
    /// Выход при подключённых туннелях; `native` — закрыть и родное окно AmneziaWG.
    Exit { native: bool },
    /// Источник сохранён — спросить, добавить ли его в AmneziaWG.
    AskImport { tunnel: String, path: PathBuf },
    /// Перезапись или удаление ждёт подтверждения.
    Confirm(Confirm),
    /// Установка или возврат в окне «Обновления и откаты» ждёт подтверждения.
    UpdatesConfirm(UpdateConfirm),
    /// Редактор .conf закрывают с несохранёнными изменениями: сохранить, не сохранять или остаться.
    EditorUnsaved,
    /// Конфиг в редакторе с ошибками сохраняют: записать всё равно (и что сделать затем) или вернуться к правке.
    EditorInvalid(super::editor::AfterSave),
    About,
    /// Окно «Настройки» с черновиком параметров.
    Settings(Box<super::settings_dialog::SettingsDialog>),
}

/// Что диалог решил в этом кадре.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Outcome {
    Keep,
    Close,
}

struct Entry {
    modal: Modal,
    /// Кадр открытия: поле ввода получает фокус один раз, а не каждый кадр.
    fresh: bool,
    /// Галочка «Больше не показывать» / «Запомнить выбор» этого диалога.
    remember: bool,
    /// Id окна диалога в этом кадре (`Turn::window`): его слой становится модальным.
    window: Option<egui::Id>,
}

/// Кадр одного диалога: его состояние в стеке и право на Enter/Esc.
pub(super) struct Turn<'a> {
    pub(super) fresh: bool,
    pub(super) remember: &'a mut bool,
    window: &'a mut Option<egui::Id>,
    top: bool,
}

impl Turn<'_> {
    /// (Enter, Esc) для верхнего диалога по правилам `dialog_keys` — забираются из ввода, нижние и окна под ними
    /// их не видят. Нижним — ничего: иначе один Enter подтвердил бы сразу два диалога.
    pub(super) fn keys(&self, ctx: &egui::Context) -> (bool, bool) {
        if self.top {
            dialog_keys(ctx)
        } else {
            (false, false)
        }
    }

    /// Окно этого диалога (`dialog_window`), модальное: пока оно открыто, главное окно и окна под ним не нажимаются
    /// и не получают фокус по Tab (`Modals::run`). Все диалоги стека строят окно только так (тест
    /// `modal_dialogs_build_their_window_through_the_turn`).
    pub(super) fn window<'o>(
        &mut self,
        ctx: &egui::Context,
        title: impl Into<egui::WidgetText>,
        id: &str,
        open: &'o mut bool,
    ) -> egui::Window<'o> {
        *self.window = Some(egui::Id::new(id));
        dialog_window(ctx, title, id, open)
    }
}

#[derive(Default)]
pub(super) struct Modals {
    /// Снизу вверх: последний — верхний.
    stack: Vec<Entry>,
}

impl Modals {
    /// Открыть диалог поверх остальных; открытый диалог того же вида закрывается.
    pub(super) fn open(&mut self, modal: Modal) {
        self.stack.retain(|e| discriminant(&e.modal) != discriminant(&modal));
        self.stack.push(Entry { modal, fresh: true, remember: false, window: None });
    }

    /// Открыть, только если диалога этого вида ещё нет (вопрос об импорте не перебивает уже заданный).
    pub(super) fn open_if_absent(&mut self, modal: Modal) -> bool {
        if self.stack.iter().any(|e| discriminant(&e.modal) == discriminant(&modal)) {
            return false;
        }
        self.open(modal);
        true
    }

    /// Открыт хоть один диалог — клавиши таблицы и поиска не работают.
    pub(super) fn any_open(&self) -> bool {
        !self.stack.is_empty()
    }

    /// Открыт ли диалог, на который указывает `kind`.
    pub(super) fn is_open(&self, kind: impl Fn(&Modal) -> bool) -> bool {
        self.stack.iter().any(|e| kind(&e.modal))
    }

    /// Закрыть диалоги, на которые указывает `kind`: их окно-владелец закрылось.
    pub(super) fn close(&mut self, kind: impl Fn(&Modal) -> bool) {
        self.stack.retain(|e| !kind(&e.modal));
    }

    /// Кадр: каждый диалог снизу вверх; закрытые уходят из стека.
    /// Модальность, как в Windows: в кадре открытия фокус уходит с элемента главного окна (иначе Enter нажал бы его,
    /// а не кнопку диалога), а окно верхнего оставшегося диалога становится модальным (`make_modal`): со следующего
    /// кадра всё под ним не нажимается и не получает фокус по Tab.
    pub(super) fn run(&mut self, ctx: &egui::Context, mut show: impl FnMut(&mut Modal, Turn<'_>) -> Outcome) {
        let top = self.stack.len().checked_sub(1);
        let mut index = 0;
        let mut top_window = None;
        self.stack.retain_mut(|e| {
            if e.fresh {
                ctx.memory_mut(|m| {
                    if let Some(id) = m.focused() {
                        m.surrender_focus(id);
                    }
                });
            }
            e.window = None;
            let turn = Turn { fresh: e.fresh, remember: &mut e.remember, window: &mut e.window, top: Some(index) == top };
            let outcome = show(&mut e.modal, turn);
            e.fresh = false;
            index += 1;
            if outcome == Outcome::Keep {
                top_window = e.window.or(top_window);
            }
            outcome == Outcome::Keep
        });
        if let Some(window) = top_window {
            make_modal(ctx, window);
        }
    }

    /// Диалоги, открытые действиями внутри кадра (`run` шёл на вынутом стеке), — поверх оставшихся.
    pub(super) fn absorb(&mut self, opened: Modals) {
        for e in opened.stack {
            self.open(e.modal);
        }
    }
}

/// Окно `window` (любого порядка: подтверждения бывают в `Foreground`) — модальное, как в Windows:
/// 1. Под ним невидимая подложка во всё главное окно забирает щелчки и перетаскивания: модальный слой egui сам их
///    не держит, он ограничивает только фокус. Подложка — над всеми окнами, окно диалога — сразу над подложкой.
/// 2. Слой окна — модальный слой egui: Tab не уходит к окнам под ним.
///
/// Окна, ещё не попавшего в порядок слоёв egui, нет — блокировать нечего (кадр открытия; со следующего блокирует).
fn make_modal(ctx: &egui::Context, window: egui::Id) {
    let Some(layer) = ctx.memory(|m| m.layer_ids().find(|l| l.id == window)) else { return };
    let screen = ctx.content_rect();
    let backdrop = egui::Area::new(egui::Id::new("modal-backdrop"))
        .order(egui::Order::Middle)
        .fixed_pos(screen.min)
        .constrain(false)
        .show(ctx, |ui| {
            // Щелчки и перетаскивания, но не фокус: Tab на подложку не попадает.
            ui.allocate_exact_size(screen.size(), egui::Sense::CLICK | egui::Sense::DRAG);
        })
        .response
        .layer_id;
    ctx.move_to_top(backdrop);
    ctx.set_sublayer(backdrop, layer);
    ctx.memory_mut(|m| m.set_modal_layer(layer));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(key: egui::Key) -> egui::Event {
        egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE }
    }

    fn kind(m: &Modal) -> &'static str {
        match m {
            Modal::About => "about",
            Modal::ModeSwitch(_) => "mode",
            Modal::Exit { .. } => "exit",
            _ => "other",
        }
    }

    /// Один кадр с этими событиями ввода: (вид, Enter, Esc) по каждому диалогу снизу вверх.
    /// Контекст общий для кадров: признак автоповтора egui ставит сам по удержанным клавишам.
    fn frame(ctx: &egui::Context, modals: &mut Modals, events: Vec<egui::Event>, close: &[&str]) -> Vec<(&'static str, bool, bool)> {
        let mut seen = Vec::new();
        let _ = ctx.run_ui(egui::RawInput { events, ..Default::default() }, |ui| {
            let ctx = ui.ctx();
            modals.run(ctx, |m, turn| {
                let (enter, escape) = turn.keys(ctx);
                seen.push((kind(m), enter, escape));
                if close.contains(&kind(m)) {
                    Outcome::Close
                } else {
                    Outcome::Keep
                }
            });
        });
        seen
    }

    #[test]
    fn enter_and_escape_go_to_the_top_dialog_only() {
        let ctx = egui::Context::default();
        let mut modals = Modals::default();
        modals.open(Modal::About);
        modals.open(Modal::ModeSwitch(Mode::Engine));
        modals.open(Modal::Exit { native: false });
        let seen = frame(&ctx, &mut modals, vec![press(egui::Key::Enter)], &[]);
        assert_eq!(seen, [("about", false, false), ("mode", false, false), ("exit", true, false)]);
        let seen = frame(&ctx, &mut modals, vec![press(egui::Key::Escape)], &[]);
        assert_eq!(seen, [("about", false, false), ("mode", false, false), ("exit", false, true)]);
        // Enter не отпущен с первого кадра — следующее нажатие автоповтор: не подтверждает и верхний.
        let seen = frame(&ctx, &mut modals, vec![press(egui::Key::Enter)], &[]);
        assert_eq!(seen[2], ("exit", false, false));
    }

    #[test]
    fn closing_the_top_hands_the_keys_to_the_next_one() {
        let ctx = egui::Context::default();
        let mut modals = Modals::default();
        modals.open(Modal::About);
        modals.open(Modal::Exit { native: true });
        frame(&ctx, &mut modals, vec![press(egui::Key::Escape)], &["exit"]);
        assert!(modals.any_open());
        let seen = frame(&ctx, &mut modals, vec![press(egui::Key::Escape)], &["about"]);
        assert_eq!(seen, [("about", false, true)]);
        assert!(!modals.any_open());
    }

    #[test]
    fn reopening_a_kind_replaces_it_on_top() {
        let ctx = egui::Context::default();
        let mut modals = Modals::default();
        assert!(!modals.any_open());
        modals.open(Modal::ModeSwitch(Mode::Overlay));
        modals.open(Modal::About);
        modals.open(Modal::ModeSwitch(Mode::Engine));
        let seen = frame(&ctx, &mut modals, vec![press(egui::Key::Enter)], &[]);
        assert_eq!(seen, [("about", false, false), ("mode", true, false)]);
        assert!(matches!(modals.stack[1].modal, Modal::ModeSwitch(Mode::Engine)));
        // Уже заданный вопрос не перебивается.
        assert!(!modals.open_if_absent(Modal::About));
        assert_eq!(modals.stack.len(), 2);
    }

    #[test]
    fn closing_an_owner_window_closes_only_its_dialogs() {
        // Окно обновлений закрыли крестиком, пока висело его подтверждение: подтверждение уходит, остальные — нет.
        let updates_confirm = |m: &Modal| matches!(m, Modal::UpdatesConfirm(_));
        let mut modals = Modals::default();
        modals.open(Modal::About);
        modals.open(Modal::UpdatesConfirm(UpdateConfirm::Apply(Vec::new())));
        assert!(modals.is_open(updates_confirm));
        modals.close(updates_confirm);
        assert!(!modals.is_open(updates_confirm));
        assert!(modals.is_open(|m| matches!(m, Modal::About)));
        // Ключи переходят к оставшемуся.
        let seen = frame(&egui::Context::default(), &mut modals, vec![press(egui::Key::Escape)], &[]);
        assert_eq!(seen, [("about", false, true)]);
    }

    #[test]
    fn dialogs_opened_during_a_frame_land_on_top() {
        let mut modals = Modals::default();
        modals.open(Modal::About);
        modals.open(Modal::ModeSwitch(Mode::Engine));
        let mut opened = Modals::default();
        opened.open(Modal::Exit { native: false });
        opened.open(Modal::ModeSwitch(Mode::Overlay));
        modals.absorb(opened);
        let kinds: Vec<_> = modals.stack.iter().map(|e| kind(&e.modal)).collect();
        assert_eq!(kinds, ["about", "exit", "mode"]);
        assert!(matches!(modals.stack[2].modal, Modal::ModeSwitch(Mode::Overlay)));
    }

    #[test]
    fn focus_is_given_once_and_remember_flags_are_per_dialog() {
        // Два диалога с галочкой открыты сразу (выход из трея поверх смены режима): галочка одного
        // не должна оказаться отмеченной в другом.
        let ctx = egui::Context::default();
        let mut modals = Modals::default();
        modals.open(Modal::ModeSwitch(Mode::Engine));
        modals.open(Modal::Exit { native: false });
        let mut fresh = Vec::new();
        modals.run(&ctx, |m, turn| {
            fresh.push(turn.fresh);
            if kind(m) == "exit" {
                *turn.remember = true;
            }
            Outcome::Keep
        });
        assert_eq!(fresh, [true, true]);
        let mut seen = Vec::new();
        modals.run(&ctx, |m, turn| {
            seen.push((kind(m), turn.fresh, *turn.remember));
            Outcome::Keep
        });
        assert_eq!(seen, [("mode", false, false), ("exit", false, true)]);
    }

    /// Кадр: окно под диалогами (одна кнопка; немодальное, как окно обновлений, — слой окон под модальным) и стек
    /// диалогов (у каждого окно с кнопкой). (Кнопка окна под диалогами нажата, её прямоугольник и id.)
    fn window_frame(ctx: &egui::Context, modals: &mut Modals, events: Vec<egui::Event>) -> (bool, egui::Rect, egui::Id) {
        let mut main = (false, egui::Rect::NOTHING, egui::Id::NULL);
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 500.0));
        let _ = ctx.run_ui(egui::RawInput { screen_rect: Some(screen), events, ..Default::default() }, |ui| {
            let ctx = ui.ctx();
            let mut shown = true;
            super::super::dialog::dialog_window(ctx, "main", "test-main", &mut shown)
                .pivot(egui::Align2::LEFT_TOP)
                .default_pos([20.0, 20.0])
                .show(ctx, |ui| {
                    let b = ui.button("main window button");
                    main = (b.clicked(), b.rect, b.id);
                });
            modals.run(ctx, |_, mut turn| {
                let mut open = true;
                turn.window(ctx, "t", "test-modal", &mut open).show(ctx, |ui| {
                    let _ = ui.button("dialog button");
                });
                Outcome::Keep
            });
        });
        main
    }

    /// Щелчок в точке: нажатие в одном кадре, отпускание в следующем. `true` — кнопка главного окна нажата.
    fn click(ctx: &egui::Context, modals: &mut Modals, at: egui::Pos2) -> bool {
        let button = |pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
        let down = window_frame(ctx, modals, vec![egui::Event::PointerMoved(at), button(true)]).0;
        down || window_frame(ctx, modals, vec![button(false)]).0
    }

    /// U18: модальный диалог, как в Windows, блокирует окно под ним: щелчок по главному окну ничего не нажимает,
    /// фокус с кнопки главного окна снимается при открытии (иначе Enter нажал бы её), Tab ходит только по диалогу.
    #[test]
    fn a_modal_dialog_blocks_the_window_under_it() {
        let ctx = egui::Context::default();
        let mut modals = Modals::default();
        let (_, rect, main) = (0..3).map(|_| window_frame(&ctx, &mut modals, Vec::new())).last().unwrap();
        assert!(click(&ctx, &mut modals, rect.center()), "without a dialog the main window button works");

        ctx.memory_mut(|m| m.request_focus(main));
        modals.open(Modal::About);
        window_frame(&ctx, &mut modals, Vec::new());
        assert_ne!(ctx.memory(|m| m.focused()), Some(main), "focus stayed on the main window");
        window_frame(&ctx, &mut modals, Vec::new());
        assert!(!click(&ctx, &mut modals, rect.center()), "the main window was clickable under a modal dialog");

        window_frame(&ctx, &mut modals, vec![press(egui::Key::Tab)]);
        let focused = ctx.memory(|m| m.focused()).expect("Tab focuses a widget");
        let layer = ctx.read_response(focused).expect("focused widget is on screen").layer_id;
        assert_eq!(layer.id, egui::Id::new("test-modal"), "Tab left the dialog");

        // Диалог закрыт — главное окно снова работает.
        modals.close(|_| true);
        window_frame(&ctx, &mut modals, Vec::new());
        assert!(click(&ctx, &mut modals, rect.center()), "the main window stayed blocked after the dialog closed");
    }

    /// Диалог стека строит окно через `Turn::window`, а не `dialog_window` напрямую: иначе окно не станет модальным.
    /// Функция диалога — та, что принимает `Turn`; её тело — до закрывающей скобки метода (отступ 4).
    #[test]
    fn modal_dialogs_build_their_window_through_the_turn() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        super::super::dialog::tests::collect_rs(&root.join("app"), &mut files);
        files.push(root.join("app.rs"));
        let needle = concat!("dialog_", "window(");
        let (mut dialogs, mut bad) = (0, Vec::new());
        for path in &files {
            let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            let lines: Vec<&str> = text.lines().collect();
            for (n, line) in lines.iter().enumerate() {
                if !(line.contains("fn ") && line.contains("Turn")) || line.contains("impl ") {
                    continue;
                }
                dialogs += 1;
                let body = lines[n + 1..].iter().take_while(|l| **l != "    }");
                if body.clone().any(|l| l.contains(needle)) {
                    bad.push(format!("{}:{}", path.display(), n + 1));
                }
            }
        }
        assert!(dialogs >= 10, "dialog functions not found: {dialogs}");
        assert!(bad.is_empty(), "modal dialogs that bypass Turn::window:\n{}", bad.join("\n"));
    }
}
