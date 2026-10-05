#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod archive;
mod backend;
mod conf;
mod crash;
mod daemon;
mod elevated;
mod engine;
mod events;
mod fmt;
mod fsutil;
mod groups;
mod health;
mod i18n;
mod icon;
mod ini;
mod monitor;
mod native;
mod ping;
mod scm;
mod settings;
mod shortcut;
mod stats;
mod store;
mod taskbar;
mod tray;
mod uapi;
mod update;
mod win;

use std::path::PathBuf;
use std::sync::Arc;

use backend::{Demo, Real};
use daemon::CoreApi;
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

/// Заголовок окна: имя и версия. По нему же второй запуск находит уже открытое окно.
fn window_title() -> String {
    format!("{APP_TITLE} v{}", env!("CARGO_PKG_VERSION"))
}

const USAGE: &str =
    "awg-ui [--tray] [--demo] [--about] [--snapshot file.png] | --install-core | --uninstall-core | --status | --autostart on|off";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|a| a == flag);
    let value = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
    if has("--help") || has("-h") {
        println!("{USAGE}");
        return;
    }
    // Процессы, которые запускает Windows: служба туннеля встроенного движка и служба ядра (обе — SYSTEM).
    if let Some(conf) = value(engine::SERVICE_FLAG) {
        std::process::exit(engine::run_service(std::path::Path::new(&conf)));
    }
    if has(daemon::SERVICE_FLAG) {
        // Отодвинутые обновлением файлы прошлой версии служба удаляет сама — когда новое ядро уже отвечает.
        std::process::exit(daemon::service::main());
    }
    // Перезапуск ядра после обновления сборки (его запускает само ядро, от SYSTEM).
    if has(update::ours::RESTART_FLAG) {
        std::process::exit(update::ours::restart_core());
    }
    // Помощник ядра в сеансе пользователя: действие в родном окне AmneziaWG.
    if let Some(task) = value(daemon::helper::FLAG) {
        std::process::exit(daemon::helper::main(std::path::Path::new(&task)));
    }
    let demo = has("--demo");
    let snapshot = value("--snapshot");
    // Установка ядра и проверки без окна требуют прав администратора — один запрос UAC. Само окно работает без них.
    const ADMIN_FLAGS: [&str; 11] = [
        daemon::install::INSTALL_FLAG,
        daemon::install::UNINSTALL_FLAG,
        "--status",
        "--engine-import",
        "--export-native",
        "--engine-connect",
        "--engine-disconnect",
        "--edit-native",
        "--native-details",
        "--sync-roundtrip",
        "--import-native",
    ];
    if ADMIN_FLAGS.iter().any(|f| has(f)) && !win::is_elevated() {
        let quoted: Vec<String> = args.iter().map(|a| win::quote_arg(a)).collect();
        std::process::exit(win::run_elevated_wait(&quoted.join(" ")).map_or(1, |code| code as i32));
    }
    // Окно передаёт свою учётную запись (`--owner`) и канал для итога (`--result`): установку может подтвердить
    // паролем другой администратор, и тогда у этого процесса другой пользователь.
    if has(daemon::install::INSTALL_FLAG) || has(daemon::install::UNINSTALL_FLAG) {
        let result = if has(daemon::install::INSTALL_FLAG) {
            let owner = value("--owner").map_or_else(win::current_user_sid, Ok);
            owner.and_then(|sid| daemon::install::install(&sid)).map(|autostart| {
                if autostart { daemon::install::AUTOSTART_NOTE } else { "" }.to_string()
            })
        } else {
            daemon::install::uninstall().map(|()| String::new())
        };
        elevated::report(value(elevated::RESULT_FLAG).as_deref(), &result);
        std::process::exit(i32::from(result.is_err()));
    }
    if has("--status") {
        std::process::exit(print_status());
    }
    // Встроенный движок без окна и без ядра: положить .conf в хранилище, поднять и опустить туннель.
    if let Some(file) = value("--engine-import") {
        let path = std::path::Path::new(&file);
        let result = store::read(path).and_then(|text| store::write(&engine::tunnel_name(path), &text));
        println!("engine-import {file}: {result:?}");
        std::process::exit(i32::from(result.is_err()));
    }
    // Родной «Export all tunnels to zip» в файл; печатает только число туннелей (в архиве — ключи).
    if let Some(file) = value("--export-native") {
        let result = native::export_all(&backend::native_exe(), std::path::Path::new(&file)).map(|e| e.len());
        println!("export-native {file}: {result:?}");
        std::process::exit(i32::from(result.is_err()));
    }
    if let Some(tunnel) = value("--engine-connect") {
        let result = engine::connect(&store::path(&tunnel));
        println!("engine-connect {tunnel}: {result:?}");
        std::process::exit(i32::from(result.is_err()));
    }
    if let Some(tunnel) = value("--engine-disconnect") {
        let result = engine::disconnect(&tunnel);
        println!("engine-disconnect {tunnel}: {result:?}");
        std::process::exit(i32::from(result.is_err()));
    }
    // Проверка автоматизации родного окна без нашего окна: открыть его редактор туннеля.
    if let Some(tunnel) = value("--edit-native") {
        let result = Real::new().edit_in_native(&tunnel);
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
        let result = Real::new().import_in_native(Some(std::path::Path::new(&file)));
        println!("import-native {file}: {result:?}");
        std::process::exit(i32::from(result.is_err()));
    }
    // Проверка ядра теми же запросами, что шлёт окно (права администратора не нужны — канал открыт владельцу).
    if has("--core-status") {
        std::process::exit(print_core_status());
    }
    if let Some(tunnel) = value("--core-details") {
        let result = daemon::PipeClient.info(daemon::proto::Request::Details(tunnel.clone())).map(|i| (i.addresses.len(), i.peers.len()));
        println!("core-details {tunnel}: (addresses, peers) = {result:?}");
        std::process::exit(i32::from(result.is_err()));
    }
    if has("--core-take-native") {
        let result = daemon::PipeClient.report(daemon::proto::Request::TakeNative).map(|r| (r.added.len(), r.existing.len(), r.bad_name.len()));
        println!("core-take-native: (added, existing, bad name) = {result:?}");
        std::process::exit(i32::from(result.is_err()));
    }
    for (flag, plan) in [("--core-connect", daemon::proto::Plan::Connect), ("--core-disconnect", daemon::proto::Plan::Disconnect)] {
        if let Some(tunnel) = value(flag) {
            // «Несколько туннелей одновременно» — из настроек окна, как при нажатии кнопки в нём.
            let multiple = Settings::from_ini(&ini::Ini::load(&exe_dir().join("Settings.ini"))).multiple;
            let result = daemon::PipeClient.ok(daemon::proto::Request::Switch { tunnel: tunnel.clone(), plan, multiple });
            println!("{flag} {tunnel}: {result:?}");
            std::process::exit(i32::from(result.is_err()));
        }
    }
    if let Some(mode) = value("--core-mode") {
        let mode = if mode == "engine" { settings::Mode::Engine } else { settings::Mode::Overlay };
        let result = daemon::PipeClient.ok(daemon::proto::Request::SetMode(mode));
        println!("core-mode {mode:?}: {result:?}");
        std::process::exit(i32::from(result.is_err()));
    }
    // Проверка менеджера обновлений: `state` — без сети; `check` — проверка источников, ждёт её конца (до 2 мин);
    // `restore <id>` — помощник окна с правами администратора: отправить возврат ядру и выйти, ход виден в окне.
    if let Some(pos) = args.iter().position(|a| a == update::CLI_FLAG) {
        use update::UpdateOp;
        let op = match update::parse_cli(&args[pos + 1..]) {
            Ok(op) => op,
            Err(e) => {
                println!("{e}");
                std::process::exit(2);
            }
        };
        if let UpdateOp::Restore(id) = op {
            std::process::exit(restore_helper(id, value(elevated::RESULT_FLAG).as_deref()));
        }
        let what = &args[pos + 1];
        let mut result = daemon::PipeClient.updates(op);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        while matches!(&result, Ok(s) if s.busy.is_some()) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_secs(1));
            result = daemon::PipeClient.updates(UpdateOp::State);
        }
        match &result {
            Ok(s) => {
                println!("core-updates {what}: checked_at {:?}, busy {:?}", s.checked_at, s.busy);
                for c in &s.components {
                    let available = c.available.as_ref().map(|a| a.version.as_str());
                    println!("  {:?}: installed {:?}, available {available:?}, update {}, error {:?}", c.component, c.installed, c.update, c.error);
                }
                println!("  history: {} entries", s.history.len());
                for e in s.history.iter().take(5) {
                    println!("  #{} {:?} {:?} {:?} -> {:?} ok {} backup {:?} error {:?}", e.id, e.action, e.component, e.from, e.to, e.ok, e.backup, e.error);
                }
            }
            Err(e) => println!("core-updates {what}: {e}"),
        }
        std::process::exit(i32::from(!matches!(&result, Ok(s) if s.busy.is_none())));
    }
    if let Some(mode) = value("--autostart") {
        let result = win::set_autostart(mode == "on");
        println!("autostart: {} ({:?})", win::autostart_enabled(), result);
        std::process::exit(i32::from(result.is_err()));
    }
    // После самообновления окна: дождаться выхода прежнего процесса, убрать его отодвинутый exe.
    update::ours::window_startup_cleanup();
    if !demo && snapshot.is_none() && win::another_instance(&window_title()) {
        return;
    }

    let dir = if demo { std::env::temp_dir().join("awg-ui-demo") } else { exe_dir() };
    let settings_path = dir.join("Settings.ini");
    let (settings_ini, settings_problem) = ini::Ini::load_guarded(&settings_path);
    let mut settings = Settings::from_ini(&settings_ini);
    if demo && !settings.book.has_groups() {
        seed_demo_groups(&mut settings);
    }
    i18n::set(&dir.join("lang"), &settings.language);
    // Окно без консоли: паника и сбой запуска иначе прошли бы молча — в crash.log и сообщением пользователю.
    let log_dir = crash::window_log_dir(&dir, &settings.log_dir);
    crash::install_window(log_dir.clone());
    let options = Options {
        ping: settings.view.ping,
        ping_host: settings.ping_host.clone(),
        notify: settings.notify,
        tray: settings.tray,
        taskbar: settings.taskbar,
    };
    // Окно — клиент ядра: туннели, статистика и журнал живут там. Демо — выдуманное ядро прямо в окне.
    // Одни и те же выдуманные туннели — у опроса окна (`Shared`) и у демо-ядра, которому окно шлёт команды.
    let demo = demo.then(|| Arc::new(Demo::new()));
    let shared = match &demo {
        Some(d) => Arc::new(Shared::new(Some(d.clone()), options, Some(dir.join("Stats.ini")), None)),
        None => Arc::new(Shared::new(None, options, None, None)),
    };
    if let Some(d) = &demo {
        seed_demo(&shared, d);
    }

    let mut viewport = eframe::egui::ViewportBuilder::default()
        .with_title(window_title())
        .with_inner_size([1280.0, 800.0])
        .with_min_inner_size([760.0, 480.0])
        .with_icon(eframe::egui::IconData { rgba: icon::themed(64, settings.mode() == settings::Mode::Engine), width: 64, height: 64 })
        .with_maximized(settings.maximized);
    if let Some(w) = settings.window {
        viewport = viewport.with_inner_size([w.width.max(760.0), w.height.max(480.0)]);
        if win::on_screen(w.x, w.y) {
            viewport = viewport.with_position([w.x, w.y]);
        }
    }
    let options = eframe::NativeOptions { viewport, renderer: eframe::Renderer::Wgpu, ..Default::default() };
    let start = app::Start { settings, settings_path, base_dir: dir, hidden: has("--tray"), snapshot_file: snapshot, demo, about: has("--about"), settings_problem };
    if let Err(e) = eframe::run_native(APP_TITLE, options, Box::new(move |cc| Ok(Box::new(app::App::new(cc, shared, start))))) {
        crash::report_window_failure(&log_dir, &e.to_string(), "crash.start");
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
    s.book.replace_groups(&paths);
    for name in Demo::new().names() {
        if let Some((g, _)) = rules.iter().find(|(_, key)| name.contains(key)) {
            if let Err(groups::NoSuchGroup(g)) = s.book.assign(name, Some(g)) {
                eprintln!("demo: группа {g} не создана, туннель {name} остаётся без группы");
            }
        }
    }
}

