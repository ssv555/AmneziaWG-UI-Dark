//! Палитры тем, моноширинный шрифт чисел и запасные шрифты окна.
//!
//! Палитра — единственный источник цветов окна: три темы (`GRAPHITE`, `SLATE`, `DAYLIGHT`), активная — в `palette()`.
//! Чистые функции берут `&Palette`, чтобы тесты проверяли любую тему, не трогая общую активную.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use eframe::egui::{self, Color32, FontId, RichText, Sense, Ui, Vec2};

use crate::events::Severity;
use crate::health::Level;
use crate::settings::Theme;

use super::a11y::{self, Painted};

pub(super) const NUM_FONT: f32 = 13.5;

/// Цвета одной темы по ролям. Текст и цвета состояний читаются на `panel`, `window` и `graph_bg` (тест контраста).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Palette {
    /// Тёмная тема: от неё база egui (`Visuals::dark`/`light`) и тёмный заголовок окна Windows.
    pub(super) dark: bool,
    /// Фон главного окна.
    pub(super) panel: Color32,
    /// Фон окон-диалогов.
    pub(super) window: Color32,
    /// Колодец графика и пинга; он же фон полей ввода.
    pub(super) graph_bg: Color32,
    /// Едва заметная подложка: блоки кода в справке, полосы.
    pub(super) faint: Color32,
    /// Фон `code` в справке.
    pub(super) code_bg: Color32,
    pub(super) control: Color32,
    pub(super) control_hover: Color32,
    /// Разделители, рамки окон. Контраст не проверяется: границы намеренно тихие.
    pub(super) border: Color32,
    /// Обычный текст (надписи). В Графите тусклее текста кнопок — так у egui.
    pub(super) label: Color32,
    /// Текст кнопок и полей.
    pub(super) text: Color32,
    /// Текст под указателем.
    pub(super) text_hover: Color32,
    pub(super) text_strong: Color32,
    /// Слабый текст (подписи, даты, линии графика). Всегда свой цвет: производный egui (`label` на 60 %) давал ≈2.7.
    pub(super) text_weak: Color32,
    pub(super) link: Color32,
    /// Цель перетаскивания, передача в подробностях.
    pub(super) accent: Color32,
    pub(super) selection_fill: Color32,
    pub(super) selection_text: Color32,
    /// Полоса цвета `accent` у выбранной строки: светлая заливка выбора одна почти не видна.
    pub(super) selection_bar: bool,
    pub(super) connected: Color32,
    /// Подключается, предупреждение.
    pub(super) warning: Color32,
    /// Ошибка, потерянный пинг.
    pub(super) error: Color32,
    /// Отключён, неизвестно, даты.
    pub(super) idle: Color32,
    pub(super) graph_rx: Color32,
    pub(super) graph_tx: Color32,
    pub(super) graph_ping: Color32,
    /// Рамка и разделители режима 2.
    pub(super) mode2_frame: Color32,
    /// Метка режима 2 текстом: на светлом фоне жёлтый цвет рамки как текст нечитаем.
    pub(super) mode2_text: Color32,
}

