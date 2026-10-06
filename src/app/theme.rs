//! Цвета, моноширинный шрифт чисел и запасные шрифты окна.

use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui::{self, Color32, FontId, RichText, Sense, Ui, Vec2};

use crate::events::Severity;
use crate::health::Level;

use super::a11y::{self, Painted};

pub(super) const NUM_FONT: f32 = 13.5;
pub(super) const GREEN: Color32 = Color32::from_rgb(90, 200, 120);
pub(super) const YELLOW: Color32 = Color32::from_rgb(230, 185, 70);
/// Цвет режима 2 (встроенный движок): разделители, рамки, метка в строке состояния.
pub(super) const NEON: Color32 = Color32::from_rgb(255, 214, 10);
pub(super) const RED: Color32 = Color32::from_rgb(235, 95, 85);
pub(super) const GRAY: Color32 = Color32::from_rgb(120, 120, 120);
pub(super) const BLUE: Color32 = Color32::from_rgb(90, 160, 240);
pub(super) const VIOLET: Color32 = Color32::from_rgb(180, 140, 240);

pub(super) fn level_color(level: Level) -> Color32 {
    match level {
        Level::Off => GRAY,
        Level::Ok => GREEN,
        Level::Busy | Level::Warn => YELLOW,
        Level::Bad => RED,
    }
}

pub(super) fn severity_color(s: Severity) -> Color32 {
    match s {
        Severity::Info => GREEN,
        Severity::Warn => YELLOW,
        Severity::Bad => RED,
    }
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
