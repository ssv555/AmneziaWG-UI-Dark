//! Хранилище туннелей режима 2: `C:\Program Files\AmneziaWG UI Dark\tunnels\<имя>.conf.dpapi`.
//! Шифрование — DPAPI Windows в области компьютера: окно (администратор) и служба (SYSTEM) читают одни файлы,
//! скопированный на другой компьютер файл не расшифровать, пароля в программе нет.
//! DPAPI компьютера расшифрует любая учётная запись этого ПК, поэтому у папки свои права: только SYSTEM и
//! администраторы — без чтения для пользователей (в Program Files оно есть) и без записи: конфиг, который
//! исполняет служба SYSTEM, обычная программа не прочитает и не подменит.
//! Меняют хранилище два процесса: ядро (переименование, удаление — под своим `switching`) и агент (запись, импорт,
//! новый туннель). Каждое изменение идёт под замком хранилища (`StoreLock`, файл `.lock` в этой папке, открыт без
//! совместного доступа): иначе запись агента после переименования в ядре вернула бы прежний туннель. Порядок замков —
//! `switching`, затем замок хранилища; агент `switching` не берёт и под замком хранилища ни к кому не обращается, поэтому
//! взаимной блокировки нет. Замок держится только на время файловых операций, ожидание ограничено (`LOCK_WAIT`).

use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::time::Duration;

use windows_sys::Win32::Foundation::{LocalFree, ERROR_SHARING_VIOLATION};
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
/// Файл замка хранилища: имя без `EXT`, в список туннелей не попадает.
const LOCK_FILE: &str = ".lock";
/// Сколько ждать замок. Его держат миллисекунды (одна запись файла); дольше — сбой, а ядро под `switching` не должно
/// ждать бесконечно.
const LOCK_WAIT: Duration = Duration::from_secs(5);
const LOCK_STEP: Duration = Duration::from_millis(10);

pub fn dir() -> PathBuf {
    crate::engine::install_dir().join("tunnels")
}

pub fn path(tunnel: &str) -> PathBuf {
    path_in(&dir(), tunnel)
}

fn path_in(dir: &Path, tunnel: &str) -> PathBuf {
    dir.join(format!("{tunnel}{EXT}"))
}

/// Замок хранилища, общий для ядра и агента (см. описание модуля). Снимается при `drop`, а при падении процесса —
/// системой вместе с его описателями, так что брошенного замка не бывает.
struct StoreLock {
    _file: std::fs::File,
}

/// Замок — файл, открытый без совместного доступа: второй открывающий (в любом процессе) получает
/// `ERROR_SHARING_VIOLATION` и ждёт до `wait`.
fn lock_in(dir: &Path, wait: Duration) -> Result<StoreLock, String> {
    let file = dir.join(LOCK_FILE);
    let open = || std::fs::OpenOptions::new().read(true).write(true).create(true).share_mode(0).open(&file);
    let busy = |r: &std::io::Result<std::fs::File>| matches!(r, Err(e) if e.raw_os_error() == Some(ERROR_SHARING_VIOLATION as i32));
    let mut last = open();
    if busy(&last) {
        crate::fsutil::wait_until(wait, LOCK_STEP, || {
            last = open();
            !busy(&last)
        });
    }
    let timed_out = busy(&last);
    match last {
        Ok(handle) => Ok(StoreLock { _file: handle }),
        Err(_) if timed_out => Err(crate::i18n::trf("store.busy", &[&wait.as_secs().to_string()])),
        Err(e) => Err(crate::fsutil::io_ctx(&file, e)),
    }
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
    list_in(&dir())
}

