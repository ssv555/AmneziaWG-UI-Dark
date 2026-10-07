//! Релизы GitHub (REST API): последняя версия, релиз по тегу, файлы релиза с их SHA-256.

use serde::Deserialize;

use super::net;

/// Репозиторий оригинального клиента AmneziaWG.
#[cfg_attr(not(test), allow(dead_code))]
pub const NATIVE_REPO: &str = "amnezia-vpn/amneziawg-windows-client";
/// Репозиторий этой программы.
#[cfg_attr(not(test), allow(dead_code))]
pub const APP_REPO: &str = "ssv555/AmneziaWG-UI-Dark";

/// Предел ответа API, байт.
pub(super) const API_MAX: usize = 2 * 1024 * 1024;
/// Предел текста релиза, символов.
const NOTES_MAX: usize = 2000;

/// Релиз GitHub.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub tag: String,
    /// Тег без ведущей `v`.
    pub version: String,
    /// Дата выхода (unix, сек); 0 — не указана.
    pub published: u64,
    /// Текст релиза: обрезан по пробелам и до ~2000 символов.
    pub notes: String,
    pub assets: Vec<Asset>,
}

/// Файл релиза.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    /// `browser_download_url`.
    pub url: String,
    pub size: u64,
    /// SHA-256 в нижнем регистре, из поля `digest` (`sha256:<hex>`); у старых релизов поля нет.
    pub sha256: Option<String>,
}

#[derive(Deserialize)]
struct RawRelease {
    tag_name: String,
    published_at: Option<String>,
    body: Option<String>,
    #[serde(default)]
    assets: Vec<RawAsset>,
}

#[derive(Deserialize)]
struct RawAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
    digest: Option<String>,
}

impl Release {
    /// Файл релиза по точному имени.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|a| a.name == name)
    }
}

/// `sha256:<64 hex>` → hex в нижнем регистре; другой алгоритм или мусор — `None`.
fn parse_digest(digest: &str) -> Option<String> {
    let (alg, hex) = digest.split_once(':')?;
    if !alg.eq_ignore_ascii_case("sha256") || hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(hex.to_ascii_lowercase())
}

/// Текст релиза: без пробелов по краям, не длиннее `NOTES_MAX` символов (резать по границе символа).
fn trim_notes(body: &str) -> String {
    let body = body.trim();
    match body.char_indices().nth(NOTES_MAX) {
        Some((end, _)) => body[..end].trim_end().to_string(),
        None => body.to_string(),
    }
}

/// Разбор JSON релиза из API; лишние поля игнорируются.
#[cfg_attr(not(test), allow(dead_code))]
pub fn parse_release(json: &str) -> Result<Release, String> {
    let raw: RawRelease = serde_json::from_str(json).map_err(|e| format!("release JSON: {e}"))?;
    let published = raw
        .published_at
        .as_deref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map_or(0, |t| t.timestamp().max(0) as u64);
    let version = raw.tag_name.strip_prefix('v').unwrap_or(&raw.tag_name).to_string();
    Ok(Release {
        version,
        tag: raw.tag_name,
        published,
        notes: trim_notes(raw.body.as_deref().unwrap_or("")),
        assets: raw
            .assets
            .into_iter()
            .map(|a| Asset {
                name: a.name,
                url: a.browser_download_url,
                size: a.size,
                sha256: a.digest.as_deref().and_then(parse_digest),
            })
            .collect(),
    })
}

/// Запрос релиза по адресу API.
fn fetch(url: &str) -> Result<Release, String> {
    let body = net::get(url, Some("application/vnd.github+json"), API_MAX)?;
    let text = String::from_utf8(body).map_err(|_| "release JSON: not UTF-8".to_string())?;
    parse_release(&text)
}

/// Последний опубликованный релиз репозитория (`owner/name`).
#[cfg_attr(not(test), allow(dead_code))]
pub fn latest(repo: &str) -> Result<Release, String> {
    fetch(&format!("https://api.github.com/repos/{repo}/releases/latest"))
}

