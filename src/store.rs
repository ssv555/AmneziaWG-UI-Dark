//! Хранилище туннелей режима 2: `C:\Program Files\AmneziaWG UI Dark\tunnels\<имя>.conf.dpapi`.
//! Шифрование — DPAPI Windows в области компьютера: окно (администратор) и служба (SYSTEM) читают одни файлы,
//! скопированный на другой компьютер файл не расшифровать, пароля в программе нет.
//! DPAPI компьютера расшифрует любая учётная запись этого ПК, поэтому у папки свои права: только SYSTEM и
//! администраторы — без чтения для пользователей (в Program Files оно есть) и без записи: конфиг, который
//! исполняет служба SYSTEM, обычная программа не прочитает и не подменит.

use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Cryptography::{
    BCryptGenRandom, CryptProtectData, CryptUnprotectData, BCRYPT_USE_SYSTEM_PREFERRED_RNG, CRYPTPROTECT_LOCAL_MACHINE,
    CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};

pub const EXT: &str = ".conf.dpapi";
/// Не секрет: только отличает наши файлы от чужих DPAPI-блоков.
const ENTROPY: &[u8] = b"AmneziaWG UI Dark tunnel";
/// Права папки: SYSTEM и администраторы — полный доступ, наследование от Program Files отключено.
const DIR_SDDL: &str = "O:BAD:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";
/// Временный архив родного экспорта (в нём ключи открытым текстом) — только внутри защищённой папки.
const EXPORT_PREFIX: &str = ".export-";

pub fn dir() -> PathBuf {
    crate::engine::install_dir().join("tunnels")
}

pub fn path(tunnel: &str) -> PathBuf {
    dir().join(format!("{tunnel}{EXT}"))
}

/// Создать папку хранилища (если нет) и выставить ей права (каждый раз: вдруг их поменяли).
fn secure_dir() -> Result<PathBuf, String> {
    let d = dir();
    crate::win::protect_dir(&d, DIR_SDDL)?;
    Ok(d)
}

/// При запуске в режиме встроенного движка: права папки и уборка архивов экспорта, оставшихся после сбоя.
pub fn startup() -> Result<(), String> {
    if !dir().exists() {
        return Ok(());
    }
    let d = secure_dir()?;
    for entry in std::fs::read_dir(&d).map_err(|e| crate::fsutil::io_ctx(&d, e))?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(EXPORT_PREFIX) {
            std::fs::remove_file(entry.path()).map_err(|e| crate::fsutil::io_ctx(entry.path(), e))?;
        }
    }
    Ok(())
}

/// Путь для временного архива родного экспорта: в защищённой папке, имя случайное.
pub fn export_temp() -> Result<PathBuf, String> {
    let d = secure_dir()?;
    Ok(d.join(format!("{EXPORT_PREFIX}{}.zip", random_hex())))
}

/// Случайное имя для временных файлов (32 шестнадцатеричных знака).
pub fn random_hex() -> String {
    random::<16>().iter().map(|b| format!("{b:02x}")).collect()
}

/// Имена туннелей в хранилище; папки ещё нет — пусто.
pub fn list() -> std::io::Result<Vec<String>> {
    let entries = match std::fs::read_dir(dir()) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        other => other?,
    };
    let mut names = Vec::new();
    for entry in entries {
        let file = entry?.file_name().to_string_lossy().into_owned();
        if let Some(name) = file.strip_suffix(EXT) {
            names.push(name.to_string());
        }
    }
    Ok(names)
}

/// Есть ли туннель с таким именем без учёта регистра (NTFS регистр не различает).
pub fn find<'a>(names: &'a [String], name: &str) -> Option<&'a String> {
    names.iter().find(|n| n.eq_ignore_ascii_case(name))
}

/// Текст конфига: `.dpapi` расшифровывается, обычный `.conf` читается как есть.
pub fn read(file: &Path) -> Result<String, String> {
    let data = std::fs::read(file).map_err(|e| crate::fsutil::io_ctx(&file, e))?;
    let plain = if file.to_string_lossy().ends_with(".dpapi") { unprotect(&data)? } else { data };
    String::from_utf8(plain).map_err(|e| crate::fsutil::io_ctx(&file, e))
}

/// Записать конфиг туннеля зашифрованным (через временный файл, чтобы не оставить половину).
pub fn write(tunnel: &str, text: &str) -> Result<PathBuf, String> {
    secure_dir()?;
    let target = path(tunnel);
    crate::fsutil::write_atomic(&target, &protect(text.as_bytes())?).map_err(|e| crate::fsutil::io_ctx(&target, e))?;
    Ok(target)
}

/// Переименовать туннель (в том числе только регистр букв). Занятое другим туннелем имя — ошибка.
pub fn rename(old: &str, new: &str) -> Result<(), String> {
    let names = list().map_err(|e| e.to_string())?;
    if find(&names, new).is_some_and(|n| !n.eq_ignore_ascii_case(old)) {
        return Err(format!("{}: exists", path(new).display()));
    }
    std::fs::rename(path(old), path(new)).map_err(|e| crate::fsutil::io_ctx(path(old), e))
}

/// Итог импорта: добавлены; уже были и не тронуты; имя не подходит движку.
#[derive(Default, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ImportReport {
    pub added: Vec<String>,
    pub existing: Vec<String>,
    pub bad_name: Vec<String>,
}

