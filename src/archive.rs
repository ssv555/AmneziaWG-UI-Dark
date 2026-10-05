//! Архивы туннелей режима 2: импорт из zip (родной «Export all tunnels to zip» или наша резервная копия)
//! и резервная копия с паролем пользователя (AES-256) — на случай переустановки Windows или нового ПК.
//! Внутри архива — обычные `<имя>.conf`; такой архив открывается и в 7-Zip/WinRAR.

use std::io::{Read, Write};
use std::path::Path;

use zip::write::SimpleFileOptions;
use zip::{AesMode, CompressionMethod, ZipArchive, ZipWriter};

/// Предел размера одного конфига в архиве.
const MAX_CONF: u64 = 1 << 20;

/// Туннель из архива или файла: имя и текст конфига.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Entry {
    pub name: String,
    pub text: String,
}

#[derive(Debug, PartialEq)]
pub enum ReadError {
    /// Архив зашифрован, а пароль не задан — спросить у пользователя.
    NeedPassword,
    WrongPassword,
    Other(String),
}

/// Все `*.conf` из архива (папки внутри не важны). Пароль нужен только для зашифрованных записей.
pub fn read(file: &Path, password: Option<&str>) -> Result<Vec<Entry>, ReadError> {
    let other = |e: &dyn std::fmt::Display| ReadError::Other(crate::fsutil::io_ctx(&file, e));
    let reader = std::fs::File::open(file).map_err(|e| other(&e))?;
    let mut zip = ZipArchive::new(reader).map_err(|e| other(&e))?;
    let mut out = Vec::new();
    for i in 0..zip.len() {
        let (encrypted, path) = {
            let raw = zip.by_index_raw(i).map_err(|e| other(&e))?;
            (raw.encrypted(), raw.name().replace('\\', "/"))
        };
        let Some(name) = conf_name(&path) else { continue };
        let mut entry = match (encrypted, password) {
            (false, _) => zip.by_index(i).map_err(|e| other(&e))?,
            (true, None) => return Err(ReadError::NeedPassword),
            (true, Some(p)) => zip.by_index_decrypt(i, p.as_bytes()).map_err(|e| match e {
                zip::result::ZipError::InvalidPassword => ReadError::WrongPassword,
                e => other(&e),
            })?,
        };
        let mut bytes = Vec::new();
        // Неверный пароль AES иногда обнаруживается только при чтении (проверка подписи данных).
        // Конфиг — килобайты; больше предела — не конфиг (и не даём «zip-бомбе» съесть память).
        (&mut entry)
            .take(MAX_CONF + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| if encrypted { ReadError::WrongPassword } else { other(&e) })?;
        if bytes.len() as u64 > MAX_CONF {
            return Err(ReadError::Other(format!("{}: {path}: > {MAX_CONF} B", file.display())));
        }
        out.push(Entry { name, text: decode_text(&bytes) });
    }
    Ok(out)
}

/// Резервная копия: каждый туннель — `<имя>.conf`, зашифрован AES-256 паролем пользователя.
pub fn write_backup(file: &Path, password: &str, entries: &[Entry]) -> Result<(), String> {
    let err = |e: &dyn std::fmt::Display| crate::fsutil::io_ctx(&file, e);
    // Архив собирается в памяти (конфиги — килобайты), чтобы запись на диск была одна и атомарная.
    let mut zip = ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .with_aes_encryption(AesMode::Aes256, password);
    for e in entries {
        zip.start_file(format!("{}.conf", e.name), options).map_err(|e| err(&e))?;
        zip.write_all(e.text.as_bytes()).map_err(|e| err(&e))?;
    }
    let archive = zip.finish().map_err(|e| err(&e))?.into_inner();
    crate::fsutil::write_atomic(file, &archive).map_err(|e| err(&e))
}

/// Обычный `.conf` с диска.
pub fn read_conf(file: &Path) -> Result<Entry, String> {
    let bytes = std::fs::read(file).map_err(|e| crate::fsutil::io_ctx(&file, e))?;
    let name = crate::engine::tunnel_name(file);
    Ok(Entry { name, text: decode_text(&bytes) })
}

/// `dir/office.conf` → `office`; не `.conf` — None.
fn conf_name(path: &str) -> Option<String> {
    let file = path.rsplit('/').next()?;
    let name = file.strip_suffix(".conf").or_else(|| file.strip_suffix(".CONF"))?;
    (!name.is_empty()).then(|| name.to_string())
}

/// UTF-8 (с BOM или без) или UTF-16 LE с BOM — как сохраняют Блокнот и родной клиент.
fn decode_text(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        let units: Vec<u16> = rest.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        return String::from_utf16_lossy(&units);
    }
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("awg-ui-test-{}-{name}", std::process::id()))
    }

    fn sample() -> Vec<Entry> {
        vec![
            Entry { name: "office".into(), text: "[Interface]\nAddress = 10.0.0.2/32\n".into() },
            Entry { name: "home.nl".into(), text: "[Interface]\nAddress = 10.0.0.3/32\n".into() },
        ]
    }

    #[test]
    fn backup_roundtrip_needs_the_right_password() {
        let file = temp("backup.zip");
        write_backup(&file, "correct horse", &sample()).unwrap();
        assert_eq!(read(&file, None), Err(ReadError::NeedPassword));
        assert_eq!(read(&file, Some("wrong")), Err(ReadError::WrongPassword));
        assert_eq!(read(&file, Some("correct horse")).unwrap(), sample());
        let raw = std::fs::read(&file).unwrap();
        assert!(!raw.windows(9).any(|w| w == b"Interface"), "в архиве нет открытого текста");
        std::fs::remove_file(file).unwrap();
    }

    #[test]
    fn plain_zip_like_native_export_is_read() {
        let file = temp("export.zip");
        let mut zip = ZipWriter::new(std::fs::File::create(&file).unwrap());
        zip.start_file("office.conf", SimpleFileOptions::default()).unwrap();
        zip.write_all(b"\xEF\xBB\xBF[Interface]\n").unwrap();
        zip.start_file("readme.txt", SimpleFileOptions::default()).unwrap();
        zip.write_all(b"not a tunnel").unwrap();
        zip.finish().unwrap();
        let entries = read(&file, None).unwrap();
        assert_eq!(entries, vec![Entry { name: "office".into(), text: "[Interface]\n".into() }]);
        std::fs::remove_file(file).unwrap();
    }

    #[test]
    fn oversized_entry_is_refused() {
        let file = temp("big.zip");
        let mut zip = ZipWriter::new(std::fs::File::create(&file).unwrap());
        zip.start_file("big.conf", SimpleFileOptions::default()).unwrap();
        zip.write_all(&vec![b'#'; (MAX_CONF + 1) as usize]).unwrap();
        zip.finish().unwrap();
        assert!(matches!(read(&file, None), Err(ReadError::Other(_))));
        std::fs::remove_file(file).unwrap();
    }

    #[test]
    fn utf16_conf_is_decoded() {
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend("[Peer]".encode_utf16().flat_map(u16::to_le_bytes));
        assert_eq!(decode_text(&bytes), "[Peer]");
        assert_eq!(conf_name("dir/sub/a.b.conf").as_deref(), Some("a.b"));
        assert_eq!(conf_name("notes.txt"), None);
    }
}
