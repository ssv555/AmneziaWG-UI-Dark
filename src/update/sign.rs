//! Подписанный манифест наших релизов: версии и SHA-256 файлов программы и движка. Подпись — SSHSIG (Ed25519,
//! `ssh-keygen -Y sign`), открытый ключ вшит в программу.

use std::io::Read;
use std::path::Path;

use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256, Sha512};

/// Открытый ключ, которым подписываются манифесты релизов.
#[cfg_attr(not(test), allow(dead_code))]
pub const UPDATE_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIDcLVi/t6PKyjuZeY6DRKU8BKCe+z/2v0ECZ1dVfn2LF awg-ui-update";
/// Пространство имён подписи (`ssh-keygen -Y sign -n`).
pub const NAMESPACE: &str = "awg-ui-update";

const KEY_TYPE: &str = "ssh-ed25519";

/// Манифест релиза (`update-manifest.json`).
#[derive(Serialize, Deserialize, Debug, Clone)]
#[cfg_attr(not(test), allow(dead_code))]
pub struct Manifest {
    pub version: String,
    /// Время публикации, RFC 3339.
    pub published: String,
    /// `awg-ui.exe`.
    pub app: FileEntry,
    pub engine: Engine,
}

/// Движок режима 2.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[cfg_attr(not(test), allow(dead_code))]
pub struct Engine {
    /// Тег amneziawg-windows без `v`.
    pub version: String,
    pub wintun: String,
    /// Ровно `tunnel.dll` и `wintun.dll`.
    pub files: Vec<FileEntry>,
}

/// Файл релиза: имя ассета, SHA-256 (hex, строчные), размер в байтах.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[cfg_attr(not(test), allow(dead_code))]
pub struct FileEntry {
    pub name: String,
    pub sha256: String,
    pub size: u64,
}

impl FileEntry {
    /// Размер и SHA-256 `data` совпадают с записью (сумма — строчными hex, как в манифесте).
    pub fn matches(&self, data: &[u8]) -> bool {
        data.len() as u64 == self.size && sha256_hex(data) == self.sha256
    }
}

/// SHA-256 в виде 64 строчных hex-цифр — формат сумм манифеста, вшитых сумм движка и дайджестов релиза.
pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

