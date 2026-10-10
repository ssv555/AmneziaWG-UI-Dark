//! Управление родным окном AmneziaWG через UI Automation Windows: выделить туннель и нажать его кнопку
//! «Edit» или «Add Tunnel» (импорт из файла). Правит, сохраняет и шифрует конфиги сам родной клиент.

use std::path::Path;
use std::time::{Duration, Instant};

use windows::core::BSTR;
use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationInvokePattern, IUIAutomationSelectionItemPattern,
    TreeScope_Descendants, UIA_ButtonControlTypeId, UIA_ControlTypePropertyId, UIA_InvokePatternId,
    UIA_NamePropertyId, UIA_SelectionItemPatternId, UIA_SplitButtonControlTypeId,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible, SetForegroundWindow, ShowWindow, SW_RESTORE,
};

use crate::i18n::trf;

const WINDOW_TIMEOUT: Duration = Duration::from_secs(10);
/// Подписи кнопок родного клиента (английская и русская сборки).
const EDIT_NAMES: &[&str] = &["Edit", "Редактировать"];
const ADD_NAMES: &[&str] = &["Add Tunnel", "Добавить туннель"];
const REMOVE_NAMES: &[&str] = &["Remove selected tunnel(s)", "Удалить выбранные туннели"];
const EXPORT_NAMES: &[&str] = &["Export all tunnels to zip", "Экспорт всех туннелей в zip-архив"];

/// Открыть в родном окне редактор туннеля.
pub fn edit(exe: &Path, tunnel: &str) -> Result<(), String> {
    with_window(exe, |uia, root, _| {
        let item = find_by_name(uia, root, tunnel)?.ok_or_else(|| trf("err.native_item", &[tunnel]))?;
        unsafe {
            let sel: IUIAutomationSelectionItemPattern =
                item.GetCurrentPatternAs(UIA_SelectionItemPatternId).map_err(|e| e.to_string())?;
            sel.Select().map_err(|e| e.to_string())?;
        }
        std::thread::sleep(Duration::from_millis(200));
        press(uia, root, EDIT_NAMES)
    })
}

/// Открыть в родном окне импорт туннеля из файла (основное действие кнопки «Add Tunnel»).
/// С `file` — перейти в его папку и выделить его; «Открыть» пользователь нажимает сам.
pub fn import(exe: &Path, file: Option<&Path>) -> Result<(), String> {
    with_window(exe, |uia, root, hwnd| {
        press(uia, root, ADD_NAMES)?;
        match file {
            Some(file) => preselect(uia, hwnd, file),
            None => Ok(()),
        }
    })
}

/// Удалить туннель в родном клиенте: выделить, «Remove selected tunnel(s)», в его подтверждении — «Yes».
/// Вызывать только после подтверждения пользователя в awg-ui.
pub fn delete(exe: &Path, tunnel: &str) -> Result<(), String> {
    with_window(exe, |uia, root, hwnd| {
        select(uia, root, tunnel)?;
        press(uia, root, REMOVE_NAMES)?;
        let dialog = wait_dialog(hwnd).ok_or_else(|| trf("err.native_dialog", &[]))?;
        unsafe {
            let el = uia.ElementFromHandle(dialog).map_err(|e| e.to_string())?;
            // Подтверждение родного клиента — стандартный MessageBox: «Yes» = IDYES (6).
            press_id(uia, &el, "6")
        }
    })
}

/// Сведения о туннеле из панели родного окна (открытые данные: ключи там только открытые).
pub fn read_details(exe: &Path, tunnel: &str) -> Result<crate::conf::TunnelInfo, String> {
    use windows::Win32::UI::Accessibility::{IUIAutomationValuePattern, UIA_ValuePatternId};
    let mut fields: Vec<(String, String)> = Vec::new();
    with_window(exe, |uia, root, _| unsafe {
        select(uia, root, tunnel)?;
        std::thread::sleep(Duration::from_millis(400));
        let cond = uia.CreateTrueCondition().map_err(|e| e.to_string())?;
        let all = root.FindAll(TreeScope_Descendants, &cond).map_err(|e| e.to_string())?;
        // Подпись «Имя:» (Static) и следующее за ней поле Edit со значением.
        let mut label: Option<String> = None;
        for i in 0..all.Length().map_err(|e| e.to_string())? {
            let Ok(e) = all.GetElement(i) else { continue };
            let class = e.CurrentClassName().map(|b| b.to_string()).unwrap_or_default();
            let name = e.CurrentName().map(|b| b.to_string()).unwrap_or_default();
            if class == "Static" && name.ends_with(':') {
                label = Some(name.trim_end_matches(':').to_string());
            } else if class == "Edit" {
                if let Some(l) = label.take() {
                    let value = e
                        .GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
                        .and_then(|v| v.CurrentValue())
                        .map(|b| b.to_string())
                        .unwrap_or_default();
                    fields.push((l, value));
                }
            }
        }
        Ok(())
    })?;
    Ok(details_from_fields(&fields))
}

