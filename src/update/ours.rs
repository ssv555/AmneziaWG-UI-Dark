//! Наши компоненты из наших релизов (подписанный манифест): движок режима 2 и сборка программы.
//! Резервная копия — файлы как есть в папке копии; возврат — копирование обратно.
//!
//! Замена файла в Program Files: текущий отодвигается (`<имя>.old-<random>`; загруженную DLL и работающий exe
//! переименовать можно), новый копируется на его место; ошибка — всё отодвинутое возвращается. Отодвинутые
//! удаляет ядро, когда новая версия доложила о готовности. Не вернувшийся при откате прежний файл сохраняется как
//! `<имя>.keep-<random>` — такие ядро не трогает. Вместе с DLL кладётся проверенный манифест: по нему движку
//! доверяют (`engine::verify`) и показывают его версию.

//! Модуль — фасад: публичный интерфейс и общие факты здесь, остальное по ответственности в `ours/`:
//! `fileset` — транзакция над набором файлов и установка сборки, `fallback` — процесс `--restart-core` с возвратом,
//! `window` — выкладка и самообновление окна, `backup` — копии и возврат, `release` — загрузка и установка релиза.

use std::path::{Path, PathBuf};

use super::busy::Busy;
use super::sources::{OursError, Sources};
use super::{feed, net, sign};
use crate::engine::install_dir;
use crate::i18n::trf;

mod backup;
mod fallback;
mod fileset;
mod fs;
mod release;
mod window;
#[cfg(test)]
pub(in crate::update) mod testutil;

pub use fallback::restart_core;
pub use window::{self_update_window, window_startup_cleanup};

/// Куда ставится сборка и что рядом: папка программы, хранилище обновлений (история) и папка, откуда окно берёт
/// новую сборку. Операции над ними — методы в `backup`, `release`, `fileset`, `window`; в тестах цель указывает
/// на временные папки.
pub(super) struct InstallTarget {
    dir: PathBuf,
    store: PathBuf,
    window_dir: PathBuf,
}

impl InstallTarget {
    /// Настоящая установка: Program Files и данные ядра.
    fn current() -> Self {
        let store = super::manager::store_dir();
        // `<хранилище>\app`: SYSTEM и администраторы — полный доступ, владелец окна — чтение (`publish_for_window`).
        let window_dir = store.join("app");
        InstallTarget { dir: install_dir(), store, window_dir }
    }
}

/// Копия текущего движка (DLL и локальный манифест) в `dir`; размер копии в байтах.
#[allow(dead_code)] // зовёт менеджер обновлений ядра
pub fn backup_engine(dir: &Path) -> Result<u64, String> {
    InstallTarget::current().backup_engine(dir)
}

/// Вернуть движок из копии `dir` (сделанной `backup_engine`).
#[allow(dead_code)] // зовёт менеджер обновлений ядра
pub fn restore_engine(dir: &Path) -> Result<(), String> {
    InstallTarget::current().restore_engine(dir)
}

/// Копия текущей сборки программы в `dir`; размер в байтах.
#[allow(dead_code)] // зовёт менеджер обновлений ядра
pub fn backup_app(dir: &Path) -> Result<u64, String> {
    InstallTarget::current().backup_app(dir, &app_version())
}

/// Вернуть сборку программы из копии `dir` (как `update_app`, только набор из копии).
#[allow(dead_code)] // зовёт менеджер обновлений ядра
pub fn restore_app(dir: &Path) -> Result<(), String> {
    InstallTarget::current().restore_app(dir)
}

/// Скачать файлы движка из релиза в `work`, проверить по манифесту и поставить в Program Files.
#[allow(dead_code)] // зовёт менеджер обновлений ядра
pub(super) fn update_engine(src: &dyn Sources, rel: &feed::Release, m: &sign::Manifest, work: &Path, busy: &mut dyn FnMut(Busy)) -> Result<(), String> {
    InstallTarget::current().update_engine(src, &fs::RealFs, rel, m, work, busy)
}

/// Скачать exe из релиза, проверить по манифесту, поставить вместо exe ядра, отдать окну и перезапустить ядро
/// (подробности — `InstallTarget::update_app`).
#[allow(dead_code)] // зовёт менеджер обновлений ядра
pub(super) fn update_app(src: &dyn Sources, rel: &feed::Release, m: &sign::Manifest, work: &Path, busy: &mut dyn FnMut(Busy)) -> Result<(), String> {
    InstallTarget::current().update_app(src, rel, m, work, busy)
}

