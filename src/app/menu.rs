//! Строка меню окна и доступ к ней с клавиатуры.

use eframe::egui::{self, Ui};

use crate::i18n::{self, tr, trf};
use crate::settings::{Mode, Settings};

use super::{updates, Action, Confirm};

/// Масштабы в меню «Вид»; Ctrl+Plus/Minus даёт и промежуточные.
const SCALES: [f32; 8] = [0.8, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0];

/// Меню закрывается щелчком мимо него или явным `ui.close()` у пункта-команды. С egui 0.32 по умолчанию
/// меню закрывает любой щелчок внутри — тогда «Вид» с флажками закрывался бы на каждом флажке.
const CLOSE: egui::PopupCloseBehavior = egui::PopupCloseBehavior::CloseOnClickOutside;

/// Верхние меню слева направо. Мнемоника — буква после `&` в переводе («&Файл»), как в ресурсах Windows.
const MENUS: [&str; 5] = ["menu.file", "menu.view", "menu.settings", "menu.language", "menu.help"];

/// Команды с горячими клавишами. Одна таблица и для нажатий, и для текста у пунктов меню.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Command {
    NewTunnel,
    Import,
    About,
    CheckUpdates,
}

const SHORTCUTS: [(Command, egui::KeyboardShortcut); 4] = [
    (Command::NewTunnel, egui::KeyboardShortcut::new(egui::Modifiers::CTRL, egui::Key::N)),
    (Command::Import, egui::KeyboardShortcut::new(egui::Modifiers::CTRL, egui::Key::I)),
    (Command::About, egui::KeyboardShortcut::new(egui::Modifiers::NONE, egui::Key::F1)),
    (Command::CheckUpdates, egui::KeyboardShortcut::new(egui::Modifiers::NONE, egui::Key::F5)),
];

impl Command {
    fn action(self) -> Action {
        match self {
            Command::NewTunnel => Action::EngineNew,
            Command::Import => Action::EngineImport,
            Command::About => Action::About,
            Command::CheckUpdates => Action::CheckUpdates,
        }
    }

    /// Новый туннель и импорт есть только в режиме 2 (своё ядро); в режиме 1 их клавиши ничего не делают.
    fn available(self, engine: bool) -> bool {
        engine || !matches!(self, Command::NewTunnel | Command::Import)
    }

    fn shortcut(self) -> egui::KeyboardShortcut {
        SHORTCUTS.iter().find(|(c, _)| *c == self).map(|(_, s)| *s).expect("every command has a shortcut")
    }
}

/// Команда для нажатия: модификаторы — точно как в таблице (Ctrl+Shift+N — не Ctrl+N).
fn shortcut_command(key: egui::Key, modifiers: egui::Modifiers, engine: bool) -> Option<Command> {
    SHORTCUTS
        .iter()
        .find(|(c, s)| s.logical_key == key && modifiers.matches_exact(s.modifiers) && c.available(engine))
        .map(|(c, _)| *c)
}

/// Текст меню без `&` и мнемоника: байтовое смещение буквы в тексте и сама буква (строчная). `&&` — сам знак `&`.
/// Нет `&` — нет мнемоники (перевод из .lng без пометки): меню всё равно доступно через F10 и стрелки.
fn split_mnemonic(text: &str) -> (String, Option<(usize, char)>) {
    let mut out = String::with_capacity(text.len());
    let mut mnemonic = None;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '&' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('&') => out.push('&'),
            Some(m) => {
                if mnemonic.is_none() {
                    mnemonic = Some((out.len(), m.to_lowercase().next().unwrap_or(m)));
                }
                out.push(m);
            }
            None => {}
        }
    }
    (out, mnemonic)
}

/// Клавиша мнемоники. egui-winit отдаёт клавишу по латинской раскладке (для кириллицы — физическую), поэтому
/// русская буква переводится в клавишу, на которой она стоит в ЙЦУКЕН: Alt+Ф работает и в английской раскладке.
fn mnemonic_key(c: char) -> Option<egui::Key> {
    const RU: &str = "йцукенгшщзфывапролдячсмить";
    const EN: &str = "qwertyuiopasdfghjklzxcvbnm";
    let latin = match RU.chars().position(|r| r == c) {
        Some(i) => EN.chars().nth(i)?,
        None => c,
    };
    if !latin.is_ascii_alphanumeric() {
        return None;
    }
    egui::Key::from_name(&latin.to_ascii_uppercase().to_string())
}

/// Где стоит клавиатура в строке меню. Мышь ею не управляет: состояние сверяется с открытыми меню каждый кадр.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum Nav {
    #[default]
    Idle,
    /// Строка меню под фокусом (F10 или одиночный Alt), выделено меню `i`, список не открыт.
    Bar(usize),
    /// Открыт список меню `i`.
    Open(usize),
}

/// Клавиша, которую понимает строка меню.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum NavKey {
    /// F10 или одиночный Alt.
    Toggle,
    Mnemonic(usize),
    Left,
    Right,
    Up,
    Down,
    Enter,
    Escape,
}

/// Где фокус клавиатуры в открытом списке: от этого зависят Влево, Вправо и Esc у подменю.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct ListFocus {
    /// Пункт с подменю («Масштаб интерфейса», «Ядро»).
    opens_submenu: bool,
    /// Пункт внутри подменю.
    in_submenu: bool,
}

/// Ход по клавише: новое состояние строки меню или шаг по подменю.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Move {
    To(Nav),
    /// Открыть подменю пункта под фокусом и поставить фокус на его первый пункт.
    OpenSubmenu,
    /// Закрыть подменю, в котором фокус, и вернуть фокус на его пункт в родительском списке.
    CloseSubmenu,
}

/// Ход клавиатуры, как в меню Windows: в открытом списке Вправо на пункте с подменю открывает его, Влево и Esc внутри
/// подменю закрывают его; остальное — `Nav::step` (Вправо на обычном пункте, и в подменю тоже, — соседнее меню).
fn next_move(nav: Nav, key: NavKey, count: usize, at: ListFocus) -> Option<Move> {
    if let Nav::Open(_) = nav {
        match key {
            NavKey::Right if at.opens_submenu => return Some(Move::OpenSubmenu),
            NavKey::Left | NavKey::Escape if at.in_submenu => return Some(Move::CloseSubmenu),
            _ => {}
        }
    }
    nav.step(key, count).map(Move::To)
}

