//! Окно: меню «Вид»/«Настройки», слева таблица туннелей (группы или плоский список),
//! справа статус сверху, итоги, график, детали; снизу журнал событий и строка состояния.
//! Числа — моноширинным шрифтом в ячейках фиксированной ширины: при смене значений ничего не сдвигается.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use eframe::egui;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use crate::daemon::proto::{NativeOp, Plan, Request, Response};
use crate::daemon::{CoreApi, PipeClient};
use crate::fmt;
use crate::groups;
use crate::health::{Health, Level};
use crate::i18n::{self, tr, trf};
use crate::monitor::{self, FrameView, Shared};
use crate::settings::{Mode, Settings, SortKey};
use crate::{tray, win};

mod about;
mod core_ui;
mod demo_core;
mod details;
mod dialog;
mod editor;
mod engine_mode;
mod errors;
mod exit;
mod graph;
mod group_dialog;
mod list;
mod markdown;
mod menu;
mod modals;
mod sources;
mod status;
mod theme;
mod updates;
mod watcher;
mod window;
use core_ui::{send_language, CoreLink, Probe};
use details::{details, Detail};
use dialog::dialog_buttons;
use editor::Editor;
use errors::ErrorSink;
use group_dialog::Dialog;
use list::{tunnel_list, Keys, List, ROW_H};
use menu::menu_bar;
use modals::{Modal, Modals, Outcome, Turn};
use sources::Confirm;
use updates::UpdatesWindow;
use status::{event_log, status_bar, StatusBar};
use theme::*;
use watcher::SourceWatcher;
use window::WindowState;

pub struct Start {
    pub settings: Settings,
    pub settings_path: PathBuf,
    /// Папка программы: Settings.ini, Stats.ini, lang.
    pub base_dir: PathBuf,
    /// Запуск из автозапуска: сразу спрятаться в трей.
    pub hidden: bool,
    pub snapshot_file: Option<String>,
    /// Демо-режим: выдуманные туннели вместо ядра (те же, что у `Shared` окна).
    pub demo: Option<Arc<crate::backend::Demo>>,
    /// Открыть «О программе» сразу (для снимка окна).
    pub about: bool,
    /// Settings.ini был, но не прочитался (отодвинут в сторону): сказать об этом в журнале и в `window-errors.log`.
    pub settings_problem: Option<crate::ini::Unreadable>,
}

enum Action {
    Select(String),
    Switch(String, Plan),
    /// «Повторить»: ядро переподключает туннель по расписанию с начала.
    Retry(String),
    Assign(String, Option<String>),
    MoveGroup(String, isize),
    /// Группа к новому родителю (`None` — верхний уровень).
    Reparent(String, Option<String>),
    DeleteGroup(String),
    /// Выделить строку группы (или «Без группы») — для клавиш F2, Delete, стрелок.
    SelectGroup(String),
    ToggleCollapse(String),
    Sort(SortKey),
    Open(Dialog),
    Autostart(bool),
    OpenOriginal,
    EditNative(String),
    OpenConf,
    AddConf,
    EditSource(String),
    SetSource(String),
    ImportSource(String),
    Exit,
    ExitWithNative,
    Language(String),
    AddLanguage,
    OpenLangFolder,
    About,
    /// Справка → «Проверить обновления…».
    CheckUpdates,
    ShowLog,
    ClearNotice,
    DesktopShortcut,
    Confirm(Confirm),
    /// Прочитать сведения о неподключённом туннеле из родного окна.
    ReadNative(String),
    /// Сменить режим работы (сначала окно со справкой).
    ChooseMode(Mode),
    EngineImport,
    EngineRestore,
    EngineTakeNative,
    EngineBackup,
    EngineNew,
    EngineEdit(String),
    EngineRename(String),
    /// Установить или обновить ядро (запрос UAC).
    InstallCore,
}

