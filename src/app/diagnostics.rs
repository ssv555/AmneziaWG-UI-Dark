//! «Скопировать диагностику»: текстовый отчёт для разбора жалоб — версии, режим, Windows, состояние ядра и агента,
//! туннели (только имена и состояние), последние события, версии компонентов обновления.
//!
//! Отчёт вставляют в публичные обращения, поэтому в нём нет ничего личного. Это держится не на том, что каждый
//! источник «не кладёт лишнего», а на одном проходе `scrub` по готовому тексту: ключи, адреса, имя пользователя
//! и компьютера, путь профиля вырезаются из всего, что попало в отчёт, включая тексты событий и ошибок.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};

use crate::daemon::agent::client::AgentApi;
use crate::daemon::CoreApi;
use crate::events::Event;
use crate::fmt;
use crate::i18n::tr;
use crate::monitor::{Snapshot, Stamp};
use crate::settings::Mode;
use crate::update::{Component, UpdateOp, UpdatesState};

use super::App;

/// Сколько последних событий журнала идёт в отчёт.
const EVENTS: usize = 50;

/// Состояние туннеля в отчёте.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TunnelState {
    Up,
    /// Подключается или отключается (команда пользователя в работе).
    Busy,
    /// Ядро переподключает туннель по расписанию.
    Reconnecting,
    Off,
    /// Связи с ядром нет — состояние неизвестно.
    Unknown,
}

impl TunnelState {
    fn as_str(self) -> &'static str {
        match self {
            TunnelState::Up => "connected",
            TunnelState::Busy => "busy",
            TunnelState::Reconnecting => "reconnecting",
            TunnelState::Off => "off",
            TunnelState::Unknown => "unknown (no link to the core)",
        }
    }
}

/// Ядро или агент, как их видит окно: ответ на `Hello` (версия или ошибка) и с какого момента окно видит это состояние.
pub(super) struct Process {
    pub version: Result<String, String>,
    pub seen: Stamp,
}

impl Process {
    fn line(&self, name: &str, window_version: &str) -> String {
        let (state, detail) = match &self.version {
            Ok(v) if v == window_version => ("up", format!("version {v}")),
            Ok(v) => ("up", format!("version {v}, differs from the window ({window_version})")),
            Err(e) => ("down", e.clone()),
        };
        let mut line = format!("{name}: {state}, {detail}");
        // Время берём, только если наблюдение согласно с ответом: только что ожившее ядро ещё числится «не отвечает».
        if self.seen.since != 0 && self.seen.down == self.version.is_err() {
            line.push_str(&format!(", since {} (as seen by the window)", fmt::date_time(self.seen.since)));
        }
        line
    }
}

/// Всё, из чего строится отчёт. Собирает `App::copy_diagnostics`; `build` — чистая функция над этим.
pub(super) struct Facts {
    pub now: u64,
    pub app: String,
    pub mode: Mode,
    pub windows: String,
    pub core: Process,
    /// `None` — демо-режим: агента нет.
    pub agent: Option<Process>,
    pub tunnels: Vec<(String, TunnelState)>,
    /// Компонент → установленная версия; ошибка — агент не ответил.
    pub components: Result<Vec<(String, String)>, String>,
    /// От старых к новым.
    pub events: Vec<Event>,
}

/// Кто «личный» на этом компьютере: из окружения, а в тестах — подставной.
#[derive(Clone, Debug, Default)]
pub(super) struct Identity {
    pub profile: String,
    pub user: String,
    pub machine: String,
}

impl Identity {
    pub(super) fn from_env() -> Identity {
        let var = |name: &str| std::env::var(name).unwrap_or_default();
        Identity { profile: var("USERPROFILE"), user: var("USERNAME"), machine: var("COMPUTERNAME") }
    }
}