fn hex(digest: &[u8]) -> String {
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Манифест, подписанный ключом [`UPDATE_KEY`].
#[cfg_attr(not(test), allow(dead_code))]
pub fn manifest(json: &[u8], armored_sig: &str) -> Result<Manifest, String> {
    manifest_with_key(json, armored_sig, UPDATE_KEY)
}

/// Проверка подписи ключом `public_key`, разбор и проверка полей манифеста.
#[cfg_attr(not(test), allow(dead_code))]
pub fn manifest_with_key(json: &[u8], armored_sig: &str, public_key: &str) -> Result<Manifest, String> {
    verify(json, armored_sig, public_key)?;
    let m: Manifest = serde_json::from_slice(json).map_err(|e| format!("манифест: {e}"))?;
    validate(&m)?;
    Ok(m)
}

/// Размер и SHA-256 файла совпадают с записью манифеста.
#[cfg_attr(not(test), allow(dead_code))]
pub fn check_file(path: &Path, entry: &FileEntry) -> Result<(), String> {
    let mut f = std::fs::File::open(path).map_err(|e| crate::fsutil::io_ctx(&path, e))?;
    let size = f.metadata().map_err(|e| crate::fsutil::io_ctx(&path, e))?.len();
    if size != entry.size {
        return Err(format!("{}: размер {size}, ожидался {}", entry.name, entry.size));
    }
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).map_err(|e| crate::fsutil::io_ctx(&path, e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let hash = hex(&hasher.finalize());
    if hash != entry.sha256 {
        return Err(format!("{}: SHA-256 {hash}, ожидалась {}", entry.name, entry.sha256));
    }
    Ok(())
}

/// Проверка подписи SSHSIG (`-----BEGIN SSH SIGNATURE-----`) над `message` ключом `public_key`
/// (`ssh-ed25519 <base64> [комментарий]`) в пространстве имён [`NAMESPACE`].
#[cfg_attr(not(test), allow(dead_code))]
pub fn verify(message: &[u8], armored_sig: &str, public_key: &str) -> Result<(), String> {
    let key_blob = parse_public_key(public_key)?;
    let blob = dearmor(armored_sig)?;
    let mut r = Reader(&blob);
    if r.take(6)? != b"SSHSIG" {
        return Err("подпись: нет заголовка SSHSIG".into());
    }
    if r.u32()? != 1 {
        return Err("подпись: неизвестная версия SSHSIG".into());
    }
    let sig_key = r.string()?;
    let namespace = r.string()?;
    let reserved = r.string()?;
    let hash_alg = r.string()?;
    let sig = r.string()?;
    if !r.0.is_empty() {
        return Err("подпись: лишние данные в конце".into());
    }
    if namespace != NAMESPACE.as_bytes() {
        return Err(format!("подпись: пространство имён «{}», ожидалось «{NAMESPACE}»", String::from_utf8_lossy(namespace)));
    }
    if sig_key != key_blob.as_slice() {
        return Err("подпись: сделана другим ключом".into());
    }
    let digest: Vec<u8> = match hash_alg {
        b"sha512" => Sha512::digest(message).to_vec(),
        b"sha256" => Sha256::digest(message).to_vec(),
        _ => return Err(format!("подпись: хеш «{}» не поддерживается", String::from_utf8_lossy(hash_alg))),
    };

    // Сама подпись: string(тип ключа) string(64 байта Ed25519).
    let mut s = Reader(sig);
    if s.string()? != KEY_TYPE.as_bytes() {
        return Err("подпись: тип подписи не ssh-ed25519".into());
    }
    let raw: [u8; 64] = s.string()?.try_into().map_err(|_| "подпись: длина Ed25519 не 64 байта".to_string())?;
    if !s.0.is_empty() {
        return Err("подпись: лишние данные после подписи".into());
    }

    // Ключ из блоба: string("ssh-ed25519") string(32 байта).
    let mut k = Reader(&key_blob);
    k.string()?;
    let pk: [u8; 32] = k.string()?.try_into().map_err(|_| "ключ: длина Ed25519 не 32 байта".to_string())?;
    let vk = VerifyingKey::from_bytes(&pk).map_err(|e| format!("ключ: {e}"))?;

    let mut signed = Vec::with_capacity(64 + digest.len());
    signed.extend_from_slice(b"SSHSIG");
    for part in [namespace, reserved, hash_alg, digest.as_slice()] {
        put_string(&mut signed, part);
    }
    vk.verify_strict(&signed, &Signature::from_bytes(&raw)).map_err(|_| "подпись не сходится".to_string())
}

/// Блоб открытого ключа из строки `ssh-ed25519 <base64> [комментарий]`.
fn parse_public_key(line: &str) -> Result<Vec<u8>, String> {
    let mut parts = line.split_whitespace();
    if parts.next() != Some(KEY_TYPE) {
        return Err("ключ: ожидался ssh-ed25519".into());
    }
    let blob = parts.next().and_then(base64_decode).ok_or("ключ: неверный base64")?;
    let mut r = Reader(&blob);
    if r.string()? != KEY_TYPE.as_bytes() || r.string()?.len() != 32 || !r.0.is_empty() {
        return Err("ключ: неверный блоб ssh-ed25519".into());
    }
    Ok(blob)
}

/// Двоичный блоб из брони `-----BEGIN SSH SIGNATURE-----`.
fn dearmor(armored: &str) -> Result<Vec<u8>, String> {
    const BEGIN: &str = "-----BEGIN SSH SIGNATURE-----";
    const END: &str = "-----END SSH SIGNATURE-----";
    let text = armored.trim();
    let body = text
        .strip_prefix(BEGIN)
        .and_then(|t| t.strip_suffix(END))
        .ok_or("подпись: нет брони SSH SIGNATURE")?;
    let b64: String = body.split_whitespace().collect();
    base64_decode(&b64).ok_or_else(|| "подпись: неверный base64".into())
}

/// Стандартный base64 с выравниванием `=`; длина кратна 4.
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    }
    let bytes = s.as_bytes();
    if bytes.is_empty() || bytes.len() % 4 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let last = bytes.len() / 4 - 1;
    for (i, chunk) in bytes.chunks(4).enumerate() {
        let pad = chunk.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 || (pad > 0 && i != last) {
            return None;
        }
        let mut n = 0u32;
        for &c in &chunk[..4 - pad] {
            n = (n << 6) | val(c)?;
        }
        n <<= 6 * pad as u32;
        out.extend_from_slice(&n.to_be_bytes()[1..4 - pad]);
    }
    Some(out)
}

/// Чтение полей SSH-формата (uint32, string) с проверкой границ.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        if self.0.len() < n {
            return Err("подпись: обрезанные данные".into());
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Ok(head)
    }

    fn u32(&mut self) -> Result<u32, String> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn string(&mut self) -> Result<&'a [u8], String> {
        let n = self.u32()? as usize;
        self.take(n)
    }
}

fn put_string(out: &mut Vec<u8>, s: &[u8]) {
    out.extend_from_slice(&(s.len() as u32).to_be_bytes());
    out.extend_from_slice(s);
}