impl Nav {
    /// Следующее состояние; `None` — клавиша не для меню (её получает окно). Вверх, вниз и Enter внутри открытого
    /// списка ведёт сам egui (фокус по стрелкам, Enter нажимает пункт), поэтому здесь они только открывают список.
    fn step(self, key: NavKey, count: usize) -> Option<Nav> {
        let prev = |i: usize| (i + count - 1) % count;
        let next = |i: usize| (i + 1) % count;
        Some(match (self, key) {
            (_, NavKey::Mnemonic(i)) => Nav::Open(i),
            (Nav::Idle, NavKey::Toggle) => Nav::Bar(0),
            (Nav::Bar(_) | Nav::Open(_), NavKey::Toggle) => Nav::Idle,
            (Nav::Bar(i), NavKey::Left) => Nav::Bar(prev(i)),
            (Nav::Bar(i), NavKey::Right) => Nav::Bar(next(i)),
            (Nav::Open(i), NavKey::Left) => Nav::Open(prev(i)),
            (Nav::Open(i), NavKey::Right) => Nav::Open(next(i)),
            (Nav::Bar(i), NavKey::Up | NavKey::Down | NavKey::Enter) => Nav::Open(i),
            (Nav::Open(i), NavKey::Escape) => Nav::Bar(i),
            (Nav::Bar(_), NavKey::Escape) => Nav::Idle,
            _ => return None,
        })
    }
}

/// Клавиша из ввода для строки меню. `enabled` — поверх окна нет диалога: открыть меню нельзя, но уже открытое
/// клавиши получает. Мнемоника — Alt+буква без других модификаторов (AltGr — это Ctrl+Alt, он печатает).
fn nav_key(key: egui::Key, modifiers: egui::Modifiers, nav: Nav, mnemonics: &[Option<egui::Key>], enabled: bool) -> Option<NavKey> {
    if modifiers.matches_exact(egui::Modifiers::ALT) {
        return mnemonics.iter().position(|m| *m == Some(key)).filter(|_| enabled).map(NavKey::Mnemonic);
    }
    if !modifiers.is_none() || (nav == Nav::Idle && !(enabled && key == egui::Key::F10)) {
        return None;
    }
    Some(match key {
        egui::Key::F10 => NavKey::Toggle,
        egui::Key::ArrowLeft => NavKey::Left,
        egui::Key::ArrowRight => NavKey::Right,
        egui::Key::ArrowUp => NavKey::Up,
        egui::Key::ArrowDown => NavKey::Down,
        egui::Key::Enter | egui::Key::Space => NavKey::Enter,
        egui::Key::Escape => NavKey::Escape,
        _ => return None,
    })
}

/// Клавиатура строки меню (правило 5 стандарта): F10 и одиночный Alt ставят фокус на первое меню, Alt+буква открывает
/// меню с этой мнемоникой, стрелки влево/вправо — соседнее меню, вверх/вниз — по пунктам, Enter — пункт, Esc —
/// назад; горячие клавиши команд — из `SHORTCUTS`. egui сам этого не умеет: он видит только кнопки и всплывающие окна.
#[derive(Default)]
pub(super) struct MenuNav {
    nav: Nav,
    /// Кнопки верхних меню с прошлого кадра (id стабильны между кадрами); по ним находятся их всплывающие списки.
    buttons: Vec<egui::Id>,
    /// Пункты с подменю с прошлого кадра; подменю пункта — `SubMenu::id_from_widget_id(пункт)`, это же id его слоя.
    submenus: Vec<egui::Id>,
    /// Список, открытый с клавиатуры, и путь от его кнопки к первому пункту (вниз у меню, вправо у подменю): когда
    /// список уже нарисован, фокус встаёт на первый пункт.
    focus_into: Option<(egui::Id, egui::FocusDirection)>,
}

impl MenuNav {
    fn popup(&self, i: usize) -> Option<egui::Id> {
        // Как `Popup::default_response_id`: список кнопки меню — id кнопки с "popup".
        self.buttons.get(i).map(|b| b.with("popup"))
    }

    /// Мышь могла открыть или закрыть меню, Tab или щелчок — увести фокус со строки: состояние берётся из egui.
    fn reconcile(&mut self, ctx: &egui::Context) {
        let open = (0..self.buttons.len()).find(|&i| self.popup(i).is_some_and(|p| egui::Popup::is_id_open(ctx, p)));
        let focused = ctx.memory(|m| m.focused());
        self.nav = match (self.nav, open) {
            (_, Some(i)) => Nav::Open(i),
            (Nav::Bar(i), None) if focused == self.buttons.get(i).copied() => Nav::Bar(i),
            _ => Nav::Idle,
        };
    }

    /// Подменю, в котором стоит виджет: его пункт в родительском списке. Слой подменю — его id (`Popup` рисует его
    /// в `Area` с этим id).
    fn submenu_holding(&self, ctx: &egui::Context, widget: egui::Id) -> Option<egui::Id> {
        let layer = ctx.read_response(widget)?.layer_id.id;
        self.submenus.iter().copied().find(|s| egui::containers::menu::SubMenu::id_from_widget_id(*s) == layer)
    }

    fn list_focus(&self, ctx: &egui::Context) -> ListFocus {
        let Some(focused) = ctx.memory(|m| m.focused()) else { return ListFocus::default() };
        ListFocus { opens_submenu: self.submenus.contains(&focused), in_submenu: self.submenu_holding(ctx, focused).is_some() }
    }

    /// Первая клавиша кадра, понятая строкой меню, забирается из ввода и даёт ход.
    fn take_key(&self, ctx: &egui::Context, mnemonics: &[Option<egui::Key>], enabled: bool) -> Option<Move> {
        // До `input_mut`: он держит контекст, а фокус и ответы виджетов читаются через тот же контекст.
        let at = if matches!(self.nav, Nav::Open(_)) { self.list_focus(ctx) } else { ListFocus::default() };
        ctx.input_mut(|input| {
            let mut found = None;
            input.events.retain(|e| {
                let egui::Event::Key { key, pressed: true, modifiers, .. } = e else { return true };
                if found.is_some() {
                    return true;
                }
                found = nav_key(*key, *modifiers, self.nav, mnemonics, enabled).and_then(|k| next_move(self.nav, k, MENUS.len(), at));
                found.is_none()
            });
            found
        })
    }

    /// Вправо на пункте с подменю. Закрытое подменю открывает нажатие пункта с клавиатуры (Enter — так его открывает
    /// и сам egui); уже открытое мышью не трогаем — Enter его бы закрыл. Фокус уходит внутрь, когда подменю нарисовано.
    fn open_submenu(&mut self, ctx: &egui::Context) {
        let Some(item) = ctx.memory(|m| m.focused()) else { return };
        let sub = egui::containers::menu::SubMenu::id_from_widget_id(item);
        if ctx.read_response(sub).is_none() {
            press_focused(ctx);
        }
        self.focus_into = Some((sub, egui::FocusDirection::Right));
        // egui в конце кадра сдвинул бы фокус по той же стрелке, а подменю ещё не нарисовано.
        ctx.memory_mut(|m| m.move_focus(egui::FocusDirection::None));
    }

