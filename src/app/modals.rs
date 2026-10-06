//! Модальные диалоги окна — один стек вместо отдельного поля `App` на каждый диалог.
//! Enter и Esc получает только верхний (последний открытый: egui рисует новое окно поверх прежних);
//! фокус поля ввода — только в кадре открытия; галочка «Больше не показывать» — у каждого диалога своя.
//!
//! Перечисление, а не `Box<dyn Modal>`: каждый диалог действует через `App` (настройки, ядро, строка состояния),
//! набор диалогов закрыт, и одна `match` в `App::show_modal` проще, чем трейт с окружением из половины `App`.

use std::mem::discriminant;
use std::path::PathBuf;

use eframe::egui;

use super::dialog::dialog_keys;
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
}

/// Кадр одного диалога: его состояние в стеке и право на Enter/Esc.
pub(super) struct Turn<'a> {
    pub(super) fresh: bool,
    pub(super) remember: &'a mut bool,
    top: bool,
}

impl Turn<'_> {
    /// (Enter, Esc) для верхнего диалога — забираются из ввода, нижние и окна под ними их не видят.
    /// Нижним — ничего: иначе один Enter подтвердил бы сразу два диалога.
    pub(super) fn keys(&self, ctx: &egui::Context) -> (bool, bool) {
        if self.top {
            dialog_keys(ctx)
        } else {
            (false, false)
        }
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
        self.stack.push(Entry { modal, fresh: true, remember: false });
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
    pub(super) fn run(&mut self, mut show: impl FnMut(&mut Modal, Turn<'_>) -> Outcome) {
        let top = self.stack.len().checked_sub(1);
        let mut index = 0;
        self.stack.retain_mut(|e| {
            let turn = Turn { fresh: e.fresh, remember: &mut e.remember, top: Some(index) == top };
            let outcome = show(&mut e.modal, turn);
            e.fresh = false;
            index += 1;
            outcome == Outcome::Keep
        });
    }

    /// Диалоги, открытые действиями внутри кадра (`run` шёл на вынутом стеке), — поверх оставшихся.
    pub(super) fn absorb(&mut self, opened: Modals) {
        for e in opened.stack {
            self.open(e.modal);
        }
    }
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
            modals.run(|m, turn| {
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
        let mut modals = Modals::default();
        modals.open(Modal::ModeSwitch(Mode::Engine));
        modals.open(Modal::Exit { native: false });
        let mut fresh = Vec::new();
        modals.run(|m, turn| {
            fresh.push(turn.fresh);
            if kind(m) == "exit" {
                *turn.remember = true;
            }
            Outcome::Keep
        });
        assert_eq!(fresh, [true, true]);
        let mut seen = Vec::new();
        modals.run(|m, turn| {
            seen.push((kind(m), turn.fresh, *turn.remember));
            Outcome::Keep
        });
        assert_eq!(seen, [("mode", false, false), ("exit", false, true)]);
    }
}
