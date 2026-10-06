//! Значок в трее (Shell_NotifyIcon) и уведомления Windows через него.
//! Сообщения значка ловим подклассом главного окна; цвет точки в углу иконки = состояние туннелей.
//! Та же точка — поверх кнопки окна в панели задач (`set_window_state`, см. `taskbar`).
//! Функции можно звать из любого потока; до `install` они ничего не делают. Вызовы COM панели задач делает
//! поток окна: остальные потоки шлют окну `WM_TASKBAR`.

use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::sync::{Mutex, OnceLock};

use eframe::egui;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    CreateBitmap, CreateDIBSection, DeleteObject, GetDC, ReleaseDC, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
};
use windows_sys::Win32::UI::Shell::{
    DefSubclassProc, SetWindowSubclass, Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO,
    NIIF_WARNING, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, ChangeWindowMessageFilterEx, CreateIconIndirect, CreatePopupMenu, DestroyMenu, GetCursorPos,
    GetSystemMetrics, IsIconic, IsWindowVisible, PostMessageW, RegisterWindowMessageW, SetForegroundWindow,
    SetMenuDefaultItem, ShowWindow, TrackPopupMenu, HMENU, ICONINFO, MF_CHECKED, MF_GRAYED, MF_POPUP, MF_SEPARATOR,
    MF_STRING, ICON_BIG, ICON_SMALL, MSGFLT_ALLOW, SM_CXICON, SM_CXSMICON, SW_HIDE, SW_RESTORE, SW_SHOW, TPM_NONOTIFY,
    TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_DESTROY, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP,
    WM_SETICON,
};

use crate::health::Level;
use crate::i18n::tr;
use crate::icon;
use crate::taskbar;
use crate::win::wide;

const WM_TRAY: u32 = WM_APP + 1;
/// Применить к кнопке в панели задач то, что указано флагами в wParam (в потоке окна).
const WM_TASKBAR: u32 = WM_APP + 2;
const TASKBAR_OVERLAY: usize = 1;
const TASKBAR_IDENTITY: usize = 2;
/// Кнопка в панели задач (пере)создана: взять новый объект панели задач и применить всё заново.
const TASKBAR_BUTTON: usize = 4;
const SUBCLASS_ID: usize = 0x4157_4755; // "AWGU"
const CMD_OPEN: usize = 1;
const CMD_EXIT: usize = 2;
/// Серая строка-пояснение: выбрать её нельзя, а 0 — тот же ответ, что «меню закрыто без выбора».
const CMD_NOTE: usize = 0;
/// Пункты туннелей: `CMD_TUNNEL + i`, `i` — индекс в списке имён, который возвращает `layout`.
const CMD_TUNNEL: usize = 100;

/// Строка меню трея, как её видит окно: туннель или группа с вложенными строками.
#[derive(Clone, Debug, PartialEq)]
pub enum Entry {
    Tunnel {
        name: String,
        /// Не отключён: подключён, переключается или его переподключает ядро (отметка у пункта).
        connected: bool,
        /// Можно переключить (не занят другим переключением).
        enabled: bool,
    },
    Group { title: String, entries: Vec<Entry> },
    /// Серая строка-пояснение без действия (например, «нет связи с ядром» над серыми туннелями): у пунктов
    /// Win32-меню нет подсказок, поэтому причина серости — отдельной строкой.
    Note(String),
}

/// Что меню трея берёт у окна. Зовётся в потоке окна, в том числе когда окно скрыто и кадров не рисует, — поэтому
/// не через кадр, а напрямую.
pub trait Hooks: Send + Sync {
    /// Туннели для меню на момент щелчка правой кнопкой.
    fn entries(&self) -> Vec<Entry>;
    /// Пункт туннеля выбран: подключить или отключить — тем же запросом, что и окно.
    fn toggle(&self, tunnel: &str);
    /// Пункт «Выход».
    fn exit(&self);
}