pub(super) fn build(facts: &Facts, who: &Identity) -> String {
    let mut out = Vec::new();
    out.push(format!("{} diagnostics", crate::APP_TITLE));
    out.push(format!("Generated: {}", fmt::date_time(facts.now)));
    out.push(format!("App: {}", facts.app));
    out.push(format!("Mode: {}", facts.mode.as_str()));
    out.push(format!("Windows: {}", facts.windows));
    out.push(facts.core.line("Core", &facts.app));
    out.push(match &facts.agent {
        Some(agent) => agent.line("Agent", &facts.app),
        None => "Agent: none (demo mode)".to_string(),
    });
    out.push(String::new());
    out.push("Components:".to_string());
    match &facts.components {
        Ok(list) if list.is_empty() => out.push("  (no data)".to_string()),
        Ok(list) => out.extend(list.iter().map(|(name, version)| format!("  {name}: {version}"))),
        Err(e) => out.push(format!("  unavailable: {e}")),
    }
    out.push(String::new());
    out.push(format!("Tunnels ({}):", facts.tunnels.len()));
    out.extend(facts.tunnels.iter().map(|(name, state)| format!("  {name}: {}", state.as_str())));
    out.push(String::new());
    let skip = facts.events.len().saturating_sub(EVENTS);
    out.push(format!("Last {} events:", facts.events.len() - skip));
    out.extend(facts.events[skip..].iter().map(event_line));
    let keep: Vec<String> = facts.tunnels.iter().map(|(name, _)| name.clone()).collect();
    scrub(&out.join("\r\n"), who, &keep)
}

fn event_line(e: &Event) -> String {
    let tunnel = if e.tunnel.is_empty() { String::new() } else { format!("{}: ", e.tunnel) };
    // Одна строка на событие, чтобы отчёт читался построчно.
    let text = e.text.split_whitespace().collect::<Vec<_>>().join(" ");
    format!("  {} {} {tunnel}{text}", fmt::date_time_sec(e.at), e.severity.as_str())
}

// --- сбор

/// Состояния туннелей из снимка опроса.
pub(super) fn tunnel_states(snap: &Snapshot, pending: &BTreeMap<String, &'static str>) -> Vec<(String, TunnelState)> {
    snap.tunnels
        .iter()
        .map(|name| {
            let state = if snap.core_lost {
                TunnelState::Unknown
            } else if pending.contains_key(name) {
                TunnelState::Busy
            } else if snap.running.contains_key(name) {
                TunnelState::Up
            } else if snap.retries.contains_key(name) {
                TunnelState::Reconnecting
            } else {
                TunnelState::Off
            };
            (name.clone(), state)
        })
        .collect()
}

/// Версии компонентов из ответа агента о состоянии обновлений.
pub(super) fn component_versions(state: &UpdatesState) -> Vec<(String, String)> {
    state
        .components
        .iter()
        .filter_map(|c| {
            let name = match c.component? {
                Component::Native => "AmneziaWG",
                Component::Engine => "Engine",
                Component::App => "App",
            };
            let mut shown = c.installed.clone().unwrap_or_else(|| "not installed".to_string());
            if let (true, Some(found)) = (c.update, &c.available) {
                shown.push_str(&format!(" (update available: {})", found.shown()));
            }
            Some((name.to_string(), shown))
        })
        .collect()
}

/// Версия Windows из `reg query "HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion"`: «Windows 11 Pro 24H2 (build 26100.1)».
pub(super) fn parse_windows_version(reg_output: &str) -> Option<String> {
    let value = |name: &str| {
        reg_output.lines().find_map(|line| {
            let mut words = line.split_whitespace();
            (words.next()? == name).then(|| {
                let kind = words.next()?;
                let rest = &line[line.find(kind)? + kind.len()..];
                Some(rest.trim().to_string())
            })?
        })
    };
    let build: u32 = value("CurrentBuildNumber")?.parse().ok()?;
    let mut name = value("ProductName")?;
    // Windows 11 по-прежнему называется в реестре «Windows 10 …».
    if build >= 22000 {
        name = name.replacen("Windows 10", "Windows 11", 1);
    }
    let release = value("DisplayVersion").or_else(|| value("ReleaseId")).map(|v| format!(" {v}")).unwrap_or_default();
    let revision = value("UBR")
        .and_then(|v| u32::from_str_radix(v.trim_start_matches("0x"), 16).ok())
        .map(|u| format!(".{u}"))
        .unwrap_or_default();
    Some(format!("{name}{release} (build {build}{revision})"))
}

