//! Ярлык программы на рабочем столе пользователя (IShellLink).

use std::path::PathBuf;

use windows::core::{Interface, HSTRING};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, IPersistFile, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Shell::{FOLDERID_Desktop, IShellLinkW, SHGetKnownFolderPath, ShellLink, KF_FLAG_DEFAULT};

/// Создать (или перезаписать) `<name>.lnk` на рабочем столе: цель — этот exe, иконка — из него же.
pub fn create_on_desktop(name: &str, description: &str) -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = exe.parent().map(PathBuf::from).unwrap_or_default();
    let err = |e: windows::core::Error| e.to_string();
    unsafe {
        // Главный поток уже в STA (winit/OLE); повторная инициализация безвредна.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let desktop = SHGetKnownFolderPath(&FOLDERID_Desktop, KF_FLAG_DEFAULT, None).map_err(err)?;
        let desktop_dir = desktop.to_string().map_err(|e| e.to_string());
        CoTaskMemFree(Some(desktop.0 as *const _));
        let path = PathBuf::from(desktop_dir?).join(format!("{name}.lnk"));

        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).map_err(err)?;
        link.SetPath(&HSTRING::from(exe.as_os_str())).map_err(err)?;
        link.SetWorkingDirectory(&HSTRING::from(dir.as_os_str())).map_err(err)?;
        link.SetIconLocation(&HSTRING::from(exe.as_os_str()), 0).map_err(err)?;
        link.SetDescription(&HSTRING::from(description)).map_err(err)?;
        let file: IPersistFile = link.cast().map_err(err)?;
        file.Save(&HSTRING::from(path.as_os_str()), true).map_err(err)?;
        Ok(path)
    }
}