/// О чём последнее показанное уведомление — от этого зависит, что откроет щелчок по нему.
#[derive(Clone, Debug, PartialEq)]
enum Balloon {
    None,
    /// О неустановленном обновлении: щелчок открывает «Обновления и откаты».
    Update,
    /// О туннеле: щелчок выбирает его в окне.
    Tunnel(String),
}

/// Щелчок по уведомлению, который окно ещё не обработало (`take_click`).
#[derive(Clone, Debug, PartialEq)]
pub enum Clicked {
    Update,
    Tunnel(String),
}

struct Tray {
    hwnd: isize,
    /// По режиму (0 — поверх AmneziaWG, 1 — встроенный движок): серый, зелёный, жёлтый, красный.
    icons: [[isize; 4]; 2],
    /// Значки окна (заголовок, Alt+Tab) по режиму, [большой, малый]; без точки — она поверх кнопки в панели задач.
    window_icons: [[isize; 2]; 2],
    /// Точки поверх кнопки в панели задач: серая, зелёная, жёлтая, красная.
    overlay_icons: [isize; 4],
    taskbar_created: u32,
    taskbar_button_created: u32,
    ctx: egui::Context,
    hooks: Box<dyn Hooks>,
    state: Mutex<State>,
    /// Один вызов `Shell_NotifyIconW` за раз: решение, вызов и запись результата идут как одно целое. Без этого два
    /// потока оба решили бы «добавить», и неудача второго (значок уже есть) сбросила бы `visible` у живого значка.
    shell: Mutex<()>,
    /// О чём последнее показанное уведомление. Каждое новое заменяет прежнее, чтобы щелчок по сообщению о туннеле
    /// не открыл обновления, и наоборот.
    balloon: Mutex<Balloon>,
    /// Щелчок по уведомлению, который окно ещё не обработало (`take_click`).
    clicked: Mutex<Option<Clicked>>,
}

struct State {
    /// Пользователь хочет значок в трее (настройка).
    wanted: bool,
    /// Значок реально добавлен в Explorer. Может быть `false` при `wanted`: добавление не удалось (Explorer ещё
    /// поднимается после перезапуска) — `set_state` повторяет его на каждом тике.
    visible: bool,
    level: Level,
    tip: String,
    /// Индекс в `overlay_icons` — точка поверх кнопки в панели задач; `None` — без точки.
    overlay: Option<usize>,
    /// Режим 2 — иконки жёлтые; `None` — ещё не выставлен (значок окна задан при запуске).
    engine: Option<bool>,
}

static TRAY: OnceLock<Tray> = OnceLock::new();

/// Подключить трей к окну. `hooks` — туннели для меню, их переключение и «Выход».
pub fn install(hwnd: isize, ctx: egui::Context, visible: bool, hooks: Box<dyn Hooks>) {
    let size = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16) as u32;
    let dots = [icon::GRAY, icon::GREEN, icon::YELLOW, icon::RED];
    let icons = [false, true].map(|engine| dots.map(|c| make_icon(size, &icon::rgba_with_dot(size, engine, c))));
    let window_sizes = unsafe { [GetSystemMetrics(SM_CXICON), GetSystemMetrics(SM_CXSMICON)] }.map(|s| s.max(16) as u32);
    let window_icons = [false, true].map(|engine| window_sizes.map(|s| make_icon(s, &icon::themed(s, engine))));
    let overlay_icons = dots.map(|c| make_icon(size, &icon::dot_icon(size, c)));
    let taskbar_created = unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) };
    let taskbar_button_created = unsafe { RegisterWindowMessageW(wide("TaskbarButtonCreated").as_ptr()) };
    let tray = Tray {
        hwnd,
        icons,
        window_icons,
        overlay_icons,
        taskbar_created,
        taskbar_button_created,
        ctx,
        hooks,
        state: Mutex::new(State { wanted: false, visible: false, level: Level::Off, tip: crate::APP_TITLE.to_string(), overlay: None, engine: None }),
        shell: Mutex::new(()),
        balloon: Mutex::new(Balloon::None),
        clicked: Mutex::new(None),
    };
    if TRAY.set(tray).is_err() {
        return;
    }
    unsafe {
        SetWindowSubclass(hwnd as HWND, Some(subclass_proc), SUBCLASS_ID, 0);
        // Окно с правами администратора иначе не получит эти сообщения от Explorer (UIPI).
        for msg in [taskbar_created, taskbar_button_created] {
            ChangeWindowMessageFilterEx(hwnd as HWND, msg, MSGFLT_ALLOW, null_mut());
        }
        // Окно уже видно — его кнопка могла появиться до подкласса, и «кнопка создана» мы пропустили.
        if IsWindowVisible(hwnd as HWND) != 0 {
            PostMessageW(hwnd as HWND, WM_TASKBAR, TASKBAR_BUTTON | TASKBAR_IDENTITY | TASKBAR_OVERLAY, 0);
        }
    }
    set_visible(visible);
}

