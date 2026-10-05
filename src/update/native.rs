//! Оригинальный AmneziaWG: установленная версия (MSI), проверка подписи установщика, установка и удаление через
//! msiexec, резервная копия и возврат папки туннелей (зашифрованные файлы копируются как есть, не расшифровываются).

use std::ffi::c_void;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};

use crate::win::wide;

/// UpgradeCode MSI AmneziaWG для amd64 (общий для всех версий), верхний регистр, в фигурных скобках.
pub const UPGRADE_CODE: &str = "{876B57E4-4490-4442-A983-721EE141B00D}";

/// Владелец подписи официального MSI (CN сертификата подписавшего).
#[cfg_attr(not(test), allow(dead_code))]
pub const PUBLISHER: &str = "Privacy Technologies OU";

/// Установленный AmneziaWG по данным установщика Windows.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub version: String,
    pub product_code: String,
}

fn from_wide(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// Установленный AmneziaWG: код продукта по UpgradeCode и его версия; `None` — не установлен.
#[cfg_attr(not(test), allow(dead_code))]
pub fn installed() -> Option<Installed> {
    use windows_sys::Win32::System::ApplicationInstallationAndServicing::{
        MsiEnumRelatedProductsW, MsiGetProductInfoW, INSTALLPROPERTY_VERSIONSTRING,
    };
    const ERROR_MORE_DATA: u32 = 234;
    let upgrade = wide(UPGRADE_CODE);
    let mut code = [0u16; 39];
    if unsafe { MsiEnumRelatedProductsW(upgrade.as_ptr(), 0, 0, code.as_mut_ptr()) } != 0 {
        return None;
    }
    let mut buf = vec![0u16; 64];
    loop {
        let mut len = buf.len() as u32;
        let r = unsafe { MsiGetProductInfoW(code.as_ptr(), INSTALLPROPERTY_VERSIONSTRING, buf.as_mut_ptr(), &mut len) };
        match r {
            0 => break,
            ERROR_MORE_DATA => buf = vec![0u16; len as usize + 1],
            _ => return None,
        }
    }
    Some(Installed { version: from_wide(&buf), product_code: from_wide(&code) })
}

/// Имя файла MSI в релизе GitHub.
#[cfg_attr(not(test), allow(dead_code))]
pub fn msi_asset(version: &str) -> String {
    format!("amneziawg-amd64-{version}.msi")
}

/// Описатель MSI (база, представление, запись); закрывается при выходе из области видимости.
struct MsiHandle(u32);

impl Drop for MsiHandle {
    fn drop(&mut self) {
        use windows_sys::Win32::System::ApplicationInstallationAndServicing::MsiCloseHandle;
        if self.0 != 0 {
            unsafe { MsiCloseHandle(self.0) };
        }
    }
}

/// Значение свойства из таблицы Property открытой базы MSI.
fn msi_property(db: &MsiHandle, name: &str) -> Result<String, String> {
    use windows_sys::Win32::System::ApplicationInstallationAndServicing::{
        MsiDatabaseOpenViewW, MsiRecordGetStringW, MsiViewExecute, MsiViewFetch,
    };
    const ERROR_MORE_DATA: u32 = 234;
    const ERROR_NO_MORE_ITEMS: u32 = 259;
    // Имя свойства — внутренняя константа, не пользовательский ввод.
    let query = wide(&format!("SELECT `Value` FROM `Property` WHERE `Property`='{name}'"));
    let mut view = MsiHandle(0);
    let r = unsafe { MsiDatabaseOpenViewW(db.0, query.as_ptr(), &mut view.0) };
    if r != 0 {
        return Err(format!("MSI: таблица Property не открыта (код {r})"));
    }
    let r = unsafe { MsiViewExecute(view.0, 0) };
    if r != 0 {
        return Err(format!("MSI: запрос к Property не выполнен (код {r})"));
    }
    let mut rec = MsiHandle(0);
    match unsafe { MsiViewFetch(view.0, &mut rec.0) } {
        0 => {}
        ERROR_NO_MORE_ITEMS => return Err(format!("MSI: нет свойства {name}")),
        r => return Err(format!("MSI: свойство {name} не прочитано (код {r})")),
    }
    let mut buf = vec![0u16; 64];
    loop {
        let mut len = buf.len() as u32;
        match unsafe { MsiRecordGetStringW(rec.0, 1, buf.as_mut_ptr(), &mut len) } {
            0 => return Ok(from_wide(&buf)),
            ERROR_MORE_DATA => buf = vec![0u16; len as usize + 1],
            r => return Err(format!("MSI: свойство {name} не прочитано (код {r})")),
        }
    }
}

/// UpgradeCode (верхний регистр, в фигурных скобках) и ProductVersion из таблицы Property самого MSI;
/// база открывается только для чтения, ничего не устанавливается.
#[cfg_attr(not(test), allow(dead_code))]
pub fn msi_identity(path: &Path) -> Result<(String, String), String> {
    use windows_sys::Win32::System::ApplicationInstallationAndServicing::{MsiOpenDatabaseW, MSIDBOPEN_READONLY};
    let file = wide(&path.to_string_lossy());
    let mut db = MsiHandle(0);
    let r = unsafe { MsiOpenDatabaseW(file.as_ptr(), MSIDBOPEN_READONLY, &mut db.0) };
    if r != 0 {
        return Err(format!("{}: не открывается как MSI (код {r})", path.display()));
    }
    let code = msi_property(&db, "UpgradeCode")?;
    let version = msi_property(&db, "ProductVersion")?;
    let code = code.trim().trim_start_matches('{').trim_end_matches('}').to_uppercase();
    Ok((format!("{{{code}}}"), version.trim().to_string()))
}

/// Подпись Authenticode установщика: цепочка доверия с проверкой отзыва всей цепочки, затем CN подписавшего
/// должен совпасть с [`PUBLISHER`]. Ok — CN.
#[cfg_attr(not(test), allow(dead_code))]
pub fn verify_msi(path: &Path) -> Result<String, String> {
    win_verify_trust(path)?;
    let cn = signer_cn(path)?;
    if cn != PUBLISHER {
        return Err(format!("MSI подписан «{cn}», ожидался «{PUBLISHER}»"));
    }
    Ok(cn)
}

/// WinVerifyTrust без окон, с проверкой отзыва всей цепочки.
fn win_verify_trust(path: &Path) -> Result<(), String> {
    use windows_sys::Win32::Security::WinTrust::{
        WinVerifyTrust, WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA, WINTRUST_FILE_INFO, WTD_CHOICE_FILE,
        WTD_REVOCATION_CHECK_CHAIN, WTD_REVOKE_WHOLECHAIN, WTD_STATEACTION_CLOSE, WTD_STATEACTION_VERIFY, WTD_UI_NONE,
    };
    const TRUST_E_NOSIGNATURE: i32 = 0x800B_0100_u32 as i32;
    let file = wide(&path.to_string_lossy());
    let mut fi: WINTRUST_FILE_INFO = unsafe { std::mem::zeroed() };
    fi.cbStruct = size_of::<WINTRUST_FILE_INFO>() as u32;
    fi.pcwszFilePath = file.as_ptr();
    let mut wd: WINTRUST_DATA = unsafe { std::mem::zeroed() };
    wd.cbStruct = size_of::<WINTRUST_DATA>() as u32;
    wd.dwUIChoice = WTD_UI_NONE;
    wd.fdwRevocationChecks = WTD_REVOKE_WHOLECHAIN;
    wd.dwUnionChoice = WTD_CHOICE_FILE;
    wd.Anonymous.pFile = &mut fi;
    wd.dwStateAction = WTD_STATEACTION_VERIFY;
    wd.dwProvFlags = WTD_REVOCATION_CHECK_CHAIN;
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    let r = unsafe { WinVerifyTrust(null_mut(), &mut action, (&mut wd as *mut WINTRUST_DATA).cast()) };
    wd.dwStateAction = WTD_STATEACTION_CLOSE;
    unsafe { WinVerifyTrust(null_mut(), &mut action, (&mut wd as *mut WINTRUST_DATA).cast()) };
    match r {
        0 => Ok(()),
        TRUST_E_NOSIGNATURE => Err("MSI не подписан (нет подписи Authenticode)".into()),
        e => Err(format!("подпись MSI не прошла проверку: 0x{:08X}", e as u32)),
    }
}

/// CN сертификата первого подписавшего (простое отображаемое имя).
fn signer_cn(path: &Path) -> Result<String, String> {
    use windows_sys::Win32::Security::Cryptography::{
        CertCloseStore, CertFindCertificateInStore, CertFreeCertificateContext, CertGetNameStringW, CryptMsgClose,
        CryptMsgGetParam, CryptQueryObject, CERT_FIND_SUBJECT_CERT, CERT_INFO, CERT_NAME_SIMPLE_DISPLAY_TYPE,
        CERT_QUERY_CONTENT_FLAG_PKCS7_SIGNED_EMBED, CERT_QUERY_FORMAT_FLAG_BINARY, CERT_QUERY_OBJECT_FILE,
        CMSG_SIGNER_INFO, CMSG_SIGNER_INFO_PARAM, HCERTSTORE, PKCS_7_ASN_ENCODING, X509_ASN_ENCODING,
    };
    let file = wide(&path.to_string_lossy());
    let (mut enc, mut ctype, mut ftype) = (0, 0, 0);
    let mut store: HCERTSTORE = null_mut();
    let mut msg: *mut c_void = null_mut();
    let ok = unsafe {
        CryptQueryObject(
            CERT_QUERY_OBJECT_FILE,
            file.as_ptr().cast(),
            CERT_QUERY_CONTENT_FLAG_PKCS7_SIGNED_EMBED,
            CERT_QUERY_FORMAT_FLAG_BINARY,
            0,
            &mut enc,
            &mut ctype,
            &mut ftype,
            &mut store,
            &mut msg,
            null_mut(),
        )
    };
    if ok == 0 {
        return Err(format!("подпись MSI не прочитана: {}", std::io::Error::last_os_error()));
    }
    let result = (|| unsafe {
        let mut size = 0u32;
        if CryptMsgGetParam(msg, CMSG_SIGNER_INFO_PARAM, 0, null_mut(), &mut size) == 0 {
            return Err(format!("нет данных подписавшего: {}", std::io::Error::last_os_error()));
        }
        // Буфер u64 — выравнивание под CMSG_SIGNER_INFO.
        let mut buf = vec![0u64; (size as usize).div_ceil(8)];
        if CryptMsgGetParam(msg, CMSG_SIGNER_INFO_PARAM, 0, buf.as_mut_ptr().cast(), &mut size) == 0 {
            return Err(format!("нет данных подписавшего: {}", std::io::Error::last_os_error()));
        }
        let si = &*(buf.as_ptr() as *const CMSG_SIGNER_INFO);
        let mut ci: CERT_INFO = std::mem::zeroed();
        ci.Issuer = si.Issuer;
        ci.SerialNumber = si.SerialNumber;
        let cert = CertFindCertificateInStore(
            store,
            X509_ASN_ENCODING | PKCS_7_ASN_ENCODING,
            0,
            CERT_FIND_SUBJECT_CERT,
            (&ci as *const CERT_INFO).cast(),
            null(),
        );
        if cert.is_null() {
            return Err("сертификат подписавшего не найден в подписи".into());
        }
        let mut name = [0u16; 512];
        let n = CertGetNameStringW(cert, CERT_NAME_SIMPLE_DISPLAY_TYPE, 0, null(), name.as_mut_ptr(), name.len() as u32);
        CertFreeCertificateContext(cert);
        if n <= 1 {
            return Err("у сертификата подписавшего нет имени".into());
        }
        Ok(from_wide(&name))
    })();
    unsafe {
        CryptMsgClose(msg);
        CertCloseStore(store, 0);
    }
    result
}

/// Аргументы msiexec для тихой установки без перезагрузки и без запуска клиента, с подробным журналом.
#[cfg_attr(not(test), allow(dead_code))]
pub fn install_args(msi: &Path, log: &Path) -> Vec<String> {
    let (msi, log) = (msi.to_string_lossy().into_owned(), log.to_string_lossy().into_owned());
    ["/i", &msi, "/qn", "/norestart", "DO_NOT_LAUNCH=1", "/l*v", &log].map(String::from).to_vec()
}

/// Аргументы msiexec для тихого удаления по коду продукта, с подробным журналом.
#[cfg_attr(not(test), allow(dead_code))]
pub fn uninstall_args(product_code: &str, log: &Path) -> Vec<String> {
    let log = log.to_string_lossy().into_owned();
    ["/x", product_code, "/qn", "/norestart", "/l*v", &log].map(String::from).to_vec()
}

/// Установить MSI (msiexec из System32, ждёт завершения).
#[cfg_attr(not(test), allow(dead_code))]
pub fn install(msi: &Path, log: &Path) -> Result<(), String> {
    msiexec(&install_args(msi, log))
}

/// Удалить продукт по коду (msiexec из System32, ждёт завершения).
#[cfg_attr(not(test), allow(dead_code))]
pub fn uninstall(product_code: &str, log: &Path) -> Result<(), String> {
    msiexec(&uninstall_args(product_code, log))
}

/// msiexec по абсолютному пути; 0, 3010 (нужна перезагрузка) и 1641 (перезагрузка начата) — успех.
fn msiexec(args: &[String]) -> Result<(), String> {
    use windows_sys::Win32::UI::Shell::FOLDERID_System;
    let exe = crate::win::known_folder(&FOLDERID_System).ok_or("папка System32 не найдена")?.join("msiexec.exe");
    let status = std::process::Command::new(&exe).args(args).status().map_err(|e| format!("msiexec: {e}"))?;
    match status.code() {
        Some(0 | 3010 | 1641) => Ok(()),
        Some(c) => Err(format!("msiexec завершился с кодом {c}")),
        None => Err("msiexec завершился без кода".into()),
    }
}

/// Папка туннелей AmneziaWG (зашифрованные `*.conf.dpapi`).
#[cfg_attr(not(test), allow(dead_code))]
pub fn config_dir() -> PathBuf {
    crate::win::program_files()
        .join(r"AmneziaWG\Data\Configurations")
}

/// Точки повторной обработки (символические ссылки, соединения) не открываются и не копируются.
fn is_reparse(md: &std::fs::Metadata) -> bool {
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    md.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 || md.file_type().is_symlink()
}

/// Копирует `*.conf.dpapi` (регистр расширения не важен) из `from` в `to` как есть; создаёт `to`.
/// Подпапки, прочие файлы и точки повторной обработки пропускаются. Ok — число скопированных файлов.
#[cfg_attr(not(test), allow(dead_code))]
pub fn copy_configs(from: &Path, to: &Path) -> Result<u64, String> {
    let err = |p: &Path, e: std::io::Error| crate::fsutil::io_ctx(&p, e);
    let entries = std::fs::read_dir(from).map_err(|e| err(from, e))?;
    std::fs::create_dir_all(to).map_err(|e| err(to, e))?;
    let mut count = 0;
    for entry in entries {
        let entry = entry.map_err(|e| err(from, e))?;
        let name = entry.file_name();
        if !name.to_string_lossy().to_lowercase().ends_with(".conf.dpapi") {
            continue;
        }
        // Метаданные самой записи, без перехода по ссылке.
        let md = std::fs::symlink_metadata(entry.path()).map_err(|e| err(&entry.path(), e))?;
        if !md.is_file() || is_reparse(&md) {
            continue;
        }
        let dest = to.join(&name);
        // Назначения нет (или его не прочитать) — обычная копия: если что-то не так, ошибку даст `copy` ниже.
        if let Ok(dmd) = std::fs::symlink_metadata(&dest) {
            if is_reparse(&dmd) || !dmd.is_file() {
                return Err(format!("{}: на месте файла ссылка или папка", dest.display()));
            }
        }
        std::fs::copy(entry.path(), &dest).map_err(|e| err(&dest, e))?;
        count += 1;
    }
    Ok(count)
}

/// Копия туннелей AmneziaWG в `dest`.
#[cfg_attr(not(test), allow(dead_code))]
pub fn backup_configs(dest: &Path) -> Result<u64, String> {
    copy_configs(&config_dir(), dest)
}

/// Возврат туннелей из копии `src` в папку AmneziaWG.
#[cfg_attr(not(test), allow(dead_code))]
pub fn restore_configs(src: &Path) -> Result<u64, String> {
    copy_configs(src, &config_dir())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Версия файла из ресурсов (только чтение), для сравнения с версией MSI.
    fn file_version(path: &Path) -> Option<String> {
        use windows_sys::Win32::System::ApplicationInstallationAndServicing::MsiGetFileVersionW;
        let p = wide(&path.to_string_lossy());
        let mut buf = [0u16; 64];
        let mut len = buf.len() as u32;
        let r = unsafe { MsiGetFileVersionW(p.as_ptr(), buf.as_mut_ptr(), &mut len, null_mut(), null_mut()) };
        (r == 0).then(|| from_wide(&buf))
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let d = std::env::temp_dir().join(format!("awg-native-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn args() {
        let (msi, log) = (Path::new(r"C:\t\a.msi"), Path::new(r"C:\t\i.log"));
        assert_eq!(install_args(msi, log), ["/i", r"C:\t\a.msi", "/qn", "/norestart", "DO_NOT_LAUNCH=1", "/l*v", r"C:\t\i.log"]);
        assert_eq!(
            uninstall_args("{ABC}", log),
            ["/x", "{ABC}", "/qn", "/norestart", "/l*v", r"C:\t\i.log"]
        );
    }

    /// Сигнатуры API для менеджера обновлений; install и uninstall здесь не вызываются.
    #[test]
    fn api_signatures() {
        let _: fn(&Path, &Path) -> Result<(), String> = install;
        let _: fn(&str, &Path) -> Result<(), String> = uninstall;
        let _: fn(&Path) -> Result<u64, String> = backup_configs;
        let _: fn(&Path) -> Result<u64, String> = restore_configs;
        assert!(config_dir().ends_with(r"AmneziaWG\Data\Configurations"));
    }

    #[test]
    fn asset_name() {
        assert_eq!(msi_asset("3.1.0"), "amneziawg-amd64-3.1.0.msi");
    }

    #[test]
    fn copy_roundtrip() {
        let (src, mid, back) = (temp_dir("src"), temp_dir("mid"), temp_dir("back"));
        std::fs::write(src.join("a.conf.dpapi"), b"\x01\x02opaque").unwrap();
        std::fs::write(src.join("B.CONF.DPAPI"), b"\xffbytes").unwrap();
        std::fs::write(src.join("c.conf"), b"plain").unwrap();
        std::fs::write(src.join("d.txt"), b"x").unwrap();
        std::fs::create_dir(src.join("sub.conf.dpapi")).unwrap();
        std::fs::write(src.join("sub.conf.dpapi").join("e.conf.dpapi"), b"y").unwrap();
        // Ссылка создаётся, только если есть право на символические ссылки; иначе эта часть не проверяется.
        let link = std::os::windows::fs::symlink_file(src.join("a.conf.dpapi"), src.join("link.conf.dpapi")).is_ok();

        let to = mid.join("nested");
        assert_eq!(copy_configs(&src, &to).unwrap(), 2);
        let mut names: Vec<_> = std::fs::read_dir(&to).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        names.sort();
        assert_eq!(names, ["B.CONF.DPAPI", "a.conf.dpapi"]);
        assert_eq!(copy_configs(&to, &back).unwrap(), 2);
        for n in ["a.conf.dpapi", "B.CONF.DPAPI"] {
            assert_eq!(std::fs::read(src.join(n)).unwrap(), std::fs::read(back.join(n)).unwrap());
        }
        println!("--> symlink part checked: {link}");
        for d in [src, mid, back] {
            std::fs::remove_dir_all(d).unwrap();
        }
    }

    #[test]
    fn verify_rejects_garbage() {
        let d = temp_dir("garbage");
        let f = d.join("x.msi");
        std::fs::write(&f, b"not an msi").unwrap();
        let r = verify_msi(&f);
        std::fs::remove_dir_all(&d).unwrap();
        assert!(r.is_err(), "{r:?}");
    }

    #[test]
    fn msi_identity_rejects_garbage() {
        let d = temp_dir("identity");
        let f = d.join("x.msi");
        std::fs::write(&f, b"not an msi").unwrap();
        let r = msi_identity(&f);
        let missing = msi_identity(&d.join("missing.msi"));
        std::fs::remove_dir_all(&d).unwrap();
        assert!(r.is_err(), "{r:?}");
        assert!(missing.is_err(), "{missing:?}");
    }

    #[test]
    #[ignore]
    fn live_msi_identity() {
        use windows_sys::Win32::UI::Shell::FOLDERID_System;
        let d = temp_dir("identity-live");
        let f = d.join(msi_asset("3.1.0"));
        let url = "https://github.com/amnezia-vpn/amneziawg-windows-client/releases/download/3.1.0/amneziawg-amd64-3.1.0.msi";
        let curl = crate::win::known_folder(&FOLDERID_System).unwrap().join("curl.exe");
        let st = std::process::Command::new(curl).args(["-sSfL", "--ssl-no-revoke", "-o"]).arg(&f).arg(url).status().unwrap();
        let id = msi_identity(&f);
        std::fs::remove_dir_all(&d).unwrap();
        println!("--> curl: {st}");
        println!("--> msi_identity: {id:?}");
        assert!(st.success());
        let (code, version) = id.unwrap();
        assert_eq!(code, UPGRADE_CODE);
        assert_eq!(version, "3.1.0");
    }

    #[test]
    #[ignore]
    fn live_installed() {
        let i = installed();
        println!("--> installed: {i:?}");
        let exe = config_dir().parent().and_then(|p| p.parent()).unwrap().join("amneziawg.exe");
        println!("--> amneziawg.exe file version: {:?}", file_version(&exe));
        assert!(i.is_some());
    }

    #[test]
    #[ignore]
    fn live_verify_official_msi() {
        use windows_sys::Win32::UI::Shell::FOLDERID_System;
        let d = temp_dir("msi");
        let f = d.join(msi_asset("3.1.0"));
        let url = "https://github.com/amnezia-vpn/amneziawg-windows-client/releases/download/3.1.0/amneziawg-amd64-3.1.0.msi";
        let curl = crate::win::known_folder(&FOLDERID_System).unwrap().join("curl.exe");
        let st = std::process::Command::new(curl).args(["-sSfL", "--ssl-no-revoke", "-o"]).arg(&f).arg(url).status().unwrap();
        let size = std::fs::metadata(&f).map(|m| m.len()).unwrap_or(0);
        let trust = win_verify_trust(&f);
        let cn = signer_cn(&f);
        let verify = verify_msi(&f);
        std::fs::remove_dir_all(&d).unwrap();
        println!("--> curl: {st}, size: {size}");
        println!("--> trust: {trust:?}");
        println!("--> signer CN: {cn:?}");
        println!("--> verify_msi: {verify:?}");
        assert!(st.success());
        assert!(verify.is_ok(), "{verify:?}");
    }
}