const fn hex(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

/// Тема по умолчанию — прежний вид egui: `visuals()` = `Visuals::dark()`, кроме слабого текста (тест `graphite_is_todays_look`).
/// Остальные значения egui — как есть; все роли проходят цели контраста.
pub(super) const GRAPHITE: Palette = Palette {
    dark: true,
    panel: hex(0x1B1B1B),
    window: hex(0x1B1B1B),
    graph_bg: hex(0x0A0A0A),
    // Аддитивная подложка egui: чуть светлее того, что под ней.
    faint: Color32::from_additive_luminance(5),
    code_bg: hex(0x404040),
    control: hex(0x3C3C3C),
    control_hover: hex(0x464646),
    border: hex(0x3C3C3C),
    label: hex(0x8C8C8C),
    text: hex(0xB4B4B4),
    text_hover: hex(0xF0F0F0),
    text_strong: hex(0xFFFFFF),
    // Владелец 2026-10-07: слабый текст ярче (у egui ≈2.7). Самый тёмный серый с AA на 1B1B1B: 4.54 (82 — 4.48); label 8C — 5.12.
    text_weak: hex(0x838383),
    link: hex(0x5AAAFF),
    accent: hex(0x5AA0F0),
    selection_fill: hex(0x005C80),
    selection_text: hex(0xC0DEFF),
    selection_bar: false,
    connected: hex(0x5AC878),
    warning: hex(0xE6B946),
    error: hex(0xEB5F55),
    // Было 787878: контраст с фоном 3.90, меньше 4.5 для текста. Единственное намеренное отличие от прежнего вида.
    idle: hex(0x9A9A9A),
    graph_rx: hex(0x5AC878),
    graph_tx: hex(0x5AA0F0),
    graph_ping: hex(0xB48CF0),
    mode2_frame: hex(0xFFD60A),
    mode2_text: hex(0xFFD60A),
};

/// Мягкая тёмная сине-серая.
pub(super) const SLATE: Palette = Palette {
    dark: true,
    panel: hex(0x242933),
    window: hex(0x2A303B),
    graph_bg: hex(0x1C2028),
    faint: hex(0x2E3440),
    code_bg: hex(0x3B4252),
    control: hex(0x3B4252),
    control_hover: hex(0x434C5E),
    border: hex(0x434C5E),
    label: hex(0xD8DEE9),
    text: hex(0xD8DEE9),
    text_hover: hex(0xECEFF4),
    text_strong: hex(0xECEFF4),
    text_weak: hex(0x98A2B3),
    link: hex(0x88C0D0),
    accent: hex(0x88C0D0),
    selection_fill: hex(0x3B4A63),
    selection_text: hex(0xECEFF4),
    selection_bar: false,
    connected: hex(0xA3BE8C),
    warning: hex(0xEBCB8B),
    error: hex(0xE57F87),
    // 8A93A3 давал 4.28 на фоне диалогов (2A303B); чуть светлее — 4.56.
    idle: hex(0x8F98A8),
    graph_rx: hex(0xA3BE8C),
    graph_tx: hex(0x81A1C1),
    graph_ping: hex(0xC49BC0),
    mode2_frame: hex(0xFFD60A),
    mode2_text: hex(0xFFD60A),
};

/// Светлая, как Windows 11.
pub(super) const DAYLIGHT: Palette = Palette {
    dark: false,
    panel: hex(0xF3F3F3),
    window: hex(0xF9F9F9),
    graph_bg: hex(0xFFFFFF),
    faint: hex(0xEBEBEB),
    code_bg: hex(0xE6E6E6),
    control: hex(0xFBFBFB),
    control_hover: hex(0xF0F0F0),
    border: hex(0xD1D1D1),
    label: hex(0x1B1B1B),
    text: hex(0x1B1B1B),
    text_hover: hex(0x000000),
    text_strong: hex(0x000000),
    text_weak: hex(0x5C5C5C),
    link: hex(0x005FB8),
    accent: hex(0x005FB8),
    selection_fill: hex(0xCCE4F7),
    selection_text: hex(0x0A2E50),
    selection_bar: true,
    connected: hex(0x0F7B0F),
    warning: hex(0x8A5300),
    error: hex(0xC42B1C),
    idle: hex(0x6B6B6B),
    graph_rx: hex(0x0F7B0F),
    graph_tx: hex(0x005FB8),
    graph_ping: hex(0x7A4FB0),
    mode2_frame: hex(0xB07D00),
    mode2_text: hex(0x7A5800),
};

impl Palette {
    pub(super) fn level(&self, level: Level) -> Color32 {
        match level {
            Level::Off => self.idle,
            Level::Ok => self.connected,
            Level::Busy | Level::Warn => self.warning,
            Level::Bad => self.error,
        }
    }

    pub(super) fn severity(&self, s: Severity) -> Color32 {
        match s {
            Severity::Info => self.connected,
            Severity::Warn => self.warning,
            Severity::Bad => self.error,
        }
    }

    /// Вид egui для темы. В режиме 2 (`engine`) рамки окон и разделители — цвета `mode2_frame`, иначе `border`.
    /// `warn_fg_color`/`error_fg_color` остаются от базы egui: окно их не использует, свои цвета состояний — в палитре.
    pub(super) fn visuals(&self, engine: bool) -> egui::Visuals {
        let mut v = if self.dark { egui::Visuals::dark() } else { egui::Visuals::light() };
        v.panel_fill = self.panel;
        v.window_fill = self.window;
        v.extreme_bg_color = self.graph_bg;
        v.faint_bg_color = self.faint;
        v.code_bg_color = self.code_bg;
        v.hyperlink_color = self.link;
        v.weak_text_color = Some(self.text_weak);
        v.selection.bg_fill = self.selection_fill;
        v.selection.stroke.color = self.selection_text;
        v.text_cursor.stroke.color = self.selection_text;
        let w = &mut v.widgets;
        w.noninteractive.bg_fill = self.window;
        w.noninteractive.weak_bg_fill = self.window;
        w.noninteractive.fg_stroke.color = self.label;
        w.inactive.bg_fill = self.control;
        w.inactive.weak_bg_fill = self.control;
        w.inactive.fg_stroke.color = self.text;
        w.hovered.bg_fill = self.control_hover;
        w.hovered.weak_bg_fill = self.control_hover;
        w.hovered.fg_stroke.color = self.text_hover;
        w.active.fg_stroke.color = self.text_strong;
        let edge = if engine { self.mode2_frame } else { self.border };
        w.noninteractive.bg_stroke.color = edge;
        v.window_stroke.color = edge;
        v
    }
}

/// Тема с готовой палитрой: «Как в Windows» уже заменена на светлую или тёмную (`resolve`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum ThemeId {
    Graphite = 0,
    Slate = 1,
    Daylight = 2,
}

