//! Значок в трее (Shell_NotifyIcon) и уведомления Windows через него.
//! Сообщения значка ловим подклассом главного окна; цвет точки в углу иконки = состояние туннелей.
//! Та же точка — на значке окна в панели задач (`set_window_state`).
//! Функции можно звать из любого потока; до `install` они ничего не делают.

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
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, DestroyMenu, GetCursorPos, GetSystemMetrics, IsIconic,
    PostMessageW, RegisterWindowMessageW, SetForegroundWindow, ShowWindow, TrackPopupMenu, ICONINFO, MF_SEPARATOR,
    MF_STRING, ICON_BIG, ICON_SMALL, SM_CXICON, SM_CXSMICON, SW_HIDE, SW_RESTORE, SW_SHOW, TPM_NONOTIFY, TPM_RETURNCMD,
    TPM_RIGHTBUTTON, WM_APP, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WM_SETICON,
};

use crate::health::Level;
use crate::i18n::tr;
use crate::icon;

const WM_TRAY: u32 = WM_APP + 1;
const SUBCLASS_ID: usize = 0x4157_4755; // "AWGU"
const CMD_OPEN: usize = 1;
const CMD_EXIT: usize = 2;

struct Tray {
    hwnd: isize,
    /// Серый, зелёный, жёлтый, красный.
    icons: [isize; 4],
    /// Значки окна [большой, малый]: без точки, затем с точкой тех же четырёх цветов.
    window_icons: [[isize; 2]; 5],
    taskbar_created: u32,
    ctx: egui::Context,
    on_exit: Box<dyn Fn() + Send + Sync>,
    state: Mutex<State>,
}

struct State {
    visible: bool,
    level: Level,
    tip: String,
    /// Индекс в `window_icons`, выставленный окну.
    window_icon: usize,
}

static TRAY: OnceLock<Tray> = OnceLock::new();

/// Подключить трей к окну. `on_exit` вызывается пунктом «Выход».
pub fn install(hwnd: isize, ctx: egui::Context, visible: bool, on_exit: Box<dyn Fn() + Send + Sync>) {
    let size = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16) as u32;
    let dots = [icon::GRAY, icon::GREEN, icon::YELLOW, icon::RED];
    let icons = dots.map(|c| make_icon(size, &icon::rgba_with_dot(size, c)));
    let window_sizes = unsafe { [GetSystemMetrics(SM_CXICON), GetSystemMetrics(SM_CXSMICON)] }.map(|s| s.max(16) as u32);
    let mut window_icons = [window_sizes.map(|s| make_icon(s, &icon::rgba(s))); 5];
    for (i, c) in dots.into_iter().enumerate() {
        window_icons[i + 1] = window_sizes.map(|s| make_icon(s, &icon::rgba_with_dot(s, c)));
    }
    let taskbar_created = unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) };
    let tray = Tray {
        hwnd,
        icons,
        window_icons,
        taskbar_created,
        ctx,
        on_exit,
        state: Mutex::new(State { visible: false, level: Level::Off, tip: crate::APP_TITLE.to_string(), window_icon: 0 }),
    };
    if TRAY.set(tray).is_err() {
        return;
    }
    unsafe { SetWindowSubclass(hwnd as HWND, Some(subclass_proc), SUBCLASS_ID, 0) };
    set_visible(visible);
}

pub fn set_visible(visible: bool) {
    let Some(t) = TRAY.get() else { return };
    let mut st = t.state.lock().unwrap();
    if st.visible == visible {
        return;
    }
    st.visible = visible;
    let mut data = base(t);
    if visible {
        fill_icon(t, &st, &mut data);
        unsafe { Shell_NotifyIconW(NIM_ADD, &data) };
    } else {
        unsafe { Shell_NotifyIconW(NIM_DELETE, &data) };
    }
}

pub fn remove() {
    set_visible(false);
}

pub fn set_state(level: Level, tip: &str) {
    let Some(t) = TRAY.get() else { return };
    let mut st = t.state.lock().unwrap();
    if st.level == level && st.tip == tip {
        return;
    }
    st.level = level;
    st.tip = tip.to_string();
    if st.visible {
        let mut data = base(t);
        fill_icon(t, &st, &mut data);
        unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) };
    }
}

/// Точка состояния на значке окна (панель задач и заголовок); `None` — обычный значок.
pub fn set_window_state(level: Option<Level>) {
    let Some(t) = TRAY.get() else { return };
    let index = level.map_or(0, |l| dot_index(l) + 1);
    let mut st = t.state.lock().unwrap();
    if st.window_icon == index {
        return;
    }
    st.window_icon = index;
    let [big, small] = t.window_icons[index];
    // PostMessage: зовут из фонового потока, ждать окно незачем.
    unsafe {
        PostMessageW(t.hwnd as HWND, WM_SETICON, ICON_BIG as WPARAM, big);
        PostMessageW(t.hwnd as HWND, WM_SETICON, ICON_SMALL as WPARAM, small);
    }
}

/// Всплывающее уведомление Windows (в Windows 10/11 показывается как toast).
pub fn notify(title: &str, text: &str, warning: bool) {
    let Some(t) = TRAY.get() else { return };
    if !t.state.lock().unwrap().visible {
        return;
    }
    let mut data = base(t);
    data.uFlags = NIF_INFO;
    copy(&mut data.szInfoTitle, title);
    copy(&mut data.szInfo, text);
    data.dwInfoFlags = if warning { NIIF_WARNING } else { NIIF_INFO };
    unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) };
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
    data.hIcon = t.icons[dot_index(st.level)] as _;
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
            match lparam as u32 {
                WM_LBUTTONUP | WM_LBUTTONDBLCLK => show_window(),
                WM_RBUTTONUP => menu(t, hwnd),
                _ => {}
            }
            return 0;
        }
        if msg == t.taskbar_created {
            // Explorer перезапустился — значок надо добавить заново.
            let visible = {
                let mut st = t.state.lock().unwrap();
                std::mem::replace(&mut st.visible, false)
            };
            set_visible(visible);
        }
    }
    DefSubclassProc(hwnd, msg, wparam, lparam)
}

unsafe fn menu(t: &Tray, hwnd: HWND) {
    let m = CreatePopupMenu();
    AppendMenuW(m, MF_STRING, CMD_OPEN, wide(&tr("tray.open")).as_ptr());
    AppendMenuW(m, MF_SEPARATOR, 0, null());
    AppendMenuW(m, MF_STRING, CMD_EXIT, wide(&tr("tray.exit")).as_ptr());
    let mut pt = POINT { x: 0, y: 0 };
    GetCursorPos(&mut pt);
    SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(m, TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, null()) as usize;
    PostMessageW(hwnd, WM_NULL, 0, 0);
    DestroyMenu(m);
    match cmd {
        CMD_OPEN => show_window(),
        CMD_EXIT => (t.on_exit)(),
        _ => {}
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

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn copy_truncates_with_nul() {
        let mut buf = [1u16; 4];
        super::copy(&mut buf, "abcdef");
        assert_eq!(buf, [b'a' as u16, b'b' as u16, b'c' as u16, 0]);
    }
}
