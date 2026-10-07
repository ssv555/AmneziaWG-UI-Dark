//! Общее для тестов частей `ours`: фикстуры подписанного манифеста и работа с временными папками.

use std::path::{Path, PathBuf};

use super::fs::{Fs, RealFs};
use super::InstallTarget;
use crate::update::sign;

pub const JSON: &[u8] = include_bytes!("../../../tests/fixtures/update/manifest.json");
pub const SIG: &str = include_str!("../../../tests/fixtures/update/manifest.json.sig");
const TEST_KEY: &str = include_str!("../../../tests/fixtures/update/test_key.pub");

pub fn temp(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("awg-ui-ours-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub fn test_key(json: &[u8], sig: &str) -> Result<sign::Manifest, String> {
    sign::manifest_with_key(json, sig, TEST_KEY)
}

pub fn write_all(dir: &Path, files: &[(&str, &str)]) {
    std::fs::create_dir_all(dir).unwrap();
    for (n, d) in files {
        std::fs::write(dir.join(n), d).unwrap();
    }
}

pub fn read(dir: &Path, name: &str) -> String {
    std::fs::read_to_string(dir.join(name)).unwrap()
}

pub fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    v.sort();
    v
}

/// Цель с хранилищем и папкой окна рядом с папкой программы (не внутри: тесты сравнивают её содержимое по именам).
pub fn target(dir: &Path) -> InstallTarget {
    let beside = dir.parent().unwrap_or(dir);
    InstallTarget { dir: dir.to_path_buf(), store: beside.join("store"), window_dir: beside.join("window") }
}

/// Файловая система, где переименование задаёт тест (прочее — настоящее).
pub struct RenameFs<F>(F);

impl<F: Fn(&Path, &Path) -> std::io::Result<()>> RenameFs<F> {
    // Конструктор, а не кортежный литерал: только так замыкание получает ожидаемую сигнатуру со ссылками.
    pub fn new(rename: F) -> Self {
        RenameFs(rename)
    }
}

impl<F: Fn(&Path, &Path) -> std::io::Result<()>> Fs for RenameFs<F> {
    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        (self.0)(from, to)
    }
    fn remove_file(&self, path: &Path) -> std::io::Result<()> {
        RealFs.remove_file(path)
    }
    fn copy(&self, from: &Path, to: &Path) -> std::io::Result<u64> {
        RealFs.copy(from, to)
    }
}

/// Операция, которую `FaultFs` может сломать.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Op {
    Rename,
    RemoveFile,
    Copy,
}

/// Файловая система с отказами: `fail(операция, откуда, куда)` — `true` ломает шаг ошибкой «injected»; остальное —
/// настоящее. Для `RemoveFile` оба пути — удаляемый файл.
pub struct FaultFs<F>(F);

impl<F: Fn(Op, &Path, &Path) -> bool> FaultFs<F> {
    pub fn new(fail: F) -> Self {
        FaultFs(fail)
    }

    fn check(&self, op: Op, from: &Path, to: &Path) -> std::io::Result<()> {
        if (self.0)(op, from, to) {
            Err(std::io::Error::other("injected"))
        } else {
            Ok(())
        }
    }
}

impl<F: Fn(Op, &Path, &Path) -> bool> Fs for FaultFs<F> {
    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        self.check(Op::Rename, from, to)?;
        RealFs.rename(from, to)
    }
    fn remove_file(&self, path: &Path) -> std::io::Result<()> {
        self.check(Op::RemoveFile, path, path)?;
        RealFs.remove_file(path)
    }
    fn copy(&self, from: &Path, to: &Path) -> std::io::Result<u64> {
        self.check(Op::Copy, from, to)?;
        RealFs.copy(from, to)
    }
}

/// Как `target`, но с хранилищем обновлений в `store` (история возврата).
pub fn target_with_store(dir: &Path, store: &Path) -> InstallTarget {
    InstallTarget { store: store.to_path_buf(), ..target(dir) }
}

/// Подписант тестов: подписывает любой манифест (SSHSIG, как `ssh-keygen -Y sign`) своим ключом, чтобы тест мог
/// собрать релиз с нужными суммами; проверяет его настоящий `sign`. Ключ только для тестов, из фиксированного семени.
mod signer {
    use ed25519_dalek::{Signer, SigningKey};
    use sha2::{Digest, Sha512};

    use crate::update::sign::{self, Manifest};

    fn key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    fn put(out: &mut Vec<u8>, s: &[u8]) {
        out.extend_from_slice(&(s.len() as u32).to_be_bytes());
        out.extend_from_slice(s);
    }

    fn base64(data: &[u8]) -> String {
        const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in data.chunks(3) {
            let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | (*b as u32) << (16 - 8 * i));
            for i in 0..4 {
                if i <= chunk.len() {
                    out.push(ABC[(n >> (18 - 6 * i) & 63) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    fn key_blob() -> Vec<u8> {
        let mut blob = Vec::new();
        put(&mut blob, b"ssh-ed25519");
        put(&mut blob, key().verifying_key().as_bytes());
        blob
    }

    fn public_key() -> String {
        format!("ssh-ed25519 {} awg-ui-test-signer", base64(&key_blob()))
    }

    /// Подпись над `json` в броне SSHSIG.
    pub fn sign(json: &[u8]) -> String {
        let digest = Sha512::digest(json);
        let mut signed = b"SSHSIG".to_vec();
        for part in [sign::NAMESPACE.as_bytes(), b"", b"sha512", digest.as_slice()] {
            put(&mut signed, part);
        }
        let mut sig = Vec::new();
        put(&mut sig, b"ssh-ed25519");
        put(&mut sig, &key().sign(&signed).to_bytes());
        let mut blob = b"SSHSIG".to_vec();
        blob.extend_from_slice(&1u32.to_be_bytes());
        for part in [key_blob().as_slice(), sign::NAMESPACE.as_bytes(), b"", b"sha512", sig.as_slice()] {
            put(&mut blob, part);
        }
        let b64 = base64(&blob);
        let lines: Vec<&str> = b64.as_bytes().chunks(70).map(|c| std::str::from_utf8(c).unwrap()).collect();
        format!("-----BEGIN SSH SIGNATURE-----\n{}\n-----END SSH SIGNATURE-----\n", lines.join("\n"))
    }

    pub fn verify(json: &[u8], sig: &str) -> Result<Manifest, String> {
        sign::manifest_with_key(json, sig, &public_key())
    }
}

pub use signer::sign as sign_as_test_signer;
pub use signer::verify as signed_by_test_signer;