/// Демо: несколько прошлых событий, чтобы журнал не был пустым.
fn seed_demo_events(shared: &Shared) {
    use events::{Event, Severity};
    use i18n::{tr, trf};
    let now = monitor::unix_now();
    let items = [
        (5400, "office.gw-primary", Severity::Info, tr("ev.connected")),
        (4100, "office.gw-primary", Severity::Warn, trf("health.stale", &["3 min"])),
        (4040, "office.gw-primary", Severity::Info, tr("ev.restored")),
        (2700, "office.gw-primary", Severity::Info, tr("ev.disconnected")),
        (1500, "travel.fi-hel.v4", Severity::Info, tr("ev.connected")),
        (900, "travel.fi-hel.v4", Severity::Bad, tr("ev.dropped")),
        (30, "home.nl-ams.full", Severity::Info, tr("ev.connected")),
    ];
    for (ago, tunnel, severity, text) in items {
        shared.push_event(Event::new(now - ago, tunnel, severity, &text, false));
    }
}

/// Демо: правдоподобная статистика, чтобы было видно колонки.
fn seed_demo(shared: &Shared, demo: &Demo) {
    seed_demo_events(shared);
    let names = demo.names();
    let since = monitor::unix_now() - 30 * 86_400;
    shared.update_stats(|stats| {
        if !stats.is_empty() {
            return;
        }
        for (i, name) in names.iter().enumerate() {
            let k = (i as u64 * 7919 % 97 + 3) as f64;
            let st = stats.entry(name.clone()).or_default();
            st.observe(0, 0, 1, None, since);
            st.observe((k * 41e6) as u64, (k * 6e6) as u64, 1, Some(1.0), since);
            st.seconds = k * 3600.0;
            st.peak_rx = k * 120_000.0;
            st.peak_tx = k * 30_000.0;
        }
    });
}

