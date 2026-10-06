//! Небольшой разбор markdown для текста релиза с GitHub («Что нового»): заголовки, списки, абзацы, блоки кода,
//! ссылки и выделение. Остальное (таблицы, HTML, картинки) не поддерживается и показывается как обычный текст.
//! Разбор (`parse`, `inlines`) не знает про egui и проверяется тестами; рисует блоки `show`.

use eframe::egui::{self, RichText, Ui};

/// Блок текста релиза.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Block {
    Heading { level: u8, inlines: Vec<Inline> },
    /// Пункт списка: вложенность и номер (`None` — маркированный).
    Item { depth: usize, number: Option<u32>, inlines: Vec<Inline> },
    Paragraph(Vec<Inline>),
    /// Текст в ограде из трёх обратных кавычек, как есть.
    Code(String),
    Rule,
}

/// Кусок строки внутри блока.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Inline {
    Text(String),
    Bold(String),
    Italic(String),
    Code(String),
    Link { label: String, url: String },
}

/// Вложенность пунктов выше этой не показывается глубже: список с десятком отступов съел бы всю ширину окна.
const MAX_DEPTH: usize = 4;

/// Текст в блоках. Пустые строки разделяют абзацы; строка без собственного маркера продолжает предыдущий абзац
/// или пункт (так читает и GitHub).
pub(super) fn parse(src: &str) -> Vec<Block> {
    let mut out = Vec::new();
    let mut pending: Option<Pending> = None;
    let mut fence: Option<Vec<&str>> = None;
    for raw in src.lines() {
        if let Some(code) = fence.as_mut() {
            if raw.trim_start().starts_with("```") {
                out.push(Block::Code(code.join("\n")));
                fence = None;
            } else {
                code.push(raw.trim_end());
            }
            continue;
        }
        let line = strip_quote(raw.trim_end());
        let text = line.trim_start();
        if text.is_empty() {
            flush(&mut pending, &mut out);
        } else if text.starts_with("```") {
            flush(&mut pending, &mut out);
            fence = Some(Vec::new());
        } else if is_rule(text) {
            flush(&mut pending, &mut out);
            out.push(Block::Rule);
        } else if let Some((level, title)) = heading(text) {
            flush(&mut pending, &mut out);
            out.push(Block::Heading { level, inlines: inlines(title) });
        } else if let Some((number, rest)) = list_marker(text) {
            flush(&mut pending, &mut out);
            let depth = (indent_of(line) / 2).min(MAX_DEPTH);
            pending = Some(Pending { kind: Kind::Item { depth, number }, text: rest.to_string() });
        } else if let Some(p) = pending.as_mut() {
            p.text.push(' ');
            p.text.push_str(text);
        } else {
            pending = Some(Pending { kind: Kind::Paragraph, text: text.to_string() });
        }
    }
    // Ограда без закрытия: содержимое всё равно показывается, а не пропадает.
    if let Some(code) = fence {
        out.push(Block::Code(code.join("\n")));
    }
    flush(&mut pending, &mut out);
    out
}

/// Абзац или пункт, собираемый из нескольких строк.
struct Pending {
    kind: Kind,
    text: String,
}

enum Kind {
    Paragraph,
    Item { depth: usize, number: Option<u32> },
}

fn flush(pending: &mut Option<Pending>, out: &mut Vec<Block>) {
    let Some(p) = pending.take() else { return };
    let inlines = inlines(&p.text);
    out.push(match p.kind {
        Kind::Paragraph => Block::Paragraph(inlines),
        Kind::Item { depth, number } => Block::Item { depth, number, inlines },
    });
}

/// Цитата `> текст` читается как обычный текст.
fn strip_quote(line: &str) -> &str {
    let t = line.trim_start();
    match t.strip_prefix('>') {
        Some(rest) => rest.trim_start(),
        None => line,
    }
}

/// Отступ в пробелах; табуляция — четыре.
fn indent_of(line: &str) -> usize {
    line.chars().take_while(|c| c.is_whitespace()).map(|c| if c == '\t' { 4 } else { 1 }).sum()
}

/// `---`, `***`, `___`: три и больше одинаковых знака в строке.
fn is_rule(text: &str) -> bool {
    let Some(first) = text.chars().next().filter(|c| matches!(c, '-' | '*' | '_')) else { return false };
    text.chars().count() >= 3 && text.chars().all(|c| c == first)
}