pub fn set_visible(visible: bool) {
    let Some(t) = TRAY.get() else { return };
    step(t, |st| {
        st.wanted = visible;
        plan_icon(st, false)
    });
}

/// Что сделать со значком в Explorer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Call {
    Add,
    Modify,
    Delete,
}

/// Привести значок в Explorer к желаемому: добавить (при отказе — повтор на следующем тике), убрать или, если
/// значок есть и его вид изменился (`changed`), обновить. Только решение — вызов делает `step`.
fn plan_icon(st: &mut State, changed: bool) -> Option<Call> {
    if st.wanted && !st.visible {
        Some(Call::Add)
    } else if !st.wanted && st.visible {
        st.visible = false;
        Some(Call::Delete)
    } else if st.visible && changed {
        Some(Call::Modify)
    } else {
        None
    }
}

/// Одна операция над значком в трее. Решение принимается под блокировкой состояния, а `Shell_NotifyIconW` вызывается
/// уже без неё: Explorer может отвечать долго, а состояние читают поток окна и монитор. Итог вызова записывается
/// обратно отдельно; порядок шагов держит `Tray::shell`.
fn step(t: &Tray, plan: impl FnOnce(&mut State) -> Option<Call>) {
    let _one_at_a_time = t.shell.lock().unwrap();
    let (call, data) = {
        let mut st = t.state.lock().unwrap();
        let Some(call) = plan(&mut st) else { return };
        let mut data = base(t);
        if call != Call::Delete {
            fill_icon(t, &st, &mut data);
        }
        (call, data)
    };
    let message = match call {
        Call::Add => NIM_ADD,
        Call::Modify => NIM_MODIFY,
        Call::Delete => NIM_DELETE,
    };
    let done = unsafe { Shell_NotifyIconW(message, &data) } != 0;
    let mut st = t.state.lock().unwrap();
    match call {
        // NIM_ADD в момент старта Explorer нередко отказывает; `visible` остаётся false, добавление повторится.
        Call::Add => {
            st.visible = done;
            if !done {
                eprintln!("трей: не удалось добавить значок, повтор на следующем тике");
            }
        }
        // Отказ (значка в Explorer уже нет) — снова считаем его не добавленным.
        Call::Modify if !done => {
            st.visible = false;
            eprintln!("трей: значок не обновился, будет добавлен заново");
        }
        Call::Modify | Call::Delete => {}
    }
}

pub fn remove() {
    set_visible(false);
}

pub fn set_state(level: Level, tip: &str) {
    let Some(t) = TRAY.get() else { return };
    let mut announce = false;
    step(t, |st| {
        let changed = st.level != level || st.tip != tip;
        announce = st.tip != tip && st.overlay.is_some();
        st.level = level;
        st.tip = tip.to_string();
        plan_icon(st, changed)
    });
    if announce {
        // Подпись точки для экранного диктора — та же, что подсказка значка в трее.
        post_taskbar(t, TASKBAR_OVERLAY);
    }
}