pub struct App {
    s: Settings,
    shared: Arc<Shared>,
    ctx: egui::Context,
    base_dir: PathBuf,
    about_icon: Option<egui::TextureHandle>,
    editor: Option<Editor>,
    /// Источники, открытые на правку, и кэш сведений о конфигах.
    sources: SourceWatcher,
    /// Туннели, удалённые в AmneziaWG фоновым потоком, — убрать из настроек.
    deleted: Arc<Mutex<Vec<String>>>,
    /// Подсказка пользователю в строке состояния.
    notice: Arc<Mutex<Option<String>>>,
    /// None — неизвестно (демо-режим).
    autostart: Option<bool>,
    action_error: ErrorSink,
    search: String,
    /// Кадры, скрытый запуск, масштаб, запись настроек, снимок.
    window: WindowState,
    /// Связь с ядром: полоса «установить / обновить», сверка в фоне.
    core_link: CoreLink,
    hwnd: isize,
    /// Режим, чей вид (иконки, разделители, рамка окна) уже выставлен.
    look: Option<Mode>,
    /// Ошибка ушла в журнал, пока панель журнала скрыта.
    unseen_error: bool,
    /// «Выход» из меню трея ждёт разбора в кадре.
    exit_request: Arc<AtomicBool>,
    /// Модальные диалоги: стек, Enter и Esc — верхнему.
    modals: Modals,
    /// Окно «Обновления и откаты» и отметка «есть новое» в меню «Справка».
    updates: UpdatesWindow,
    /// Ядро, с которым говорит окно (в демо — `DemoCore`).
    core: Core,
}

