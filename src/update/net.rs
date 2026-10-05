//! HTTPS-загрузка через WinHTTP: без внешних зависимостей, с перенаправлениями, пределом размера и ходом загрузки.

use std::ffi::c_void;
use std::io::Write;
use std::path::Path;
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Networking::WinHttp::{
    WinHttpAddRequestHeaders, WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest,
    WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest,
    WinHttpSetOption, WinHttpSetTimeouts, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
    WINHTTP_ADDREQ_FLAG_ADD, WINHTTP_FLAG_SECURE, WINHTTP_OPTION_REDIRECT_POLICY,
    WINHTTP_OPTION_REDIRECT_POLICY_DISALLOW_HTTPS_TO_HTTP, WINHTTP_QUERY_CONTENT_LENGTH,
    WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE,
};

use crate::win::wide;

/// Таймаут разрешения имени, соединения, отправки и приёма, мс.
const TIMEOUT_MS: i32 = 30_000;
/// Размер куска при чтении тела.
const CHUNK: usize = 64 * 1024;
/// Общий срок `get`: таймаут чтения сбрасывается с каждым куском, и без этого медленный сервер держит запрос вечно.
const GET_DEADLINE: Duration = Duration::from_secs(2 * 60);
/// Общий срок `download`.
const DOWNLOAD_DEADLINE: Duration = Duration::from_secs(30 * 60);

/// Срок вышел: с `start` прошло не меньше `limit`.
fn expired(start: Instant, now: Instant, limit: Duration) -> bool {
    now.saturating_duration_since(start) >= limit
}

/// Ошибка просроченного срока.
fn timed_out(limit: Duration) -> String {
    format!("timed out: transfer took longer than {} s", limit.as_secs())
}

/// Дескриптор WinHTTP; закрывается сам.
struct Handle(*mut c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { WinHttpCloseHandle(self.0) };
        }
    }
}

/// Ответ с готовыми заголовками, тело ещё не прочитано. Поля закрываются в порядке объявления: запрос, соединение, сеанс.
struct Response {
    req: Handle,
    _conn: Handle,
    _session: Handle,
    /// Content-Length, если сервер его прислал.
    length: Option<u64>,
}

/// Ошибка вызова WinHTTP с кодом `GetLastError`.
fn last_error(call: &str) -> String {
    format!("{call}: error {}", unsafe { GetLastError() })
}

/// Части адреса: узел, порт, путь с запросом.
struct Url {
    host: String,
    port: u16,
    path: String,
}