/// Точка состояния поверх кнопки окна в панели задач; `None` — без точки.
pub fn set_window_state(level: Option<Level>) {
    let Some(t) = TRAY.get() else { return };
    let index = level.map(dot_index);
    let mut st = t.state.lock().unwrap();
    if st.overlay == index {
        return;
    }
    st.overlay = index;
    post_taskbar(t, TASKBAR_OVERLAY);
}

/// Цвет иконок по режиму работы: в режиме 2 (встроенный движок) — жёлтые, в трее, панели задач и заголовке.
pub fn set_engine(engine: bool) {
    let Some(t) = TRAY.get() else { return };
    let mut changed = false;
    step(t, |st| {
        if st.engine == Some(engine) {
            return None;
        }
        st.engine = Some(engine);
        changed = true;
        plan_icon(st, true)
    });
    if !changed {
        return;
    }
    let [big, small] = t.window_icons[engine as usize];
    // PostMessage: могут звать не из потока окна, ждать окно незачем.
    unsafe {
        PostMessageW(t.hwnd as HWND, WM_SETICON, ICON_BIG as WPARAM, big);
        PostMessageW(t.hwnd as HWND, WM_SETICON, ICON_SMALL as WPARAM, small);
    }
    post_taskbar(t, TASKBAR_IDENTITY);
}

/// Попросить поток окна применить к кнопке в панели задач то, что в `what`.
fn post_taskbar(t: &Tray, what: usize) {
    unsafe { PostMessageW(t.hwnd as HWND, WM_TASKBAR, what, 0) };
}

/// Применить к кнопке в панели задач текущее состояние (только в потоке окна). Блокировка состояния на время
/// вызовов COM не держится: они могут ждать Explorer.
fn apply_taskbar(t: &Tray, what: usize) {
    let (engine, overlay, tip) = {
        let st = t.state.lock().unwrap();
        (st.engine, st.overlay, st.tip.clone())
    };
    let report = |r: Result<(), String>| {
        if let Err(e) = r {
            eprintln!("панель задач: {e}");
        }
    };
    if what & TASKBAR_BUTTON != 0 {
        report(taskbar::button_created());
    }
    if what & TASKBAR_IDENTITY != 0 {
        if let Some(engine) = engine {
            report(taskbar::set_identity(t.hwnd, engine));
            // Ярлык с прошлой версии мог остаться без AppUserModelID — поправить один раз за запуск.
            static SHORTCUT_CHECKED: std::sync::Once = std::sync::Once::new();
            SHORTCUT_CHECKED.call_once(|| report(crate::shortcut::repair_desktop_app_id(crate::APP_TITLE).map(drop)));
        }
    }
    if what & (TASKBAR_OVERLAY | TASKBAR_BUTTON) != 0 {
        report(taskbar::set_overlay(t.hwnd, overlay.map_or(0, |i| t.overlay_icons[i]), &tip));
    }
}

/// Всплывающее уведомление Windows о туннеле (в Windows 10/11 показывается как toast); щелчок по нему поднимает
/// окно и выбирает туннель (`take_click`). Пустое имя — уведомление ни о каком туннеле: щелчок только поднимает окно.
pub fn notify_tunnel(tunnel: &str, title: &str, text: &str, warning: bool) {
    let about = if tunnel.is_empty() { Balloon::None } else { Balloon::Tunnel(tunnel.to_string()) };
    balloon(title, text, warning, about);
}

/// Уведомление о неустановленном обновлении: показывается и при видимом окне; щелчок по нему поднимает окно и
/// открывает «Обновления и откаты» (`take_click`). Без значка в трее показать его нечем.
pub fn notify_update(title: &str, text: &str) {
    balloon(title, text, false, Balloon::Update);
}

/// Щелчок по уведомлению, который окну надо обработать: открыть обновления или выбрать туннель. Читается один раз.
pub fn take_click() -> Option<Clicked> {
    TRAY.get().and_then(|t| t.clicked.lock().unwrap().take())
}