/// Пары «подпись — значение» из панели родного окна → сведения о туннеле.
/// Повторная «Public key» начинает пира (первая — ключ интерфейса).
fn details_from_fields(fields: &[(String, String)]) -> crate::conf::TunnelInfo {
    use crate::conf::{PeerInfo, TunnelInfo};
    let list = |v: &str| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect::<Vec<_>>();
    let mut info = TunnelInfo::default();
    let mut in_peer = false;
    for (label, value) in fields {
        match (label.as_str(), in_peer) {
            ("Public key" | "Открытый ключ", false) if info.public_key.is_empty() => info.public_key = value.clone(),
            ("Public key" | "Открытый ключ", _) => {
                in_peer = true;
                info.peers.push(PeerInfo { public_key: value.clone(), ..Default::default() });
            }
            ("Listen port", false) => info.listen_port = value.clone(),
            ("MTU", false) => info.mtu = value.clone(),
            ("Addresses", false) => info.addresses = list(value),
            ("DNS servers", false) => info.dns = list(value),
            (_, true) => {
                let Some(p) = info.peers.last_mut() else { continue };
                match label.as_str() {
                    "Preshared key" => p.preshared = value == "enabled",
                    "Allowed IPs" => p.allowed_ips = list(value),
                    "Endpoint" => p.endpoint = value.clone(),
                    "Persistent keepalive" => p.keepalive = value.clone(),
                    _ => {}
                }
            }
            _ => {}
        }
    }
    info
}

/// Выделить туннель в списке родного окна.
fn select(uia: &IUIAutomation, root: &IUIAutomationElement, tunnel: &str) -> Result<(), String> {
    let item = find_by_name(uia, root, tunnel)?.ok_or_else(|| trf("err.native_item", &[tunnel]))?;
    unsafe {
        let sel: IUIAutomationSelectionItemPattern =
            item.GetCurrentPatternAs(UIA_SelectionItemPatternId).map_err(|e| e.to_string())?;
        sel.Select().map_err(|e| e.to_string())?;
    }
    std::thread::sleep(Duration::from_millis(200));
    Ok(())
}

fn with_window(
    exe: &Path,
    action: impl FnOnce(&IUIAutomation, &IUIAutomationElement, HWND) -> Result<(), String>,
) -> Result<(), String> {
    let hwnd = window(exe)?;
    unsafe {
        let _ = ShowWindow(hwnd, SW_RESTORE);
        let _ = SetForegroundWindow(hwnd);
        // Повторный вызов в том же потоке вернёт S_FALSE — это не ошибка.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let uia: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).map_err(|e| e.to_string())?;
        let root = uia.ElementFromHandle(hwnd).map_err(|e| e.to_string())?;
        // Главное окно заблокировано своим диалогом — назвать его, а не отдавать код UIA.
        if let Some(title) = blocking_dialog(hwnd) {
            return Err(trf("err.native_busy", &[&title]));
        }
        action(&uia, &root, hwnd).map_err(|e| match blocking_dialog(hwnd) {
            Some(title) => trf("err.native_busy", &[&title]),
            None => e,
        })
    }
}

/// Видимое окно родного процесса поверх главного (его диалог) — заголовок; None — главное свободно.
fn blocking_dialog(main: HWND) -> Option<String> {
    use windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled;
    struct Search {
        pid: u32,
        main: HWND,
        title: Option<String>,
    }
    unsafe extern "system" fn visit(h: HWND, data: LPARAM) -> windows::core::BOOL {
        let s = &mut *(data.0 as *mut Search);
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, Some(&mut pid));
        if pid == s.pid && h != s.main && IsWindowVisible(h).as_bool() {
            let mut buf = [0u16; 256];
            let n = GetWindowTextW(h, &mut buf) as usize;
            if n > 0 {
                s.title = Some(String::from_utf16_lossy(&buf[..n]));
                return false.into();
            }
        }
        true.into()
    }
    unsafe {
        if IsWindowEnabled(main).as_bool() {
            return None;
        }
        let mut search = Search { pid: 0, main, title: None };
        GetWindowThreadProcessId(main, Some(&mut search.pid));
        // `Err` здесь — обход остановлен обратным вызовом (нашли окно), а не сбой; результат в `search`.
        let _ = EnumWindows(Some(visit), LPARAM(&mut search as *mut Search as isize));
        Some(search.title.unwrap_or_else(|| "?".into()))
    }
}

