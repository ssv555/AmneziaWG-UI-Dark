//! Состояние окна: размер и положение, закрытие, запись настроек, масштаб, снимок `--snapshot`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use eframe::egui;

use crate::monitor::Options;
use crate::settings::{WindowRect, MAX_SCALE, MIN_SCALE};
use crate::tray;

use super::App;

/// Настройки пишутся, когда изменения затихли на это время (перетаскивание не пишет файл на каждый пиксель).
const SAVE_DELAY: Duration = Duration::from_millis(300);
/// `--snapshot`: размер окна в точках, высота графика и предельное ожидание его заполнения.
const SNAPSHOT_SIZE: [f32; 2] = [1600.0, 1100.0];
const SNAPSHOT_GRAPH_H: f32 = 240.0;
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(600);
/// Скрытый запуск: окно прячем на этом кадре — eframe показывает его после первых кадров.
const HIDE_AT_FRAME: u64 = 5;

/// Что сделать с настройками на этом кадре (`WindowState::save_step`).
#[derive(PartialEq, Eq, Debug)]
pub(super) enum SaveStep {
    /// Файл совпадает с настройками.
    Clean,
    /// Изменения свежие — подождать, пока затихнут.
    Wait,
    /// Затихли — писать.
    Write,
}

/// Состояние окна как программы: кадры, скрытый запуск, масштаб, запись Settings.ini с задержкой, `--snapshot`.
/// Размер и положение окна лежат в `Settings` — здесь только то, что не сохраняется.
pub(super) struct WindowState {
    frame: u64,
    hide_on_frame: Option<u64>,
    /// Масштаб, который окно уже получило (сравнение ловит Ctrl+Plus/Minus).
    zoom: f32,
    settings_path: PathBuf,
    /// Текст Settings.ini, как он лежит на диске.
    saved_text: String,
    changed_at: Option<Instant>,
    snapshot_file: Option<String>,
    started: Instant,
}

impl WindowState {
    pub(super) fn new(settings_path: PathBuf, saved_text: String, snapshot_file: Option<String>, start_hidden: bool) -> Self {
        WindowState {
            frame: 0,
            hide_on_frame: start_hidden.then_some(HIDE_AT_FRAME),
            zoom: 1.0,
            settings_path,
            saved_text,
            changed_at: None,
            snapshot_file,
            started: Instant::now(),
        }
    }

    /// Начало кадра. `true` — пора спрятать окно в трей (один раз).
    pub(super) fn begin_frame(&mut self) -> bool {
        self.frame += 1;
        let due = self.hide_on_frame.is_some_and(|f| self.frame >= f);
        if due {
            self.hide_on_frame = None;
        }
        due
    }

    /// Окно ещё ждёт кадра, чтобы спрятаться: показано лишь формально.
    pub(super) fn hide_pending(&self) -> bool {
        self.hide_on_frame.is_some()
    }

    pub(super) fn is_snapshot(&self) -> bool {
        self.snapshot_file.is_some()
    }

    pub(super) fn settings_path(&self) -> &Path {
        &self.settings_path
    }

    /// Масштаб интерфейса: окно (Ctrl+Plus/Minus) и настройки (меню) могут разойтись; побеждает то, что изменилось.
    /// Возвращает масштаб для настроек и масштаб, который надо выставить окну, если тот отличается.
    pub(super) fn sync_zoom(&mut self, window: f32, setting: f32) -> (f32, Option<f32>) {
        let setting = if (window - self.zoom).abs() > 0.001 { window.clamp(MIN_SCALE, MAX_SCALE) } else { setting };
        let apply = ((setting - window).abs() > 0.001).then_some(setting);
        // Новый масштаб вступает в силу со следующего кадра.
        self.zoom = setting;
        (setting, apply)
    }

    /// Писать ли настройки: `text` — их нынешний вид. Запись откладывается, пока изменения не затихли на `SAVE_DELAY`.
    pub(super) fn save_step(&mut self, text: &str, now: Instant) -> SaveStep {
        if text == self.saved_text {
            self.changed_at = None;
            return SaveStep::Clean;
        }
        let since = *self.changed_at.get_or_insert(now);
        if now.duration_since(since) < SAVE_DELAY {
            SaveStep::Wait
        } else {
            SaveStep::Write
        }
    }