/// Разбор версий из `engine\build.ps1` — тот же файл, что подключает `build.rs`.
#[cfg(test)]
#[path = "engine_tag.rs"]
mod engine_tag;

/// Локальный манифест движка в папке программы и его подпись (те же имена, что у файлов релиза).
pub const MANIFEST: &str = "update-manifest.json";
pub const MANIFEST_SIG: &str = "update-manifest.json.sig";
/// Перезапуск ядра новой сборкой (процесс от SYSTEM, его запускает само ядро).
pub const RESTART_FLAG: &str = "--restart-core";
/// Окно, обновившее себя, передаёт новому процессу свой PID: тот ждёт его выхода (мьютекс одного экземпляра).
pub const PARENT_ENV: &str = "AWG_UI_UPDATED_FROM";
const ENGINE_FILES: [&str; 2] = ["tunnel.dll", "wintun.dll"];
const APP_EXE: &str = "awg-ui.exe";
const VERSION_TXT: &str = "version.txt";
/// Набор сборки программы: exe ядра и то, по чему он доверяет движку.
const APP_SET: [&str; 5] = [APP_EXE, ENGINE_FILES[0], ENGINE_FILES[1], MANIFEST, MANIFEST_SIG];
#[allow(dead_code)]
const MANIFEST_MAX: usize = 64 * 1024;
#[allow(dead_code)]
const FILE_MAX: u64 = 256 * 1024 * 1024;

/// Версия движка в Program Files: из локального подписанного манифеста, если движок обновлялся, иначе вшитая
/// при сборке; `None` — движок не установлен.
#[allow(dead_code)] // зовёт менеджер обновлений ядра
pub fn engine_version() -> Option<String> {
    InstallTarget::current().engine_version()
}

impl InstallTarget {
    fn engine_version(&self) -> Option<String> {
        let tunnel = self.dir.join(ENGINE_FILES[0]);
        if !tunnel.exists() {
            return None;
        }
        // Манифест верен, только если DLL — его: движок могли вернуть установкой ядра из старой сборки.
        let manifest = local_manifest(&self.dir)
            .filter(|m| m.engine.files.iter().any(|f| f.name == ENGINE_FILES[0] && sign::check_file(&tunnel, f).is_ok()))
            .map(|m| m.engine.version);
        engine_version_of(manifest, || crate::engine::installed_files_ok().is_ok(), option_env!("AWG_ENGINE_TAG"))
    }
}

/// Выбор версии движка: из манифеста; нет его — вшитая при сборке, если DLL совпадают со вшитыми суммами
/// (`pinned_ok`).
fn engine_version_of(manifest: Option<String>, pinned_ok: impl FnOnce() -> bool, build_tag: Option<&str>) -> Option<String> {
    manifest.or_else(|| match (pinned_ok(), build_tag) {
        (true, Some(tag)) => Some(tag.to_string()),
        // DLL не опознаны — версия 0, любое обновление движка предлагается.
        _ => Some("0".to_string()),
    })
}

/// Версия программы (ядра).
#[allow(dead_code)] // зовёт менеджер обновлений ядра
pub fn app_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// Проверенный локальный манифест в `dir`; нет, не читается или подпись неверна — `None`.
pub fn local_manifest(dir: &Path) -> Option<sign::Manifest> {
    read_manifest(dir, sign::manifest)
}

fn read_manifest(dir: &Path, check: impl Fn(&[u8], &str) -> Result<sign::Manifest, String>) -> Option<sign::Manifest> {
    let json = std::fs::read(dir.join(MANIFEST)).ok()?;
    let sig = std::fs::read_to_string(dir.join(MANIFEST_SIG)).ok()?;
    check(&json, &sig).ok()
}

/// Последний наш релиз и его проверенный манифест.
#[allow(dead_code)] // зовёт менеджер обновлений ядра
pub(super) fn latest() -> Result<(feed::Release, sign::Manifest), OursError> {
    let rel = feed::latest(feed::APP_REPO)?;
    let (json, sig) = manifest_files(&rel, &|url| net::get(url, None, MANIFEST_MAX))?;
    let m = sign::manifest(&json, &sig)?;
    Ok((rel, m))
}