impl ThemeId {
    /// Индекс совпадает с дискриминантом: по нему `ActiveTheme` хранит тему в одном байте.
    const ALL: [ThemeId; 3] = [ThemeId::Graphite, ThemeId::Slate, ThemeId::Daylight];

    pub(super) fn palette(self) -> &'static Palette {
        match self {
            ThemeId::Graphite => &GRAPHITE,
            ThemeId::Slate => &SLATE,
            ThemeId::Daylight => &DAYLIGHT,
        }
    }
}

/// Тема из настроек: «Как в Windows» — День при светлой теме системы, иначе Графит.
pub(super) fn resolve(theme: Theme, system_is_light: bool) -> ThemeId {
    match theme {
        Theme::Graphite => ThemeId::Graphite,
        Theme::Slate => ThemeId::Slate,
        Theme::Daylight => ThemeId::Daylight,
        Theme::System if system_is_light => ThemeId::Daylight,
        Theme::System => ThemeId::Graphite,
    }
}

/// Активная тема в одном байте. Кроме байта ничего не публикуется, поэтому `Relaxed`.
struct ActiveTheme(AtomicU8);

impl ActiveTheme {
    const fn new(id: ThemeId) -> Self {
        ActiveTheme(AtomicU8::new(id as u8))
    }

    fn set(&self, id: ThemeId) {
        self.0.store(id as u8, Ordering::Relaxed);
    }

    fn get(&self) -> ThemeId {
        // Байт пишет только `set`; индекс вне массива — ошибка в этом файле, и пусть она будет громкой.
        ThemeId::ALL[usize::from(self.0.load(Ordering::Relaxed))]
    }
}

/// Пишет только смена темы в цикле кадра (`set_active`); читают все, кто рисует.
static ACTIVE: ActiveTheme = ActiveTheme::new(ThemeId::Graphite);

/// Палитра активной темы.
pub(super) fn palette() -> &'static Palette {
    ACTIVE.get().palette()
}

/// Сделать тему активной. Только из цикла кадра до рисования: иначе один кадр смешает две темы.
pub(super) fn set_active(id: ThemeId) {
    ACTIVE.set(id);
}

pub(super) fn level_color(level: Level) -> Color32 {
    palette().level(level)
}

pub(super) fn severity_color(s: Severity) -> Color32 {
    palette().severity(s)
}

/// Цветная точка состояния. `meaning` — её смысл словами для диктора: цвет он не передаёт.
pub(super) fn dot(ui: &mut Ui, color: Color32, radius: f32, meaning: &str) {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(radius * 2.0 + 4.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), radius, color);
    a11y::describe(&resp, Painted::Dot { meaning });
}

pub(super) fn mono(text: impl Into<String>, color: Color32) -> RichText {
    RichText::new(text).font(FontId::monospace(NUM_FONT)).color(color)
}