impl App {
    pub fn new(cc: &eframe::CreationContext, shared: Arc<Shared>, start: Start) -> Self {
        let ctx = cc.egui_ctx.clone();
        ctx.set_theme(egui::Theme::Dark);
        add_fallback_fonts(&ctx);
        ctx.all_styles_mut(|s| {
            for (style, font) in s.text_styles.iter_mut() {
                font.size = match style {
                    egui::TextStyle::Heading => 20.0,
                    egui::TextStyle::Small => 12.0,
                    egui::TextStyle::Monospace => 14.0,
                    _ => 15.0,
                };
            }
        });
        let exit_request = Arc::new(AtomicBool::new(false));
        let hwnd = match cc.window_handle().map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Win32(w)) => w.hwnd.get(),
            _ => 0,
        };
        if hwnd != 0 {
            win::dark_title_bar(hwnd);
            // «Выход» в трее — как в меню: при подключённых туннелях окно спросит, отключать ли их.
            // Скрытое окно кадров не рисует — его сначала показываем; без туннелей выходим сразу.
            let (request, repaint, on_exit_shared) = (exit_request.clone(), ctx.clone(), shared.clone());
            let on_exit = Box::new(move || {
                if !on_exit_shared.any_running() {
                    on_exit_shared.save_stats();
                    tray::remove();
                    std::process::exit(0);
                }
                request.store(true, std::sync::atomic::Ordering::SeqCst);
                tray::show_window();
                repaint.request_repaint();
            });
            tray::install(hwnd, ctx.clone(), start.settings.tray && start.snapshot_file.is_none(), on_exit);
        }
        let repaint = ctx.clone();
        monitor::spawn(shared.clone(), Box::new(move || repaint.request_repaint()));
        // Пинг и служба менеджера AmneziaWG — забота ядра; в демо пинг выдуманный, прямо в окне.
        let action_error = ErrorSink::new(shared.clone(), ctx.clone());
        let core: Core = match &start.demo {
            Some(demo) => Arc::new(demo_core::DemoCore(demo.clone())),
            None => Arc::new(PipeClient),
        };
        // Журнал в памяти пропадёт вместе с окном — ошибки действий ещё и в файл рядом с `crash.log` (в демо не пишем).
        let action_error = if start.demo.is_some() {
            action_error
        } else {
            action_error.persisted_to(crate::crash::window_log_dir(&start.base_dir, &start.settings.log_dir).join(errors::FILE_NAME))
        };
        // Настройки при запуске не прочитались — окно открылось со значениями по умолчанию; без этой записи
        // пользователь не узнал бы, почему они сбросились и где копия прежнего файла.
        if let Some(problem) = &start.settings_problem {
            action_error.push(problem.text());
        }
        let core_link = if start.demo.is_some() { CoreLink::demo() } else { CoreLink::checking() };
        if start.demo.is_some() {
            crate::ping::spawn(shared.clone());
        } else {
            core_link.check(core.clone(), ctx.clone(), action_error.clone(), Probe::Start);
        }
        let mut modals = Modals::default();
        if start.about {
            modals.open(Modal::About);
        }
        let notice = Arc::<Mutex<Option<String>>>::default();
        let updates = UpdatesWindow::new(updates::Link::new(core.clone(), action_error.clone(), notice.clone(), ctx.clone(), shared.clone()));
        let start_hidden = start.hidden && start.settings.tray;
        let saved_text = if start.settings_path.exists() { start.settings.to_ini().to_text() } else { String::new() };
        App {
            autostart: start.demo.is_none().then(win::autostart_enabled),
            s: start.settings,
            shared,
            ctx,
            base_dir: start.base_dir,
            about_icon: None,
            editor: None,
            sources: SourceWatcher::default(),
            deleted: Default::default(),
            notice,
            action_error,
            search: String::new(),
            window: WindowState::new(start.settings_path, saved_text, start.snapshot_file, start_hidden),
            core_link,
            hwnd,
            look: None,
            unseen_error: false,
            exit_request,
            modals,
            updates,
            core,
        }
    }

    /// Окно обновлений и его подтверждение: поведение — в `UpdatesWindow`, здесь только то, что принадлежит App
    /// (настройки, скрытое в трей окно, диалоги `Modals`).
    fn show_updates_window(&mut self, ctx: &egui::Context) {
        let fresh = self.updates.tick(&mut self.s.update_notified);
        let hidden = tray::window_hidden() || self.window.hide_pending();
        if !fresh.is_empty() && hidden && self.s.notify {
            tray::notify(crate::APP_TITLE, &updates::news_text(&fresh), false);
        }
        self.updates.show_news(ctx);
        let frame = self.updates.show(ctx, self.modals.is_open(updates::is_updates_confirm));
        if let Some(confirm) = frame.confirm {
            // Подтверждение рисуется со следующего кадра (диалоги уже прошли) — кадр нужен сразу, а не через секунду опроса.
            self.modals.open(Modal::UpdatesConfirm(confirm));
            ctx.request_repaint();
        }
        if frame.closed {
            self.modals.close(updates::is_updates_confirm);
        }
    }

    fn apply(&mut self, action: Action) {
        let s = &mut self.s;
        match action {
            Action::Select(name) => s.book.select_tunnel(&name),
            Action::Switch(name, plan) => self.switch(name, plan),
            Action::Retry(name) => self.retry(name),
            Action::Assign(tunnel, group) => {
                if let Err(groups::NoSuchGroup(g)) = s.book.assign(&tunnel, group.as_deref()) {
                    self.action_error.push(trf("grp.err_missing", &[&g]));
                }
            }
            Action::MoveGroup(group, delta) => s.book.move_sibling(&group, delta),
            Action::Reparent(group, to) => {
                s.book.reparent(&group, to.as_deref());
            }
            Action::DeleteGroup(group) => s.book.delete(&group),
            Action::SelectGroup(group) => s.book.select_group(&group),
            Action::ToggleCollapse(group) => s.book.toggle(&group),
            Action::Sort(key) => {
                if s.sort == key {
                    s.sort_desc = !s.sort_desc;
                } else {
                    s.sort = key;
                    s.sort_desc = key != SortKey::Name;
                }
            }
            Action::Open(dialog) => self.modals.open(Modal::Group(dialog)),
            Action::Autostart(on) => {
                if let Err(e) = win::set_autostart(on) {
                    self.action_error.push(e);
                }
                self.autostart = Some(win::autostart_enabled());
            }
            Action::OpenOriginal => {
                if let Err(e) = self.core.native(NativeOp::Open) {
                    self.action_error.push(e);
                }
            }
            Action::ShowLog => {
                s.view.log = true;
                self.unseen_error = false;
            }
            Action::ClearNotice => *self.notice.lock().unwrap() = None,
            Action::Confirm(c) => self.modals.open(Modal::Confirm(c)),
            Action::ReadNative(tunnel) => {
                let (core, infos, loading, error) =
                    (self.core.clone(), self.sources.infos_handle(), self.sources.loading_handle(), self.action_error.clone());
                // Кнопка на время запроса заблокирована: «уже запрашивается» здесь не бывает.
                self.sources.begin_load(&tunnel);
                std::thread::spawn(move || {
                    match core.details(&tunnel) {
                        Ok(info) => {
                            let origin = trf("det.from_native", &[&fmt::time_sec(monitor::unix_now())]);
                            infos.lock().unwrap().insert(tunnel.clone(), (info, origin));
                        }
                        Err(e) => error.push(e),
                    }
                    loading.lock().unwrap().remove(&tunnel);
                });
            }
            // Сначала задача запуска без UAC: ярлык ведёт на exe, а тот поднимает себя через неё.
            Action::DesktopShortcut => match crate::shortcut::create_on_desktop(crate::APP_TITLE, &tr("about.text")) {
                Ok(path) => *self.notice.lock().unwrap() = Some(trf("set.shortcut_done", &[&path.display().to_string()])),
                Err(e) => self.action_error.push(e),
            },
            Action::Language(code) => {
                i18n::set(&self.base_dir.join("lang"), &code);
                // Язык журнала ядра — как у окна (демо-ядро просто соглашается).
                send_language(self.core.clone(), code.clone(), self.action_error.clone());
                self.s.language = code;
            }
            Action::AddLanguage => {
                // Шаблон с ключами и английским текстом — в блокнот; пользователь сохраняет как <код>.lng.
                let result = i18n::create_template(&self.base_dir.join("lang"))
                    .map_err(|e| e.to_string())
                    .and_then(|path| std::process::Command::new("notepad.exe").arg(path).spawn().map(drop).map_err(|e| e.to_string()));
                if let Err(e) = result {
                    self.action_error.push(e);
                }
            }
            Action::OpenLangFolder => {
                let dir = self.base_dir.join("lang");
                let result = std::fs::create_dir_all(&dir)
                    .and_then(|_| std::process::Command::new("explorer.exe").arg(&dir).spawn().map(drop));
                if let Err(e) = result {
                    self.action_error.push(crate::fsutil::io_ctx(&dir, e));
                }
            }
            Action::About => self.modals.open(Modal::About),
            Action::CheckUpdates => self.updates.open(),
            Action::AddConf => {
                if let Some(path) = win::pick_conf(false, None) {
                    // Родной клиент называет туннель именем файла без расширения.
                    if let Some(tunnel) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) {
                        self.s.book.set_source(&tunnel, path.display().to_string());
                    }
                    self.import_source(path);
                }
            }
            Action::EditSource(tunnel) => {
                let Some(path) = self.s.book.source(&tunnel).map(PathBuf::from) else { return };
                match win::shell_open(&path) {
                    Ok(()) => {
                        *self.notice.lock().unwrap() = Some(trf("src.watching", &[&path.display().to_string()]));
                        self.sources.watch(tunnel, path);
                    }
                    Err(e) => self.action_error.push(trf("err.open_file", &[&path.display().to_string(), &e])),
                }
            }
            Action::SetSource(tunnel) => {
                let current = self.s.book.source(&tunnel).map(PathBuf::from);
                if let Some(path) = win::pick_conf(false, current.as_deref()) {
                    self.s.book.set_source(&tunnel, path.display().to_string());
                }
            }
            Action::ImportSource(tunnel) => {
                if let Some(path) = self.s.book.source(&tunnel).map(PathBuf::from) {
                    self.import_source(path);
                }
            }
            Action::Exit => self.request_exit(false),
            Action::ExitWithNative => self.request_exit(true),
            Action::EditNative(name) => {
                let (core, error) = (self.core.clone(), self.action_error.clone());
                std::thread::spawn(move || {
                    if let Err(e) = core.native(NativeOp::Edit(name)) {
                        error.push(e);
                    }
                });
            }
            Action::ChooseMode(mode) => {
                if mode != self.s.mode() {
                    self.modals.open(Modal::ModeSwitch(mode));
                }
            }
            Action::EngineImport => self.engine_import(),
            Action::EngineRestore => self.engine_restore(),
            Action::EngineTakeNative => self.engine_take_native(),
            Action::EngineBackup => self.engine_backup(),
            Action::EngineNew => self.ask_tunnel_name(None),
            Action::EngineEdit(t) => self.engine_edit(t),
            Action::EngineRename(t) => self.ask_tunnel_name(Some(t)),
            Action::InstallCore => self.run_core_setup(crate::daemon::install::INSTALL_FLAG),
            Action::OpenConf => {
                if let Some(path) = win::pick_conf(false, None) {
                    match std::fs::read_to_string(&path) {
                        Ok(text) => self.editor = Some(Editor::new(path, text, None)),
                        Err(e) => self.action_error.push(crate::fsutil::io_ctx(&path, e)),
                    }
                }
            }
        }
    }

    /// Ошибки действий окна уже в журнале событий (`ErrorSink`). Журнал скрыт — в строке состояния ссылка на него.
    fn drain_errors(&mut self) {
        let fresh = self.action_error.take_fresh();
        if self.s.view.log {
            self.unseen_error = false;
        } else if fresh {
            self.unseen_error = true;
        }
    }

    /// Кадр модальных диалогов. Стек вынут на время кадра: диалог действует через `App` и может открыть
    /// другой (неверный пароль — снова окно пароля); открытые так ложатся поверх оставшихся.
    fn show_modals(&mut self, ctx: &egui::Context) {
        let mut modals = std::mem::take(&mut self.modals);
        modals.run(|modal, turn| self.show_modal(ctx, modal, turn));
        let opened = std::mem::replace(&mut self.modals, modals);
        self.modals.absorb(opened);
    }

    fn show_modal(&mut self, ctx: &egui::Context, modal: &mut Modal, turn: Turn) -> Outcome {
        match modal {
            Modal::Group(dialog) => self.show_group_dialog(ctx, dialog, turn),
            Modal::Engine(dialog) => self.show_engine_dialog(ctx, dialog, turn),
            Modal::ModeSwitch(target) => self.show_mode_switch(ctx, *target, turn),
            Modal::Exit { native } => self.show_exit_confirm(ctx, *native, turn),
            Modal::AskImport { tunnel, path } => self.show_ask_import(ctx, tunnel, path, turn),
            Modal::Confirm(c) => self.show_confirm(ctx, c, turn),
            Modal::UpdatesConfirm(c) => self.updates.show_confirm(ctx, c, turn),
            Modal::EditorUnsaved => self.show_editor_unsaved(ctx, turn),
            Modal::About => self.show_about(ctx, turn),
        }
    }

    /// Подключить, отключить или переподключить. Всё переключение — одна команда ядру: без «несколько сразу» оно
    /// снимает остальные туннели, как родной клиент, подключает и ждёт службу.
    fn switch(&self, name: String, plan: Plan) {
        let label = match plan {
            Plan::Connect => "busy.connect",
            Plan::Disconnect => "busy.disconnect",
            Plan::Reconnect => "busy.reconnect",
        };
        // Повторное нажатие, пока туннель переключается, — ничего не делать. Пометка снимается охранником —
        // и тогда, когда поток переключения упал.
        let Some(busy) = self.shared.try_pending_guard(&name, label) else { return };
        let (core, error, ctx) = (self.core.clone(), self.action_error.clone(), self.ctx.clone());
        let multiple = self.s.multiple;
        std::thread::spawn(move || {
            switch_through_core(core.as_ref(), &name, plan, multiple, &error);
            drop(busy);
            ctx.request_repaint();
        });
    }

    /// «Повторить» у туннеля, который ядро переподключает раз в 10 минут: расписание с начала, попытка сразу.
    fn retry(&self, name: String) {
        let (core, error, ctx) = (self.core.clone(), self.action_error.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            retry_through_core(core.as_ref(), &name, &error);
            ctx.request_repaint();
        });
    }

}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // eframe показывает окно после первых кадров — прячем после него, иначе он покажет снова.
        if self.window.begin_frame() {
            tray::hide_window();
        }
        self.handle_close(ctx);
        self.track_window(ctx);
        self.sync_zoom(ctx);
        self.sync_mode();
        self.apply_mode_look();
        self.watch_core();
        self.drain_errors();
        self.check_exit_request();

        let mut actions = Vec::new();
        {
            let shared = self.shared.clone();
            let FrameView { snap, pending, ping, stats, service } = shared.frame_view();
            self.refresh_source_info();
            self.refresh_store_info(&snap);
            self.apply_deleted();
            let infos = self.sources.infos_snapshot();
            let info_loading = self.sources.loading_snapshot();
            let notice = self.notice.lock().unwrap().clone();
            let ping_ref = self.s.view.ping.then_some(&ping);
            let healths: BTreeMap<String, Health> = snap
                .tunnels
                .iter()
                .map(|t| (t.clone(), snap.health(t, pending.get(t).copied(), ping_ref)))
                .collect();
            if self.s.book.tunnel().is_none_or(|t| !snap.tunnels.iter().any(|x| x == t)) && !snap.tunnels.is_empty() {
                self.s.book.adopt_tunnel(snap.running.keys().next().or(snap.tunnels.first()).cloned());
            }
            self.s.book.drop_stale_group(self.s.view.groups);
            let typing = ctx.memory(|m| m.focused()).is_some_and(|id| egui::TextEdit::load_state(ctx, id).is_some());
            let popup = ctx.memory(|m| m.any_popup_open());
            let keys = table_keys(&self.modals, self.updates.is_open(), self.editor.as_ref(), typing, popup);

            let updates_new = self.updates.has_new();
            egui::TopBottomPanel::top("menu").show(ctx, |ui| menu_bar(ui, &mut self.s, self.autostart, &self.base_dir.join("lang"), updates_new, &mut actions));
            self.core_banner(ctx, &mut actions);
            egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
                let bar = StatusBar {
                    mode: self.s.mode(),
                    service: &service,
                    poll_error: snap.error.as_deref(),
                    unseen_error: self.unseen_error,
                    notice: notice.as_deref(),
                };
                status_bar(ui, &bar, &mut actions)
            });
            if self.s.view.log {
                let resp = egui::TopBottomPanel::bottom("log")
                    .resizable(true)
                    .default_height(self.s.log_height)
                    .height_range(60.0..=600.0)
                    .show(ctx, |ui| event_log(ui, &shared));
                self.s.log_height = resp.response.rect.height();
            }
            let resp = egui::SidePanel::left("tunnels")
                .resizable(true)
                .default_width(self.s.left_width)
                .width_range(260.0..=1200.0)
                .show(ctx, |ui| {
                    let list = List { snap: &snap, healths: &healths, stats: &stats, keys };
                    tunnel_list(ui, &mut self.s, &mut self.search, &list, &mut actions)
                });
            self.s.left_width = resp.response.rect.width();
            egui::CentralPanel::default().show(ctx, |ui| match self.s.book.tunnel().map(str::to_string) {
                Some(name) => {
                    let ctx = Detail {
                        name: &name,
                        live: snap.running.get(&name),
                        health: healths.get(&name).cloned().unwrap_or(Health { level: Level::Off, text: tr("health.off") }),
                        busy: pending.contains_key(&name),
                        ping: &ping,
                        stats: &stats,
                        info: infos.get(&name),
                        info_loading: info_loading.contains(&name),
                        retry_slow: snap.retries.get(&name).is_some_and(|r| r.slow),
                    };
                    details(ui, &ctx, &mut self.s, &mut actions)
                }
                None => {
                    ui.label(tr("empty.no_tunnels"));
                }
            });
        }
        self.check_watched();
        // Модальные диалоги — раньше окна обновлений и редактора: верхний забирает Enter и Esc, те их уже не видят.
        self.show_modals(ctx);
        self.show_updates_window(ctx);
        self.show_editor(ctx);
        for action in actions {
            self.apply(action);
        }
        self.push_options();
        self.save_settings(ctx);
        self.handle_snapshot(ctx);
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.shared.save_stats();
        tray::remove();
    }
}