/// Версия: 1-64 символа из `[0-9A-Za-z.-]`; значение попадает в имена файлов и сообщения, лишнего в нём быть не должно.
fn validate_version(what: &str, v: &str) -> Result<(), String> {
    let ok = (1..=64).contains(&v.len()) && v.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-'));
    if !ok {
        return Err(format!("манифест: недопустимая версия {what}"));
    }
    Ok(())
}

/// Версии — по [`validate_version`], программа — `awg-ui.exe`, имена — простые имена файлов, хеши — 64 строчных hex,
/// в движке ровно `tunnel.dll` и `wintun.dll`.
fn validate(m: &Manifest) -> Result<(), String> {
    validate_version("version", &m.version)?;
    validate_version("engine.version", &m.engine.version)?;
    validate_version("engine.wintun", &m.engine.wintun)?;
    if m.app.name != "awg-ui.exe" {
        return Err(format!("манифест: app.name «{}», ожидался awg-ui.exe", m.app.name));
    }
    for f in std::iter::once(&m.app).chain(&m.engine.files) {
        validate_entry(f)?;
    }
    let mut names: Vec<&str> = m.engine.files.iter().map(|f| f.name.as_str()).collect();
    names.sort_unstable();
    if names != ["tunnel.dll", "wintun.dll"] {
        return Err(format!("манифест: файлы движка {names:?}, ожидались tunnel.dll и wintun.dll"));
    }
    Ok(())
}