fn list_in(dir: &Path) -> std::io::Result<Vec<String>> {
    let entries = match std::fs::read_dir(dir) {
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

/// Имя подходит туннелю хранилища (оно же имя службы движка и часть путей) — правило движка, одно на всех.
pub fn valid_name(name: &str) -> bool {
    crate::engine::valid_name(name)
}

/// Новый туннель хранилища со свежим ключом; возвращает его текст.
pub fn new_tunnel(name: &str) -> Result<String, String> {
    if !valid_name(name) {
        return Err(crate::i18n::trf("eng.bad_name", &[name]));
    }
    let text = template();
    if !create_in(&secure_dir()?, name, &text, LOCK_WAIT)? {
        return Err(crate::i18n::tr("dlg.err_exists"));
    }
    Ok(text)
}

/// Записать новый туннель, если имени (без учёта регистра) ещё нет: проверка и запись под одним замком.
/// `false` — имя занято, ничего не записано.
fn create_in(dir: &Path, tunnel: &str, text: &str, wait: Duration) -> Result<bool, String> {
    let _lock = lock_in(dir, wait)?;
    if find(&list_in(dir).map_err(|e| crate::fsutil::io_ctx(dir, e))?, tunnel).is_some() {
        return Ok(false);
    }
    write_file(dir, tunnel, text)?;
    Ok(true)
}

/// Текст конфига: `.dpapi` расшифровывается, обычный `.conf` читается как есть.
pub fn read(file: &Path) -> Result<String, String> {
    let data = std::fs::read(file).map_err(|e| crate::fsutil::io_ctx(&file, e))?;
    let plain = if file.to_string_lossy().ends_with(".dpapi") { unprotect(&data)? } else { data };
    String::from_utf8(plain).map_err(|e| crate::fsutil::io_ctx(&file, e))
}

/// Записать конфиг туннеля (создать или заменить) — импорт файла из командной строки.
pub fn write(tunnel: &str, text: &str) -> Result<PathBuf, String> {
    let d = secure_dir()?;
    let _lock = lock_in(&d, LOCK_WAIT)?;
    write_file(&d, tunnel, text)
}

/// Сохранить правку существующего туннеля. Пока окно редактировало, ядро могло его переименовать или удалить:
/// тогда ошибка, а не запись — иначе туннель вернулся бы под прежним именем рядом с новым.
pub fn update(tunnel: &str, text: &str) -> Result<(), String> {
    update_in(&secure_dir()?, tunnel, text, LOCK_WAIT)
}

fn update_in(dir: &Path, tunnel: &str, text: &str, wait: Duration) -> Result<(), String> {
    let _lock = lock_in(dir, wait)?;
    let names = list_in(dir).map_err(|e| crate::fsutil::io_ctx(dir, e))?;
    let Some(stored) = find(&names, tunnel) else {
        return Err(crate::i18n::trf("store.gone", &[tunnel]));
    };
    write_file(dir, stored, text).map(drop)
}

/// Зашифровать и записать через временный файл, чтобы не оставить половину. Только под замком хранилища.
fn write_file(dir: &Path, tunnel: &str, text: &str) -> Result<PathBuf, String> {
    let target = path_in(dir, tunnel);
    crate::fsutil::write_atomic(&target, &protect(text.as_bytes())?).map_err(|e| crate::fsutil::io_ctx(&target, e))?;
    Ok(target)
}

/// Переименовать туннель (в том числе только регистр букв). Занятое другим туннелем имя — ошибка.
pub fn rename(old: &str, new: &str) -> Result<(), String> {
    rename_in(&secure_dir()?, old, new, LOCK_WAIT)
}

fn rename_in(dir: &Path, old: &str, new: &str, wait: Duration) -> Result<(), String> {
    let _lock = lock_in(dir, wait)?;
    let names = list_in(dir).map_err(|e| crate::fsutil::io_ctx(dir, e))?;
    if find(&names, new).is_some_and(|n| !n.eq_ignore_ascii_case(old)) {
        return Err(format!("{}: exists", path_in(dir, new).display()));
    }
    let (from, to) = (path_in(dir, old), path_in(dir, new));
    std::fs::rename(&from, &to).map_err(|e| crate::fsutil::io_ctx_move(&from, &to, e))
}

/// Итог импорта: добавлены; уже были и не тронуты; имя не подходит движку; с командами PreUp/PostUp/… — отказ.
#[derive(Default, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ImportReport {
    pub added: Vec<String>,
    pub existing: Vec<String>,
    pub bad_name: Vec<String>,
    /// `default`: отчёт процесса старой версии (без поля) читается как «таких нет».
    #[serde(default)]
    pub scripts: Vec<String>,
}

/// Положить туннели в хранилище. Существующие (без учёта регистра, в том числе из этого же набора) не трогаются.
/// Конфиги с командами PreUp/PostUp/PreDown/PostDown не принимаются на любом пути (окно, «забрать всё из AmneziaWG»):
/// служба туннеля выполнила бы их от SYSTEM; такие записи названы в отчёте (`scripts`).
pub fn import(entries: &[crate::archive::Entry]) -> Result<ImportReport, String> {
    let d = secure_dir()?;
    let have = list_in(&d).map_err(|e| crate::fsutil::io_ctx(&d, e))?;
    let (write_these, mut report) = plan_import(have, entries);
    // Замок — на каждый туннель, а не на весь импорт: ядро под `switching` ждёт не дольше одной записи.
    for i in write_these {
        let name = &entries[i].name;
        if !create_in(&d, name, &entries[i].text, LOCK_WAIT)? {
            // Имя заняли, пока шёл импорт (переименование в ядре): чужой туннель не перезаписываем.
            report.added.retain(|n| n != name);
            report.existing.push(name.clone());
        }
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
        } else if crate::conf::has_scripts(&e.text) {
            report.scripts.push(e.name.clone());
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
    remove_in(&secure_dir()?, tunnel, LOCK_WAIT)
}

fn remove_in(dir: &Path, tunnel: &str, wait: Duration) -> Result<(), String> {
    let _lock = lock_in(dir, wait)?;
    let p = path_in(dir, tunnel);
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
    fn import_refuses_configs_with_scripts_and_names_them() {
        let e = |n: &str, t: &str| crate::archive::Entry { name: n.into(), text: t.into() };
        let entries = [
            e("clean", "[Interface]\nPrivateKey = a\n"),
            e("evil", "[Interface]\nPrivateKey = a\nPostUp = cmd /c calc\n"),
            e("down", "[Interface]\n predown=x\n"),
        ];
        let (write, report) = plan_import(Vec::new(), &entries);
        assert_eq!(write, vec![0], "записывается только конфиг без команд");
        assert_eq!(report.added, vec!["clean".to_string()]);
        assert_eq!(report.scripts, vec!["evil".to_string(), "down".to_string()]);
        assert!(report.existing.is_empty() && report.bad_name.is_empty());
    }

    #[test]
    fn template_has_fresh_valid_key() {
        let key = |t: &str| t.lines().find_map(|l| l.strip_prefix("PrivateKey = ")).unwrap().to_string();
        let (a, b) = (template(), template());
        assert_ne!(key(&a), key(&b));
        let info = crate::conf::parse(&a);
        assert_eq!(info.public_key.len(), 44, "открытый ключ выводится из закрытого");
    }

    /// Папка-хранилище для теста (без прав `secure_dir`: тесту они не нужны).
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("awg-ui-store-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn stored(dir: &Path) -> Vec<String> {
        let mut names = list_in(dir).unwrap();
        names.sort();
        names
    }

    const WAIT: Duration = Duration::from_secs(10);

    /// Сохранение в окне (агент) идёт, пока ядро переименовывает `a` в `b` под замком: запись ждёт замка, после
    /// переименования видит, что `a` нет, и отказывает. Раньше она создала бы `a` заново рядом с `b`.
    #[test]
    fn write_during_rename_waits_and_does_not_resurrect() {
        let dir = scratch("rename");
        assert!(create_in(&dir, "a", "old", WAIT).unwrap());
        let held = std::sync::Barrier::new(2);
        let saved = std::thread::scope(|s| {
            let core = s.spawn(|| {
                // То же, что `rename_in`, но с паузой под замком, пока агент не начнёт сохранять.
                let lock = lock_in(&dir, WAIT).unwrap();
                held.wait();
                std::fs::rename(path_in(&dir, "a"), path_in(&dir, "b")).unwrap();
                drop(lock);
            });
            let agent = s.spawn(|| {
                held.wait();
                update_in(&dir, "a", "new", WAIT)
            });
            core.join().unwrap();
            agent.join().unwrap()
        });
        assert_eq!(saved, Err(crate::i18n::trf("store.gone", &["a"])));
        assert_eq!(stored(&dir), ["b"], "без воскресшего `a`");
        assert_eq!(read(&path_in(&dir, "b")).unwrap(), "old");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Обратный порядок: агент пишет под замком, переименование ждёт и уносит уже новый текст.
    #[test]
    fn rename_during_write_waits_and_keeps_the_edit() {
        let dir = scratch("write");
        assert!(create_in(&dir, "a", "old", WAIT).unwrap());
        let held = std::sync::Barrier::new(2);
        let renamed = std::thread::scope(|s| {
            let agent = s.spawn(|| {
                let lock = lock_in(&dir, WAIT).unwrap();
                held.wait();
                write_file(&dir, "a", "new").unwrap();
                drop(lock);
            });
            let core = s.spawn(|| {
                held.wait();
                rename_in(&dir, "a", "b", WAIT)
            });
            agent.join().unwrap();
            core.join().unwrap()
        });
        assert_eq!(renamed, Ok(()));
        assert_eq!(stored(&dir), ["b"]);
        assert_eq!(read(&path_in(&dir, "b")).unwrap(), "new");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn write_after_delete_is_refused() {
        let dir = scratch("delete");
        assert!(create_in(&dir, "a", "old", WAIT).unwrap());
        remove_in(&dir, "a", WAIT).unwrap();
        assert_eq!(update_in(&dir, "a", "new", WAIT), Err(crate::i18n::trf("store.gone", &["a"])));
        assert!(stored(&dir).is_empty());
        // Правка существующего туннеля пишется в его файл, даже если регистр имени в запросе другой.
        assert!(create_in(&dir, "Office", "old", WAIT).unwrap());
        assert!(!create_in(&dir, "office", "dup", WAIT).unwrap(), "имя занято без учёта регистра");
        update_in(&dir, "office", "new", WAIT).unwrap();
        assert_eq!(stored(&dir), ["Office"]);
        assert_eq!(read(&path_in(&dir, "Office")).unwrap(), "new");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Замок занят дольше срока — понятная ошибка, а не вечное ожидание (ядро ждёт его под `switching`).
    #[test]
    fn busy_lock_times_out_with_a_clear_error() {
        let dir = scratch("busy");
        let held = lock_in(&dir, WAIT).unwrap();
        let busy = crate::i18n::trf("store.busy", &["0"]);
        assert_eq!(rename_in(&dir, "a", "b", Duration::ZERO).unwrap_err(), busy);
        assert_eq!(remove_in(&dir, "a", Duration::ZERO).unwrap_err(), busy);
        assert_eq!(update_in(&dir, "a", "x", Duration::ZERO).unwrap_err(), busy);
        assert_eq!(create_in(&dir, "a", "x", Duration::ZERO).unwrap_err(), busy);
        drop(held);
        assert!(create_in(&dir, "a", "x", Duration::ZERO).unwrap(), "освобождённый замок берётся сразу");
        assert_eq!(stored(&dir), ["a"], "файл замка — не туннель");
        std::fs::remove_dir_all(&dir).unwrap();
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