/// Окно родного клиента; если не открыто — запустить amneziawg.exe (он попросит менеджер показать окно).
fn window(exe: &Path) -> Result<HWND, String> {
    if let Some(h) = find_window() {
        return Ok(h);
    }
    std::process::Command::new(exe).spawn().map_err(|e| crate::fsutil::io_ctx(exe, e))?;
    let deadline = Instant::now() + WINDOW_TIMEOUT;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(250));
        if let Some(h) = find_window() {
            // Дать окну дорисовать список туннелей.
            std::thread::sleep(Duration::from_millis(500));
            return Ok(h);
        }
    }
    Err(trf("err.native_window", &[]))
}

/// Видимое верхнее окно с заголовком «AmneziaWG» не нашего процесса.
fn find_window() -> Option<HWND> {
    struct Search {
        own_pid: u32,
        found: Option<HWND>,
    }
    unsafe extern "system" fn visit(hwnd: HWND, data: LPARAM) -> windows::core::BOOL {
        let search = &mut *(data.0 as *mut Search);
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid != search.own_pid && IsWindowVisible(hwnd).as_bool() {
            let mut buf = [0u16; 256];
            let n = GetWindowTextW(hwnd, &mut buf) as usize;
            if String::from_utf16_lossy(&buf[..n]) == "AmneziaWG" {
                search.found = Some(hwnd);
                return false.into();
            }
        }
        true.into()
    }
    let mut search = Search { own_pid: std::process::id(), found: None };
    unsafe {
        // `Err` здесь — обход остановлен обратным вызовом (нашли окно), а не сбой; результат в `search`.
        let _ = EnumWindows(Some(visit), LPARAM(&mut search as *mut Search as isize));
    }
    search.found
}

/// Диалог выбора файла родного клиента: перейти в папку `file` и выделить его в списке.
fn preselect(uia: &IUIAutomation, owner: HWND, file: &Path) -> Result<(), String> {
    use windows::Win32::UI::Accessibility::{IUIAutomationScrollItemPattern, UIA_ScrollItemPatternId};
    let dialog = wait_dialog(owner).ok_or_else(|| trf("err.native_dialog", &[]))?;
    let folder = file.parent().map(|p| p.display().to_string()).unwrap_or_default();
    let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let stem = file.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    unsafe {
        let root = uia.ElementFromHandle(dialog).map_err(|e| e.to_string())?;
        // Папка в поле имени + «Открыть» = переход в папку (файл при этом не открывается).
        set_file_name(uia, &root, &folder)?;
        press_dialog_button(uia, &root, "1")?;
        std::thread::sleep(Duration::from_millis(900));
        // Выделить файл в списке (с расширением или без — как показывает Проводник).
        for shown in [&name, &stem] {
            if let Some(item) = find_by_name(uia, &root, shown)? {
                // Прокрутка и выделение — удобство для пользователя; без них файл всё равно вводится в поле имени ниже.
                if let Ok(scroll) = item.GetCurrentPatternAs::<IUIAutomationScrollItemPattern>(UIA_ScrollItemPatternId) {
                    let _ = scroll.ScrollIntoView();
                }
                if let Ok(sel) = item.GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(UIA_SelectionItemPatternId) {
                    let _ = sel.Select();
                }
                break;
            }
        }
        set_file_name(uia, &root, &name)
    }
}

