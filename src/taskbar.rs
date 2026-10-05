//! Кнопка окна в панели задач Windows. Значок из WM_SETICON панель задач Windows 11 на кнопке не показывает —
//! там значок приложения. Поэтому:
//! - точка состояния — значок поверх кнопки (`ITaskbarList3::SetOverlayIcon`);
//! - цвет режима — свой AppUserModelID окна для каждого режима со своей иконкой перезапуска (.ico в
//!   `%LOCALAPPDATA%\AmneziaWG UI Dark\icons`): у кнопки с таким ID панель задач берёт эту иконку.
//!
//! Всё, кроме чистых функций, зовётся только из потока окна (COM, однопоточный апартамент).

use std::cell::{Cell, RefCell};
use std::mem::ManuallyDrop;
use std::path::PathBuf;

use windows::core::{HSTRING, PCWSTR, PROPVARIANT};
use windows::Win32::Foundation::{HWND, RPC_E_CHANGED_MODE};
use windows::Win32::Storage::EnhancedStorage::{
    PKEY_AppUserModel_ID, PKEY_AppUserModel_RelaunchCommand, PKEY_AppUserModel_RelaunchDisplayNameResource,
    PKEY_AppUserModel_RelaunchIconResource,
};
use windows::Win32::System::Com::StructuredStorage::{PropVariantChangeType, PVCHF_DEFAULT};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
use windows::Win32::System::Variant::VT_LPWSTR;
use windows::Win32::UI::Shell::PropertiesSystem::{IPropertyStore, SHGetPropertyStoreForWindow};
use windows::Win32::UI::Shell::{ITaskbarList3, TaskbarList};
use windows::Win32::UI::WindowsAndMessaging::HICON;

use crate::icon;

/// Размеры картинок в .ico режима.
const ICO_SIZES: [u32; 6] = [16, 24, 32, 48, 64, 256];

/// AppUserModelID режима 1. Единственный источник: его же пишет в ярлыки `shortcut.rs`, чтобы окно режима 1
/// группировалось с закреплённым ярлыком в одну кнопку.
pub const MODE1_APP_ID: &str = "AmneziaWG.UIDark";
/// AppUserModelID режима 2 (жёлтая кнопка). Ярлыки его не получают — это намеренно, см. `shortcut.rs`.
const MODE2_APP_ID: &str = "AmneziaWG.UIDark.Engine";

/// AppUserModelID окна по режиму: у режимов разные ID — разные кнопки со своими иконками.
pub fn app_id(engine: bool) -> &'static str {
    if engine {
        MODE2_APP_ID
    } else {
        MODE1_APP_ID
    }
}

thread_local! {
    /// COM в потоке окна: `None` — ещё не инициализирован, иначе — удалось ли.
    static COM: Cell<Option<bool>> = const { Cell::new(None) };
    /// Объект панели задач. ManuallyDrop: при выходе процесса не звать Release в уже разбираемый COM.
    static LIST: RefCell<Option<ManuallyDrop<ITaskbarList3>>> = const { RefCell::new(None) };
}

/// COM в этом потоке (один раз). Поток уже в другом апартаменте (RPC_E_CHANGED_MODE) — COM всё равно работает.
fn com_ready() -> bool {
    COM.with(|c| {
        if c.get().is_none() {
            let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
            // S_OK и S_FALSE (уже инициализирован) — успех.
            c.set(Some(hr.is_ok() || hr == RPC_E_CHANGED_MODE));
        }
        c.get() == Some(true)
    })
}

/// Кнопка окна появилась (или пересоздана, например после перезапуска Explorer) — новый объект панели задач.
pub fn button_created() -> Result<(), String> {
    if !com_ready() {
        return Err("COM".into());
    }
    let list: ITaskbarList3 = unsafe { CoCreateInstance(&TaskbarList, None, CLSCTX_INPROC_SERVER) }.map_err(|e| e.to_string())?;
    unsafe { list.HrInit() }.map_err(|e| e.to_string())?;
    replace_list(Some(list));
    Ok(())
}

/// Окно закрывается — отпустить объект панели задач.
pub fn release() {
    replace_list(None);
}

fn replace_list(new: Option<ITaskbarList3>) {
    if let Some(mut old) = LIST.with(|l| l.replace(new.map(ManuallyDrop::new))) {
        unsafe { ManuallyDrop::drop(&mut old) };
    }
}