/// Помощник «Вернуть» (с правами администратора): отправить ядру возврат `id`, не дожидаясь его конца, итог — в
/// канал окна `result` (без консоли окну больше негде его увидеть). Код выхода: 0 — ядро приняло возврат.
fn restore_helper(id: u64, result: Option<&str>) -> i32 {
    let sent = daemon::PipeClient.updates(update::UpdateOp::Restore(id));
    println!("core-updates restore {id}: {:?}", sent.as_ref().map(|s| s.busy.clone()));
    let sent = sent.map(|_| String::new());
    elevated::report(result, &sent);
    i32::from(sent.is_err())
}

/// Ядро глазами окна: версия, режим, туннели, подключённые (без ключей и адресов).
fn print_core_status() -> i32 {
    use daemon::proto::{Request, Response};
    match daemon::PipeClient.hello() {
        Ok((version, mode)) => println!("core {version}, mode {mode:?}"),
        Err(e) => {
            println!("core: {e}");
            return 1;
        }
    }
    match daemon::pipe::call(&Request::State { events_after: 0 }) {
        Ok(Response::State(s)) => {
            println!("tunnels: {}, running: {:?}, events: {}, error: {:?}", s.tunnels.len(), s.running.keys().collect::<Vec<_>>(), s.events.len(), s.error);
            println!("service: {}", s.service);
            0
        }
        other => {
            println!("state: {other:?}");
            1
        }
    }
}

/// Проверка без окна: то же, что видит интерфейс, текстом. Статистику не трогает.
fn print_status() -> i32 {
    let options = Options { ping: false, ping_host: String::new(), notify: false, tray: false, taskbar: false };
    let host = Arc::new(Real::new());
    let shared = Shared::new(Some(host.clone()), options, None, None);
    monitor::poll(&shared, host.as_ref());
    let snap = shared.snapshot_clone();
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