/// Положить туннели в хранилище. Существующие (без учёта регистра, в том числе из этого же набора) не трогаются.
pub fn import(entries: &[crate::archive::Entry]) -> Result<ImportReport, String> {
    let have = list().map_err(|e| e.to_string())?;
    let (write_these, report) = plan_import(have, entries);
    for i in write_these {
        write(&entries[i].name, &entries[i].text)?;
    }
    Ok(report)
}

/// Что из `entries` записывать (индексы) и итог — без обращения к диску.
fn plan_import(mut have: Vec<String>, entries: &[crate::archive::Entry]) -> (Vec<usize>, ImportReport) {
    let mut report = ImportReport::default();
    let mut write_these = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        if !crate::engine::valid_name(&e.name) {
            report.bad_name.push(e.name.clone());
        } else if find(&have, &e.name).is_some() {
            report.existing.push(e.name.clone());
        } else {
            have.push(e.name.clone());
            write_these.push(i);
            report.added.push(e.name.clone());
        }
    }
    (write_these, report)
}

/// Все туннели хранилища — для резервной копии.
pub fn export_all() -> Result<Vec<crate::archive::Entry>, String> {
    let mut names = list().map_err(|e| e.to_string())?;
    names.sort();
    names.into_iter().map(|name| Ok(crate::archive::Entry { text: read(&path(&name))?, name })).collect()
}

/// Новый туннель: свежий закрытый ключ (открытый программа покажет сама) и пустые поля для данных сервера.
pub fn template() -> String {
    let mut key = random::<32>();
    // Ограничения ключа Curve25519 — как у `wg genkey`.
    key[0] &= 248;
    key[31] = (key[31] & 127) | 64;
    format!(
        "[Interface]\nPrivateKey = {}\nAddress = \nDNS = 1.1.1.1\n\n[Peer]\nPublicKey = \nEndpoint = \nAllowedIPs = 0.0.0.0/0, ::/0\nPersistentKeepalive = 25\n",
        crate::uapi::base64(&key)
    )
}

/// Криптостойкие случайные байты (системный генератор Windows).
fn random<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    let status = unsafe { BCryptGenRandom(null_mut(), buf.as_mut_ptr(), N as u32, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
    assert_eq!(status, 0, "BCryptGenRandom");
    buf
}

pub fn remove(tunnel: &str) -> Result<(), String> {
    let p = path(tunnel);
    std::fs::remove_file(&p).map_err(|e| crate::fsutil::io_ctx(&p, e))
}

fn protect(data: &[u8]) -> Result<Vec<u8>, String> {
    dpapi(data, true)
}

fn unprotect(data: &[u8]) -> Result<Vec<u8>, String> {
    dpapi(data, false)
}

fn dpapi(data: &[u8], encrypt: bool) -> Result<Vec<u8>, String> {
    let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
    let entropy = CRYPT_INTEGER_BLOB { cbData: ENTROPY.len() as u32, pbData: ENTROPY.as_ptr() as *mut u8 };
    let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: null_mut() };
    unsafe {
        let ok = if encrypt {
            CryptProtectData(&input, null(), &entropy, null(), null(), CRYPTPROTECT_LOCAL_MACHINE | CRYPTPROTECT_UI_FORBIDDEN, &mut out)
        } else {
            CryptUnprotectData(&input, null_mut(), &entropy, null(), null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out)
        };
        if ok == 0 {
            return Err(format!("DPAPI: {}", std::io::Error::last_os_error()));
        }
        let result = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        LocalFree(out.pbData.cast());
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpapi_roundtrip() {
        let text = "[Interface]\nPrivateKey = test\n";
        let blob = protect(text.as_bytes()).unwrap();
        assert!(!blob.windows(9).any(|w| w == b"Interface"), "зашифровано");
        assert_eq!(unprotect(&blob).unwrap(), text.as_bytes());
    }

    #[test]
    fn import_ignores_case_and_batch_duplicates() {
        let e = |n: &str| crate::archive::Entry { name: n.into(), text: String::new() };
        let entries = [e("Office"), e("home"), e("HOME"), e("bad name")];
        let (write, report) = plan_import(vec!["office".into()], &entries);
        assert_eq!(write, vec![1], "только home: Office уже есть как office, HOME — повтор в наборе");
        assert_eq!(report.added, vec!["home".to_string()]);
        assert_eq!(report.existing, vec!["Office".to_string(), "HOME".to_string()]);
        assert_eq!(report.bad_name, vec!["bad name".to_string()]);
    }

    #[test]
    fn template_has_fresh_valid_key() {
        let key = |t: &str| t.lines().find_map(|l| l.strip_prefix("PrivateKey = ")).unwrap().to_string();
        let (a, b) = (template(), template());
        assert_ne!(key(&a), key(&b));
        let info = crate::conf::parse(&a);
        assert_eq!(info.public_key.len(), 44, "открытый ключ выводится из закрытого");
    }

    #[test]
    fn foreign_blob_is_rejected() {
        let blob = protect(b"x").unwrap();
        let mut broken = blob.clone();
        let last = broken.len() - 1;
        broken[last] ^= 0xFF;
        assert!(unprotect(&broken).is_err());
    }
}