    /// Влево или Esc в подменю: фокус на его пункт и нажатие пункта — egui закрывает открытое подменю по Enter.
    fn close_submenu(&mut self, ctx: &egui::Context) {
        let Some(item) = ctx.memory(|m| m.focused()).and_then(|f| self.submenu_holding(ctx, f)) else { return };
        ctx.memory_mut(|m| {
            m.request_focus(item);
            m.move_focus(egui::FocusDirection::None);
        });
        press_focused(ctx);
    }

    /// Переход в новое состояние: фокус и открытый список — как у меню Windows.
    fn go(&mut self, ctx: &egui::Context, to: Nav) {
        for p in (0..self.buttons.len()).filter_map(|i| self.popup(i)) {
            if !matches!(to, Nav::Open(i) if self.popup(i) == Some(p)) {
                egui::Popup::close_id(ctx, p);
            }
        }
        match to {
            Nav::Idle => {
                if let Some(&b) = self.buttons.iter().find(|b| ctx.memory(|m| m.has_focus(**b))) {
                    ctx.memory_mut(|m| m.surrender_focus(b));
                }
            }
            Nav::Bar(i) => ctx.memory_mut(|m| m.request_focus(self.buttons[i])),
            Nav::Open(i) => {
                if let Some(p) = self.popup(i) {
                    egui::Popup::open_id(ctx, p);
                }
                ctx.memory_mut(|m| m.request_focus(self.buttons[i]));
                self.focus_into = self.popup(i).map(|p| (p, egui::FocusDirection::Down));
            }
        }
        // egui прочитал стрелку в начале кадра и в конце сдвинул бы фокус ещё раз — поверх нашего перехода.
        ctx.memory_mut(|m| m.move_focus(egui::FocusDirection::None));
        self.nav = to;
    }

    /// До рисования строки: сверка с egui, клавиши строки меню, затем горячие клавиши команд.
    fn before(&mut self, ctx: &egui::Context, enabled: bool, engine: bool, actions: &mut Vec<Action>) {
        if self.buttons.len() != MENUS.len() {
            return; // первый кадр: кнопок ещё нет
        }
        self.reconcile(ctx);
        self.focus_first_item(ctx);
        let mnemonics = current_mnemonics();
        match self.take_key(ctx, &mnemonics, enabled) {
            Some(Move::To(to)) => self.go(ctx, to),
            Some(Move::OpenSubmenu) => self.open_submenu(ctx),
            Some(Move::CloseSubmenu) => self.close_submenu(ctx),
            None if enabled && self.nav == Nav::Idle && !egui::Popup::is_any_open(ctx) => take_shortcuts(ctx, engine, actions),
            None => {}
        }
    }

    /// Фокус на первый пункт открытого с клавиатуры списка: стрелка от его кнопки к списку (вниз у меню, вправо у
    /// подменю). Список появляется со второго кадра (первый — замер размера), до того фокусу некуда встать.
    fn focus_first_item(&mut self, ctx: &egui::Context) {
        if !matches!(self.nav, Nav::Open(_)) {
            self.focus_into = None;
            return;
        }
        if let Some((_, direction)) = self.focus_into.filter(|(list, _)| ctx.read_response(*list).is_some()) {
            ctx.memory_mut(|m| m.move_focus(direction));
            self.focus_into = None;
        }
    }
}

/// Нажатие пункта под фокусом с клавиатуры: Enter в ввод кадра. egui считает его щелчком того виджета, у которого
/// фокус, когда виджет рисуется в этом кадре, — поэтому зовётся до рисования строки меню.
fn press_focused(ctx: &egui::Context) {
    let enter = egui::Event::Key { key: egui::Key::Enter, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE };
    ctx.input_mut(|i| i.events.push(enter));
}

/// Мнемоники верхних меню на текущем языке, по порядку `MENUS`.
fn current_mnemonics() -> Vec<Option<egui::Key>> {
    MENUS.iter().map(|k| split_mnemonic(&tr(k)).1.and_then(|(_, c)| mnemonic_key(c))).collect()
}

/// Горячие клавиши команд: забираются из ввода и становятся действиями. Автоповтор не считается — удержанная F5
/// не должна открывать окно раз за разом.
fn take_shortcuts(ctx: &egui::Context, engine: bool, actions: &mut Vec<Action>) {
    ctx.input_mut(|input| {
        input.events.retain(|e| {
            let egui::Event::Key { key, pressed: true, repeat: false, modifiers, .. } = e else { return true };
            let Some(c) = shortcut_command(*key, *modifiers, engine) else { return true };
            actions.push(c.action());
            false
        });
    });
}

/// Заголовок верхнего меню: текст без `&`, буква мнемоники подчёркнута. Цвет `PLACEHOLDER` подставит кнопка.
fn menu_title(ui: &Ui, key: &str) -> egui::text::LayoutJob {
    let (text, mnemonic) = split_mnemonic(&tr(key));
    let font = egui::TextStyle::Button.resolve(ui.style());
    let plain = egui::TextFormat { font_id: font, color: egui::Color32::PLACEHOLDER, ..Default::default() };
    let mut job = egui::text::LayoutJob::default();
    let Some((at, _)) = mnemonic else {
        job.append(&text, 0.0, plain);
        return job;
    };
    let end = at + text[at..].chars().next().map_or(0, char::len_utf8);
    job.append(&text[..at], 0.0, plain.clone());
    let underline = egui::Stroke::new(1.0, egui::Color32::PLACEHOLDER);
    job.append(&text[at..end], 0.0, egui::TextFormat { underline, ..plain.clone() });
    job.append(&text[end..], 0.0, plain);
    job
}

/// Пункт-команда; у команды с горячей клавишей она написана справа, как в меню Windows.
fn command_button(ui: &mut Ui, text: String, command: Option<Command>) -> egui::Response {
    let button = egui::Button::new(text);
    let button = match command {
        Some(c) => button.shortcut_text(ui.ctx().format_shortcut(&c.shortcut())),
        None => button,
    };
    ui.add(button)
}

/// Контекстное меню элемента: правый щелчок, а для элемента под фокусом клавиатуры (`keyboard`) — ещё Shift+F10
/// и клавиша меню (её `MenuKey` превращает в Shift+F10), как в Windows. Открытое с клавиатуры меню стоит под
/// элементом, а не у указателя мыши. Подменю наследуют то же правило закрытия. Единственный путь к контекстному меню
/// в окне: тест `context_menus_go_through_the_helper` не пускает правый щелчок в обход него.
pub(super) fn context_menu(resp: &egui::Response, keyboard: bool, add_contents: impl FnOnce(&mut Ui)) {
    let id = egui::Popup::default_response_id(resp);
    let by_key = id.with("by-key");
    if keyboard && resp.ctx.input_mut(|i| i.consume_key(egui::Modifiers::SHIFT, egui::Key::F10)) {
        egui::Popup::open_id(&resp.ctx, id);
        resp.ctx.data_mut(|d| d.insert_temp(by_key, true));
    } else if resp.secondary_clicked() {
        resp.ctx.data_mut(|d| d.remove::<bool>(by_key));
    }
    let popup = egui::Popup::context_menu(resp).close_behavior(CLOSE);
    // Под элементом в координатах экрана (с преобразованием слоя, как у самого egui), а не у указателя.
    let under = || egui::PopupAnchor::from(resp).rect(id, &resp.ctx).unwrap_or(resp.rect).left_bottom();
    let popup = if resp.ctx.data(|d| d.get_temp::<bool>(by_key)).unwrap_or(false) { popup.at_position(under()) } else { popup };
    popup.show(add_contents);
}