    /// `text` лёг на диск.
    pub(super) fn saved(&mut self, text: String) {
        self.saved_text = text;
        self.changed_at = None;
    }
}

impl App {
    /// Размер, положение, развёрнутость окна — в настройки (сохраняются вместе с остальным).
    pub(super) fn track_window(&mut self, ctx: &egui::Context) {
        let (outer, inner, maximized, minimized) = ctx.input(|i| {
            let v = i.viewport();
            (v.outer_rect, v.inner_rect, v.maximized, v.minimized)
        });
        if minimized == Some(true) || self.window.hide_pending() {
            return;
        }
        if let Some(m) = maximized {
            self.s.maximized = m;
        }
        if !self.s.maximized {
            if let (Some(o), Some(i)) = (outer, inner) {
                self.s.window = Some(WindowRect { x: o.min.x, y: o.min.y, width: i.width(), height: i.height() });
            }
        }
    }

    pub(super) fn handle_close(&mut self, ctx: &egui::Context) {
        if !ctx.input(|i| i.viewport().close_requested()) {
            return;
        }
        if self.window.is_snapshot() {
            return;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        if self.s.tray && self.s.close_to_tray {
            tray::hide_window();
        } else {
            // Крестик без сворачивания в трей — это выход: при подключённых туннелях спросить.
            self.request_exit(false);
        }
    }

    /// Изменения — в Settings.ini сразу, как только затихли на SAVE_DELAY.
    pub(super) fn save_settings(&mut self, ctx: &egui::Context) {
        let ini = self.s.to_ini();
        let text = ini.to_text();
        match self.window.save_step(&text, Instant::now()) {
            SaveStep::Clean => {}
            SaveStep::Wait => ctx.request_repaint_after(SAVE_DELAY),
            SaveStep::Write => match ini.save(self.window.settings_path()) {
                Ok(()) => self.window.saved(text),
                Err(e) => self.action_error.push(crate::fsutil::io_ctx(self.window.settings_path(), e)),
            },
        }
    }

    /// Окно меняет настройки — фоновым потокам нужны пинг, уведомления, трей.
    pub(super) fn push_options(&self) {
        let new = Options {
            ping: self.s.view.ping,
            ping_host: self.s.ping_host.trim().to_string(),
            notify: self.s.notify,
            tray: self.s.tray,
            taskbar: self.s.taskbar,
        };
        // Прежние настройки нужны, чтобы увидеть, что изменилось; новые встают сразу.
        let o = self.shared.replace_options(new.clone());
        if o.tray != new.tray {
            tray::set_visible(new.tray);
        }
        // Пингует ядро: ему — включение и узел при каждом изменении (запрос дешёвый, ядро пишет core.ini; демо-ядро просто
        // соглашается).
        if o.ping != new.ping || o.ping_host != new.ping_host {
            let (core, enabled, host, error) = (self.core.clone(), new.ping, new.ping_host.clone(), self.action_error.clone());
            std::thread::spawn(move || {
                if let Err(e) = core.ok(crate::daemon::proto::Request::SetPing { enabled, host }) {
                    error.push(e);
                }
            });
        }
    }

    /// Масштаб интерфейса поверх масштаба Windows: из меню — в окно, с клавиатуры (Ctrl+Plus/Minus) — в настройки.
    pub(super) fn sync_zoom(&mut self, ctx: &egui::Context) {
        if self.window.is_snapshot() {
            return; // у снимка свой масштаб
        }
        let (scale, apply) = self.window.sync_zoom(ctx.zoom_factor(), self.s.ui_scale);
        self.s.ui_scale = scale;
        if let Some(zoom) = apply {
            ctx.set_zoom_factor(zoom);
        }
    }

    /// `--snapshot файл.png`: кадр окна в масштабе Windows, когда график заполнен за весь период; потом выход.
    pub(super) fn handle_snapshot(&mut self, ctx: &egui::Context) {
        let Some(path) = self.window.snapshot_file.clone() else { return };
        ctx.request_repaint();
        if self.window.frame == 3 {
            // Размер в точках: при масштабе Windows 150 % снимок выйдет в полтора раза больше в пикселях.
            self.s.graph_height = SNAPSHOT_GRAPH_H;
            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(SNAPSHOT_SIZE.into()));
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(0.0, 0.0)));
        }
        let elapsed = self.window.started.elapsed();
        let period = self.s.graph_period.max(60) as f64 + 3.0;
        let graph_full = self.shared.observed_for(period);
        if elapsed > Duration::from_secs(6) && (graph_full || elapsed > SNAPSHOT_TIMEOUT) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
            self.window.started += Duration::from_secs(3600);
        }
        let image = ctx.input(|i| {
            i.raw.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(image) = image {
            if let Err(e) = save_png(&path, &image) {
                eprintln!("{path}: {e}");
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

fn save_png(path: &str, image: &egui::ColorImage) -> std::io::Result<()> {
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut encoder = png::Encoder::new(file, image.size[0] as u32, image.size[1] as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(std::io::Error::other)?;
    let data: Vec<u8> = image.pixels.iter().flat_map(|c| c.to_array()).collect();
    writer.write_image_data(&data).map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(hidden: bool, saved: &str) -> WindowState {
        WindowState::new(PathBuf::from("Settings.ini"), saved.into(), None, hidden)
    }

    #[test]
    fn save_waits_until_changes_settle_then_writes() {
        let mut w = state(false, "a=1");
        let t0 = Instant::now();
        assert_eq!(w.save_step("a=1", t0), SaveStep::Clean);
        // Первое изменение запускает отсчёт; пока идёт перетаскивание, файл не пишется.
        assert_eq!(w.save_step("a=2", t0), SaveStep::Wait);
        assert_eq!(w.save_step("a=3", t0 + SAVE_DELAY / 2), SaveStep::Wait);
        assert_eq!(w.save_step("a=3", t0 + SAVE_DELAY), SaveStep::Write);
        // Запись не удалась (текст не отмечен сохранённым) — следующий кадр пробует снова, не ждёт заново.
        assert_eq!(w.save_step("a=3", t0 + SAVE_DELAY * 2), SaveStep::Write);
        w.saved("a=3".into());
        assert_eq!(w.save_step("a=3", t0 + SAVE_DELAY * 3), SaveStep::Clean);
    }

    #[test]
    fn reverting_a_change_cancels_the_pending_save() {
        let mut w = state(false, "a=1");
        let t0 = Instant::now();
        assert_eq!(w.save_step("a=2", t0), SaveStep::Wait);
        assert_eq!(w.save_step("a=1", t0 + SAVE_DELAY / 2), SaveStep::Clean);
        // Новое изменение снова отсчитывает задержку с нуля.
        assert_eq!(w.save_step("a=2", t0 + SAVE_DELAY), SaveStep::Wait);
    }

    #[test]
    fn hidden_start_hides_the_window_once_after_the_first_frames() {
        let mut w = state(true, "");
        assert!(w.hide_pending());
        for _ in 1..HIDE_AT_FRAME {
            assert!(!w.begin_frame());
        }
        assert!(w.begin_frame());
        assert!(!w.hide_pending());
        assert!(!w.begin_frame());
        let mut shown = state(false, "");
        assert!(!shown.hide_pending() && !shown.begin_frame());
    }

    #[test]
    fn zoom_follows_whichever_side_changed() {
        let mut w = state(false, "");
        // Ничего не менялось: окно уже в масштабе настроек.
        assert_eq!(w.sync_zoom(1.0, 1.0), (1.0, None));
        // Меню поменяло настройку — окну выставить новое.
        assert_eq!(w.sync_zoom(1.0, 1.5), (1.5, Some(1.5)));
        // Окно получило 1.5; Ctrl+Minus сдвинул его — настройка идёт за окном, окну ничего не выставляется.
        assert_eq!(w.sync_zoom(1.5, 1.5), (1.5, None));
        assert_eq!(w.sync_zoom(1.25, 1.5), (1.25, None));
        // Ctrl+Plus за пределы допустимого: настройка зажата, окну выставляется зажатое значение.
        let (scale, apply) = w.sync_zoom(MAX_SCALE + 1.0, 1.25);
        assert_eq!((scale, apply), (MAX_SCALE, Some(MAX_SCALE)));
    }
}