/// Клавиши таблицы и поиска на этот кадр. Поверх таблицы открыто окно (диалог, «Обновления», редактор .conf) —
/// ни тех, ни других: иначе Delete, F2 или стрелки, нажатые в окне вне поля ввода, сработали бы на таблице под ним.
/// Стрелки и Delete ещё и не во время ввода текста и не при открытом меню.
fn table_keys(modals: &Modals, updates_open: bool, editor: Option<&Editor>, typing: bool, popup: bool) -> Keys {
    let window = modals.any_open() || updates_open || editor.is_some();
    Keys { list: !window && !typing && !popup, search: !window }
}

/// Ядро, с которым говорит окно: канал к службе, в демо — `DemoCore`, в тестах — `FakeCore`.
type Core = Arc<dyn CoreApi>;

/// Переключить туннель через ядро. Ошибку самого переключения ядро пишет в журнал — она придёт с состоянием;
/// а если до ядра не достучались, оно отказало, не взяв запрос (`Refused`: занято), или ответ не тот, не напишет
/// никто, и щелчок остался бы без следа.
fn switch_through_core(core: &dyn CoreApi, name: &str, plan: Plan, multiple: bool, error: &ErrorSink) {
    let reply = core.call(Request::Switch { tunnel: name.to_string(), plan, multiple });
    if let Some(e) = switch_transport_error(reply) {
        error.push_for(name, e);
    }
}