/// Разбор `https://узел[:порт]/путь?запрос`; другие схемы и логин в адресе не принимаются.
fn parse_https(url: &str) -> Result<Url, String> {
    let rest = match url.get(..8) {
        Some(p) if p.eq_ignore_ascii_case("https://") => &url[8..],
        _ => return Err(format!("only https:// URLs are allowed: {url}")),
    };
    let rest = rest.split('#').next().unwrap_or("");
    let end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    if authority.is_empty() || authority.contains('@') {
        return Err(format!("bad URL: {url}"));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !p.contains(']') => {
            (h, p.parse::<u16>().map_err(|_| format!("bad port in URL: {url}"))?)
        }
        _ => (authority, 443),
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.is_empty() {
        return Err(format!("bad URL: {url}"));
    }
    let path = if tail.starts_with('/') { tail.to_string() } else { format!("/{tail}") };
    Ok(Url { host: host.to_string(), port, path })
}

/// Отправляет GET и принимает заголовки; перенаправления WinHTTP проходит сам, на http не уходит.
fn open(url: &str, accept: Option<&str>) -> Result<Response, String> {
    let u = parse_https(url)?;
    unsafe {
        let ua = wide(concat!("AmneziaWG-UI-Dark/", env!("CARGO_PKG_VERSION")));
        let session = Handle(WinHttpOpen(
            ua.as_ptr(),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            null(),
            null(),
            0,
        ));
        if session.0.is_null() {
            return Err(last_error("WinHttpOpen"));
        }
        if WinHttpSetTimeouts(session.0, TIMEOUT_MS, TIMEOUT_MS, TIMEOUT_MS, TIMEOUT_MS) == 0 {
            return Err(last_error("WinHttpSetTimeouts"));
        }
        let host = wide(&u.host);
        let conn = Handle(WinHttpConnect(session.0, host.as_ptr(), u.port, 0));
        if conn.0.is_null() {
            return Err(last_error("WinHttpConnect"));
        }
        let verb = wide("GET");
        let path = wide(&u.path);
        let req = Handle(WinHttpOpenRequest(
            conn.0,
            verb.as_ptr(),
            path.as_ptr(),
            null(),
            null(),
            null_mut(),
            WINHTTP_FLAG_SECURE,
        ));
        if req.0.is_null() {
            return Err(last_error("WinHttpOpenRequest"));
        }
        // Перенаправление с https на http запрещено (это и так значение по умолчанию, но фиксируем явно).
        let policy: u32 = WINHTTP_OPTION_REDIRECT_POLICY_DISALLOW_HTTPS_TO_HTTP;
        if WinHttpSetOption(
            req.0,
            WINHTTP_OPTION_REDIRECT_POLICY,
            &policy as *const u32 as *const c_void,
            std::mem::size_of::<u32>() as u32,
        ) == 0
        {
            return Err(last_error("WinHttpSetOption"));
        }
        if let Some(a) = accept {
            let h = wide(&format!("Accept: {a}"));
            if WinHttpAddRequestHeaders(req.0, h.as_ptr(), u32::MAX, WINHTTP_ADDREQ_FLAG_ADD) == 0 {
                return Err(last_error("WinHttpAddRequestHeaders"));
            }
        }
        if WinHttpSendRequest(req.0, null(), 0, null(), 0, 0, 0) == 0 {
            return Err(last_error("WinHttpSendRequest"));
        }
        if WinHttpReceiveResponse(req.0, null_mut()) == 0 {
            return Err(last_error("WinHttpReceiveResponse"));
        }
        let mut status: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;
        if WinHttpQueryHeaders(
            req.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            null(),
            &mut status as *mut u32 as *mut c_void,
            &mut size,
            null_mut(),
        ) == 0
        {
            return Err(last_error("WinHttpQueryHeaders(status)"));
        }
        if status != 200 {
            return Err(format!("HTTP status {status}"));
        }
        // Content-Length читаем строкой: число WinHTTP отдаёт только в 32 битах, а файлы бывают больше.
        let mut buf = [0u16; 32];
        let mut size = std::mem::size_of_val(&buf) as u32;
        let length = if WinHttpQueryHeaders(
            req.0,
            WINHTTP_QUERY_CONTENT_LENGTH,
            null(),
            buf.as_mut_ptr() as *mut c_void,
            &mut size,
            null_mut(),
        ) != 0
        {
            String::from_utf16_lossy(&buf[..(size / 2) as usize]).trim().parse::<u64>().ok()
        } else {
            None
        };
        Ok(Response { req, _conn: conn, _session: session, length })
    }
}

impl Response {
    /// Читает следующий кусок тела; 0 — тело закончилось.
    fn read(&self, buf: &mut [u8]) -> Result<usize, String> {
        let mut got: u32 = 0;
        let ok = unsafe {
            WinHttpReadData(self.req.0, buf.as_mut_ptr() as *mut c_void, buf.len() as u32, &mut got)
        };
        if ok == 0 {
            return Err(last_error("WinHttpReadData"));
        }
        Ok(got as usize)
    }
}

/// Скачивает всё тело в память; больше `max` байт — ошибка. Только `https://`.
#[cfg_attr(not(test), allow(dead_code))]
pub fn get(url: &str, accept: Option<&str>, max: usize) -> Result<Vec<u8>, String> {
    let start = Instant::now();
    let resp = open(url, accept)?;
    if let Some(len) = resp.length {
        if len > max as u64 {
            return Err(format!("response too large: {len} > {max} bytes"));
        }
    }
    let mut body = Vec::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        if expired(start, Instant::now(), GET_DEADLINE) {
            return Err(timed_out(GET_DEADLINE));
        }
        let n = resp.read(&mut buf)?;
        if n == 0 {
            return Ok(body);
        }
        if body.len() + n > max {
            return Err(format!("response too large: more than {max} bytes"));
        }
        body.extend_from_slice(&buf[..n]);
    }
}