/// Релиз репозитория по тегу.
#[cfg_attr(not(test), allow(dead_code))]
pub fn by_tag(repo: &str, tag: &str) -> Result<Release, String> {
    let mut enc = String::new();
    for b in tag.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_') {
            enc.push(b as char);
        } else {
            enc.push_str(&format!("%{b:02X}"));
        }
    }
    fetch(&format!("https://api.github.com/repos/{repo}/releases/tags/{enc}"))
}

/// Репозиторий движка, из которого собран `tunnel.dll`: у него свои метки, их и сверяем с нашей.
pub const ENGINE_UPSTREAM_REPO: &str = "amnezia-vpn/amneziawg-windows";
/// Страниц меток по 100 штук; столько же смотрит `engine\check-upstream.ps1`.
const TAG_PAGES: u32 = 10;

#[derive(Deserialize)]
struct RawListedRelease {
    tag_name: String,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    draft: bool,
}

#[derive(Deserialize)]
struct RawTag {
    name: String,
}

/// Метка стабильного релиза: `v` и числа через точку (`v3.1.20260814`); `-rc1` и прочие суффиксы — нет.
fn is_stable_tag(name: &str) -> bool {
    let Some(rest) = name.strip_prefix('v') else { return false };
    !rest.is_empty() && rest.split('.').all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// Метки релизов, отмеченных как предварительные или черновики: их метки пропускаются (правило
/// `engine\check-upstream.ps1`).
fn unstable_tags(releases_json: &str) -> Result<Vec<String>, String> {
    let list: Vec<RawListedRelease> = serde_json::from_str(releases_json).map_err(|e| format!("releases JSON: {e}"))?;
    Ok(list.into_iter().filter(|r| r.prerelease || r.draft).map(|r| r.tag_name).collect())
}

/// Самая новая стабильная метка среди `names`, кроме `skip`.
fn newest_stable_tag<'a>(names: impl IntoIterator<Item = &'a str>, skip: &[String]) -> Option<String> {
    let mut best: Option<&str> = None;
    for name in names {
        if is_stable_tag(name) && !skip.iter().any(|s| s == name) && best.map_or(true, |b| newer(name, b)) {
            best = Some(name);
        }
    }
    best.map(str::to_string)
}

fn fetch_text(url: &str) -> Result<String, String> {
    let body = net::get(url, Some("application/vnd.github+json"), API_MAX)?;
    String::from_utf8(body).map_err(|_| format!("{url}: not UTF-8"))
}

/// Самая новая стабильная метка репозитория `repo` (`owner/name`). Метки, а не «последний релиз»: у движка релизы
/// не всегда оформлены, зато метка есть у каждой версии.
pub fn latest_stable_tag(repo: &str) -> Result<String, String> {
    let skip = unstable_tags(&fetch_text(&format!("https://api.github.com/repos/{repo}/releases?per_page=100"))?)?;
    let mut names: Vec<String> = Vec::new();
    for page in 1..=TAG_PAGES {
        let text = fetch_text(&format!("https://api.github.com/repos/{repo}/tags?per_page=100&page={page}"))?;
        let tags: Vec<RawTag> = serde_json::from_str(&text).map_err(|e| format!("tags JSON: {e}"))?;
        let last = tags.len() < 100;
        names.extend(tags.into_iter().map(|t| t.name));
        if last {
            break;
        }
    }
    newest_stable_tag(names.iter().map(String::as_str), &skip).ok_or_else(|| format!("no stable tag found in {repo}"))
}