/// Клавиша меню (Apps) для egui: egui-winit её не переводит, и событие теряется. Каждое нажатие переключает младший
/// бит `GetKeyState(VK_APPS)` — по его смене кадр узнаёт о нажатии, даже если нажатие и отпускание пришли между
/// кадрами, — и добавляет в ввод Shift+F10, равнозначное ей в Windows.
/// Там же одиночный Alt (нажат и отпущен без других клавиш и щелчков): egui видит только смену модификаторов, а в
/// Windows такое нажатие ставит фокус на строку меню — как F10. Его ловит `AltTap` и добавляет в ввод F10.
#[derive(Default)]
pub(super) struct MenuKey {
    /// Бит переключения на прошлом кадре; `None` — точки отсчёта нет (первый кадр, окно без фокуса).
    toggle: Option<bool>,
    alt: AltTap,
}

impl MenuKey {
    /// Зовётся из `raw_input_hook` каждый кадр.
    pub(super) fn hook(&mut self, raw: &mut egui::RawInput) {
        self.step(raw, apps_key_toggle());
        self.alt.step(raw);
    }

    /// Без фокуса окна нажатия уходят другим программам, а при возврате фокуса Windows может сверить состояние
    /// клавиш — точка отсчёта берётся заново, без ложного открытия меню.
    fn step(&mut self, raw: &mut egui::RawInput, toggle: bool) {
        let refocused = raw.events.iter().any(|e| matches!(e, egui::Event::WindowFocused(true)));
        let pressed = raw.focused && !refocused && self.toggle.is_some_and(|t| t != toggle);
        self.toggle = raw.focused.then_some(toggle);
        if pressed {
            let modifiers = egui::Modifiers::SHIFT;
            raw.events.push(egui::Event::Key { key: egui::Key::F10, physical_key: None, pressed: true, repeat: false, modifiers });
        }
    }
}

/// Одиночный Alt по событиям `ModifiersChanged` (egui-winit шлёт их при каждой смене модификаторов). Отменяют его
/// любая клавиша или кнопка мыши, пока Alt нажат (Alt+F4, Alt+Tab, Alt+буква, Alt+щелчок), другой модификатор
/// (AltGr — это Ctrl+Alt) и смена фокуса окна.
///
/// Порядок событий на Windows: winit шлёт `ModifiersChanged` раньше самого нажатия (`event_loop.rs`, «Send new
/// modifiers before sending key events»), а egui-winit 0.36 отдаёт сам Alt как `Key::AltLeft` / `Key::AltRight`
/// (`key_from_key_code`). Поэтому нажатие Alt приходит как «клавиша после смены модификаторов» — своё нажатие
/// одиночный Alt не отменяет, иначе он не срабатывает никогда (и его автоповтор при удержании тоже).
#[derive(Default)]
struct AltTap {
    /// Alt нажат.
    held: bool,
    /// С нажатия Alt ничего другого не было: отпускание — одиночный Alt.
    clean: bool,
}

impl AltTap {
    fn step(&mut self, raw: &mut egui::RawInput) {
        if !raw.focused {
            *self = Self::default();
            return;
        }
        let mut taps = 0;
        for e in &raw.events {
            match e {
                egui::Event::ModifiersChanged(m) => {
                    let alt_only = m.alt && !m.ctrl && !m.shift && !m.command && !m.mac_cmd;
                    if !m.alt && self.held && self.clean {
                        taps += 1;
                    }
                    self.clean = if alt_only { self.clean || !self.held } else { false };
                    self.held = m.alt;
                }
                egui::Event::Key { key: egui::Key::AltLeft | egui::Key::AltRight, .. } => {}
                egui::Event::Key { pressed: true, .. } | egui::Event::PointerButton { pressed: true, .. } => self.clean = false,
                egui::Event::WindowFocused(_) => *self = Self::default(),
                _ => {}
            }
        }
        for _ in 0..taps {
            let modifiers = egui::Modifiers::NONE;
            raw.events.push(egui::Event::Key { key: egui::Key::F10, physical_key: None, pressed: true, repeat: false, modifiers });
        }
    }
}

fn apps_key_toggle() -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_APPS};
    // SAFETY: GetKeyState только читает состояние клавиатуры потока и указателей не принимает.
    unsafe { GetKeyState(VK_APPS as i32) & 1 != 0 }
}

/// Подкласс окна для системных клавиш; свой номер, рядом с подклассом трея.
const SYSTEM_KEYS_SUBCLASS: usize = 0x4157_474B; // "AWGK"

/// Отнять у Windows вход в меню окна по клавиатуре: строка меню своя, на egui. F10, одиночный Alt и Alt+буква
/// доходят до `DefWindowProc` и возвращаются окну как `WM_SYSCOMMAND` / `SC_KEYMENU`; без фильтра Windows входит в
/// режим системного меню — следующая стрелка вниз раскрывает его поверх нашей строки, остальные клавиши уходят ему.
/// Сами нажатия (`WM_SYSKEYDOWN`) доходят до egui как раньше.
pub(super) fn install_system_key_filter(hwnd: isize) -> Result<(), String> {
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::UI::Shell::SetWindowSubclass;
    // SAFETY: `hwnd` — окно этого потока из `CreationContext`, процедура — функция программы, живёт дольше окна.
    let ok = unsafe { SetWindowSubclass(hwnd as HWND, Some(system_keys_proc), SYSTEM_KEYS_SUBCLASS, 0) };
    if ok == 0 {
        return Err(format!("{} (SetWindowSubclass)", tr("menu.sys_keys_failed")));
    }
    Ok(())
}

/// Проглотить ли сообщение окна: только `SC_KEYMENU` без пробела. Alt+Пробел (`SC_KEYMENU` с `' '`) открывает
/// системное меню — оставляем, как в Windows; Alt+F4 приходит как `SC_CLOSE` и сюда не попадает. Младшие 4 бита
/// `wparam` у `WM_SYSCOMMAND` — служебные Windows, сравнивать без них.
fn swallows_system_key(msg: u32, wparam: usize, lparam: isize) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{SC_KEYMENU, WM_SYSCOMMAND};
    msg == WM_SYSCOMMAND && wparam & 0xFFF0 == SC_KEYMENU as usize && lparam != b' ' as isize
}