fn windows_version() -> String {
    let out = crate::win::hidden_command("reg").args(["query", r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion"]).output();
    match out {
        Ok(o) if o.status.success() => parse_windows_version(&String::from_utf8_lossy(&o.stdout)).unwrap_or_else(|| "unknown (registry data unreadable)".into()),
        Ok(o) => format!("unknown (reg exit code {})", o.status.code().unwrap_or(-1)),
        Err(e) => format!("unknown ({e})"),
    }
}

/// Ответы ядра и агента на сбор отчёта.
pub(super) struct Answers {
    pub core: Result<String, String>,
    /// `None` — агента нет (демо).
    pub agent: Option<Result<String, String>>,
    pub components: Result<Vec<(String, String)>, String>,
}

/// Что окно спрашивает у ядра и агента при сборе: версии и состояние обновлений. Каждый ответ в отчёте — как есть, в том
/// числе «не отвечает»: ради этого отчёт и просят.
pub(super) fn ask(core: &dyn CoreApi, agent: Option<&dyn AgentApi>) -> Answers {
    let components = match agent {
        Some(a) => a.updates(UpdateOp::State).map(|s| component_versions(&s)).map_err(|e| e.to_string()),
        None => Err("no secondary service (demo mode)".to_string()),
    };
    Answers { core: core.hello().map(|(version, _)| version), agent: agent.map(|a| a.version()), components }
}


impl App {
    /// Собрать отчёт и положить в буфер обмена. Ядро и агент спрашиваются в фоне: зависший не должен держать окно.
    pub(super) fn copy_diagnostics(&self) {
        let (tunnels, stamps) = {
            let view = self.shared.frame_view();
            (tunnel_states(&view.snap, &view.pending), self.shared.link_stamps())
        };
        let events: Vec<Event> = self.shared.with_events(|log| {
            let skip = log.items.len().saturating_sub(EVENTS);
            log.items.iter().skip(skip).cloned().collect()
        });
        let (core, agent, ctx, notice, mode) = (self.core.clone(), self.agent.clone(), self.ctx.clone(), self.notice.clone(), self.s.mode());
        std::thread::spawn(move || {
            let answers = ask(core.as_ref(), agent.as_deref());
            let facts = Facts {
                now: crate::monitor::unix_now(),
                app: env!("CARGO_PKG_VERSION").to_string(),
                mode,
                windows: windows_version(),
                core: Process { version: answers.core, seen: stamps.0 },
                agent: answers.agent.map(|version| Process { version, seen: stamps.1 }),
                tunnels,
                components: answers.components,
                events,
            };
            ctx.copy_text(build(&facts, &Identity::from_env()));
            notice.done(tr("diag.copied"));
            ctx.request_repaint();
        });
    }
}

// --- вычистка личного

/// Из готового отчёта: путь профиля → `%USERPROFILE%`, имя в пути `\Users\…`, имя пользователя и компьютера, адреса
/// (IPv4/IPv6, с портом и без), `хост:порт`, голые доменные имена, ключи (длинные токены base64/hex).
/// Голое имя — это `метка.метка…` с буквенной последней меткой; от имён файлов его отличает список расширений
/// (`FILE_EXTENSIONS`), от собственных адресов программы — `OWN_HOSTS`, от имён туннелей (их отчёт показывает
/// намеренно, а `my.tunnel` по виду — доменное имя) — `keep`. Версии (`0.5.0`) и времена (`12:34:56`) не подходят: в них
/// нет буквенной последней метки.
fn scrub(text: &str, who: &Identity, keep: &[String]) -> String {
    let mut text = text.to_string();
    if !who.profile.is_empty() {
        text = replace_ci(&text, &who.profile, "%USERPROFILE%", false);
    }
    text = hide_user_dirs(&text);
    for (word, with) in [(&who.user, "<user>"), (&who.machine, "<host>")] {
        if !word.is_empty() {
            text = replace_ci(&text, word, with, true);
        }
    }
    scrub_tokens(&text, keep)
}

/// Без учёта регистра (по символам, не по байтам: имя может быть кириллицей); `whole` — только слово целиком.
fn replace_ci(text: &str, needle: &str, with: &str, whole: bool) -> String {
    let needle: Vec<char> = needle.chars().collect();
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let same = |a: char, b: char| a == b || a.to_lowercase().eq(b.to_lowercase());
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let (mut out, mut i) = (String::new(), 0);
    while i < chars.len() {
        let end = i + needle.len();
        let hit = end <= chars.len()
            && chars[i..end].iter().zip(&needle).all(|(&(_, a), &b)| same(a, b))
            && (!whole || (!(i > 0 && word(chars[i - 1].1)) && !chars.get(end).is_some_and(|&(_, c)| word(c))));
        if hit {
            out.push_str(with);
            i = end;
        } else {
            out.push(chars[i].1);
            i += 1;
        }
    }
    out
}

/// `\Users\<имя>` → `\Users\<user>`: профили других учёток и пути без переменной окружения.
fn hide_user_dirs(text: &str) -> String {
    const MARK: &str = "\\users\\";
    let lower = text.to_ascii_lowercase();
    let (mut out, mut at) = (String::new(), 0);
    while let Some(found) = lower[at..].find(MARK) {
        let name_start = at + found + MARK.len();
        out.push_str(&text[at..name_start]);
        let name_len = text[name_start..].find(|c: char| matches!(c, '\\' | '/' | '"' | '\'' | '<' | '>' | '\r' | '\n')).unwrap_or(text.len() - name_start);
        // Имя профиля может содержать пробел («John Smith»): сначала профиль из окружения заменён целиком, здесь —
        // остальные; для них режем по первому разделителю.
        if name_len == 0 {
            at = name_start;
            continue;
        }
        out.push_str("<user>");
        at = name_start + name_len;
    }
    out.push_str(&text[at..]);
    out
}

fn is_delimiter(c: char) -> bool {
    c.is_whitespace() || matches!(c, ',' | ';' | '(' | ')' | '"' | '\'' | '<' | '>' | '{' | '}')
}

fn scrub_tokens(text: &str, keep: &[String]) -> String {
    let (mut out, mut token) = (String::new(), String::new());
    for c in text.chars() {
        if is_delimiter(c) {
            flush_token(&mut token, &mut out, keep);
            out.push(c);
        } else {
            token.push(c);
        }
    }
    flush_token(&mut token, &mut out, keep);
    out
}

fn flush_token(token: &mut String, out: &mut String, keep: &[String]) {
    let core_len = token.trim_end_matches(['.', ':', '!', '?']).len();
    let (core, tail) = token.split_at(core_len);
    out.push_str(&scrub_token(core, keep));
    out.push_str(tail);
    token.clear();
}

fn scrub_token(token: &str, keep: &[String]) -> String {
    if let Some(url) = scrub_url(token) {
        return url;
    }
    // `имя=значение`: имя оставляем, значение разбираем.
    if let Some((name, value)) = token.split_once('=') {
        if !name.is_empty() && !value.is_empty() && name.chars().all(|c| c.is_ascii_alphabetic() || c == '_') {
            return format!("{name}={}", scrub_token(value, keep));
        }
    }
    if is_address(token) {
        "<ip>".to_string()
    } else if is_host_port(token) {
        "<endpoint>".to_string()
    } else if is_bare_host(token) && !keep.iter().any(|name| name.eq_ignore_ascii_case(token)) {
        "<host>".to_string()
    } else if is_key(token) {
        "<key>".to_string()
    } else {
        token.to_string()
    }
}

/// `схема://[пользователь@]хост[:порт]/путь?запрос`: схему, путь и запрос оставляем, а хост с пользователем и портом
/// (в них адрес сервера или логин) заменяем на `<host>`. Свои адреса программы (`OWN_HOSTS`, без пользователя) не трогаем.
/// `None` — токен не URL. Обязательно до разбора `имя=значение` и до проверки адресов: в URL есть `/`, `:` и `=`.
fn scrub_url(token: &str) -> Option<String> {
    let (scheme, rest) = token.split_once("://")?;
    let scheme_ok = scheme.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) && scheme.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    let authority_len = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_len);
    if !scheme_ok || authority.is_empty() {
        return None;
    }
    let own = !authority.contains('@') && {
        let host = authority.rsplit_once(':').map_or(authority, |(host, port)| if port.chars().all(|c| c.is_ascii_digit()) { host } else { authority });
        OWN_HOSTS.contains(&host.to_lowercase().as_str())
    };
    if own {
        return Some(token.to_string());
    }
    // Запрос и фрагмент могут нести токены и логины: от них остаётся только факт, что они были.
    let (path_and_query, fragment) = tail.split_once('#').map_or((tail, None), |(p, f)| (p, Some(f)));
    let (path, query) = path_and_query.split_once('?').map_or((path_and_query, None), |(p, q)| (p, Some(q)));
    let mut out = format!("{scheme}://<host>{path}");
    if query.is_some_and(|q| !q.is_empty()) {
        out.push_str("?<query>");
    }
    if fragment.is_some_and(|f| !f.is_empty()) {
        out.push_str("#<fragment>");
    }
    Some(out)
}