/// Скачивает в файл `dest` (создаётся заново), вызывая `progress(скачано, всего если известно)`.
/// Возвращает число записанных байт; при любой ошибке недокачанный файл удаляется.
#[cfg_attr(not(test), allow(dead_code))]
pub fn download(
    url: &str,
    dest: &Path,
    max: u64,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<u64, String> {
    let start = Instant::now();
    let resp = open(url, None)?;
    if let Some(len) = resp.length {
        if len > max {
            return Err(format!("file too large: {len} > {max} bytes"));
        }
    }
    let result = save(&resp, dest, max, start, progress);
    if result.is_err() {
        // Вернётся причина сбоя загрузки; недокачанный файл, если не убрался, перезапишет следующая загрузка.
        let _ = std::fs::remove_file(dest);
    }
    result
}

/// Тело ответа в файл с проверкой предела и полноты.
fn save(
    resp: &Response,
    dest: &Path,
    max: u64,
    start: Instant,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<u64, String> {
    let mut file = std::fs::File::create(dest).map_err(|e| format!("create {}: {e}", dest.display()))?;
    let mut buf = vec![0u8; CHUNK];
    let mut done: u64 = 0;
    progress(0, resp.length);
    loop {
        if expired(start, Instant::now(), DOWNLOAD_DEADLINE) {
            return Err(timed_out(DOWNLOAD_DEADLINE));
        }
        let n = resp.read(&mut buf)?;
        if n == 0 {
            break;
        }
        done += n as u64;
        if done > max {
            return Err(format!("file too large: more than {max} bytes"));
        }
        file.write_all(&buf[..n]).map_err(|e| format!("write {}: {e}", dest.display()))?;
        progress(done, resp.length);
    }
    file.flush().map_err(|e| format!("write {}: {e}", dest.display()))?;
    if let Some(total) = resp.length {
        if done != total {
            return Err(format!("download truncated: {done} of {total} bytes"));
        }
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_rejects_non_https() {
        assert!(get("http://example.com/", None, 1024).unwrap_err().contains("https"));
        assert!(get("ftp://example.com/", None, 1024).is_err());
        assert!(get("example.com/x", None, 1024).is_err());
        assert!(get("", None, 1024).is_err());
    }

    #[test]
    fn download_rejects_non_https_without_file() {
        let dest = std::env::temp_dir().join("awg-net-test-reject.bin");
        let mut calls = 0;
        let r = download("http://example.com/x", &dest, 10, &mut |_, _| calls += 1);
        assert!(r.is_err());
        assert_eq!(calls, 0);
        assert!(!dest.exists());
    }

    #[test]
    fn deadline_logic() {
        let t0 = Instant::now();
        let limit = Duration::from_secs(120);
        assert!(!expired(t0, t0, limit));
        assert!(!expired(t0, t0 + Duration::from_secs(119), limit));
        assert!(expired(t0, t0 + limit, limit));
        assert!(expired(t0, t0 + Duration::from_secs(121), limit));
        // «Сейчас» раньше начала (часы не идут назад, но функция не должна паниковать): срок не вышел.
        assert!(!expired(t0 + limit, t0, limit));
        assert!(timed_out(GET_DEADLINE).contains("timed out"));
        assert!(GET_DEADLINE < DOWNLOAD_DEADLINE);
    }

    #[test]
    fn parse_url_parts() {
        let u = parse_https("https://api.github.com/repos/a/b/releases/latest").unwrap();
        assert_eq!((u.host.as_str(), u.port, u.path.as_str()), ("api.github.com", 443, "/repos/a/b/releases/latest"));
        let u = parse_https("HTTPS://host:8443/p?q=1#frag").unwrap();
        assert_eq!((u.host.as_str(), u.port, u.path.as_str()), ("host", 8443, "/p?q=1"));
        let u = parse_https("https://host?q=1").unwrap();
        assert_eq!(u.path, "/?q=1");
        assert!(parse_https("https://user@host/").is_err());
        assert!(parse_https("https:///x").is_err());
        assert!(parse_https("https://host:99999/").is_err());
    }

    /// Сеть: небольшой файл с raw.githubusercontent.com скачивается, ход и размер сходятся.
    #[test]
    #[ignore]
    fn live_download_small() {
        let dest = std::env::temp_dir().join("awg-net-live-readme.md");
        let _ = std::fs::remove_file(&dest);
        let (mut last, mut total) = (0u64, None);
        let n = download(
            "https://raw.githubusercontent.com/amnezia-vpn/amneziawg-windows-client/master/README.md",
            &dest,
            1 << 20,
            &mut |d, t| {
                last = d;
                total = t;
            },
        )
        .unwrap();
        let on_disk = std::fs::metadata(&dest).unwrap().len();
        let _ = std::fs::remove_file(&dest);
        println!("live_download_small: {n} bytes, progress last={last} total={total:?}");
        assert!(n > 0);
        assert_eq!(n, on_disk);
        assert_eq!(n, last);
    }

    /// Сеть: предел размера срабатывает и при чтении, и по Content-Length; недокачанный файл удаляется.
    #[test]
    #[ignore]
    fn live_max_enforced() {
        let url = "https://raw.githubusercontent.com/amnezia-vpn/amneziawg-windows-client/master/README.md";
        assert!(get(url, None, 10).unwrap_err().contains("too large"));
        let dest = std::env::temp_dir().join("awg-net-live-max.md");
        assert!(download(url, &dest, 10, &mut |_, _| {}).unwrap_err().contains("too large"));
        assert!(!dest.exists());
        let missing = "https://raw.githubusercontent.com/amnezia-vpn/amneziawg-windows-client/master/NO-SUCH-FILE";
        assert!(get(missing, None, 1024).unwrap_err().contains("404"));
    }
}