fn validate_entry(f: &FileEntry) -> Result<(), String> {
    if !crate::fsutil::plain_name(&f.name) {
        return Err(format!("манифест: недопустимое имя файла «{}»", f.name));
    }
    if f.sha256.len() != 64 || !f.sha256.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')) {
        return Err(format!("манифест: {}: SHA-256 не 64 строчных hex", f.name));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &[u8] = include_bytes!("../../tests/fixtures/update/manifest.json");
    const SIG: &str = include_str!("../../tests/fixtures/update/manifest.json.sig");
    const SIG_OTHER_NS: &str = include_str!("../../tests/fixtures/update/manifest.other-namespace.sig");
    const TEST_KEY: &str = include_str!("../../tests/fixtures/update/test_key.pub");
    const OTHER_KEY: &str = include_str!("../../tests/fixtures/update/other_key.pub");

    #[test]
    fn valid_signature() {
        verify(MANIFEST, SIG, TEST_KEY).unwrap();
        let m = manifest_with_key(MANIFEST, SIG, TEST_KEY).unwrap();
        assert_eq!(m.version, "0.4.0");
        assert_eq!(m.app.name, "awg-ui.exe");
        assert_eq!(m.engine.version, "3.1.20260814");
        assert_eq!(m.engine.files.len(), 2);
    }

    #[test]
    fn tampered_message_fails() {
        let mut bad = MANIFEST.to_vec();
        let i = bad.iter().position(|&c| c == b'4').unwrap();
        bad[i] = b'5';
        assert!(verify(&bad, SIG, TEST_KEY).is_err());
    }

    #[test]
    fn wrong_namespace_fails() {
        let e = verify(MANIFEST, SIG_OTHER_NS, TEST_KEY).unwrap_err();
        assert!(e.contains("пространство имён"), "{e}");
    }

    #[test]
    fn other_key_fails() {
        assert!(verify(MANIFEST, SIG, OTHER_KEY).is_err());
        // Продакшн-ключ тоже не принимает подпись тестового.
        assert!(manifest(MANIFEST, SIG).is_err());
    }

    #[test]
    fn malformed_armor_fails() {
        assert!(verify(MANIFEST, "", TEST_KEY).is_err());
        assert!(verify(MANIFEST, "-----BEGIN SSH SIGNATURE-----\n!!!!\n-----END SSH SIGNATURE-----", TEST_KEY).is_err());
        // Обрезанный блоб: каждая длина — ошибка, не паника.
        let blob = dearmor(SIG).unwrap();
        for n in 0..blob.len() {
            let armored = format!("-----BEGIN SSH SIGNATURE-----\n{}\n-----END SSH SIGNATURE-----", b64(&blob[..n]));
            assert!(verify(MANIFEST, &armored, TEST_KEY).is_err(), "длина {n}");
        }
        // Огромная длина строки не вызывает выделения памяти и паники.
        let mut huge = blob[..10].to_vec();
        huge.extend_from_slice(&[0xff, 0xff, 0xff, 0xff]);
        let armored = format!("-----BEGIN SSH SIGNATURE-----\n{}\n-----END SSH SIGNATURE-----", b64(&huge));
        assert!(verify(MANIFEST, &armored, TEST_KEY).is_err());
    }

    #[test]
    fn manifest_validation() {
        let good: Manifest = serde_json::from_slice(MANIFEST).unwrap();
        validate(&good).unwrap();
        for name in ["../awg-ui.exe", "..\\x.exe", "dir/awg-ui.exe", "C:awg.exe", "..", ""] {
            let mut m = good.clone();
            m.app.name = name.into();
            assert!(validate(&m).is_err(), "{name}");
        }
        for hash in ["ABCDEF".repeat(10) + "abcd", "0".repeat(63), "g".repeat(64)] {
            let mut m = good.clone();
            m.engine.files[0].sha256 = hash.clone();
            assert!(validate(&m).is_err(), "{hash}");
        }
        let mut m = good.clone();
        m.engine.files[1].name = "tunnel.dll".into();
        assert!(validate(&m).is_err());
        let mut m = good;
        m.engine.files.pop();
        assert!(validate(&m).is_err());
    }

    #[test]
    fn manifest_rejects_bad_versions() {
        let good: Manifest = serde_json::from_slice(MANIFEST).unwrap();
        let long = "1".repeat(65);
        let bad = ["", "1 0", "1.0\n", "1.0/../x", "1.0_x", "1.0+x", "версия", long.as_str()];
        for v in bad {
            let mut m = good.clone();
            m.version = v.into();
            assert!(validate(&m).is_err(), "version {v:?}");
            let mut m = good.clone();
            m.engine.version = v.into();
            assert!(validate(&m).is_err(), "engine.version {v:?}");
            let mut m = good.clone();
            m.engine.wintun = v.into();
            assert!(validate(&m).is_err(), "engine.wintun {v:?}");
        }
        // Границы допустимого: 1 и 64 символа, весь алфавит.
        for v in ["1", "0.4.0-rc.1", "aZ09.-", "1".repeat(64).as_str()] {
            let mut m = good.clone();
            m.version = v.into();
            m.engine.version = v.into();
            m.engine.wintun = v.into();
            validate(&m).unwrap_or_else(|e| panic!("{v:?}: {e}"));
        }
    }

    #[test]
    fn manifest_rejects_other_app_name() {
        let good: Manifest = serde_json::from_slice(MANIFEST).unwrap();
        for name in ["tunnel.dll", "AWG-UI.EXE", "awg-ui.exe ", "other.exe"] {
            let mut m = good.clone();
            m.app.name = name.into();
            assert!(validate(&m).unwrap_err().contains("app.name"), "{name}");
        }
    }

    #[test]
    fn entry_matches_checks_size_and_lowercase_hash() {
        let ok = FileEntry {
            name: "x".into(),
            sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into(),
            size: 3,
        };
        assert_eq!(sha256_hex(b"abc"), ok.sha256);
        assert!(ok.matches(b"abc"));
        assert!(!ok.matches(b"abd"), "другое содержимое");
        assert!(!ok.matches(b"abcd"), "другой размер");
        assert!(!FileEntry { sha256: ok.sha256.to_uppercase(), ..ok.clone() }.matches(b"abc"), "сумма в верхнем регистре не принимается");
        assert_eq!(hex(&[0x00, 0x0a, 0xff]), "000aff", "ведущие нули не теряются");
    }

    #[test]
    fn check_file_ok_and_mismatch() {
        let path = std::env::temp_dir().join(format!("awg-ui-sign-{}", std::process::id()));
        std::fs::write(&path, b"abc").unwrap();
        let ok = FileEntry {
            name: "x".into(),
            sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into(),
            size: 3,
        };
        let r = check_file(&path, &ok);
        let bad_size = check_file(&path, &FileEntry { size: 4, ..ok.clone() });
        let bad_hash = check_file(&path, &FileEntry { sha256: "0".repeat(64), ..ok.clone() });
        std::fs::remove_file(&path).unwrap();
        r.unwrap();
        assert!(bad_size.is_err());
        assert!(bad_hash.is_err());
    }

    #[test]
    fn base64_roundtrip() {
        for n in 0..10 {
            let data: Vec<u8> = (0..n as u8).map(|i| i.wrapping_mul(37)).collect();
            if n == 0 {
                assert_eq!(base64_decode(""), None);
            } else {
                assert_eq!(base64_decode(&b64(&data)), Some(data));
            }
        }
    }

    /// Кодировщик base64 для тестов.
    fn b64(data: &[u8]) -> String {
        const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut s = String::new();
        for c in data.chunks(3) {
            let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
            for i in 0..4 {
                s.push(if i <= c.len() { T[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
            }
        }
        s
    }
}