fn is_address(token: &str) -> bool {
    let bare = token.split_once('/').map_or(token, |(addr, prefix)| if prefix.chars().all(|c| c.is_ascii_digit()) { addr } else { token });
    let bare = bare.strip_prefix('[').and_then(|b| b.strip_suffix(']')).unwrap_or(bare);
    bare.parse::<IpAddr>().is_ok() || bare.parse::<SocketAddr>().is_ok()
}

fn is_host_port(token: &str) -> bool {
    let Some((host, port)) = token.rsplit_once(':') else { return false };
    (1..=5).contains(&port.len())
        && port.chars().all(|c| c.is_ascii_digit())
        && host.contains('.')
        && host.chars().all(|c| c.is_alphanumeric() || matches!(c, '.' | '-'))
}

/// Имена файлов, которые программа и Windows называют в сообщениях: по ним `метка.метка` — файл, а не сервер.
const FILE_EXTENSIONS: &[&str] = &[
    "conf", "dpapi", "exe", "dll", "sys", "msi", "msix", "log", "ini", "json", "toml", "txt", "md", "zip", "lng", "png", "ico", "bak", "tmp", "dat", "cmd",
    "bat", "ps1", "pdb", "xz", "gz", "7z", "rar", "pdf", "old", "lock", "rs", "cab",
];