fn balloon(title: &str, text: &str, warning: bool, about: Balloon) {
    let Some(t) = TRAY.get() else { return };
    // Тот же порядок вызовов оболочки, что у `step`: уведомление не вклинивается между его решением и вызовом.
    // Состояние — только прочитать; вызывающий не должен держать чужих блокировок (`monitor::record_and_notify`).
    let _one_at_a_time = t.shell.lock().unwrap();
    if !t.state.lock().unwrap().visible {
        return;
    }
    let mut data = base(t);
    data.uFlags = NIF_INFO;
    copy(&mut data.szInfoTitle, title);
    copy(&mut data.szInfo, text);
    data.dwInfoFlags = if warning { NIIF_WARNING } else { NIIF_INFO };
    // Признак — до вызова: щелчок по уведомлению может прийти сразу, а окно разберёт его уже по этому признаку.
    *t.balloon.lock().unwrap() = about;
    if unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) } == 0 {
        eprintln!("трей: уведомление не показано: {title}");
    }
}

/// Сообщение трея `NIN_BALLOONUSERCLICK` (WM_USER + 5): пользователь щёлкнул по всплывшему уведомлению.
const NIN_BALLOONUSERCLICK: u32 = 0x0400 + 5;

/// Что сделать окну по сообщению значка: щелчок по уведомлению об обновлении открывает обновления, о туннеле —
/// выбирает туннель; иначе — ничего, кроме подъёма окна (`None`).
fn balloon_click(tray_message: u32, last: &Balloon) -> Option<Clicked> {
    if tray_message != NIN_BALLOONUSERCLICK {
        return None;
    }
    match last {
        Balloon::None => None,
        Balloon::Update => Some(Clicked::Update),
        Balloon::Tunnel(name) => Some(Clicked::Tunnel(name.clone())),
    }
}

pub fn show_window() {
    let Some(t) = TRAY.get() else { return };
    let hwnd = t.hwnd as HWND;
    unsafe {
        ShowWindow(hwnd, if IsIconic(hwnd) != 0 { SW_RESTORE } else { SW_SHOW });
        SetForegroundWindow(hwnd);
    }
    t.ctx.request_repaint();
}

pub fn hide_window() {
    if let Some(t) = TRAY.get() {
        unsafe { ShowWindow(t.hwnd as HWND, SW_HIDE) };
    }
}

fn base(t: &Tray) -> NOTIFYICONDATAW {
    let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    data.hWnd = t.hwnd as HWND;
    data.uID = 1;
    data
}

fn fill_icon(t: &Tray, st: &State, data: &mut NOTIFYICONDATAW) {
    data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    data.uCallbackMessage = WM_TRAY;
    data.hIcon = t.icons[(st.engine == Some(true)) as usize][dot_index(st.level)] as _;
    copy(&mut data.szTip, &st.tip);
}

/// Цвет точки по состоянию: серый, зелёный, жёлтый, красный.
fn dot_index(level: Level) -> usize {
    match level {
        Level::Off => 0,
        Level::Ok => 1,
        Level::Busy | Level::Warn => 2,
        Level::Bad => 3,
    }
}