/// Значок поверх кнопки окна; `icon == 0` — убрать. `description` — для экранного диктора.
/// Пока кнопки нет (`button_created` не звали) — ничего не делает: после её появления вызов повторят.
pub fn set_overlay(hwnd: isize, icon: isize, description: &str) -> Result<(), String> {
    // Копия (AddRef), а не заём: на время вызова COM поток может принять сообщения окна и снова сюда зайти.
    let Some(list) = LIST.with(|l| l.borrow().as_deref().cloned()) else { return Ok(()) };
    let text = HSTRING::from(description);
    let (icon, text) = if icon == 0 { (HICON::default(), PCWSTR::null()) } else { (HICON(icon as _), PCWSTR(text.as_ptr())) };
    unsafe { list.SetOverlayIcon(HWND(hwnd as _), icon, text) }.map_err(|e| e.to_string())
}

/// Отметить окно как приложение режима: ID и иконка перезапуска. Тот же ID уже стоит — ничего не менять
/// (смена ID пересоздаёт кнопку, и снова пришло бы «кнопка создана»).
pub fn set_identity(hwnd: isize, engine: bool) -> Result<(), String> {
    if !com_ready() {
        return Err("COM".into());
    }
    let icon = ensure_icon(engine)?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let err = |e: windows::core::Error| e.to_string();
    unsafe {
        let store: IPropertyStore = SHGetPropertyStoreForWindow(HWND(hwnd as _)).map_err(err)?;
        if store.GetValue(&PKEY_AppUserModel_ID).map(|v| v.to_string()).ok().as_deref() == Some(app_id(engine)) {
            return Ok(());
        }
        // Иконку перезапуска панель задач учитывает, только если заданы и команда, и имя; ID — последним,
        // чтобы кнопка нового ID сразу получила свою иконку.
        let values = [
            (PKEY_AppUserModel_RelaunchCommand, format!("\"{}\"", exe.display())),
            (PKEY_AppUserModel_RelaunchDisplayNameResource, crate::APP_TITLE.to_string()),
            (PKEY_AppUserModel_RelaunchIconResource, format!("{},0", icon.display())),
            (PKEY_AppUserModel_ID, app_id(engine).to_string()),
        ];
        for (key, value) in &values {
            store.SetValue(key, &string_value(value).map_err(err)?).map_err(err)?;
        }
        store.Commit().map_err(err)
    }
}

/// Снять свойства окна перед его уничтожением (так требует SHGetPropertyStoreForWindow).
pub fn clear_identity(hwnd: isize) -> Result<(), String> {
    if !com_ready() {
        return Err("COM".into());
    }
    let err = |e: windows::core::Error| e.to_string();
    unsafe {
        let store: IPropertyStore = SHGetPropertyStoreForWindow(HWND(hwnd as _)).map_err(err)?;
        let empty = PROPVARIANT::default();
        for key in [
            PKEY_AppUserModel_ID,
            PKEY_AppUserModel_RelaunchCommand,
            PKEY_AppUserModel_RelaunchDisplayNameResource,
            PKEY_AppUserModel_RelaunchIconResource,
        ] {
            store.SetValue(&key, &empty).map_err(err)?;
        }
        store.Commit().map_err(err)
    }
}

/// Строка как PROPVARIANT типа VT_LPWSTR — этот тип ждут свойства AppUserModel.
pub(crate) fn string_value(s: &str) -> windows::core::Result<PROPVARIANT> {
    let bstr = PROPVARIANT::from(s);
    let mut value = PROPVARIANT::default();
    unsafe { PropVariantChangeType(&mut value, &bstr, PVCHF_DEFAULT, VT_LPWSTR)? };
    Ok(value)
}