/// Поле «Имя файла» стандартного диалога: Edit с AutomationId 1148 (открытие) или 1001 (сохранение).
/// Диалог достраивается не сразу — поле ждём до 5 с.
unsafe fn set_file_name(uia: &IUIAutomation, root: &IUIAutomationElement, text: &str) -> Result<(), String> {
    use windows::Win32::UI::Accessibility::{IUIAutomationValuePattern, UIA_AutomationIdPropertyId, UIA_EditControlTypeId, UIA_ValuePatternId};
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        for id in ["1148", "1001"] {
            let cond = uia
                .CreateAndCondition(
                    &uia.CreatePropertyCondition(UIA_ControlTypePropertyId, &VARIANT::from(UIA_EditControlTypeId.0)).map_err(|e| e.to_string())?,
                    &uia.CreatePropertyCondition(UIA_AutomationIdPropertyId, &VARIANT::from(BSTR::from(id))).map_err(|e| e.to_string())?,
                )
                .map_err(|e| e.to_string())?;
            if let Ok(edit) = root.FindFirst(TreeScope_Descendants, &cond) {
                let value: IUIAutomationValuePattern = edit.GetCurrentPatternAs(UIA_ValuePatternId).map_err(|e| e.to_string())?;
                return value.SetValue(&BSTR::from(text)).map_err(|e| e.to_string());
            }
        }
        if Instant::now() >= deadline {
            return Err(trf("err.native_dialog", &[]));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Кнопка диалога по AutomationId («1» — «Открыть»/«Сохранить»). Тот же id бывает у строк списка
/// файлов — поэтому ищем именно кнопку.
unsafe fn press_dialog_button(uia: &IUIAutomation, root: &IUIAutomationElement, id: &str) -> Result<(), String> {
    use windows::Win32::UI::Accessibility::UIA_AutomationIdPropertyId;
    let cond = uia
        .CreateAndCondition(
            &uia.CreatePropertyCondition(UIA_ControlTypePropertyId, &VARIANT::from(UIA_ButtonControlTypeId.0)).map_err(|e| e.to_string())?,
            &uia.CreatePropertyCondition(UIA_AutomationIdPropertyId, &VARIANT::from(BSTR::from(id))).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
    let button = root.FindFirst(TreeScope_Descendants, &cond).map_err(|e| e.to_string())?;
    let invoke: IUIAutomationInvokePattern = button.GetCurrentPatternAs(UIA_InvokePatternId).map_err(|e| e.to_string())?;
    invoke.Invoke().map_err(|e| e.to_string())
}

/// Родное «Export all tunnels to zip» в файл `zip` (каталог должен существовать, файла быть не должно).
/// Возвращает, когда архив записан и читается целиком.
pub fn export_all(exe: &Path, zip: &Path) -> Result<Vec<crate::archive::Entry>, String> {
    with_window(exe, |uia, root, hwnd| {
        press(uia, root, EXPORT_NAMES)?;
        let dialog = wait_dialog(hwnd).ok_or_else(|| trf("err.native_dialog", &[]))?;
        unsafe {
            let el = uia.ElementFromHandle(dialog).map_err(|e| e.to_string())?;
            set_file_name(uia, &el, &zip.display().to_string())?;
            press_dialog_button(uia, &el, "1")
        }
    })?;
    // Клиент пишет архив сразу после «Сохранить»; ждём, пока он прочитается целиком.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match crate::archive::read(zip, None) {
            Ok(entries) => return Ok(entries),
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(300)),
            Err(e) => return Err(crate::fsutil::io_ctx(zip, e)),
        }
    }
}

/// Текст конфига туннеля из родного редактора; редактор закрывается кнопкой Cancel, без изменений.
pub fn read_config(exe: &Path, tunnel: &str) -> Result<String, String> {
    with_editor(exe, tunnel, |uia, dialog, value| unsafe {
        let text = value.CurrentValue().map_err(|e| e.to_string())?.to_string();
        press_id(uia, dialog, EDITOR_CANCEL_ID)?;
        // Следующее открытие редактора не должно застать это окно закрывающимся.
        wait_editor_closed(Duration::from_secs(4))?;
        // RichEdit отдаёт строки через одиночный \r — в файле нужен \r\n.
        Ok(text.replace("\r\n", "\n").replace('\r', "\n").replace('\n', "\r\n"))
    })
}

/// Подставить текст в родной редактор туннеля и нажать Save; шифрует и сохраняет сам родной клиент.
/// Редактор остался открытым — клиент не принял конфиг (он сам показывает причину).
pub fn write_config(exe: &Path, tunnel: &str, text: &str) -> Result<(), String> {
    with_editor(exe, tunnel, |uia, dialog, value| unsafe {
        value.SetValue(&BSTR::from(text)).map_err(|e| e.to_string())?;
        press_id(uia, dialog, EDITOR_SAVE_ID)?;
        Ok(())
    })?;
    wait_editor_closed(Duration::from_secs(4)).map_err(|_| trf("err.native_rejected", &[tunnel]))
}