unsafe extern "system" fn subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    if let Some(t) = TRAY.get() {
        if msg == WM_TRAY {
            let message = lparam as u32;
            match message {
                WM_LBUTTONUP | WM_LBUTTONDBLCLK => show_window(),
                WM_RBUTTONUP => menu(t, hwnd),
                NIN_BALLOONUSERCLICK => {
                    let clicked = balloon_click(message, &t.balloon.lock().unwrap());
                    // Сначала признак, потом показ: первый кадр поднятого окна его уже увидит.
                    *t.clicked.lock().unwrap() = clicked;
                    show_window();
                }
                _ => {}
            }
            return 0;
        }
        if msg == WM_TASKBAR {
            apply_taskbar(t, wparam);
            return 0;
        }
        if msg == t.taskbar_button_created {
            // Приходит при каждом (пере)создании кнопки: показ окна после скрытия, перезапуск Explorer.
            apply_taskbar(t, TASKBAR_BUTTON | TASKBAR_IDENTITY | TASKBAR_OVERLAY);
        }
        if msg == WM_DESTROY {
            taskbar::release();
            if let Err(e) = taskbar::clear_identity(t.hwnd) {
                eprintln!("панель задач: {e}");
            }
        }
        if msg == t.taskbar_created {
            // Explorer перезапустился — значок надо добавить заново.
            step(t, |st| {
                st.visible = false;
                plan_icon(st, false)
            });
        }
    }
    DefSubclassProc(hwnd, msg, wparam, lparam)
}

/// Строка Win32-меню: команда со своим номером, подменю или разделитель.
#[derive(Debug, PartialEq)]
enum Item {
    Command { id: usize, text: String, checked: bool, enabled: bool, default: bool },
    Popup { text: String, items: Vec<Item> },
    Separator,
}

/// Меню трея: «Открыть» (по умолчанию, жирным), туннели (группы — подменю), «Выход». Возвращает строки и имена
/// туннелей по номерам команд: пункт `CMD_TUNNEL + i` — туннель `names[i]`.
fn layout(entries: &[Entry]) -> (Vec<Item>, Vec<String>) {
    let mut names = Vec::new();
    let mut items = vec![Item::Command { id: CMD_OPEN, text: tr("tray.open"), checked: false, enabled: true, default: true }];
    let tunnels = tunnel_items(entries, &mut names);
    if !tunnels.is_empty() {
        items.push(Item::Separator);
        items.extend(tunnels);
    }
    items.push(Item::Separator);
    items.push(Item::Command { id: CMD_EXIT, text: tr("tray.exit"), checked: false, enabled: true, default: false });
    (items, names)
}

fn tunnel_items(entries: &[Entry], names: &mut Vec<String>) -> Vec<Item> {
    entries
        .iter()
        .filter_map(|e| match e {
            Entry::Tunnel { name, connected, enabled } => {
                names.push(name.clone());
                let id = CMD_TUNNEL + names.len() - 1;
                Some(Item::Command { id, text: menu_text(name), checked: *connected, enabled: *enabled, default: false })
            }
            // Пустое подменю — тупик, его не показываем.
            Entry::Group { title, entries } => {
                let items = tunnel_items(entries, names);
                (!items.is_empty()).then(|| Item::Popup { text: menu_text(title), items })
            }
            Entry::Note(text) => Some(Item::Command { id: CMD_NOTE, text: menu_text(text), checked: false, enabled: false, default: false }),
        })
        .collect()
}

/// Текст пункта как есть: `&` в имени Win32 иначе принял бы за мнемонику и не показал.
fn menu_text(s: &str) -> String {
    s.replace('&', "&&")
}

/// Номер выбранной команды -> что сделать.
#[derive(Debug, PartialEq)]
enum Pick<'a> {
    Open,
    Exit,
    Toggle(&'a str),
    Nothing,
}

fn pick(cmd: usize, names: &[String]) -> Pick<'_> {
    match cmd {
        CMD_OPEN => Pick::Open,
        CMD_EXIT => Pick::Exit,
        _ => match cmd.checked_sub(CMD_TUNNEL).and_then(|i| names.get(i)) {
            Some(name) => Pick::Toggle(name),
            None => Pick::Nothing,
        },
    }
}