/// Шрифты Windows как запасные: встроенный шрифт egui знает только латиницу и кириллицу,
/// а пользовательский язык может быть любым (CJK, арабский, иврит, греческий …).
pub(super) fn add_fallback_fonts(ctx: &egui::Context) {
    let fonts_dir = PathBuf::from(std::env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into())).join("Fonts");
    let mut defs = egui::FontDefinitions::default();
    for file in ["segoeui.ttf", "msyh.ttc", "malgun.ttf", "YuGothM.ttc"] {
        let Ok(bytes) = std::fs::read(fonts_dir.join(file)) else { continue };
        defs.font_data.insert(file.to_string(), Arc::new(egui::FontData::from_owned(bytes)));
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            defs.families.entry(family).or_default().push(file.to_string());
        }
    }
    ctx.set_fonts(defs);
}

#[cfg(test)]
mod tests {
    use super::*;

    const PALETTES: [(&str, &Palette); 3] = [("graphite", &GRAPHITE), ("slate", &SLATE), ("daylight", &DAYLIGHT)];

    /// Относительная яркость WCAG 2.x по sRGB.
    fn luminance(c: Color32) -> f64 {
        let lin = |v: u8| {
            let v = f64::from(v) / 255.0;
            if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * lin(c.r()) + 0.7152 * lin(c.g()) + 0.0722 * lin(c.b())
    }

    fn contrast(a: Color32, b: Color32) -> f64 {
        let (x, y) = (luminance(a), luminance(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    #[test]
    fn contrast_formula_matches_wcag_reference_points() {
        assert!((contrast(Color32::BLACK, Color32::WHITE) - 21.0).abs() < 1e-9);
        assert!((contrast(GRAPHITE.panel, GRAPHITE.panel) - 1.0).abs() < 1e-9);
        // Старый серый не проходил — ради этого его и сменили.
        assert!(contrast(hex(0x787878), GRAPHITE.panel) < 4.5);
    }

    /// Текст и цвета состояний читаемы (AA, 4.5) на всех фонах темы; рамка режима 2 и акцент — 3 (крупные элементы).
    /// Границы не проверяются: они намеренно тихие.
    #[test]
    fn every_palette_meets_wcag_contrast_targets() {
        let mut failures = Vec::new();
        let mut check = |name: &str, role: &str, fg: Color32, bg_name: &str, bg: Color32, min: f64| {
            let c = contrast(fg, bg);
            if c < min {
                failures.push(format!("{name}: {role} on {bg_name} = {c:.2} < {min}"));
            }
        };
        for (name, p) in PALETTES {
            let text_roles = [
                ("label", p.label, 4.5),
                ("text_weak", p.text_weak, 4.5),
                ("text", p.text, 4.5),
                ("text_hover", p.text_hover, 4.5),
                ("text_strong", p.text_strong, 4.5),
                ("link", p.link, 4.5),
                ("connected", p.connected, 4.5),
                ("warning", p.warning, 4.5),
                ("error", p.error, 4.5),
                ("idle", p.idle, 4.5),
                ("graph_rx", p.graph_rx, 4.5),
                ("graph_tx", p.graph_tx, 4.5),
                ("graph_ping", p.graph_ping, 4.5),
                ("mode2_text", p.mode2_text, 4.5),
                ("accent", p.accent, 3.0),
                ("mode2_frame", p.mode2_frame, 3.0),
            ];
            for (bg_name, bg) in [("panel", p.panel), ("window", p.window), ("graph_bg", p.graph_bg)] {
                for (role, fg, min) in text_roles {
                    check(name, role, fg, bg_name, bg, min);
                }
            }
            check(name, "text", p.text, "control", p.control, 4.5);
            check(name, "text_hover", p.text_hover, "control_hover", p.control_hover, 4.5);
            check(name, "selection_text", p.selection_text, "selection_fill", p.selection_fill, 4.5);
        }
        // Слабый текст при всей своей читаемости остаётся второстепенным: тусклее обычного.
        for (name, p) in PALETTES {
            if contrast(p.text_weak, p.panel) >= contrast(p.label, p.panel) {
                failures.push(format!("{name}: text_weak is not weaker than label"));
            }
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }

    /// Тема по умолчанию не меняет вида: режим 1 — `Visuals::dark()`, режим 2 — то, что до тем ставил
    /// `apply_mode_look` (core_ui.rs): поверх `dark()` неоновые рамка окна и разделители.
    /// Единственное отличие от `dark()` — слабый текст 838383 вместо производного egui (≈2.7): решение владельца
    /// 2026-10-07 «поярче». Любое другое расхождение с `dark()` этот тест ловит.
    #[test]
    fn graphite_is_todays_look() {
        let mut today = egui::Visuals::dark();
        today.weak_text_color = Some(Color32::from_rgb(0x83, 0x83, 0x83));
        assert_eq!(GRAPHITE.visuals(false), today);
        let neon = Color32::from_rgb(255, 214, 10);
        let mut mode2 = today;
        mode2.widgets.noninteractive.bg_stroke.color = neon;
        mode2.window_stroke.color = neon;
        assert_eq!(GRAPHITE.visuals(true), mode2);
    }

    #[test]
    fn dark_flag_matches_the_background() {
        for (name, p) in PALETTES {
            assert_eq!(p.dark, luminance(p.panel) < 0.5, "{name}");
        }
    }

    #[test]
    fn visuals_use_the_frame_colour_only_in_mode_two() {
        for (name, p) in PALETTES {
            let mode1 = p.visuals(false);
            let mode2 = p.visuals(true);
            assert_eq!(mode1.dark_mode, p.dark, "{name}");
            assert_eq!(mode2.dark_mode, p.dark, "{name}");
            assert_eq!((mode1.window_stroke.color, mode1.widgets.noninteractive.bg_stroke.color), (p.border, p.border), "{name}");
            assert_eq!(
                (mode2.window_stroke.color, mode2.widgets.noninteractive.bg_stroke.color),
                (p.mode2_frame, p.mode2_frame),
                "{name}"
            );
        }
    }

    #[test]
    fn visuals_take_text_and_backgrounds_from_the_palette() {
        for (name, p) in PALETTES {
            let v = p.visuals(false);
            assert_eq!((v.panel_fill, v.window_fill, v.extreme_bg_color), (p.panel, p.window, p.graph_bg), "{name}");
            assert_eq!((v.faint_bg_color, v.code_bg_color, v.hyperlink_color), (p.faint, p.code_bg, p.link), "{name}");
            assert_eq!((v.text_color(), v.weak_text_color, v.strong_text_color()), (p.label, Some(p.text_weak), p.text_strong), "{name}");
            let w = &v.widgets;
            assert_eq!((w.inactive.fg_stroke.color, w.hovered.fg_stroke.color), (p.text, p.text_hover), "{name}");
            assert_eq!((w.inactive.bg_fill, w.hovered.bg_fill), (p.control, p.control_hover), "{name}");
            assert_eq!((v.selection.bg_fill, v.selection.stroke.color), (p.selection_fill, p.selection_text), "{name}");
        }
    }

    #[test]
    fn system_theme_follows_windows_light_or_dark() {
        assert_eq!(resolve(Theme::System, true), ThemeId::Daylight);
        assert_eq!(resolve(Theme::System, false), ThemeId::Graphite);
        for light in [false, true] {
            assert_eq!(resolve(Theme::Graphite, light), ThemeId::Graphite);
            assert_eq!(resolve(Theme::Slate, light), ThemeId::Slate);
            assert_eq!(resolve(Theme::Daylight, light), ThemeId::Daylight);
        }
    }

    #[test]
    fn active_theme_keeps_what_was_set() {
        for (i, id) in ThemeId::ALL.into_iter().enumerate() {
            assert_eq!(id as usize, i, "индекс в ALL = дискриминант");
        }
        // Своё хранилище, а не общее `ACTIVE`: тесты идут параллельно, и чужие цвета не должны меняться под ними.
        let active = ActiveTheme::new(ThemeId::Graphite);
        for id in [ThemeId::Daylight, ThemeId::Slate, ThemeId::Graphite] {
            active.set(id);
            assert_eq!(active.get(), id);
        }
        assert_eq!(*ThemeId::Slate.palette(), SLATE);
        assert_eq!(*ThemeId::Daylight.palette(), DAYLIGHT);
        // Общее: только тема по умолчанию, её и так видят все остальные тесты.
        set_active(ThemeId::Graphite);
        assert_eq!(*palette(), GRAPHITE);
    }

    #[test]
    fn status_colours_come_from_the_palette() {
        assert_eq!(DAYLIGHT.level(Level::Off), DAYLIGHT.idle);
        assert_eq!(DAYLIGHT.level(Level::Ok), DAYLIGHT.connected);
        assert_eq!((DAYLIGHT.level(Level::Busy), DAYLIGHT.level(Level::Warn)), (DAYLIGHT.warning, DAYLIGHT.warning));
        assert_eq!(DAYLIGHT.level(Level::Bad), DAYLIGHT.error);
        assert_eq!(SLATE.severity(Severity::Info), SLATE.connected);
        assert_eq!(SLATE.severity(Severity::Warn), SLATE.warning);
        assert_eq!(SLATE.severity(Severity::Bad), SLATE.error);
        // Графит — прежние цвета окна (кроме серого 9A9A9A), и функции по умолчанию берут его.
        let g = &GRAPHITE;
        assert_eq!([g.connected, g.warning, g.mode2_frame, g.error, g.idle, g.accent, g.graph_ping], [
            Color32::from_rgb(90, 200, 120),
            Color32::from_rgb(230, 185, 70),
            Color32::from_rgb(255, 214, 10),
            Color32::from_rgb(235, 95, 85),
            Color32::from_rgb(154, 154, 154),
            Color32::from_rgb(90, 160, 240),
            Color32::from_rgb(180, 140, 240),
        ]);
        assert_eq!(level_color(Level::Ok), GRAPHITE.connected);
        assert_eq!(severity_color(Severity::Bad), GRAPHITE.error);
    }

    /// Цвета окна — только из палитры: цвет-литерал или база egui `Visuals` в src/app (кроме этого файла) на другой
    /// теме остались бы графитовыми — белый текст на светлом фоне. Тестовые модули не исключаются: им цвета не нужны.
    #[test]
    fn colours_come_only_from_the_palette() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = vec![src.join("app.rs")];
        super::super::dialog::tests::collect_rs(&src.join("app"), &mut files);
        assert!(files.len() > 10, "src/app not scanned: {}", src.display());
        let own = src.join("app").join("theme.rs");
        let mut bad = Vec::new();
        for path in files.iter().filter(|p| **p != own) {
            let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            for (n, line) in text.lines().enumerate() {
                for found in colour_literals(line) {
                    bad.push(format!("src/{}:{}: {found}", path.strip_prefix(&src).unwrap().display(), n + 1));
                }
            }
        }
        assert!(bad.is_empty(), "colours outside the palette (src/app/theme.rs):\n{}", bad.join("\n"));
    }

    #[test]
    fn colour_literal_detection() {
        assert_eq!(colour_literals("RichText::new(t).color(Color32::WHITE)"), ["Color32::WHITE"]);
        assert_eq!(colour_literals("let c = egui::Color32::from_rgb(1, 2, 3);"), ["Color32::from_rgb"]);
        assert_eq!(colour_literals("Color32::from_rgba_unmultiplied(0, 0, 0, 9)"), ["Color32::from_rgba_unmultiplied"]);
        assert_eq!(colour_literals("Color32::BLACK, Color32::from_gray(9)"), ["Color32::BLACK", "Color32::from_gray"]);
        assert_eq!(colour_literals("let v = egui::Visuals::light();"), ["Visuals::light()"]);
        assert_eq!(colour_literals("ctx.set_visuals(Visuals::dark())"), ["Visuals::dark()"]);
        assert!(colour_literals("fill: Color32::TRANSPARENT, hint: Color32::PLACEHOLDER").is_empty());
        assert!(colour_literals("ui.visuals().dark_mode; palette().error; p: Color32,").is_empty());
    }

    /// Цвета мимо палитры в строке: `Color32::<имя>` (константа или конструктор) и базы `Visuals::dark()`/`light()`.
    /// Прозрачный и «заглушка» egui — не цвета темы, они разрешены.
    fn colour_literals(line: &str) -> Vec<&str> {
        const ALLOWED: [&str; 2] = ["TRANSPARENT", "PLACEHOLDER"];
        let path = "Color32::";
        let mut found: Vec<&str> = line
            .match_indices(path)
            .filter_map(|(i, _)| {
                let rest = &line[i + path.len()..];
                let name = &rest[..rest.find(|c: char| !(c.is_alphanumeric() || c == '_')).unwrap_or(rest.len())];
                (!ALLOWED.contains(&name)).then(|| &line[i..i + path.len() + name.len()])
            })
            .collect();
        found.extend(["Visuals::dark()", "Visuals::light()"].into_iter().filter(|base| line.contains(base)));
        found
    }
}