fn wait_editor_closed(timeout: Duration) -> Result<(), String> {
    if crate::fsutil::wait_until(timeout, Duration::from_millis(200), || editor_window().is_none()) {
        return Ok(());
    }
    Err(trf("err.native_dialog", &[]))
}

/// Id кнопок редактора туннеля (одинаковы в любой локализации клиента).
const EDITOR_SAVE_ID: &str = "3";
const EDITOR_CANCEL_ID: &str = "4";
/// Класс окна редактора туннеля (AmneziaWG унаследовал его от WireGuard).
const EDITOR_CLASS: &str = "WireGuard UI - Dialog";

/// Открыть редактор туннеля и дать доступ к его полю «Configuration» (RichEdit, ValuePattern).
fn with_editor<T>(
    exe: &Path,
    tunnel: &str,
    action: impl FnOnce(
        &IUIAutomation,
        &IUIAutomationElement,
        &windows::Win32::UI::Accessibility::IUIAutomationValuePattern,
    ) -> Result<T, String>,
) -> Result<T, String> {
    use windows::Win32::UI::Accessibility::{IUIAutomationValuePattern, UIA_DocumentControlTypeId, UIA_ValuePatternId};
    edit(exe, tunnel)?;
    let deadline = Instant::now() + WINDOW_TIMEOUT;
    let hwnd = loop {
        if let Some(h) = editor_window() {
            break h;
        }
        if Instant::now() > deadline {
            return Err(trf("err.native_dialog", &[]));
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let uia: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).map_err(|e| e.to_string())?;
        let dialog = uia.ElementFromHandle(hwnd).map_err(|e| e.to_string())?;
        let cond = uia
            .CreatePropertyCondition(UIA_ControlTypePropertyId, &VARIANT::from(UIA_DocumentControlTypeId.0))
            .map_err(|e| e.to_string())?;
        let doc = dialog.FindFirst(TreeScope_Descendants, &cond).map_err(|_| trf("err.native_dialog", &[]))?;
        let value: IUIAutomationValuePattern = doc.GetCurrentPatternAs(UIA_ValuePatternId).map_err(|e| e.to_string())?;
        action(&uia, &dialog, &value)
    }
}

/// Видимое окно редактора туннеля родного клиента.
fn editor_window() -> Option<HWND> {
    use windows::Win32::UI::WindowsAndMessaging::GetClassNameW;
    struct Search {
        found: Option<HWND>,
    }
    unsafe extern "system" fn visit(h: HWND, data: LPARAM) -> windows::core::BOOL {
        let search = &mut *(data.0 as *mut Search);
        if IsWindowVisible(h).as_bool() {
            let mut cls = [0u16; 64];
            let n = GetClassNameW(h, &mut cls) as usize;
            if String::from_utf16_lossy(&cls[..n]) == EDITOR_CLASS {
                search.found = Some(h);
                return false.into();
            }
        }
        true.into()
    }
    let mut search = Search { found: None };
    unsafe {
        // `Err` здесь — обход остановлен обратным вызовом (нашли окно), а не сбой; результат в `search`.
        let _ = EnumWindows(Some(visit), LPARAM(&mut search as *mut Search as isize));
    }
    search.found
}