/// .ico режима в `%LOCALAPPDATA%\AmneziaWG UI Dark\icons`; файл переписывается, только если содержимое другое.
fn ensure_icon(engine: bool) -> Result<PathBuf, String> {
    use windows_sys::Win32::UI::Shell::FOLDERID_LocalAppData;
    let dir = crate::win::known_folder(&FOLDERID_LocalAppData).ok_or("LocalAppData")?.join(crate::APP_TITLE).join("icons");
    let path = dir.join(if engine { "mode2.ico" } else { "mode1.ico" });
    let data = ico(&ICO_SIZES.map(|s| (s, png(s, &icon::themed(s, engine)))));
    // Не прочитался — файл просто пишется заново, и сбой записи вернётся ошибкой ниже.
    if std::fs::read(&path).ok().as_deref() != Some(data.as_slice()) {
        std::fs::create_dir_all(&dir).map_err(|e| crate::fsutil::io_ctx(&dir, e))?;
        std::fs::write(&path, &data).map_err(|e| crate::fsutil::io_ctx(&path, e))?;
    }
    Ok(path)
}

/// PNG из RGBA `size`×`size`.
fn png(size: u32, rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, size, size);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("заголовок PNG");
        writer.write_image_data(rgba).expect("данные PNG");
    }
    out
}

/// Файл .ico из картинок PNG (сторона, данные): заголовок, каталог по 16 байт на картинку, затем сами PNG.
fn ico(images: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes()); // резерв
    out.extend_from_slice(&1u16.to_le_bytes()); // тип: иконка
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len();
    for (size, data) in images {
        let side = if *size >= 256 { 0 } else { *size as u8 }; // 0 означает 256
        out.extend_from_slice(&[side, side, 0, 0]); // ширина, высота, палитра, резерв
        out.extend_from_slice(&1u16.to_le_bytes()); // плоскости
        out.extend_from_slice(&32u16.to_le_bytes()); // бит на пиксель
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(offset as u32).to_le_bytes());
        offset += data.len();
    }
    for (_, data) in images {
        out.extend_from_slice(data);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u16_at(b: &[u8], i: usize) -> u16 {
        u16::from_le_bytes([b[i], b[i + 1]])
    }

    fn u32_at(b: &[u8], i: usize) -> u32 {
        u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
    }

    #[test]
    fn mode_maps_to_app_id() {
        assert_eq!(app_id(false), "AmneziaWG.UIDark");
        assert_eq!(app_id(true), "AmneziaWG.UIDark.Engine");
    }

    #[test]
    fn ico_has_valid_header_and_directory() {
        let images = ICO_SIZES.map(|s| (s, png(s, &icon::themed(s, true))));
        let file = ico(&images);
        assert_eq!((u16_at(&file, 0), u16_at(&file, 2), u16_at(&file, 4)), (0, 1, ICO_SIZES.len() as u16), "заголовок");
        let mut expected_offset = 6 + 16 * ICO_SIZES.len();
        for (i, (size, data)) in images.iter().enumerate() {
            let e = 6 + 16 * i;
            let side = if *size == 256 { 0 } else { *size as u8 };
            assert_eq!(&file[e..e + 4], &[side, side, 0, 0], "{size}: стороны");
            assert_eq!((u16_at(&file, e + 4), u16_at(&file, e + 6)), (1, 32), "{size}: плоскости и биты");
            let (len, offset) = (u32_at(&file, e + 8) as usize, u32_at(&file, e + 12) as usize);
            assert_eq!((len, offset), (data.len(), expected_offset), "{size}: длина и смещение");
            let entry = &file[offset..offset + len];
            assert_eq!(&entry[..8], b"\x89PNG\r\n\x1a\n", "{size}: внутри PNG");
            // IHDR: ширина и высота PNG совпадают с каталогом.
            assert_eq!((u32::from_be_bytes(entry[16..20].try_into().unwrap()), u32::from_be_bytes(entry[20..24].try_into().unwrap())), (*size, *size));
            expected_offset += len;
        }
        assert_eq!(file.len(), expected_offset, "после последней картинки ничего нет");
    }

    #[test]
    fn png_round_trips() {
        let rgba = icon::themed(24, true);
        let data = png(24, &rgba);
        let mut reader = png::Decoder::new(data.as_slice()).read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size()];
        reader.next_frame(&mut buf).unwrap();
        assert_eq!(buf, rgba);
    }

    #[test]
    fn string_value_is_lpwstr() {
        let v = string_value("AmneziaWG.UIDark.Engine").unwrap();
        assert_eq!(unsafe { v.as_raw().Anonymous.Anonymous.vt }, VT_LPWSTR.0);
        assert_eq!(v.to_string(), "AmneziaWG.UIDark.Engine");
    }
}