unsafe extern "system" fn system_keys_proc(
    hwnd: windows_sys::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows_sys::Win32::Foundation::WPARAM,
    lparam: windows_sys::Win32::Foundation::LPARAM,
    _id: usize,
    _data: usize,
) -> windows_sys::Win32::Foundation::LRESULT {
    if swallows_system_key(msg, wparam, lparam) {
        return 0;
    }
    windows_sys::Win32::UI::Shell::DefSubclassProc(hwnd, msg, wparam, lparam)
}

/// Что строке меню нужно от окна на этот кадр.
pub(super) struct MenuBarInput<'a> {
    pub(super) lang_dir: &'a std::path::Path,
    /// У компонента есть обновление: точка у «Справки» и пометка у пункта проверки.
    pub(super) updates_new: bool,
    /// Поверх окна нет диалога: клавиши меню и горячие клавиши работают (иначе они принадлежат диалогу).
    pub(super) keyboard: bool,
}

pub(super) fn menu_bar(ui: &mut Ui, nav: &mut MenuNav, s: &mut Settings, input: &MenuBarInput, actions: &mut Vec<Action>) {
    let engine = s.mode() == Mode::Engine;
    nav.before(ui.ctx(), input.keyboard, engine, actions);
    let (lang_dir, updates_new) = (input.lang_dir, input.updates_new);
    let mut buttons = Vec::with_capacity(MENUS.len());
    // Пункты с подменю — для стрелок Вправо / Влево (`MenuNav::list_focus`).
    let mut submenus = Vec::new();
    egui::MenuBar::new().config(egui::containers::menu::MenuConfig::new().close_behavior(CLOSE)).ui(ui, |ui| {
        let title = menu_title(ui, MENUS[0]);
        let file = ui.menu_button(title, |ui| {
            let mut item = |ui: &mut Ui, key: &str, action: Action, shortcut: Option<Command>| {
                if command_button(ui, tr(key), shortcut).clicked() {
                    actions.push(action);
                    ui.close();
                }
            };
            if engine {
                item(ui, "eng.new", Action::EngineNew, Some(Command::NewTunnel));
                item(ui, "eng.import", Action::EngineImport, Some(Command::Import));
                if crate::backend::native_exe().exists() {
                    item(ui, "eng.take_native", Action::EngineTakeNative, None);
                }
                ui.separator();
                item(ui, "eng.backup", Action::EngineBackup, None);
                item(ui, "eng.restore", Action::EngineRestore, None);
                ui.separator();
            }
            item(ui, "file.open_conf", Action::OpenConf, None);
            ui.separator();
            item(ui, "file.exit", Action::Exit, None);
            if !engine {
                item(ui, "file.exit_native", Action::ExitWithNative, None);
            }
        });
        buttons.push(file.response.id);
        let title = menu_title(ui, MENUS[1]);
        let view = ui.menu_button(title, |ui| {
            let v = &mut s.view;
            ui.checkbox(&mut v.groups, tr("view.groups"));
            ui.checkbox(&mut v.search, tr("view.search"));
            ui.separator();
            ui.weak(tr("view.columns"));
            ui.checkbox(&mut v.col_rx, tr("view.col_rx"));
            ui.checkbox(&mut v.col_tx, tr("view.col_tx"));
            ui.checkbox(&mut v.col_peak, tr("view.col_peak"));
            ui.checkbox(&mut v.col_share, tr("view.col_share"));
            ui.separator();
            ui.weak(tr("view.right"));
            ui.checkbox(&mut v.totals, tr("view.totals"));
            ui.checkbox(&mut v.graph, tr("view.graph"));
            ui.checkbox(&mut v.ping, tr("view.ping"));
            ui.checkbox(&mut v.reconnect, tr("view.reconnect"));
            ui.checkbox(&mut v.details, tr("view.details"));
            ui.separator();
            ui.checkbox(&mut v.log, tr("view.log"));
            ui.separator();
            let scale_menu = ui.menu_button(trf("view.scale", &[&format!("{:.0}", s.ui_scale * 100.0)]), |ui| {
                for scale in SCALES {
                    if ui.radio((s.ui_scale - scale).abs() < 0.01, format!("{:.0} %", scale * 100.0)).clicked() {
                        s.ui_scale = scale;
                        ui.close();
                    }
                }
                ui.separator();
                ui.weak(tr("view.scale_keys"));
            });
            submenus.push(scale_menu.response.id);
        });
        buttons.push(view.response.id);
        // Параметры — в окне «Настройки» (черновик, проверка узла пинга, ОК/Отмена); здесь остаются только команды.
        let title = menu_title(ui, MENUS[2]);
        let settings = ui.menu_button(title, |ui| {
            if ui.button(tr("set.open")).clicked() {
                actions.push(Action::OpenSettings);
                ui.close();
            }
            ui.separator();
            // Режим 2: забрать туннели из оригинала — только по команде, когда нужно (своя копия правится отдельно).
            if engine && crate::backend::native_exe().exists() && ui.button(tr("eng.take_native")).on_hover_text(tr("eng.take_native_hint")).clicked() {
                actions.push(Action::EngineTakeNative);
                ui.close();
            }
            let core_menu = ui.menu_button(tr("core.menu"), |ui| {
                if ui.button(tr("core.reinstall")).on_hover_text(tr("core.uac_hint")).clicked() {
                    actions.push(Action::InstallCore);
                    ui.close();
                }
                if ui.button(tr("core.uninstall_title")).clicked() {
                    actions.push(Action::Confirm(Confirm::UninstallCore));
                    ui.close();
                }
            });
            submenus.push(core_menu.response.id);
            ui.separator();
            if ui.button(tr("set.shortcut")).clicked() {
                actions.push(Action::DesktopShortcut);
                ui.close();
            }
            if !engine && ui.button(tr("set.original")).clicked() {
                actions.push(Action::OpenOriginal);
                ui.close();
            }
        });
        buttons.push(settings.response.id);
        let title = menu_title(ui, MENUS[3]);
        let language = ui.menu_button(title, |ui| {
            let current = i18n::current_code();
            for (code, name) in i18n::available(lang_dir) {
                if ui.radio(code == current, format!("{name} ({code})")).clicked() {
                    actions.push(Action::Language(code));
                    ui.close();
                }
            }
            ui.separator();
            if ui.button(tr("lang.add")).clicked() {
                actions.push(Action::AddLanguage);
                ui.close();
            }
            if ui.button(tr("lang.folder")).clicked() {
                actions.push(Action::OpenLangFolder);
                ui.close();
            }
        });
        buttons.push(language.response.id);
        let help = updates::menu_title(menu_title(ui, MENUS[4]), updates_new);
        let help = ui.menu_button(help, |ui| {
            let check = tr(if updates_new { "upd.menu_new" } else { "upd.menu" });
            if command_button(ui, check, Some(Command::CheckUpdates)).clicked() {
                actions.push(Action::CheckUpdates);
                ui.close();
            }
            if ui.button(tr("help.diag")).on_hover_text(tr("help.diag_hint")).clicked() {
                actions.push(Action::CopyDiagnostics);
                ui.close();
            }
            ui.separator();
            if command_button(ui, tr("help.about"), Some(Command::About)).clicked() {
                actions.push(Action::About);
                ui.close();
            }
        });
        buttons.push(help.response.id);
    });
    debug_assert_eq!(buttons.len(), MENUS.len(), "every top menu reports its button");
    nav.buttons = buttons;
    nav.submenus = submenus;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(focused: bool, events: Vec<egui::Event>) -> egui::RawInput {
        egui::RawInput { focused, events, ..Default::default() }
    }

    fn opens_menu(raw: &egui::RawInput) -> bool {
        raw.events.iter().any(|e| matches!(e, egui::Event::Key { key: egui::Key::F10, pressed: true, modifiers, .. } if modifiers.shift))
    }

    fn plain_f10(raw: &egui::RawInput) -> bool {
        raw.events.iter().any(|e| matches!(e, egui::Event::Key { key: egui::Key::F10, pressed: true, modifiers, .. } if modifiers.is_none()))
    }

    /// Кадр с нажатием (`alt`) или отпусканием левого Alt, как его отдаёт egui-winit 0.36 на Windows: сначала
    /// `ModifiersChanged`, затем сам Alt как `Key::AltLeft`; после них `events`.
    fn with_alt(alt: bool, events: Vec<egui::Event>) -> egui::RawInput {
        let events = alt_key(egui::Key::AltLeft, alt).into_iter().chain(events).collect();
        input(true, events)
    }

    /// Нажатие или отпускание самой клавиши Alt: `ModifiersChanged` плюс её `Key`, в порядке winit.
    fn alt_key(key: egui::Key, pressed: bool) -> [egui::Event; 2] {
        let modifiers = if pressed { egui::Modifiers::ALT } else { egui::Modifiers::NONE };
        [egui::Event::ModifiersChanged(modifiers), key_event(key, pressed, false, modifiers)]
    }

    /// Кадр: смена модификаторов, за ней `events`.
    fn changed(focused: bool, modifiers: egui::Modifiers, events: Vec<egui::Event>) -> egui::RawInput {
        let events = std::iter::once(egui::Event::ModifiersChanged(modifiers)).chain(events).collect();
        egui::RawInput { focused, events, ..Default::default() }
    }

    fn press(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
        key_event(key, true, false, modifiers)
    }

    fn key_event(key: egui::Key, pressed: bool, repeat: bool, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key { key, physical_key: None, pressed, repeat, modifiers }
    }

    /// Мнемоники верхних меню на каждом встроенном языке: есть у всех и не повторяются (по клавише, на которую
    /// ложатся). Повтор — Alt+буква открывал бы только первое из меню: тест падает.
    #[test]
    fn menu_mnemonics_are_unique_per_language() {
        for (lang, pick) in [("eng", 0), ("rus", 1)] {
            let keys: Vec<egui::Key> = MENUS
                .iter()
                .map(|k| {
                    let (en, ru) = crate::i18n::builtin_pair(k).unwrap_or_else(|| panic!("{k}: no string"));
                    let text = [en, ru][pick];
                    let (_, m) = split_mnemonic(text);
                    let (_, c) = m.unwrap_or_else(|| panic!("{lang} {k} = {text:?}: no & mnemonic"));
                    mnemonic_key(c).unwrap_or_else(|| panic!("{lang} {k}: no key for {c:?}"))
                })
                .collect();
            for (i, a) in keys.iter().enumerate() {
                if let Some(j) = keys[i + 1..].iter().position(|b| b == a) {
                    panic!("{lang}: {} and {} share Alt+{}", MENUS[i], MENUS[i + 1 + j], a.name());
                }
            }
        }
    }

    #[test]
    fn mnemonic_parsing() {
        assert_eq!(split_mnemonic("&File"), ("File".to_string(), Some((0, 'f'))));
        assert_eq!(split_mnemonic("Пра&вка"), ("Правка".to_string(), Some(("Пра".len(), 'в'))));
        assert_eq!(split_mnemonic("Save && &Exit"), ("Save & Exit".to_string(), Some((7, 'e'))));
        assert_eq!(split_mnemonic("Plain"), ("Plain".to_string(), None));
        assert_eq!(mnemonic_key('ф'), Some(egui::Key::A), "Ф стоит на клавише A");
        assert_eq!(mnemonic_key('я'), Some(egui::Key::Z));
        assert_eq!(mnemonic_key('h'), Some(egui::Key::H));
        assert_eq!(mnemonic_key('&'), None);
    }

    #[test]
    fn shortcuts_map_to_commands() {
        let ctrl = egui::Modifiers::CTRL;
        let none = egui::Modifiers::NONE;
        assert_eq!(shortcut_command(egui::Key::N, ctrl, true), Some(Command::NewTunnel));
        assert_eq!(shortcut_command(egui::Key::I, ctrl, true), Some(Command::Import));
        assert_eq!(shortcut_command(egui::Key::F1, none, true), Some(Command::About));
        assert_eq!(shortcut_command(egui::Key::F5, none, false), Some(Command::CheckUpdates));
        // Новый туннель и импорт — только в режиме 2.
        assert_eq!(shortcut_command(egui::Key::N, ctrl, false), None);
        assert_eq!(shortcut_command(egui::Key::I, ctrl, false), None);
        // Модификаторы — точно: Ctrl+Shift+N, просто N, Ctrl+F5 — не команды.
        assert_eq!(shortcut_command(egui::Key::N, ctrl | egui::Modifiers::SHIFT, true), None);
        assert_eq!(shortcut_command(egui::Key::N, none, true), None);
        assert_eq!(shortcut_command(egui::Key::F5, ctrl, true), None);
        assert!(matches!(Command::NewTunnel.action(), Action::EngineNew));
        assert!(matches!(Command::Import.action(), Action::EngineImport));
        assert!(matches!(Command::About.action(), Action::About));
        assert!(matches!(Command::CheckUpdates.action(), Action::CheckUpdates));
        // Каждая команда — со своей клавишей, без повторов.
        for (i, (_, a)) in SHORTCUTS.iter().enumerate() {
            assert!(SHORTCUTS[i + 1..].iter().all(|(_, b)| b != a), "{a:?} twice");
        }
    }

    /// Ход клавиатуры по строке меню, как в Windows.
    #[test]
    fn menu_bar_keys_walk_like_windows() {
        let n = MENUS.len();
        let k = NavKey::Left;
        assert_eq!(Nav::Idle.step(NavKey::Toggle, n), Some(Nav::Bar(0)), "F10 / Alt: первое меню, без списка");
        assert_eq!(Nav::Bar(0).step(NavKey::Toggle, n), Some(Nav::Idle), "повторный F10 уходит из меню");
        assert_eq!(Nav::Open(2).step(NavKey::Toggle, n), Some(Nav::Idle));
        assert_eq!(Nav::Bar(0).step(k, n), Some(Nav::Bar(n - 1)), "влево с первого — последнее");
        assert_eq!(Nav::Bar(n - 1).step(NavKey::Right, n), Some(Nav::Bar(0)));
        assert_eq!(Nav::Open(1).step(NavKey::Right, n), Some(Nav::Open(2)), "открытое — соседнее тоже открыто");
        assert_eq!(Nav::Open(0).step(NavKey::Left, n), Some(Nav::Open(n - 1)));
        for open in [NavKey::Down, NavKey::Up, NavKey::Enter] {
            assert_eq!(Nav::Bar(3).step(open, n), Some(Nav::Open(3)));
        }
        assert_eq!(Nav::Open(3).step(NavKey::Escape, n), Some(Nav::Bar(3)), "Esc закрывает список, фокус на строке");
        assert_eq!(Nav::Bar(3).step(NavKey::Escape, n), Some(Nav::Idle));
        assert_eq!(Nav::Idle.step(NavKey::Mnemonic(4), n), Some(Nav::Open(4)));
        assert_eq!(Nav::Open(1).step(NavKey::Mnemonic(4), n), Some(Nav::Open(4)));
        // Внутри открытого списка вверх, вниз и Enter — забота egui; без меню стрелки и Esc — окну.
        for key in [NavKey::Up, NavKey::Down, NavKey::Enter] {
            assert_eq!(Nav::Open(0).step(key, n), None);
        }
        for key in [k, NavKey::Right, NavKey::Up, NavKey::Down, NavKey::Enter, NavKey::Escape] {
            assert_eq!(Nav::Idle.step(key, n), None);
        }
    }

    /// Подменю, как в Windows: Вправо на пункте с подменю открывает его, Влево и Esc в подменю закрывают его, Вправо
    /// на обычном пункте (и внутри подменю) — соседнее верхнее меню.
    #[test]
    fn arrows_open_and_close_submenus_like_windows() {
        let n = MENUS.len();
        let plain = ListFocus::default();
        let on_submenu_item = ListFocus { opens_submenu: true, in_submenu: false };
        let inside = ListFocus { opens_submenu: false, in_submenu: true };
        let open = Nav::Open(1);
        assert_eq!(next_move(open, NavKey::Right, n, on_submenu_item), Some(Move::OpenSubmenu));
        assert_eq!(next_move(open, NavKey::Right, n, plain), Some(Move::To(Nav::Open(2))), "обычный пункт — соседнее меню");
        assert_eq!(next_move(open, NavKey::Right, n, inside), Some(Move::To(Nav::Open(2))), "в подменю тоже");
        assert_eq!(next_move(open, NavKey::Left, n, inside), Some(Move::CloseSubmenu));
        assert_eq!(next_move(open, NavKey::Escape, n, inside), Some(Move::CloseSubmenu), "Esc закрывает только подменю");
        assert_eq!(next_move(open, NavKey::Left, n, on_submenu_item), Some(Move::To(Nav::Open(0))), "подменю закрыто");
        assert_eq!(next_move(open, NavKey::Escape, n, plain), Some(Move::To(Nav::Bar(1))));
        // Вложенное подменю: Вправо открывает его, Влево закрывает то, в котором фокус.
        let nested = ListFocus { opens_submenu: true, in_submenu: true };
        assert_eq!(next_move(open, NavKey::Right, n, nested), Some(Move::OpenSubmenu));
        assert_eq!(next_move(open, NavKey::Left, n, nested), Some(Move::CloseSubmenu));
        // Вне открытого списка подменю нет: на строке меню стрелки водят по меню.
        assert_eq!(next_move(Nav::Bar(1), NavKey::Right, n, on_submenu_item), Some(Move::To(Nav::Bar(2))));
        assert_eq!(next_move(Nav::Bar(1), NavKey::Left, n, inside), Some(Move::To(Nav::Bar(0))));
        // Вверх, вниз и Enter в подменю — по-прежнему egui.
        for key in [NavKey::Up, NavKey::Down, NavKey::Enter] {
            assert_eq!(next_move(open, key, n, inside), None);
        }
    }

    /// Фильтр окна: `SC_KEYMENU` (F10, одиночный Alt — `lparam` 0; Alt+буква — код буквы) не доходит до Windows,
    /// Alt+Пробел (системное меню) и Alt+F4 (`SC_CLOSE`) — доходят, сами нажатия клавиш — тоже.
    #[test]
    fn window_filter_takes_only_the_keyboard_menu_command() {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            SC_CLOSE, SC_KEYMENU, SC_MOUSEMENU, WM_SYSCHAR, WM_SYSCOMMAND, WM_SYSKEYDOWN,
        };
        let keymenu = SC_KEYMENU as usize;
        assert!(swallows_system_key(WM_SYSCOMMAND, keymenu, 0), "F10 / одиночный Alt");
        assert!(swallows_system_key(WM_SYSCOMMAND, keymenu, b'f' as isize), "Alt+F");
        assert!(swallows_system_key(WM_SYSCOMMAND, keymenu, 'ф' as isize), "Alt+Ф");
        assert!(swallows_system_key(WM_SYSCOMMAND, keymenu | 0x3, 0), "служебные биты wparam");
        assert!(!swallows_system_key(WM_SYSCOMMAND, keymenu, b' ' as isize), "Alt+Пробел — системное меню");
        assert!(!swallows_system_key(WM_SYSCOMMAND, SC_CLOSE as usize, 0), "Alt+F4");
        assert!(!swallows_system_key(WM_SYSCOMMAND, SC_MOUSEMENU as usize, 0), "щелчок по значку окна");
        assert!(!swallows_system_key(WM_SYSKEYDOWN, 0x73, 0), "нажатие Alt+F4 само по себе");
        assert!(!swallows_system_key(WM_SYSCHAR, b'f' as usize, 0));
        assert!(!swallows_system_key(WM_SYSKEYDOWN, keymenu, 0), "то же число в другом сообщении");
    }

    #[test]
    fn keys_reach_the_menu_bar_only_when_they_should() {
        let m = [Some(egui::Key::F), Some(egui::Key::V), None, None, Some(egui::Key::H)];
        let (none, alt) = (egui::Modifiers::NONE, egui::Modifiers::ALT);
        assert_eq!(nav_key(egui::Key::F10, none, Nav::Idle, &m, true), Some(NavKey::Toggle));
        assert_eq!(nav_key(egui::Key::F10, none, Nav::Idle, &m, false), None, "поверх окна диалог");
        assert_eq!(nav_key(egui::Key::F10, egui::Modifiers::SHIFT, Nav::Idle, &m, true), None, "Shift+F10 — контекстное меню");
        assert_eq!(nav_key(egui::Key::H, alt, Nav::Idle, &m, true), Some(NavKey::Mnemonic(4)));
        assert_eq!(nav_key(egui::Key::H, alt | egui::Modifiers::CTRL, Nav::Idle, &m, true), None, "AltGr печатает");
        assert_eq!(nav_key(egui::Key::Q, alt, Nav::Idle, &m, true), None);
        assert_eq!(nav_key(egui::Key::ArrowLeft, none, Nav::Idle, &m, true), None, "без меню стрелки — таблице");
        assert_eq!(nav_key(egui::Key::ArrowLeft, none, Nav::Bar(1), &m, true), Some(NavKey::Left));
        assert_eq!(nav_key(egui::Key::Escape, none, Nav::Open(1), &m, false), Some(NavKey::Escape));
    }

    #[test]
    fn lone_alt_becomes_f10() {
        let mut t = AltTap::default();
        let mut down = with_alt(true, vec![]);
        t.step(&mut down);
        assert!(!plain_f10(&down), "нажатие — ещё не одиночный Alt");
        let mut up = with_alt(false, vec![]);
        t.step(&mut up);
        assert!(plain_f10(&up), "отпустили без других клавиш");
        let mut again = with_alt(false, vec![]);
        t.step(&mut again);
        assert!(!plain_f10(&again), "одно нажатие — один F10");
        // Нажатие и отпускание между двумя кадрами — тоже одиночный Alt.
        let mut t = AltTap::default();
        let mut both = with_alt(true, alt_key(egui::Key::AltLeft, false).to_vec());
        t.step(&mut both);
        assert!(plain_f10(&both), "нажат и отпущен внутри одного кадра");
    }

    /// Alt приходит в egui как своя клавиша (`Key::AltLeft` / `AltRight`) вслед за `ModifiersChanged`, при удержании —
    /// с автоповтором. Своё нажатие одиночный Alt не отменяет; любая другая клавиша после него — отменяет.
    #[test]
    fn alt_own_key_events_do_not_cancel_the_tap() {
        for key in [egui::Key::AltLeft, egui::Key::AltRight] {
            let mut t = AltTap::default();
            t.step(&mut input(true, alt_key(key, true).to_vec()));
            let mut held = input(true, vec![key_event(key, true, true, egui::Modifiers::ALT)]);
            t.step(&mut held);
            assert!(!plain_f10(&held), "{key:?}: удержание — ещё не отпускание");
            let mut up = input(true, alt_key(key, false).to_vec());
            t.step(&mut up);
            assert!(plain_f10(&up), "{key:?}: отпустили после автоповтора");
        }
        // Alt+F в реальном порядке событий: Alt, буква, отпускание Alt — это мнемоника, не одиночный Alt.
        let mut t = AltTap::default();
        let mut frame = with_alt(true, vec![press(egui::Key::F, egui::Modifiers::ALT)]);
        frame.events.extend(alt_key(egui::Key::AltLeft, false));
        t.step(&mut frame);
        assert!(!plain_f10(&frame), "Alt+F");
    }

    #[test]
    fn alt_with_anything_else_is_not_a_lone_alt() {
        let combos = [
            vec![press(egui::Key::F4, egui::Modifiers::ALT)],
            vec![press(egui::Key::F, egui::Modifiers::ALT)],
            vec![egui::Event::PointerButton { pos: egui::Pos2::ZERO, button: egui::PointerButton::Primary, pressed: true, modifiers: egui::Modifiers::ALT }],
            vec![egui::Event::WindowFocused(false)],
        ];
        for events in combos {
            let mut t = AltTap::default();
            t.step(&mut with_alt(true, vec![]));
            t.step(&mut with_alt(true, events.clone()));
            let mut up = with_alt(false, vec![]);
            t.step(&mut up);
            assert!(!plain_f10(&up), "{events:?}");
        }
        // AltGr (Ctrl+Alt) и окно без фокуса.
        let mut t = AltTap::default();
        let ctrl_alt = egui::Modifiers { alt: true, ctrl: true, ..Default::default() };
        t.step(&mut changed(true, ctrl_alt, vec![]));
        let mut up = with_alt(false, vec![]);
        t.step(&mut up);
        assert!(!plain_f10(&up), "AltGr");
        let mut t = AltTap::default();
        t.step(&mut changed(false, egui::Modifiers::ALT, vec![]));
        let mut up = with_alt(false, vec![]);
        t.step(&mut up);
        assert!(!plain_f10(&up), "Alt нажат в другом окне");
    }

    #[test]
    fn menu_key_press_becomes_shift_f10() {
        let mut k = MenuKey::default();
        let mut raw = input(true, vec![]);
        k.step(&mut raw, false);
        assert!(!opens_menu(&raw), "первый кадр — точка отсчёта");
        for toggle in [true, false] {
            let mut raw = input(true, vec![]);
            k.step(&mut raw, toggle);
            assert!(opens_menu(&raw), "каждое нажатие меняет бит");
        }
        let mut raw = input(true, vec![]);
        k.step(&mut raw, false);
        assert!(!opens_menu(&raw), "бит не менялся — нажатия не было");
    }

    #[test]
    fn menu_key_outside_the_window_is_ignored() {
        let mut k = MenuKey::default();
        k.step(&mut input(true, vec![]), false);
        let mut away = input(false, vec![]);
        k.step(&mut away, true);
        assert!(!opens_menu(&away), "окно без фокуса");
        let mut back = input(true, vec![egui::Event::WindowFocused(true)]);
        k.step(&mut back, false);
        assert!(!opens_menu(&back), "возврат фокуса — новая точка отсчёта");
        let mut raw = input(true, vec![]);
        k.step(&mut raw, true);
        assert!(opens_menu(&raw));
    }

    /// Правило 5 стандарта: контекстное меню открывается и с клавиатуры, поэтому в src/app оно строится только через
    /// `context_menu` (там Shift+F10 и клавиша меню). Правый щелчок напрямую (`Popup::context_menu`,
    /// `Response::context_menu`, `secondary_clicked`) вне этого файла — ошибка. Иглы собраны из частей.
    #[test]
    fn context_menus_go_through_the_helper() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join("app");
        let mut files = Vec::new();
        super::super::dialog::tests::collect_rs(&root, &mut files);
        assert!(files.len() > 10, "src/app not scanned: {}", root.display());
        let own = root.join("menu.rs");
        let banned = [concat!("Popup::context", "_menu("), concat!(".context", "_menu("), concat!("secondary", "_clicked(")];
        let mut bad = Vec::new();
        for path in files.iter().filter(|p| **p != own) {
            let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            for (n, line) in text.lines().enumerate() {
                for b in banned.iter().filter(|b| line.contains(*b)) {
                    bad.push(format!("{}:{}: {b}", path.display(), n + 1));
                }
            }
        }
        assert!(bad.is_empty(), "context menu outside menu::context_menu:\n{}", bad.join("\n"));
    }
}
