//! Ярлык программы на рабочем столе пользователя (IShellLink).
//!
//! В ярлык пишется AppUserModelID режима 1 (`taskbar::MODE1_APP_ID`): так закреплённый ярлык и запущенное
//! окно режима 1 — одна кнопка в панели задач. Окно режима 2 имеет свой ID (жёлтая кнопка), поэтому в
//! режиме 2 оно — отдельная кнопка рядом с закреплённым ярлыком; так задумано.

use std::path::PathBuf;

use windows::core::{Interface, HSTRING};
use windows::Win32::Storage::EnhancedStorage::PKEY_AppUserModel_ID;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, IPersistFile, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
    STGM_READWRITE,
};
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
use windows::Win32::UI::Shell::{FOLDERID_Desktop, IShellLinkW, SHGetKnownFolderPath, ShellLink, KF_FLAG_DEFAULT};

use crate::taskbar::{string_value, MODE1_APP_ID};

/// Нужно ли переписывать AppUserModelID ярлыка: не задан (или пуст) либо отличается от нужного.
fn app_id_needs_update(current: Option<&str>, wanted: &str) -> bool {
    current.filter(|c| !c.is_empty()) != Some(wanted)
}

/// `<рабочий стол>\<name>.lnk`.
fn desktop_link_path(name: &str) -> Result<PathBuf, String> {
    unsafe {
        let desktop = SHGetKnownFolderPath(&FOLDERID_Desktop, KF_FLAG_DEFAULT, None).map_err(|e| e.to_string())?;
        let desktop_dir = desktop.to_string().map_err(|e| e.to_string());
        CoTaskMemFree(Some(desktop.0 as *const _));
        Ok(PathBuf::from(desktop_dir?).join(format!("{name}.lnk")))
    }
}

/// Записать в ярлык AppUserModelID режима 1 (сохранение файла — на вызывающем).
fn set_app_id(link: &IShellLinkW) -> windows::core::Result<()> {
    let store: IPropertyStore = link.cast()?;
    unsafe {
        store.SetValue(&PKEY_AppUserModel_ID, &string_value(MODE1_APP_ID)?)?;
        store.Commit()
    }
}

/// Создать (или перезаписать) `<name>.lnk` на рабочем столе: цель — этот exe, иконка — из него же.
pub fn create_on_desktop(name: &str, description: &str) -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = exe.parent().map(PathBuf::from).unwrap_or_default();
    let err = |e: windows::core::Error| e.to_string();
    unsafe {
        // Главный поток уже в STA (winit/OLE); повторная инициализация безвредна.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let path = desktop_link_path(name)?;

        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).map_err(err)?;
        link.SetPath(&HSTRING::from(exe.as_os_str())).map_err(err)?;
        link.SetWorkingDirectory(&HSTRING::from(dir.as_os_str())).map_err(err)?;
        link.SetIconLocation(&HSTRING::from(exe.as_os_str()), 0).map_err(err)?;
        link.SetDescription(&HSTRING::from(description)).map_err(err)?;
        set_app_id(&link).map_err(err)?;
        let file: IPersistFile = link.cast().map_err(err)?;
        file.Save(&HSTRING::from(path.as_os_str()), true).map_err(err)?;
        Ok(path)
    }
}

/// Ярлык `<name>.lnk`, созданный раньше этой правкой (без AppUserModelID), или с чужим ID: поправить ID.
/// `Ok(true)` — файл переписан, `Ok(false)` — ярлыка нет или ID уже верный (файл не трогается).
pub fn repair_desktop_app_id(name: &str) -> Result<bool, String> {
    let err = |e: windows::core::Error| e.to_string();
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let path = desktop_link_path(name)?;
        if !path.is_file() {
            return Ok(false);
        }
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).map_err(err)?;
        let file: IPersistFile = link.cast().map_err(err)?;
        let wide = HSTRING::from(path.as_os_str());
        file.Load(&wide, STGM_READWRITE).map_err(|e| crate::fsutil::io_ctx(&path, e))?;
        let store: IPropertyStore = link.cast().map_err(err)?;
        let current = store.GetValue(&PKEY_AppUserModel_ID).map(|v| v.to_string()).ok();
        if !app_id_needs_update(current.as_deref(), MODE1_APP_ID) {
            return Ok(false);
        }
        set_app_id(&link).map_err(err)?;
        file.Save(&wide, true).map_err(|e| crate::fsutil::io_ctx(&path, e))?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_or_empty_id_needs_update() {
        assert!(app_id_needs_update(None, MODE1_APP_ID));
        assert!(app_id_needs_update(Some(""), MODE1_APP_ID));
    }

    #[test]
    fn different_id_needs_update() {
        assert!(app_id_needs_update(Some("AmneziaWG.UIDark.Engine"), MODE1_APP_ID));
        assert!(app_id_needs_update(Some("amneziawg.uidark"), MODE1_APP_ID), "ID сравнивается точно");
    }

    #[test]
    fn matching_id_is_left_alone() {
        assert!(!app_id_needs_update(Some(MODE1_APP_ID), MODE1_APP_ID));
    }

    #[test]
    fn shortcut_id_is_mode1_window_id() {
        assert_eq!(MODE1_APP_ID, crate::taskbar::app_id(false));
        assert_ne!(MODE1_APP_ID, crate::taskbar::app_id(true), "режим 2 — отдельная кнопка");
    }
}
