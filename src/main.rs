#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod backend;
mod conf;
mod events;
mod fmt;
mod groups;
mod health;
mod i18n;
mod icon;
mod ini;
mod monitor;
mod native;
mod ping;
mod settings;
mod shortcut;
mod stats;
mod tray;
mod uapi;
mod win;

use std::path::PathBuf;
use std::sync::Arc;

use backend::{Backend, Demo, Real};
use monitor::{Options, Shared};
use settings::Settings;

pub const APP_TITLE: &str = "AmneziaWG UI Dark";
pub const APP_AUTHOR: &str = "ssv555";
/// Страница автора — ссылка рядом с именем в «О программе».
pub const APP_AUTHOR_URL: &str = "https://github.com/ssv555";
/// Начало разработки (ISO 8601).
pub const DEV_STARTED: &str = "2026-10-03";
/// Ссылка на репозиторий — появится в «О программе», когда будет задана.
pub const REPO_URL: Option<&str> = Some("https://github.com/ssv555/AmneziaWG-UI-Dark");

const USAGE: &str = "awg-ui [--tray] [--demo] [--about] [--snapshot file.png] | --status | --autostart on|off";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|a| a == flag);
    let value = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
    if has("--help") || has("-h") {
        println!("{USAGE}");
        return;
    }
    let demo = has("--demo");
    let snapshot = value("--snapshot");
    // Каналы туннелей, службы и задача автозапуска доступны только администратору.
    if !demo && !win::is_elevated() {
        // Обычный запуск (ярлык, двойной клик) — через задачу с повышенными правами, без запроса UAC.
        if args.is_empty() && win::run_launch_task() {
            return;
        }
        let quoted: Vec<String> = args.iter().map(|a| format!("\"{a}\"")).collect();
        if !win::relaunch_elevated(&quoted.join(" ")) {
            eprintln!("administrator rights are required");
            std::process::exit(1);
        }
        return;
    }
    if has("--status") {
        std::process::exit(print_status());
    }
    // Проверка автоматизации родного окна без нашего окна: открыть его редактор туннеля.
    if let Some(tunnel) = value("--edit-native") {
        let result = Backend::Real(Real::new()).edit_in_native(&tunnel);
        println!("edit-native {tunnel}: {result:?}");
        std::process::exit(i32::from(result.is_err()));
    }
    // Проверка чтения сведений о туннеле из родного окна (без ключей и редактора).
    if let Some(tunnel) = value("--native-details") {
        match native::read_details(&backend::native_exe(), &tunnel) {
            Ok(info) => println!("{info:#?}"),
            Err(e) => println!("native-details {tunnel}: {e}"),
        }
        return;
    }
    // Проверка синхронизации без изменения конфига: прочитать из родного редактора и записать то же самое.
    // Печатает только длину и признаки — сам текст содержит ключи.
    if let Some(tunnel) = value("--sync-roundtrip") {
        let exe = backend::native_exe();
        let result = native::read_config(&exe, &tunnel).and_then(|text| {
            println!("read: {} chars, [Interface]: {}", text.len(), text.contains("[Interface]"));
            native::write_config(&exe, &tunnel, &text)?;
            let again = native::read_config(&exe, &tunnel)?;
            Ok(again == text)
        });
        println!("sync-roundtrip {tunnel}: {result:?}");
        std::process::exit(i32::from(!matches!(result, Ok(true))));
    }
    // Проверка: родной импорт с выделенным файлом (кнопку «Открыть» не нажимает).
    if let Some(file) = value("--import-native") {
        let result = Backend::Real(Real::new()).import_in_native(Some(std::path::Path::new(&file)));
        println!("import-native {file}: {result:?}");
        std::process::exit(i32::from(result.is_err()));
    }
    // Задача запуска без UAC (её же создаёт «Создать ярлык на рабочем столе»).
    if has("--launch-task") {
        let result = win::ensure_launch_task();
        println!("launch task: {result:?}");
        std::process::exit(i32::from(result.is_err()));
    }
    if let Some(mode) = value("--autostart") {
        let result = win::set_autostart(mode == "on");
        println!("autostart: {} ({:?})", win::autostart_enabled(), result);
        std::process::exit(i32::from(result.is_err()));
    }
    if !demo && snapshot.is_none() && win::another_instance(APP_TITLE) {
        return;
    }

    let dir = if demo { std::env::temp_dir().join("awg-ui-demo") } else { exe_dir() };
    let settings_path = dir.join("Settings.ini");
    let mut settings = Settings::from_ini(&ini::Ini::load(&settings_path));
    if demo && settings.groups.is_empty() {
        seed_demo_groups(&mut settings);
    }
    i18n::set(&dir.join("lang"), &settings.language);
    let options = Options {
        ping: settings.view.ping,
        ping_host: settings.ping_host.clone(),
        notify: settings.notify,
        tray: settings.tray,
    };
    let backend = if demo { Backend::Demo(Arc::new(Demo::new())) } else { Backend::Real(Real::new()) };
    let events_path = (!demo).then(|| dir.join(&settings.log_dir).join("events.log"));
    let shared = Arc::new(Shared::new(backend, options, Some(dir.join("Stats.ini")), events_path));
    if demo {
        seed_demo(&shared);
    }

    let mut viewport = eframe::egui::ViewportBuilder::default()
        .with_title(APP_TITLE)
        .with_inner_size([1280.0, 800.0])
        .with_min_inner_size([760.0, 480.0])
        .with_icon(eframe::egui::IconData { rgba: icon::rgba(64), width: 64, height: 64 })
        .with_maximized(settings.maximized);
    if let Some(w) = settings.window {
        viewport = viewport.with_inner_size([w.width.max(760.0), w.height.max(480.0)]);
        if win::on_screen(w.x, w.y) {
            viewport = viewport.with_position([w.x, w.y]);
        }
    }
    let options = eframe::NativeOptions { viewport, renderer: eframe::Renderer::Wgpu, ..Default::default() };
    let start = app::Start { settings, settings_path, base_dir: dir, hidden: has("--tray"), snapshot_file: snapshot, demo, about: has("--about") };
    if let Err(e) = eframe::run_native(APP_TITLE, options, Box::new(move |cc| Ok(Box::new(app::App::new(cc, shared, start))))) {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

fn exe_dir() -> PathBuf {
    std::env::current_exe().ok().and_then(|p| p.parent().map(PathBuf::from)).unwrap_or_else(|| PathBuf::from("."))
}

fn seed_demo_groups(s: &mut Settings) {
    // Вложенные группы, чтобы демо показывало дерево; lab.sg.* остаются «Без группы».
    let rules = [
        ("Europe/Netherlands", "nl-ams"),
        ("Europe/Germany", "de-fra"),
        ("Office", "office."),
        ("Travel/Nordics", "travel."),
        ("Lab", "lab.us"),
    ];
    let paths: Vec<String> = rules.iter().map(|(g, _)| g.to_string()).collect();
    s.groups = groups::normalize(&paths);
    for name in Demo::new().names() {
        if let Some((g, _)) = rules.iter().find(|(_, key)| name.contains(key)) {
            s.assignment.insert(name.clone(), g.to_string());
        }
    }
}

/// Демо: правдоподобная статистика, чтобы было видно колонки.
fn seed_demo(shared: &Shared) {
    let mut stats = shared.stats.lock().unwrap();
    if !stats.is_empty() {
        return;
    }
    let names: Vec<String> = match &shared.backend {
        Backend::Demo(d) => d.names().to_vec(),
        Backend::Real(_) => return,
    };
    let since = monitor::unix_now() - 30 * 86_400;
    for (i, name) in names.iter().enumerate() {
        let k = (i as u64 * 7919 % 97 + 3) as f64;
        let st = stats.entry(name.clone()).or_default();
        st.observe(0, 0, 1, None, since);
        st.observe((k * 41e6) as u64, (k * 6e6) as u64, 1, Some(1.0), since);
        st.seconds = k * 3600.0;
        st.peak_rx = k * 120_000.0;
        st.peak_tx = k * 30_000.0;
    }
}

/// Проверка без окна: то же, что видит интерфейс, текстом. Статистику не трогает.
fn print_status() -> i32 {
    let options = Options { ping: false, ping_host: String::new(), notify: false, tray: false };
    let shared = Shared::new(Backend::Real(Real::new()), options, None, None);
    monitor::poll(&shared);
    let snap = shared.snapshot.lock().unwrap();
    if let Some(e) = &snap.error {
        println!("error: {e}");
    }
    for name in &snap.tunnels {
        match snap.running.get(name) {
            None => println!("  {name}"),
            Some(live) => match (&live.status, &live.error) {
                (Some(st), _) => println!(
                    "* {name}: handshake {} s ago, rx {} tx {}, endpoint {}, allowed_ip {}, pubkey {}",
                    monitor::unix_now().saturating_sub(st.last_handshake_sec()),
                    st.rx_bytes(),
                    st.tx_bytes(),
                    st.peers.first().map(|p| p.endpoint.as_str()).unwrap_or("-"),
                    st.peers.iter().map(|p| p.allowed_ips.len()).sum::<usize>(),
                    st.public_key,
                ),
                (None, e) => println!("* {name}: error {}", e.as_deref().unwrap_or("?")),
            },
        }
    }
    println!("autostart: {}", win::autostart_enabled());
    i32::from(snap.error.is_some())
}