/// Версия `candidate` новее `installed`: точечное сравнение по частям, недостающие части — 0, ведущая `v` не в счёт.
/// Нечисловая часть сравнивается как строка; равные версии — `false`.
#[cfg_attr(not(test), allow(dead_code))]
pub fn newer(candidate: &str, installed: &str) -> bool {
    let parts = |v: &str| -> Vec<String> {
        v.trim().strip_prefix(['v', 'V']).unwrap_or(v.trim()).split('.').map(str::to_string).collect()
    };
    let (a, b) = (parts(candidate), parts(installed));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).map_or("0", String::as_str), b.get(i).map_or("0", String::as_str));
        let ord = match (x.parse::<u64>(), y.parse::<u64>()) {
            (Ok(x), Ok(y)) => x.cmp(&y),
            _ => x.cmp(y),
        };
        if ord != std::cmp::Ordering::Equal {
            return ord == std::cmp::Ordering::Greater;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789ABCDEF";

    fn fixture() -> String {
        format!(
            r#"{{
  "url": "https://api.github.com/repos/o/r/releases/1",
  "tag_name": "v3.1.1",
  "name": "AmneziaWG 3.1.1",
  "draft": false,
  "prerelease": false,
  "published_at": "2026-08-15T10:20:30Z",
  "body": "  \r\nЧто нового\r\n- исправление\r\n  ",
  "author": {{"login": "x", "id": 1}},
  "assets": [
    {{"name": "amneziawg-amd64-3.1.1.msi", "browser_download_url": "https://github.com/o/r/releases/download/v3.1.1/amneziawg-amd64-3.1.1.msi",
      "size": 5242880, "digest": "sha256:{HASH}", "download_count": 7, "content_type": "application/x-msi"}},
    {{"name": "old.zip", "browser_download_url": "https://github.com/o/r/releases/download/v3.1.1/old.zip",
      "size": 10, "digest": null}},
    {{"name": "none.zip", "browser_download_url": "https://github.com/o/r/releases/download/v3.1.1/none.zip", "size": 11}}
  ]
}}"#
        )
    }

    #[test]
    fn parses_release() {
        let r = parse_release(&fixture()).unwrap();
        assert_eq!(r.tag, "v3.1.1");
        assert_eq!(r.version, "3.1.1");
        assert_eq!(r.published, 1_786_789_230);
        assert_eq!(r.notes, "Что нового\r\n- исправление");
        assert_eq!(r.assets.len(), 3);
        let a = r.asset("amneziawg-amd64-3.1.1.msi").unwrap();
        assert_eq!(a.size, 5_242_880);
        assert!(a.url.ends_with("/v3.1.1/amneziawg-amd64-3.1.1.msi"));
        assert_eq!(a.sha256.as_deref(), Some(HASH.to_ascii_lowercase().as_str()));
        assert!(r.asset("missing").is_none());
    }

    #[test]
    fn digest_absent_or_odd_is_none() {
        let r = parse_release(&fixture()).unwrap();
        assert_eq!(r.asset("old.zip").unwrap().sha256, None);
        assert_eq!(r.asset("none.zip").unwrap().sha256, None);
        assert_eq!(parse_digest("sha512:abcd"), None);
        assert_eq!(parse_digest("sha256:zz"), None);
        assert_eq!(parse_digest("nocolon"), None);
        assert_eq!(parse_digest(&format!("SHA256:{HASH}")), Some(HASH.to_ascii_lowercase()));
    }

    #[test]
    fn minimal_release_and_errors() {
        let r = parse_release(r#"{"tag_name":"3.0"}"#).unwrap();
        assert_eq!((r.version.as_str(), r.published, r.notes.as_str(), r.assets.len()), ("3.0", 0, "", 0));
        let r = parse_release(r#"{"tag_name":"v1","body":null,"published_at":null,"assets":[]}"#).unwrap();
        assert_eq!((r.version.as_str(), r.published), ("1", 0));
        assert!(parse_release("{}").is_err());
        assert!(parse_release("not json").is_err());
    }

    #[test]
    fn notes_cut_on_char_boundary() {
        let long = "Привет, мир! ".repeat(400);
        let cut = trim_notes(&long);
        assert!(cut.chars().count() <= NOTES_MAX);
        assert!(cut.chars().count() >= NOTES_MAX - 1);
        assert!(long.starts_with(&cut));
        let exact = "ы".repeat(NOTES_MAX);
        assert_eq!(trim_notes(&exact), exact);
        assert_eq!(trim_notes(&format!("{exact}ы")).chars().count(), NOTES_MAX);
        let json = format!(r#"{{"tag_name":"v1","body":"{long}"}}"#);
        assert!(parse_release(&json).unwrap().notes.chars().count() <= NOTES_MAX);
    }

    #[test]
    fn newer_table() {
        let t = [
            ("3.1.1", "3.1.0", true),
            ("3.1.0", "3.1", false),
            ("3.1", "3.1.0", false),
            ("3.1.20260815", "3.1.20260814", true),
            ("3.1.20260814", "3.1.20260815", false),
            ("v0.4.0", "0.3.9", true),
            ("0.3.9", "v0.4.0", false),
            ("0.4.0", "v0.4.0", false),
            ("3.10", "3.9", true),
            ("3.1.0.1", "3.1", true),
            ("1.0.0-beta", "1.0.0", true),
            ("2", "1.9.9", true),
            ("", "0", false),
        ];
        for (c, i, want) in t {
            assert_eq!(newer(c, i), want, "newer({c:?}, {i:?})");
        }
    }

    #[test]
    fn repos_are_set() {
        assert_eq!(NATIVE_REPO, "amnezia-vpn/amneziawg-windows-client");
        assert_eq!(APP_REPO, "ssv555/AmneziaWG-UI-Dark");
        assert_eq!(ENGINE_UPSTREAM_REPO, "amnezia-vpn/amneziawg-windows");
    }

    #[test]
    fn stable_tag_shape() {
        for ok in ["v3.1.20260814", "v1", "v10.2"] {
            assert!(is_stable_tag(ok), "{ok}");
        }
        for bad in ["3.1.1", "v", "v3.", "v3..1", "v3.1-rc1", "v3.1.1+x", "main", "vx"] {
            assert!(!is_stable_tag(bad), "{bad}");
        }
    }

    #[test]
    fn newest_stable_tag_skips_prereleases_and_compares_numerically() {
        let tags = ["v3.1.9", "v3.1.20260814", "v3.2.0-rc1", "v3.1.20260901", "nightly", "v3.1.100"];
        assert_eq!(newest_stable_tag(tags, &[]).as_deref(), Some("v3.1.20260901"));
        let skip = vec!["v3.1.20260901".to_string()];
        assert_eq!(newest_stable_tag(tags, &skip).as_deref(), Some("v3.1.20260814"), "метка релиза-предварительного пропускается");
        assert_eq!(newest_stable_tag(["v3.0-rc1", "main"], &[]), None);
    }

    #[test]
    fn unstable_tags_come_from_prerelease_and_draft_flags() {
        let json = r#"[{"tag_name":"v3.2","prerelease":true},{"tag_name":"v3.1","draft":true,"prerelease":false},
            {"tag_name":"v3.0","prerelease":false},{"tag_name":"v2.9"}]"#;
        assert_eq!(unstable_tags(json).unwrap(), ["v3.2", "v3.1"]);
        assert!(unstable_tags("{}").is_err());
    }

    /// Сеть: у последнего релиза оригинального клиента есть MSI для amd64 с SHA-256.
    #[test]
    #[ignore]
    fn live_latest_native() {
        let r = latest(NATIVE_REPO).unwrap();
        println!("live_latest_native: tag={} version={} published={} assets={}", r.tag, r.version, r.published, r.assets.len());
        let msi = r
            .assets
            .iter()
            .find(|a| a.name.starts_with("amneziawg-amd64-") && a.name.ends_with(".msi"))
            .expect("amneziawg-amd64-*.msi");
        println!("live_latest_native: {} size={} sha256={:?}", msi.name, msi.size, msi.sha256);
        assert!(msi.sha256.as_deref().is_some_and(|h| h.len() == 64));
        assert!(msi.size > 0 && msi.url.starts_with("https://"));
        assert!(r.published > 0 && !r.version.starts_with('v'));
        let same = by_tag(NATIVE_REPO, &r.tag).unwrap();
        assert_eq!(same.tag, r.tag);
    }
}