/// Нажать кнопку по AutomationId.
fn press_id(uia: &IUIAutomation, root: &IUIAutomationElement, id: &str) -> Result<(), String> {
    use windows::Win32::UI::Accessibility::UIA_AutomationIdPropertyId;
    unsafe {
        let cond = uia
            .CreateAndCondition(
                &uia.CreatePropertyCondition(UIA_ControlTypePropertyId, &VARIANT::from(UIA_ButtonControlTypeId.0)).map_err(|e| e.to_string())?,
                &uia.CreatePropertyCondition(UIA_AutomationIdPropertyId, &VARIANT::from(BSTR::from(id))).map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;
        let button = root.FindFirst(TreeScope_Descendants, &cond).map_err(|_| trf("err.native_button", &[id]))?;
        let invoke: IUIAutomationInvokePattern = button.GetCurrentPatternAs(UIA_InvokePatternId).map_err(|e| e.to_string())?;
        invoke.Invoke().map_err(|e| e.to_string())
    }
}

/// Диалог «Открыть» (#32770) процесса родного клиента.
fn wait_dialog(owner: HWND) -> Option<HWND> {
    use windows::Win32::UI::WindowsAndMessaging::GetClassNameW;
    struct Search {
        pid: u32,
        found: Option<HWND>,
    }
    unsafe extern "system" fn visit(hwnd: HWND, data: LPARAM) -> windows::core::BOOL {
        let search = &mut *(data.0 as *mut Search);
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == search.pid && IsWindowVisible(hwnd).as_bool() {
            let mut buf = [0u16; 64];
            let n = GetClassNameW(hwnd, &mut buf) as usize;
            if String::from_utf16_lossy(&buf[..n]) == "#32770" {
                search.found = Some(hwnd);
                return false.into();
            }
        }
        true.into()
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(owner, Some(&mut pid)) };
    let deadline = Instant::now() + WINDOW_TIMEOUT;
    while Instant::now() < deadline {
        let mut search = Search { pid, found: None };
        unsafe {
            // `Err` здесь — обход остановлен обратным вызовом (нашли окно), а не сбой; результат в `search`.
            let _ = EnumWindows(Some(visit), LPARAM(&mut search as *mut Search as isize));
        }
        if search.found.is_some() {
            std::thread::sleep(Duration::from_millis(400));
            return search.found;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    None
}

fn find_by_name(uia: &IUIAutomation, root: &IUIAutomationElement, name: &str) -> Result<Option<IUIAutomationElement>, String> {
    unsafe {
        let cond = uia
            .CreatePropertyCondition(UIA_NamePropertyId, &VARIANT::from(BSTR::from(name)))
            .map_err(|e| e.to_string())?;
        // `FindFirst` отдаёт `Err`, когда элемента нет: для поиска это «не найден», а не сбой.
        Ok(root.FindFirst(TreeScope_Descendants, &cond).ok())
    }
}

/// Нажать кнопку с одной из подписей (без учёта `&` — подчёркивания горячей клавиши).
fn press(uia: &IUIAutomation, root: &IUIAutomationElement, names: &[&str]) -> Result<(), String> {
    unsafe {
        for control in [UIA_ButtonControlTypeId, UIA_SplitButtonControlTypeId] {
            let cond = uia
                .CreatePropertyCondition(UIA_ControlTypePropertyId, &VARIANT::from(control.0))
                .map_err(|e| e.to_string())?;
            let all = root.FindAll(TreeScope_Descendants, &cond).map_err(|e| e.to_string())?;
            for i in 0..all.Length().map_err(|e| e.to_string())? {
                let el = all.GetElement(i).map_err(|e| e.to_string())?;
                let name = el.CurrentName().map(|b| b.to_string()).unwrap_or_default().replace('&', "");
                if names.iter().any(|n| name.trim() == *n) {
                    let invoke: IUIAutomationInvokePattern =
                        el.GetCurrentPatternAs(UIA_InvokePatternId).map_err(|e| e.to_string())?;
                    return invoke.Invoke().map_err(|e| e.to_string());
                }
            }
        }
    }
    Err(trf("err.native_button", &[names[0]]))
}

#[cfg(test)]
mod tests {
    use super::details_from_fields;

    #[test]
    fn fields_split_interface_and_peer() {
        let f = |a: &str, b: &str| (a.to_string(), b.to_string());
        let fields = vec![
            f("Status", "Inactive"),
            f("Public key", "IFACE="),
            f("Listen port", "51820"),
            f("MTU", "1280"),
            f("Addresses", "10.0.0.2/32, fd00::2/128"),
            f("DNS servers", "1.1.1.1"),
            f("Public key", "PEER="),
            f("Preshared key", "enabled"),
            f("Allowed IPs", "0.0.0.0/0, ::/0"),
            f("Endpoint", "203.0.113.5:51820"),
            f("Persistent keepalive", "25"),
        ];
        let info = details_from_fields(&fields);
        assert_eq!((info.public_key.as_str(), info.listen_port.as_str(), info.mtu.as_str()), ("IFACE=", "51820", "1280"));
        assert_eq!(info.addresses.len(), 2);
        assert_eq!(info.peers.len(), 1);
        assert!(info.peers[0].preshared);
        assert_eq!(info.peers[0].allowed_ips, vec!["0.0.0.0/0", "::/0"]);
    }
}