unsafe fn fill_menu(m: HMENU, items: &[Item]) {
    for item in items {
        match item {
            Item::Command { id, text, checked, enabled, default } => {
                let flags = MF_STRING | if *checked { MF_CHECKED } else { 0 } | if *enabled { 0 } else { MF_GRAYED };
                AppendMenuW(m, flags, *id, wide(text).as_ptr());
                if *default {
                    SetMenuDefaultItem(m, *id as u32, 0);
                }
            }
            Item::Popup { text, items } => {
                // Подменю уничтожается вместе с родителем (`DestroyMenu` в `menu`).
                let sub = CreatePopupMenu();
                fill_menu(sub, items);
                AppendMenuW(m, MF_POPUP, sub as usize, wide(text).as_ptr());
            }
            Item::Separator => {
                AppendMenuW(m, MF_SEPARATOR, 0, null());
            }
        }
    }
}

unsafe fn menu(t: &Tray, hwnd: HWND) {
    let (items, names) = layout(&t.hooks.entries());
    let m = CreatePopupMenu();
    fill_menu(m, &items);
    let mut pt = POINT { x: 0, y: 0 };
    GetCursorPos(&mut pt);
    SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(m, TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, null()) as usize;
    PostMessageW(hwnd, WM_NULL, 0, 0);
    DestroyMenu(m);
    match pick(cmd, &names) {
        Pick::Open => show_window(),
        Pick::Exit => t.hooks.exit(),
        Pick::Toggle(name) => t.hooks.toggle(name),
        Pick::Nothing => {}
    }
}