/// «Повторить» через ядро. Отказ ядра в журнал ядра не попадает — его записывает окно, под именем туннеля.
fn retry_through_core(core: &dyn CoreApi, name: &str, error: &ErrorSink) {
    if let Err(e) = core.retry_tunnel(name) {
        error.push_for(name, e);
    }
}

/// Ответ ядра на `Switch` -> ошибка для окна. `Response::Err` уже в журнале ядра; здесь только то, о чём ядро
/// не знает: до него не достучались, оно не взяло запрос или ответ не тот.
fn switch_transport_error(reply: Result<Response, String>) -> Option<String> {
    match reply {
        Ok(Response::Ok | Response::Err(_)) => None,
        Ok(Response::Refused(e)) => Some(e),
        Ok(other) => Some(format!("core: unexpected answer {other:?}")),
        Err(e) => Some(e),
    }
}

/// Общее для тестов потоков окна: журнал событий с приёмником ошибок окна.
#[cfg(test)]
mod testkit {
    use std::sync::Arc;

    use super::ErrorSink;
    use crate::events::Severity;
    use crate::monitor::{Options, Shared};

    pub(super) fn sink() -> (Arc<Shared>, ErrorSink) {
        let options = Options { ping: false, ping_host: String::new(), notify: false, tray: false, taskbar: false };
        let shared = Arc::new(Shared::new(None, options, None, None));
        (shared.clone(), ErrorSink::new(shared, eframe::egui::Context::default()))
    }