/// `## Заголовок` → (2, «Заголовок»); хвостовые `#` не нужны.
fn heading(text: &str) -> Option<(u8, &str)> {
    let hashes = text.chars().take_while(|c| *c == '#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = &text[hashes..];
    // «#123» без пробела — не заголовок, а номер задачи.
    if !rest.is_empty() && !rest.starts_with(' ') {
        return None;
    }
    Some((hashes as u8, rest.trim().trim_end_matches('#').trim_end()))
}

/// Маркер пункта: `- `, `* `, `+ ` или `1. ` / `1) `; возвращает номер (если нумерованный) и текст после маркера.
fn list_marker(text: &str) -> Option<(Option<u32>, &str)> {
    for m in ["- ", "* ", "+ "] {
        if let Some(rest) = text.strip_prefix(m) {
            return Some((None, rest.trim_start()));
        }
    }
    let digits = text.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 || digits > 9 {
        return None;
    }
    let rest = text[digits..].strip_prefix(". ").or_else(|| text[digits..].strip_prefix(") "))?;
    Some((text[..digits].parse().ok(), rest.trim_start()))
}

/// Ссылка — только по http и https: текст релиза пришёл из сети, и `file:`, `javascript:` или путь к программе
/// по щелчку открываться не должны.
pub(super) fn is_web_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    let rest = lower.strip_prefix("https://").or_else(|| lower.strip_prefix("http://"));
    rest.is_some_and(|r| !r.is_empty()) && !url.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// Строка в кусках: `**жирный**`, `*курсив*`, `` `код` ``, `[текст](ссылка)`, `<ссылка>` и голая ссылка.
/// Незакрытый маркер остаётся обычным текстом.
pub(super) fn inlines(s: &str) -> Vec<Inline> {
    let mut out = Vec::new();
    let mut text = String::new();
    let mut i = 0;
    while i < s.len() {
        let rest = &s[i..];
        let prev = s[..i].chars().next_back();
        match span_at(rest, prev) {
            Some((inline, len)) => {
                if !text.is_empty() {
                    out.push(Inline::Text(std::mem::take(&mut text)));
                }
                out.push(inline);
                i += len;
            }
            None => {
                let c = rest.chars().next().expect("i < s.len()");
                text.push(c);
                i += c.len_utf8();
            }
        }
    }
    if !text.is_empty() {
        out.push(Inline::Text(text));
    }
    out
}

/// Кусок, начинающийся ровно в начале `rest`, и сколько байт он занял.
fn span_at(rest: &str, prev: Option<char>) -> Option<(Inline, usize)> {
    if let Some(body) = rest.strip_prefix("**") {
        let end = body.find("**").filter(|e| *e > 0)?;
        return Some((Inline::Bold(body[..end].to_string()), end + 4));
    }
    if let Some(body) = rest.strip_prefix('*') {
        // `*` с пробелом внутри края — не выделение (умножение, лишний маркер).
        let end = body.find('*').filter(|e| *e > 0)?;
        let inner = &body[..end];
        if inner.starts_with(char::is_whitespace) || inner.ends_with(char::is_whitespace) {
            return None;
        }
        return Some((Inline::Italic(inner.to_string()), end + 2));
    }
    if let Some(body) = rest.strip_prefix('`') {
        let end = body.find('`').filter(|e| *e > 0)?;
        return Some((Inline::Code(body[..end].to_string()), end + 2));
    }
    if rest.starts_with('[') {
        return markdown_link(rest);
    }
    if let Some(body) = rest.strip_prefix('<') {
        let end = body.find('>')?;
        return is_web_url(&body[..end]).then(|| (link(&body[..end], &body[..end]), end + 2));
    }
    // Голая ссылка — только с начала слова, а не из середины (`xhttp://…`).
    if prev.map_or(true, |c| c.is_whitespace() || matches!(c, '(' | '[' | '"')) && (rest.starts_with("http://") || rest.starts_with("https://")) {
        let raw = rest.split(|c: char| c.is_whitespace() || matches!(c, '<' | '>')).next().unwrap_or("");
        // Знаки конца фразы к адресу не относятся; закрывающая скобка — только если открывающей в адресе не было.
        let mut url = raw.trim_end_matches(['.', ',', ';', ':', '!', '?', '"', '\'']);
        while url.ends_with(')') && url.matches(')').count() > url.matches('(').count() {
            url = url[..url.len() - 1].trim_end_matches(['.', ',', ';', ':', '!', '?']);
        }
        return is_web_url(url).then(|| (link(url, url), url.len()));
    }
    None
}

/// `[текст](адрес)` или `[текст](адрес "подсказка")`. Адрес не http/https — остаётся одним текстом, без ссылки.
fn markdown_link(rest: &str) -> Option<(Inline, usize)> {
    let close = rest.find(']')?;
    let label = &rest[1..close];
    if label.contains('[') {
        return None;
    }
    let target = rest[close + 1..].strip_prefix('(')?;
    let end = target.find(')')?;
    let url = target[..end].split_whitespace().next().unwrap_or("");
    let len = close + 1 + 1 + end + 1;
    let inline = if is_web_url(url) { link(if label.is_empty() { url } else { label }, url) } else { Inline::Text(label.to_string()) };
    Some((inline, len))
}

fn link(label: &str, url: &str) -> Inline {
    Inline::Link { label: label.to_string(), url: url.to_string() }
}

// ───────────────────────────── показ ─────────────────────────────

const INDENT: f32 = 16.0;

/// Рисует блоки подряд; ссылки открываются в браузере.
pub(super) fn show(ui: &mut Ui, blocks: &[Block]) {
    for (n, block) in blocks.iter().enumerate() {
        match block {
            Block::Heading { level, inlines } => {
                if n > 0 {
                    ui.add_space(8.0);
                }
                spans(ui, inlines, Some(heading_size(*level)), true);
                ui.add_space(3.0);
            }
            Block::Paragraph(inlines) => {
                spans(ui, inlines, None, false);
                ui.add_space(5.0);
            }
            Block::Item { depth, number, inlines } => {
                let marker = number.map_or_else(|| "•".to_string(), |n| format!("{n}."));
                ui.horizontal_top(|ui| {
                    ui.add_space(INDENT * *depth as f32);
                    ui.label(marker);
                    ui.vertical(|ui| spans(ui, inlines, None, false));
                });
                ui.add_space(2.0);
            }
            Block::Code(text) => {
                egui::Frame::new().fill(ui.visuals().faint_bg_color).inner_margin(6.0).corner_radius(4.0).show(ui, |ui| {
                    ui.add(egui::Label::new(RichText::new(text).monospace()).wrap());
                });
                ui.add_space(5.0);
            }
            Block::Rule => {
                ui.separator();
            }
        }
    }
}

fn heading_size(level: u8) -> f32 {
    match level {
        1 => 20.0,
        2 => 17.0,
        3 => 15.0,
        _ => 14.0,
    }
}

/// Строка из кусков с переносом по ширине окна. Промежуток между кусками нулевой: пробелы стоят в самом тексте.
fn spans(ui: &mut Ui, inlines: &[Inline], size: Option<f32>, strong: bool) {
    let style = |t: &str| {
        let mut r = RichText::new(t);
        if let Some(s) = size {
            r = r.size(s);
        }
        if strong {
            r = r.strong();
        }
        r
    };
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        for inline in inlines {
            match inline {
                Inline::Text(t) => {
                    ui.label(style(t));
                }
                Inline::Bold(t) => {
                    ui.label(style(t).strong());
                }
                Inline::Italic(t) => {
                    ui.label(style(t).italics());
                }
                Inline::Code(t) => {
                    ui.label(style(t).code());
                }
                Inline::Link { label, url } => {
                    ui.hyperlink_to(style(label), url).on_hover_text(url);
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Inline {
        Inline::Text(s.to_string())
    }

    fn lnk(label: &str, url: &str) -> Inline {
        link(label, url)
    }

    #[test]
    fn github_release_body_is_split_into_blocks() {
        let src = "## What's Changed\n* Fix tray icon by @user in #12\n* Add **dark** mode\n\n### Notes\nSee the guide.\r\n\n**Full Changelog**: https://github.com/o/r/compare/v1...v2\n";
        assert_eq!(
            parse(src),
            vec![
                Block::Heading { level: 2, inlines: vec![text("What's Changed")] },
                Block::Item { depth: 0, number: None, inlines: vec![text("Fix tray icon by @user in #12")] },
                Block::Item { depth: 0, number: None, inlines: vec![text("Add "), Inline::Bold("dark".into()), text(" mode")] },
                Block::Heading { level: 3, inlines: vec![text("Notes")] },
                Block::Paragraph(vec![text("See the guide.")]),
                Block::Paragraph(vec![
                    Inline::Bold("Full Changelog".into()),
                    text(": "),
                    lnk("https://github.com/o/r/compare/v1...v2", "https://github.com/o/r/compare/v1...v2"),
                ]),
            ]
        );
    }

    #[test]
    fn no_raw_markers_survive_in_headings_and_bullets() {
        for b in parse("# T #\n- a\n* b\n+ c\n1. d\n2) e") {
            let shown = match b {
                Block::Heading { inlines, .. } | Block::Item { inlines, .. } => inlines,
                other => panic!("{other:?}"),
            };
            let Inline::Text(t) = &shown[0] else { panic!("{shown:?}") };
            assert!(!t.starts_with(['#', '-', '*', '+']) && !t.ends_with('#') && t.len() == 1, "{t:?}");
        }
    }

    #[test]
    fn heading_levels_and_issue_numbers() {
        assert_eq!(heading("### Три"), Some((3, "Три")));
        assert_eq!(heading("#"), Some((1, "")));
        assert_eq!(heading("#123 fixed"), None, "номер задачи не заголовок");
        assert_eq!(heading("####### семь"), None);
    }

    #[test]
    fn nested_and_numbered_items_keep_depth_and_number() {
        let blocks = parse("1. first\n   - nested\n     - deeper\n2. second");
        let shape: Vec<_> = blocks
            .iter()
            .map(|b| match b {
                Block::Item { depth, number, .. } => (*depth, *number),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(shape, [(0, Some(1)), (1, None), (2, None), (0, Some(2))]);
        let deep = parse(&format!("{}- x", " ".repeat(40)));
        assert!(matches!(deep[0], Block::Item { depth: MAX_DEPTH, .. }), "глубина ограничена");
    }

    #[test]
    fn continuation_lines_join_the_item_or_paragraph() {
        assert_eq!(
            parse("- long item\n  continues here\n\nplain\ntext"),
            vec![
                Block::Item { depth: 0, number: None, inlines: vec![text("long item continues here")] },
                Block::Paragraph(vec![text("plain text")]),
            ]
        );
    }

    #[test]
    fn code_fence_rule_and_quote() {
        assert_eq!(
            parse("```\nlet a = 1;\n  # not heading\n```\n---\n> quoted"),
            vec![Block::Code("let a = 1;\n  # not heading".into()), Block::Rule, Block::Paragraph(vec![text("quoted")])]
        );
        assert_eq!(parse("```\nunclosed"), vec![Block::Code("unclosed".into())], "незакрытая ограда не теряет текст");
        assert!(parse("").is_empty() && parse("\n \n").is_empty());
    }

    #[test]
    fn links_inline_forms() {
        assert_eq!(inlines("[site](https://a.io/x) end"), vec![lnk("site", "https://a.io/x"), text(" end")]);
        assert_eq!(inlines("[](https://a.io)"), vec![lnk("https://a.io", "https://a.io")], "пустой текст — показывается адрес");
        assert_eq!(inlines("[t](https://a.io \"title\")"), vec![lnk("t", "https://a.io")]);
        assert_eq!(inlines("<https://a.io/p>"), vec![lnk("https://a.io/p", "https://a.io/p")]);
        assert_eq!(inlines("see https://a.io/p."), vec![text("see "), lnk("https://a.io/p", "https://a.io/p"), text(".")]);
        assert_eq!(inlines("(https://a.io/p)"), vec![text("("), lnk("https://a.io/p", "https://a.io/p"), text(")")]);
        assert_eq!(inlines("https://a.io/w_(x)"), vec![lnk("https://a.io/w_(x)", "https://a.io/w_(x)")]);
        assert_eq!(inlines("xhttps://a.io"), vec![text("xhttps://a.io")], "из середины слова — не ссылка");
    }

    #[test]
    fn only_web_urls_become_links() {
        for bad in ["file:///C:/Windows/System32/calc.exe", "javascript:alert(1)", "C:\\x.exe", "ftp://a", "https://", "https://a b"] {
            assert!(!is_web_url(bad), "{bad}");
        }
        assert!(is_web_url("https://github.com/x") && is_web_url("HTTP://a.io"));
        // Небезопасный адрес в разметке — просто текст подписи.
        assert_eq!(inlines("[run](file:///c:/x.exe)"), vec![text("run")]);
        assert!(inlines("<file:///c:/x.exe>").iter().all(|i| !matches!(i, Inline::Link { .. })));
    }

    #[test]
    fn emphasis_and_code_inline() {
        assert_eq!(inlines("a *b* `c` **d**"), vec![text("a "), Inline::Italic("b".into()), text(" "), Inline::Code("c".into()), text(" "), Inline::Bold("d".into())]);
        // Незакрытые и пустые маркеры остаются текстом.
        assert_eq!(inlines("2 * 3 and **open"), vec![text("2 * 3 and **open")]);
        assert_eq!(inlines("a ** b"), vec![text("a ** b")]);
        assert_eq!(inlines("[not a link] (x)"), vec![text("[not a link] (x)")]);
    }

    #[test]
    fn non_ascii_text_is_not_cut() {
        assert_eq!(inlines("Исправлено **тоннель** — см. [док](https://a.io)"), vec![text("Исправлено "), Inline::Bold("тоннель".into()), text(" — см. "), lnk("док", "https://a.io")]);
    }

    // eframe 0.36 вынес открытие ссылок в фичу "links"; без неё щелчок по ссылке молча ничего не делает (только warn).
    #[test]
    fn eframe_opens_links_in_browser() {
        let manifest = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"));
        let eframe = manifest
            .lines()
            .find(|l| l.split_once('=').is_some_and(|(key, _)| key.trim() == "eframe"))
            .expect("строка eframe в Cargo.toml");
        assert!(eframe.contains("\"links\""), "у eframe нет фичи links: {eframe}");
    }
}