/// HICON из RGBA: 32-битный DIB с альфой плюс пустая маска.
fn make_icon(size: u32, rgba: &[u8]) -> isize {
    unsafe {
        let mut bmi: BITMAPINFO = std::mem::zeroed();
        bmi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = size as i32;
        bmi.bmiHeader.biHeight = -(size as i32); // сверху вниз
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB as _;
        let mut bits: *mut c_void = null_mut();
        let dc = GetDC(null_mut());
        let color = CreateDIBSection(dc, &bmi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        ReleaseDC(null_mut(), dc);
        if color.is_null() || bits.is_null() || rgba.len() as u64 > size as u64 * size as u64 * 4 {
            // Нехватка GDI-ресурсов: «значка нет» (0), а не обращение по нулевому указателю.
            eprintln!("трей: CreateDIBSection не удалась, значок {size}x{size} не создан");
            if !color.is_null() {
                DeleteObject(color);
            }
            return 0;
        }
        let dst = std::slice::from_raw_parts_mut(bits as *mut u8, rgba.len());
        for (d, s) in dst.chunks_exact_mut(4).zip(rgba.chunks_exact(4)) {
            d.copy_from_slice(&[s[2], s[1], s[0], s[3]]);
        }
        let mask = CreateBitmap(size as i32, size as i32, 1, 1, null());
        let info = ICONINFO { fIcon: 1, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
        let icon = CreateIconIndirect(&info);
        DeleteObject(color);
        DeleteObject(mask);
        icon as isize
    }
}

fn copy(dst: &mut [u16], s: &str) {
    let units: Vec<u16> = s.encode_utf16().take(dst.len() - 1).collect();
    dst[..units.len()].copy_from_slice(&units);
    dst[units.len()] = 0;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(wanted: bool, visible: bool) -> State {
        State { wanted, visible, level: Level::Off, tip: String::new(), overlay: None, engine: None }
    }

    #[test]
    fn toast_click_opens_what_the_toast_was_about() {
        let office = Balloon::Tunnel("office".into());
        assert_eq!(balloon_click(NIN_BALLOONUSERCLICK, &Balloon::Update), Some(Clicked::Update));
        assert_eq!(
            balloon_click(NIN_BALLOONUSERCLICK, &office),
            Some(Clicked::Tunnel("office".into())),
            "щелчок по сообщению о туннеле выбирает туннель, обновления не открывает"
        );
        assert_eq!(balloon_click(NIN_BALLOONUSERCLICK, &Balloon::None), None);
        assert_eq!(balloon_click(WM_LBUTTONUP, &Balloon::Update), None, "значок в трее — не уведомление");
        assert_eq!(balloon_click(WM_RBUTTONUP, &office), None);
    }

    fn tunnel(name: &str, connected: bool, enabled: bool) -> Entry {
        Entry::Tunnel { name: name.into(), connected, enabled }
    }

    #[test]
    fn menu_lists_tunnels_between_open_and_exit() {
        let work = Entry::Group {
            title: "Work".into(),
            entries: vec![tunnel("office", true, true), Entry::Group { title: "Empty".into(), entries: vec![] }],
        };
        let (items, names) = layout(&[work, tunnel("R&D", false, false)]);
        assert_eq!(names, ["office", "R&D"]);
        assert!(matches!(&items[0], Item::Command { id: CMD_OPEN, default: true, .. }), "«Открыть» первым и по умолчанию");
        assert_eq!(items[1], Item::Separator);
        let Item::Popup { text, items: inner } = &items[2] else { panic!("группа — подменю: {:?}", items[2]) };
        assert_eq!(text, "Work");
        let office = Item::Command { id: CMD_TUNNEL, text: "office".into(), checked: true, enabled: true, default: false };
        assert_eq!(inner, &[office], "пустая подгруппа не показана");
        let rnd = Item::Command { id: CMD_TUNNEL + 1, text: "R&&D".into(), checked: false, enabled: false, default: false };
        assert_eq!(items[3], rnd, "& не становится мнемоникой; занятый туннель — серый");
        assert_eq!(items[4], Item::Separator);
        assert!(matches!(&items[5], Item::Command { id: CMD_EXIT, .. }));
        assert_eq!(items.len(), 6);
    }

    #[test]
    fn note_is_a_grey_line_that_picks_nothing() {
        let (items, names) = layout(&[Entry::Note("No core & co".into()), tunnel("a", false, false)]);
        let note = Item::Command { id: CMD_NOTE, text: "No core && co".into(), checked: false, enabled: false, default: false };
        assert_eq!(items[2], note);
        assert_eq!(names, ["a"], "пояснение не занимает номер туннеля");
        assert_eq!(pick(CMD_NOTE, &names), Pick::Nothing);
    }

    #[test]
    fn menu_without_tunnels_is_open_and_exit() {
        let (items, names) = layout(&[]);
        assert!(names.is_empty());
        assert_eq!(items.len(), 3, "без лишнего разделителя: {items:?}");
    }

    #[test]
    fn command_ids_map_back_to_tunnels() {
        let names = vec!["a".to_string(), "b".to_string()];
        assert_eq!(pick(CMD_OPEN, &names), Pick::Open);
        assert_eq!(pick(CMD_EXIT, &names), Pick::Exit);
        assert_eq!(pick(CMD_TUNNEL + 1, &names), Pick::Toggle("b"));
        assert_eq!(pick(CMD_TUNNEL + 2, &names), Pick::Nothing, "номер вне списка — ничего");
        assert_eq!(pick(0, &names), Pick::Nothing, "меню закрыто без выбора");
    }

    #[test]
    fn icon_plan_adds_removes_and_updates() {
        assert_eq!(plan_icon(&mut state(true, false), false), Some(Call::Add), "значка нет, а он нужен — в том числе повтор после отказа");
        assert_eq!(plan_icon(&mut state(true, true), false), None);
        assert_eq!(plan_icon(&mut state(true, true), true), Some(Call::Modify));
        assert_eq!(plan_icon(&mut state(false, false), true), None, "значка нет и не нужен — обновлять нечего");
        let mut shown = state(false, true);
        assert_eq!(plan_icon(&mut shown, true), Some(Call::Delete), "удаление важнее обновления");
        assert!(!shown.visible, "второе решение подряд не пошлёт удаление повторно");
        assert_eq!(plan_icon(&mut shown, true), None);
    }

    #[test]
    fn copy_truncates_with_nul() {
        let mut buf = [1u16; 4];
        super::copy(&mut buf, "abcdef");
        assert_eq!(buf, [b'a' as u16, b'b' as u16, b'c' as u16, 0]);
    }
}