/// Манифест и подпись из релиза (`get` — загрузка по адресу). Нет их в релизе — понятная ошибка: такой релиз
/// ставится только вручную.
fn manifest_files(rel: &feed::Release, get: &dyn Fn(&str) -> Result<Vec<u8>, String>) -> Result<(Vec<u8>, String), OursError> {
    let (Some(json), Some(sig)) = (rel.asset(MANIFEST), rel.asset(MANIFEST_SIG)) else {
        return Err(OursError::no_manifest(trf("updo.no_manifest", &[&rel.version])));
    };
    let sig = String::from_utf8(get(&sig.url)?).map_err(|_| format!("{MANIFEST_SIG}: UTF-8"))?;
    Ok((get(&json.url)?, sig))
}

#[cfg(test)]
mod tests {
    use super::*;
    use testutil::*;

    #[test]
    fn build_script_parses_engine_versions() {
        let ps1 = include_str!("../../engine/build.ps1");
        assert_eq!(engine_tag::engine_tag(ps1).as_deref(), Some("3.1.20260814"));
        assert_eq!(engine_tag::wintun_version(ps1).as_deref(), Some("0.14.1"));
        assert_eq!(engine_tag::engine_tag("$EngineTag = \"v1.2\"\n").as_deref(), Some("1.2"));
        assert_eq!(engine_tag::engine_tag("$EngineTagX = 'v1'\n$EngineTag = 'main'"), None);
        assert_eq!(engine_tag::wintun_version("wintun-LICENSE.txt wintun-0.15.zip").as_deref(), Some("0.15"));
        assert_eq!(engine_tag::wintun_version("no wintun here"), None);
    }

    #[test]
    fn local_manifest_trusts_listed_hash_and_rejects_unsigned() {
        let dir = temp("trust");
        std::fs::write(dir.join(MANIFEST), JSON).unwrap();
        std::fs::write(dir.join(MANIFEST_SIG), SIG).unwrap();
        let m = read_manifest(&dir, test_key).unwrap();
        let listed = &m.engine.files.iter().find(|f| f.name == "tunnel.dll").unwrap().sha256;
        assert!(crate::engine::trusted("tunnel.dll", listed, None, Some(&m)));
        assert!(!crate::engine::trusted("wintun.dll", listed, None, Some(&m)));
        assert!(!crate::engine::trusted("tunnel.dll", &"0".repeat(64), None, Some(&m)));
        assert!(crate::engine::trusted("tunnel.dll", "ab", Some("ab"), None));
        // Подпись не тем ключом, испорченная или отсутствующая — манифеста нет.
        assert!(local_manifest(&dir).is_none());
        std::fs::write(dir.join(MANIFEST_SIG), "garbage").unwrap();
        assert!(read_manifest(&dir, test_key).is_none());
        std::fs::remove_file(dir.join(MANIFEST_SIG)).unwrap();
        assert!(read_manifest(&dir, test_key).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn engine_version_without_manifest_checks_dlls() {
        let never = || -> bool { panic!("манифест верен — суммы не проверяются") };
        assert_eq!(engine_version_of(Some("3.2".into()), never, Some("3.1")).as_deref(), Some("3.2"));
        assert_eq!(engine_version_of(None, || true, Some("3.1")).as_deref(), Some("3.1"));
        assert_eq!(engine_version_of(None, || false, Some("3.1")).as_deref(), Some("0"));
        assert_eq!(engine_version_of(None, || true, None).as_deref(), Some("0"));
    }

    #[test]
    fn release_without_manifest_reads_nicely() {
        let json = r#"{"tag_name":"v0.3.0","published_at":"2026-09-01T00:00:00Z","body":"x",
            "assets":[{"name":"awg-ui.exe","browser_download_url":"https://example.invalid/awg-ui.exe","size":1}]}"#;
        let rel = feed::parse_release(json).unwrap();
        let e = manifest_files(&rel, &|_| panic!("no download without manifest")).unwrap_err();
        assert!(e.no_manifest, "релиз без манифеста — не сбой, а «только вручную»");
        assert_eq!(e.message, trf("updo.no_manifest", &["0.3.0"]));
        assert!(e.message.contains("0.3.0") && !e.message.contains("updo."), "{}", e.message);

        let json = r#"{"tag_name":"v0.4.0","assets":[
            {"name":"update-manifest.json","browser_download_url":"https://h/m","size":1},
            {"name":"update-manifest.json.sig","browser_download_url":"https://h/s","size":1}]}"#;
        let rel = feed::parse_release(json).unwrap();
        let (j, s) = manifest_files(&rel, &|url| Ok(if url.ends_with("/m") { JSON.to_vec() } else { SIG.as_bytes().to_vec() })).unwrap();
        assert_eq!(test_key(&j, &s).unwrap().version, "0.4.0");
    }
}