    /// Ошибки в журнале: (туннель, текст).
    pub(super) fn errors(shared: &Shared) -> Vec<(String, String)> {
        shared.events_since(0).into_iter().filter(|(_, e)| e.severity == Severity::Bad).map(|(_, e)| (e.tunnel, e.text)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::testkit::{errors, sink};
    use super::*;
    use crate::daemon::fake::FakeCore;

    #[test]
    fn table_keys_are_off_while_any_window_is_over_the_table() {
        let (modals, updates_open) = (Modals::default(), false);
        let none = Keys { list: false, search: false };
        assert_eq!(table_keys(&modals, updates_open, None, false, false), Keys { list: true, search: true });
        // Ввод текста и меню снимают только клавиши строк; Ctrl+F остаётся.
        assert_eq!(table_keys(&modals, updates_open, None, true, false), Keys { list: false, search: true });
        assert_eq!(table_keys(&modals, updates_open, None, false, true), Keys { list: false, search: true });
        // Редактор .conf открыт, фокус не в его тексте: Delete и F2 не должны уйти таблице.
        let editor = Editor::new(PathBuf::from("office.conf"), String::new(), None);
        assert_eq!(table_keys(&modals, updates_open, Some(&editor), false, false), none);
        let mut modals = Modals::default();
        modals.open(Modal::About);
        assert_eq!(table_keys(&modals, updates_open, None, false, false), none);
        // Окно «Обновления» открыто — тоже.
        assert_eq!(table_keys(&Modals::default(), true, None, false, false), none);
    }

    #[test]
    fn switch_click_on_unreachable_core_is_reported() {
        // Не достучались до ядра: ядро ничего не залогирует — ошибку показывает окно.
        assert_eq!(switch_transport_error(Err("core unavailable".into())).as_deref(), Some("core unavailable"));
        assert!(switch_transport_error(Ok(Response::Text("?".into()))).is_some());
        // Ошибку самого переключения ядро уже записало в журнал; успех — тоже тишина.
        assert_eq!(switch_transport_error(Ok(Response::Err("no".into()))), None);
        assert_eq!(switch_transport_error(Ok(Response::Ok)), None);
        assert_eq!(switch_transport_error(Ok(Response::Refused("busy".into()))).as_deref(), Some("busy"));
    }

    #[test]
    fn retry_click_goes_to_the_core_and_a_refusal_is_logged_under_the_tunnel() {
        let core = FakeCore::new(|_| Ok(Response::Ok));
        let (shared, error) = sink();
        retry_through_core(&core, "office", &error);
        assert_eq!(core.requests(), [r#"Retry("office")"#]);
        assert!(errors(&shared).is_empty());
        let core = FakeCore::new(|_| Ok(Response::Err("office is not among the tunnels".into())));
        retry_through_core(&core, "office", &error);
        assert_eq!(errors(&shared), [("office".to_string(), "office is not among the tunnels".to_string())]);
    }

    #[test]
    fn switch_sends_one_request_and_leaves_a_conflict_to_the_core_log() {
        // Переключение в ядре не удалось — ошибка уже в журнале ядра, окно не дублирует её.
        let core = FakeCore::new(|_| Ok(Response::Err("office: busy".into())));
        let (shared, error) = sink();
        switch_through_core(&core, "office", Plan::Connect, false, &error);
        assert_eq!(core.requests(), [r#"Switch { tunnel: "office", plan: Connect, multiple: false }"#]);
        assert!(errors(&shared).is_empty());
        assert!(!error.take_fresh());
    }

    #[test]
    fn switch_refused_by_a_busy_core_is_logged_once_under_the_tunnel() {
        // Ядро не взяло запрос (все соединения заняты) и в свой журнал его не писало — след оставляет окно.
        let core = FakeCore::new(|_| Ok(Response::Refused("core busy".into())));
        let (shared, error) = sink();
        switch_through_core(&core, "office", Plan::Connect, false, &error);
        assert_eq!(errors(&shared), [("office".to_string(), "core busy".to_string())]);
        assert!(error.take_fresh());
    }

    #[test]
    fn switch_with_unreachable_core_lands_in_the_log_under_the_tunnel() {
        let core = FakeCore::unreachable("core unavailable");
        let (shared, error) = sink();
        switch_through_core(&core, "office", Plan::Reconnect, true, &error);
        assert_eq!(errors(&shared), [("office".to_string(), "core unavailable".to_string())]);
        assert!(error.take_fresh());
    }

    #[test]
    fn switch_with_a_wrong_answer_is_reported() {
        let core = FakeCore::new(|_| Ok(Response::Text("?".into())));
        let (shared, error) = sink();
        switch_through_core(&core, "office", Plan::Disconnect, false, &error);
        let logged = errors(&shared);
        assert_eq!(logged.len(), 1);
        assert!(logged[0].1.starts_with("core: unexpected answer"), "{logged:?}");
    }
}
