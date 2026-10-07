//! Окно «О программе».

use eframe::egui::{self, RichText, Vec2};

use crate::fmt;
use crate::i18n::{tr, trf};
use crate::settings::Mode;

use super::dialog::{dialog_body, dialog_buttons};
use super::modals::{Outcome, Turn};
use super::App;

impl App {
    pub(super) fn show_about(&mut self, ctx: &egui::Context, mut turn: Turn) -> Outcome {
        let icon = self
            .about_icon
            .get_or_insert_with(|| {
                let image = egui::ColorImage::from_rgba_unmultiplied([64, 64], &crate::icon::themed(64, self.s.mode() == Mode::Engine));
                ctx.load_texture("about-icon", image, Default::default())
            })
            .clone();
        let mut open = true;
        let mut close = false;
        let mut copy = false;
        turn.window(ctx, tr("help.about"), "about", &mut open)
            .show(ctx, |ui| {
                dialog_body(ui, 460.0, |ui| {
                    ui.horizontal(|ui| {
                        ui.add(egui::Image::new(&icon).fit_to_exact_size(Vec2::splat(64.0)));
                        ui.vertical(|ui| {
                            ui.heading(crate::APP_TITLE);
                            ui.label(RichText::new(trf("about.version", &[env!("CARGO_PKG_VERSION")])).weak());
                        });
                    });
                    ui.add_space(8.0);
                    ui.add(egui::Label::new(tr("about.text")).wrap());
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label(trf("about.author", &[crate::APP_AUTHOR]));
                        let shown = crate::APP_AUTHOR_URL.trim_start_matches("https://");
                        ui.hyperlink_to(shown, crate::APP_AUTHOR_URL).on_hover_text(crate::APP_AUTHOR_URL);
                    });
                    ui.label(trf("about.started", &[crate::DEV_STARTED]));
                    ui.label(tr("about.built"));
                    if let Some(url) = crate::REPO_URL {
                        ui.horizontal(|ui| {
                            ui.label(tr("about.repo"));
                            ui.hyperlink(url);
                        });
                    }
                    ui.add_space(6.0);
                    ui.add(egui::Label::new(RichText::new(trf("about.files", &[&fmt::short_path(&self.base_dir)])).weak()).wrap());
                    ui.add_space(8.0);
                    copy = ui.button(tr("help.diag")).on_hover_text(tr("help.diag_hint")).clicked();
                });
                ui.add_space(8.0);
                close = dialog_buttons(ui, &tr("btn.close"), true, None).0;
            });
        if copy {
            self.copy_diagnostics();
        }
        let (enter, escape) = turn.keys(ctx);
        if open && !close && !enter && !escape {
            Outcome::Keep
        } else {
            Outcome::Close
        }
    }
}
