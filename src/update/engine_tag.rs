//! Версии движка из `engine\build.ps1`: общий код `build.rs` (вшивает их в exe) и тестов.

/// `$EngineTag = 'v3.1.20260814'` → `3.1.20260814`.
pub fn engine_tag(ps1: &str) -> Option<String> {
    ps1.lines().find_map(|line| {
        let value = line.trim().strip_prefix("$EngineTag")?.trim_start().strip_prefix('=')?.trim();
        let value = value.trim_matches(|c| c == '\'' || c == '"');
        version(value.strip_prefix('v').unwrap_or(value))
    })
}

/// Версия wintun из адреса архива `wintun-0.14.1.zip`.
pub fn wintun_version(ps1: &str) -> Option<String> {
    ps1.match_indices("wintun-").find_map(|(i, m)| {
        let rest = &ps1[i + m.len()..];
        version(&rest[..rest.find(".zip")?])
    })
}

/// Только цифры и точки, начинается с цифры.
fn version(v: &str) -> Option<String> {
    (v.starts_with(|c: char| c.is_ascii_digit()) && v.bytes().all(|b| b.is_ascii_digit() || b == b'.')).then(|| v.to_string())
}
