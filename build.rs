//! Вшивает в exe иконку (PNG из assets/icon) и сведения о версии.

use std::path::Path;

#[path = "src/update/engine_tag.rs"]
mod engine_tag;

/// Размеры в .ico: те же PNG, что показывает само окно.
const SIZES: [u32; 7] = [16, 24, 32, 48, 64, 128, 256];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/icon");
    pin_engine();
    engine_versions();
    let out = std::env::var("OUT_DIR").unwrap();
    let ico = Path::new(&out).join("awg-ui.ico");
    write_ico(&ico);

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon(ico.to_str().unwrap());
        res.set("FileDescription", "AmneziaWG UI Dark");
        res.set("ProductName", "awg-ui");
        res.compile().expect("ресурсы Windows (нужен rc.exe из Windows SDK)");
    }
}

/// SHA-256 файлов движка из `engine\out` (собирает `engine\build.ps1`) — в exe. Программа копирует в Program Files
/// для службы SYSTEM только DLL с этими суммами: подложенную рядом с exe библиотеку служба не загрузит.
/// Движок не собран — сумм нет, и встроенный режим в такой сборке не включается.
fn pin_engine() {
    use sha2::{Digest, Sha256};
    println!("cargo:rerun-if-changed=engine/out");
    for (file, var) in [("tunnel.dll", "AWG_TUNNEL_SHA256"), ("wintun.dll", "AWG_WINTUN_SHA256")] {
        let path = Path::new("engine/out").join(file);
        println!("cargo:rerun-if-changed={}", path.display());
        if let Ok(bytes) = std::fs::read(&path) {
            let hex: String = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
            println!("cargo:rustc-env={var}={hex}");
        }
    }
}

/// Версии движка и wintun из `engine\build.ps1` — в exe (`AWG_ENGINE_TAG`, `AWG_WINTUN_VERSION`). Не нашлись — не
/// задаются, программа покажет версию из манифеста или «нет».
fn engine_versions() {
    println!("cargo:rerun-if-changed=engine/build.ps1");
    let Ok(ps1) = std::fs::read_to_string("engine/build.ps1") else { return };
    if let Some(tag) = engine_tag::engine_tag(&ps1) {
        println!("cargo:rustc-env=AWG_ENGINE_TAG={tag}");
    }
    if let Some(v) = engine_tag::wintun_version(&ps1) {
        println!("cargo:rustc-env=AWG_WINTUN_VERSION={v}");
    }
}

/// ICO с PNG внутри (поддерживается с Windows Vista).
fn write_ico(path: &Path) {
    let images: Vec<Vec<u8>> = SIZES
        .iter()
        .map(|s| std::fs::read(format!("assets/icon/icon-{s}.png")).expect("PNG иконки в assets/icon"))
        .collect();
    let mut ico = Vec::new();
    ico.extend_from_slice(&[0, 0, 1, 0]);
    ico.extend_from_slice(&(SIZES.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * SIZES.len() as u32;
    for (size, data) in SIZES.iter().zip(&images) {
        let side = if *size >= 256 { 0 } else { *size as u8 };
        ico.extend_from_slice(&[side, side, 0, 0]);
        ico.extend_from_slice(&1u16.to_le_bytes());
        ico.extend_from_slice(&32u16.to_le_bytes());
        ico.extend_from_slice(&(data.len() as u32).to_le_bytes());
        ico.extend_from_slice(&offset.to_le_bytes());
        offset += data.len() as u32;
    }
    for data in &images {
        ico.extend_from_slice(data);
    }
    std::fs::write(path, ico).unwrap();
}