/// Собственные адреса программы (проверка обновлений): не личное, и по ним видно, откуда пришла ошибка.
const OWN_HOSTS: &[&str] = &["github.com", "api.github.com", "raw.githubusercontent.com", "objects.githubusercontent.com"];

/// Голое доменное имя: две и больше меток из букв, цифр и `-`, последняя — только буквы (или `xn--` у IDN) и не
/// расширение известного файла. Порт здесь не рассматривается — это `is_host_port`.
fn is_bare_host(token: &str) -> bool {
    let labels: Vec<&str> = token.split('.').collect();
    let well_formed = |label: &&str| !label.is_empty() && !label.starts_with('-') && !label.ends_with('-') && label.chars().all(|c| c.is_alphanumeric() || c == '-');
    if labels.len() < 2 || !labels.iter().all(well_formed) {
        return false;
    }
    let tld = labels[labels.len() - 1].to_lowercase();
    let is_tld = (tld.chars().count() >= 2 && tld.chars().all(char::is_alphabetic)) || tld.starts_with("xn--");
    is_tld && !FILE_EXTENSIONS.contains(&tld.as_str()) && !OWN_HOSTS.contains(&token.to_lowercase().as_str())
}

/// Ключ WireGuard — 44 знака base64; ключ в hex — 64. Берём с запасом всё длинное без других знаков.
fn is_key(token: &str) -> bool {
    token.len() >= 40 && token.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '_' | '-'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::agent::client::FakeAgent;
    use crate::daemon::agent::proto::AgentResponse;
    use crate::daemon::fake::FakeCore;
    use crate::daemon::proto::{Response, RetryState};
    use crate::events::Severity;
    use crate::update::ComponentState;

    const PRIVATE: &str = "yAnYXlpbmcgYW5kIHNoYXJlZCBzZWNyZXRzISEhISEhISE=";
    const PRESHARED: &str = "cHJlc2hhcmVkIGtleSBmb3IgdGhlIHRlc3Qgb25seSEhISE=";

    fn who() -> Identity {
        Identity { profile: r"C:\Users\Ivan Petrov".into(), user: "Ivan".into(), machine: "WORKSTATION-7".into() }
    }

    fn event(at: u64, tunnel: &str, text: &str) -> Event {
        Event::new(at, tunnel, Severity::Warn, text, false)
    }

    fn facts() -> Facts {
        Facts {
            now: 1_790_000_000,
            app: "0.5.0".into(),
            mode: Mode::Engine,
            windows: "Windows 11 Pro 24H2 (build 26100.1)".into(),
            core: Process { version: Ok("0.5.0".into()), seen: Stamp { down: false, since: 1_789_990_000 } },
            agent: Some(Process { version: Err("agent unavailable: pipe closed".into()), seen: Stamp { down: true, since: 1_789_999_000 } }),
            tunnels: vec![("office".into(), TunnelState::Up), ("home".into(), TunnelState::Off)],
            components: Ok(vec![("App".into(), "0.5.0".into()), ("Engine".into(), "3.1.20260814".into())]),
            events: Vec::new(),
        }
    }

    #[test]
    fn report_lists_versions_status_tunnels_and_events() {
        let mut f = facts();
        f.events = vec![event(1_789_999_500, "office", "Handshake is stale")];
        let report = build(&f, &who());
        for expected in [
            "App: 0.5.0",
            "Mode: engine",
            "Windows: Windows 11 Pro 24H2 (build 26100.1)",
            "Core: up, version 0.5.0, since ",
            "Agent: down, agent unavailable: pipe closed, since ",
            "  App: 0.5.0",
            "Tunnels (2):",
            "  office: connected",
            "  home: off",
            "Last 1 events:",
            "WARN office: Handshake is stale",
        ] {
            assert!(report.contains(expected), "нет «{expected}» в отчёте:\n{report}");
        }
    }

    #[test]
    fn report_keeps_only_the_last_fifty_events() {
        let mut f = facts();
        f.events = (0..60).map(|i| event(1_789_000_000 + i, "", &format!("event number {i}"))).collect();
        let report = build(&f, &who());
        assert!(report.contains("Last 50 events:"));
        assert!(!report.contains("event number 9"), "старое событие вошло");
        assert!(report.contains("event number 10") && report.contains("event number 59"));
        assert_eq!(report.matches("event number").count(), 50);
    }

    #[test]
    fn a_core_of_another_version_is_called_out() {
        let mut f = facts();
        f.core.version = Ok("0.4.0".into());
        assert!(build(&f, &who()).contains("Core: up, version 0.4.0, differs from the window (0.5.0)"));
    }

    /// Главное свойство: в отчёте из состояния, где есть всё личное, этого личного нет.
    #[test]
    fn report_has_no_keys_addresses_or_personal_names() {
        let mut f = facts();
        f.core = Process {
            version: Err(r"open \\.\pipe\awg for WORKSTATION-7 failed: C:\Users\Ivan Petrov\AppData\x.log".into()),
            seen: Stamp::default(),
        };
        f.tunnels.push((r"Ivan".into(), TunnelState::Up));
        f.events = vec![
            event(1, "office", &format!("PrivateKey = {PRIVATE}")),
            event(2, "office", &format!("PresharedKey={PRESHARED}")),
            event(3, "office", "Endpoint 203.0.113.7:51820 unreachable; backup 198.51.100.4"),
            event(4, "office", "connect to vpn.example.com:51820 failed"),
            event(5, "office", "peer [2001:db8::1]:51820 and fe80::1 and 2001:db8:0:0:0:0:0:2"),
            event(6, "", r"cannot write C:\USERS\ivan petrov\AppData\Roaming\awg\Settings.ini"),
            event(7, "", r"copied D:\Backup\Users\Olga\conf.zip, user Ivan on workstation-7"),
            event(8, "", "Address = 10.8.0.2/32, DNS = 10.8.0.1"),
            event(9, "office", "lookup of gw.corp.example.net failed: no such host"),
        ];
        let report = build(&f, &who());
        for secret in [
            PRIVATE, PRESHARED, "203.0.113.7", "198.51.100.4", "vpn.example.com", "corp.example", "2001:db8", "fe80::1", "10.8.0.2", "10.8.0.1", "51820",
            "Ivan", "ivan", "Petrov", "petrov", "Olga", "WORKSTATION", "workstation",
        ] {
            assert!(!report.contains(secret), "«{secret}» просочилось в отчёт:\n{report}");
        }
        // Вырезано, а не выброшено целиком: остальное читается.
        assert!(report.contains("%USERPROFILE%") || report.contains(r"\Users\<user>"), "{report}");
        assert!(report.contains("<ip>") && report.contains("<endpoint>") && report.contains("<key>"), "{report}");
        assert!(report.contains("Settings.ini"), "{report}");
    }

    #[test]
    fn profile_path_becomes_a_variable() {
        let out = scrub(r"log in C:\Users\Ivan Petrov\AppData\Local\awg\a.log and c:\users\ivan petrov\b", &who(), &[]);
        assert_eq!(out, r"log in %USERPROFILE%\AppData\Local\awg\a.log and %USERPROFILE%\b");
    }

    #[test]
    fn user_name_inside_another_word_stays() {
        let id = Identity { user: "Ivan".into(), ..Default::default() };
        assert_eq!(scrub("Ivanovo and ivan, Ivan.", &id, &[]), "Ivanovo and <user>, <user>.");
    }

    #[test]
    fn times_versions_and_plain_words_are_not_mistaken_for_addresses() {
        let text = "12:34:56 version 3.1.20260814 handshake 1.5 s, file Settings.ini tunnel office-2";
        let out = scrub(text, &Identity::default(), &[]);
        assert!(out.contains("12:34:56") && out.contains("3.1.20260814") && out.contains("office-2"), "{out}");
    }

    #[test]
    fn bare_host_names_are_removed_but_files_versions_and_own_names_stay() {
        let out = scrub(
            "resolve vpn.example.com failed; peer vpn.example.com:51820 gone; host=Edge.Example.ORG. and sub.domain.co.uk, server.рф ok; at 203.0.113.7",
            &Identity::default(),
            &[],
        );
        assert_eq!(out, "resolve <host> failed; peer <endpoint> gone; host=<host>. and <host>, <host> ok; at <ip>");
        let stays = "see events.log, core.ini, Settings.ini, awg-ui.exe, amneziawg.exe, tunnel.dll, a.conf, office.conf.dpapi, b.zip, c.msi, d.json, \
                     e.lng, f.png, version 0.5.0 build 3.1.20260814 took 1.5 s at 12:34:56, e.g. this, from api.github.com, Settings.ini.unreadable-2026-10-05.";
        assert_eq!(scrub(stays, &Identity::default(), &[]), stays);
    }

    #[test]
    fn urls_lose_their_host_but_keep_scheme_path_and_own_github_addresses() {
        let out = scrub(
            "get https://vpn.example.com/releases/latest failed; http://203.0.113.7:8080/x?a=1&b=2 refused; \
             url=https://user:secret@Edge.Example.org:8443/p/q?token=1#frag. ftp://[2001:db8::1]:21/f (https://host.example.net) wss://localhost",
            &Identity::default(),
            &[],
        );
        assert_eq!(
            out,
            "get https://<host>/releases/latest failed; http://<host>/x?<query> refused; \
             url=https://<host>/p/q?<query>#<fragment>. ftp://<host>/f (https://<host>) wss://<host>"
        );
        let stays = "update https://api.github.com/repos/o/r/releases/latest and https://github.com/o/r/releases/download/v1/a.msi, \
                     https://objects.githubusercontent.com:443/x?y=1 ok";
        assert_eq!(scrub(stays, &Identity::default(), &[]), stays);
        // Пользователь в адресе GitHub — личное, адрес в целом не «свой».
        assert_eq!(scrub("https://me@github.com/o/r", &Identity::default(), &[]), "https://<host>/o/r");
        assert_eq!(scrub("https://h.example?k=cHJlc2hhcmVkIGtleSBmb3IgdGhlIHRlc3Qgb25seSEhISE#top", &Identity::default(), &[]), "https://<host>?<query>#<fragment>");
        assert_eq!(scrub("https://h.example/a?#", &Identity::default(), &[]), "https://<host>/a");
        // Не URL: `://` без схемы или хоста остаётся обычным текстом.
        assert_eq!(scrub("a ://x b https:///p", &Identity::default(), &[]), "a ://x b https:///p");
    }

    #[test]
    fn a_tunnel_named_like_a_host_is_kept_where_the_report_lists_names() {
        let keep = ["work.vpn".to_string(), "Home.Example".to_string()];
        let out = scrub("work.vpn: connected; home.example: off; other.example failed", &Identity::default(), &keep);
        assert_eq!(out, "work.vpn: connected; home.example: off; <host> failed");
    }

    #[test]
    fn tunnel_states_follow_the_snapshot() {
        let mut snap = Snapshot::default();
        snap.tunnels = ["a", "b", "c", "d", "e"].iter().map(|s| s.to_string()).collect();
        snap.running.insert("a".into(), Default::default());
        snap.retries.insert("c".into(), RetryState { attempt: 1, next_in_s: 5, last_error: String::new(), slow: false });
        let pending = BTreeMap::from([("b".to_string(), "busy.connect")]);
        let states: Vec<TunnelState> = tunnel_states(&snap, &pending).into_iter().map(|(_, s)| s).collect();
        assert_eq!(states, [TunnelState::Up, TunnelState::Busy, TunnelState::Reconnecting, TunnelState::Off, TunnelState::Off]);
        snap.core_lost = true;
        assert!(tunnel_states(&snap, &pending).iter().all(|(_, s)| *s == TunnelState::Unknown));
    }

    #[test]
    fn windows_version_comes_from_the_registry_listing() {
        let reg = "\r\nHKEY_LOCAL_MACHINE\\SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\r\n    ProductName    REG_SZ    Windows 10 Pro\r\n    DisplayVersion    REG_SZ    24H2\r\n    CurrentBuildNumber    REG_SZ    26100\r\n    UBR    REG_DWORD    0x4061\r\n";
        assert_eq!(parse_windows_version(reg).as_deref(), Some("Windows 11 Pro 24H2 (build 26100.16481)"));
        let old = "    ProductName    REG_SZ    Windows 10 Pro\r\n    CurrentBuildNumber    REG_SZ    19045\r\n";
        assert_eq!(parse_windows_version(old).as_deref(), Some("Windows 10 Pro (build 19045)"));
        assert_eq!(parse_windows_version("garbage"), None);
    }

    #[test]
    fn components_show_installed_version_and_pending_update() {
        let state = UpdatesState {
            components: vec![
                ComponentState { component: Some(Component::Native), installed: None, ..Default::default() },
                ComponentState {
                    component: Some(Component::App),
                    installed: Some("0.5.0".into()),
                    update: true,
                    available: Some(crate::update::Available { version: "0.6.0".into(), ..Default::default() }),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert_eq!(
            component_versions(&state),
            [("AmneziaWG".to_string(), "not installed".to_string()), ("App".to_string(), "0.5.0 (update available: 0.6.0)".to_string())]
        );
    }

    #[test]
    fn asking_an_unreachable_core_and_agent_reports_the_errors() {
        let core = FakeCore::unreachable("pipe: not found");
        let agent = FakeAgent::unreachable("agent pipe: not found");
        let a = ask(&core, Some(&agent));
        assert_eq!(a.core, Err("pipe: not found".to_string()));
        assert_eq!(a.agent, Some(Err("agent pipe: not found".to_string())));
        assert!(a.components.unwrap_err().contains("agent pipe: not found"));
    }

    #[test]
    fn asking_live_core_and_agent_collects_versions() {
        let core = FakeCore::new(|_| Ok(Response::Hello { version: "0.5.0".into(), mode: Mode::Engine }));
        let agent = FakeAgent(Box::new(|req| match req {
            crate::daemon::agent::proto::AgentRequest::Hello => Ok(AgentResponse::Hello { version: "0.5.0".into() }),
            _ => Ok(AgentResponse::Updates(Box::default())),
        }));
        let a = ask(&core, Some(&agent));
        assert_eq!(a.core, Ok("0.5.0".to_string()));
        assert_eq!(a.agent, Some(Ok("0.5.0".to_string())));
        assert_eq!(a.components, Ok(Vec::new()));
    }
}
