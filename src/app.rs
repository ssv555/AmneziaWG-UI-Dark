//! Окно: меню «Вид»/«Настройки», слева таблица туннелей (группы или плоский список),
//! справа статус сверху, итоги, график, детали; снизу журнал событий и строка состояния.
//! Числа — моноширинным шрифтом в ячейках фиксированной ширины: при смене значений ничего не сдвигается.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use eframe::egui;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use crate::crash::lock;
use crate::daemon::proto::{NativeOp, Plan, Request, Response};
use crate::daemon::agent::client::{AgentApi, AgentPipe, Routed};
use crate::daemon::{CoreApi, PipeClient};
use crate::fmt;
use crate::groups;
use crate::health::{Health, Level};
use crate::i18n::{self, tr, trf};
use crate::monitor::{self, FrameView, Shared};
use crate::settings::{Mode, Settings, SortKey};
use crate::{tray, win};

mod a11y;
mod about;
mod core_ui;
mod demo_core;
mod details;
mod diagnostics;
mod dialog;
mod editor;
mod engine_mode;
mod event_log;
mod errors;
mod exit;
mod graph;
mod group_dialog;
mod history_feed;
mod history_graph;
mod list;
mod markdown;
mod menu;
mod modals;
mod notice;
mod native_reopen;
mod reminder;
mod settings_dialog;
mod sources;
mod status;
mod theme;
mod tray_menu;
mod updates;
mod watcher;
mod window;
#[cfg(test)]
mod fit;
use core_ui::{send_language, CoreLink, Look, Probe};
use details::{details, Detail};
use dialog::dialog_buttons;
use editor::Editor;
use errors::ErrorSink;
use group_dialog::Dialog;
use list::{tunnel_list, Keys, List, ROW_H};
use menu::menu_bar;
use modals::{Modal, Modals, Outcome, Turn};
use native_reopen::{LiveNativeWindow, NativeReopen};
use notice::Notices;
use sources::Confirm;
use updates::UpdatesWindow;
use graph::{GraphPause, GraphState};
use event_log::{event_log, LogFilter};
use status::{status_bar, StatusBar};
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
    /// Справка → «Скопировать диагностику» (и кнопка в «О программе»).
    CopyDiagnostics,
    /// Справка → «Проверить обновления…».
    CheckUpdates,
    ShowLog,
    /// Журнал → «Сохранить как…»: видимые строки в файл, который укажет пользователь.
    SaveLog(String),
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
    /// Меню «Настройки…»: окно со всеми параметрами.
    OpenSettings,
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
    /// Подсказка пользователю в строке состояния (успех и предупреждение — ещё и в журнал событий).
    notice: Notices,
    /// None — неизвестно (демо-режим).
    autostart: Option<bool>,
    action_error: ErrorSink,
    search: String,
    /// Пауза графика скорости и история от агента: на время работы окна, в настройки не пишется.
    graph: GraphState,
    /// Фильтр и поиск панели журнала событий.
    log_filter: LogFilter,
    /// Клавиша меню (Apps) -> Shift+F10 для контекстных меню (`menu::context_menu`).
    menu_key: menu::MenuKey,
    /// Клавиатура строки меню: F10, Alt, мнемоники, стрелки, горячие клавиши команд.
    menu_nav: menu::MenuNav,
    /// Кадры, скрытый запуск, масштаб, запись настроек, снимок.
    window: WindowState,
    /// Связь с ядром: полоса «установить / обновить», сверка в фоне.
    core_link: CoreLink,
    hwnd: isize,
    /// Выставленный вид: режим (иконки, разделители, рамка окна) и тема (палитра, заголовок окна).
    look: Option<Look>,
    /// Ошибка ушла в журнал, пока панель журнала скрыта.
    unseen_error: bool,
    /// «Выход» из меню трея ждёт разбора в кадре.
    exit_request: Arc<AtomicBool>,
    /// Отключение из трея ждёт подтверждения в кадре.
    disconnect_ask: tray_menu::DisconnectAsk,
    /// Модальные диалоги: стек, Enter и Esc — верхнему.
    modals: Modals,
    /// Окно «Обновления и откаты» и отметка «есть новое» в меню «Справка».
    updates: UpdatesWindow,
    /// Ядро, с которым говорит окно (в демо — `DemoCore`).
    core: Core,
    /// Агент (пинг; по плану core-split — и остальное вторичное). `None` — демо-режим: пинг выдуманный, прямо в окне.
    agent: Option<Arc<dyn AgentApi>>,
    /// Группы и «несколько сразу» для меню трея: оно строится и при скрытом окне, без кадра.
    tray_layout: tray_menu::SharedLayout,
}

/// Стиль egui окна, не зависящий от темы: размеры шрифтов и поправки к новым умолчаниям egui. Отдельно от `App::new`,
/// чтобы тест умещаемости (`fit`) раскладывал окно теми же шрифтами и отступами.
fn base_style(ctx: &egui::Context) {
    ctx.all_styles_mut(|s| {
        for (style, font) in s.text_styles.iter_mut() {
            font.size = match style {
                egui::TextStyle::Heading => 20.0,
                egui::TextStyle::Small => 12.0,
                egui::TextStyle::Monospace => 14.0,
                _ => 15.0,
            };
        }
        // egui 0.34 затеняет края прокручиваемых областей; нижняя строка списка выглядела бы недорисованной.
        s.spacing.scroll.fade.strength = 0.0;
        // egui 0.35 замедлил анимации до 0,2 с; меню и подсказки появлялись бы заметно медленнее, чем раньше.
        s.animation_time = 0.1;
        // egui 0.35 обрезает содержимое прокрутки ровно по краю, а 0.36 убрал clip_rect_margin; без отступа внутри
        // прокрутки подсветка крайних строк списка срезалась бы.
        s.spacing.scroll.content_margin = egui::Margin::same(3);
    });
}

/// Наименьшая ширина таблицы туннелей и наименьшая ширина карточки туннеля справа от неё: две кнопки по 140–150 pt
/// и хотя бы ~80 pt имени рядом с ними, сетка состояния из четырёх колонок (4x60 + 3x24). Таблица не шире, чем
/// остаётся после карточки, и не больше 60 % окна: раньше сохранённые 520 pt в окне 760 оставляли карточке 240, и её
/// кнопки с сеткой обрезались справа.
const LEFT_MIN: f32 = 260.0;
const CARD_MIN: f32 = 420.0;

/// Предел ширины таблицы туннелей в окне шириной `window`.
fn left_panel_max(window: f32) -> f32 {
    (window - CARD_MIN).min(window * 0.6).max(LEFT_MIN)
}

impl App {
    pub fn new(cc: &eframe::CreationContext, shared: Arc<Shared>, start: Start) -> Self {
        let ctx = cc.egui_ctx.clone();
        // Тему (стиль egui, заголовок и рамку окна Windows) ставит `apply_look` в каждом кадре до рисования.
        add_fallback_fonts(&ctx);
        base_style(&ctx);
        // egui 0.34 закрывает окно по Ctrl+Q. У нас закрытие — по крестику и из меню, с вопросом про туннели и трей.
        ctx.options_mut(|o| o.quit_shortcuts.clear());
        let exit_request = Arc::new(AtomicBool::new(false));
        let disconnect_ask = tray_menu::DisconnectAsk::default();
        let hwnd = match cc.window_handle().map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Win32(w)) => w.hwnd.get(),
            _ => 0,
        };
        // Служба менеджера AmneziaWG — забота ядра, пинг — агента; в демо пинг выдуманный, прямо в окне.
        let action_error = ErrorSink::new(shared.clone(), ctx.clone());
        let core: Core = match &start.demo {
            Some(demo) => Arc::new(demo_core::DemoCore(demo.clone())),
            // Конфиги туннелей и родное окно AmneziaWG — у агента; `Routed` отправляет туда эти запросы.
            None => Arc::new(Routed::new(Arc::new(PipeClient), Arc::new(AgentPipe))),
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
        if hwnd != 0 {
            if let Err(e) = menu::install_system_key_filter(hwnd) {
                action_error.push(e);
            }
        }
        // Трей — когда ядро и журнал ошибок готовы: его меню переключает туннели тем же `Switcher`, что и окно.
        let tray_layout: tray_menu::SharedLayout = Arc::new(Mutex::new(tray_menu::Layout::of(&start.settings)));
        if hwnd != 0 {
            let hooks = tray_menu::TrayHooks {
                layout: tray_layout.clone(),
                switcher: Switcher { shared: shared.clone(), core: core.clone(), error: action_error.clone(), ctx: ctx.clone() },
                exit_request: exit_request.clone(),
                disconnect_ask: disconnect_ask.clone(),
            };
            tray::install(hwnd, ctx.clone(), start.settings.tray && start.snapshot_file.is_none(), Box::new(hooks));
        }
        let repaint = ctx.clone();
        monitor::spawn(shared.clone(), Box::new(move || repaint.request_repaint()));
        let core_link =if start.demo.is_some() { CoreLink::demo() } else { CoreLink::checking() };
        let agent: Option<Arc<dyn AgentApi>> = start.demo.is_none().then(|| Arc::new(AgentPipe) as Arc<dyn AgentApi>);
        match &agent {
            None => {
                // Демо: пинг и статистику окно считает само по выдуманным туннелям (у настоящего окна их ведёт агент).
                crate::ping::spawn(shared.clone());
                demo_core::spawn_stats(shared.clone());
            }
            Some(agent) => {
                core_link.check(core.clone(), ctx.clone(), action_error.clone(), Probe::Start);
                let repaint = ctx.clone();
                let mut reopen = NativeReopen::default();
                let native_window = LiveNativeWindow { core: core.clone(), error: action_error.clone() };
                let on_native_ui = Box::new(move |mark| {
                    reopen.observe(mark, &native_window);
                });
                monitor::spawn_agent(shared.clone(), agent.clone(), Box::new(move || repaint.request_repaint()), on_native_ui);
            }
        }
        let mut modals = Modals::default();
        if start.about {
            modals.open(Modal::About);
        }
        let notice = Notices::new(shared.clone());
        let updates = UpdatesWindow::new(updates::Link::new(agent.clone(), action_error.clone(), notice.clone(), ctx.clone()));
        let start_hidden = start.hidden && start.settings.tray;
        let saved_text = if start.settings_path.exists() { start.settings.to_ini().to_text() } else { String::new() };
        let graph = GraphState { pause: GraphPause::default(), feed: history_feed::HistoryFeed::real(agent.clone(), ctx.clone()) };
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
            graph,
            log_filter: LogFilter::default(),
            menu_key: menu::MenuKey::default(),
            menu_nav: menu::MenuNav::default(),
            window: WindowState::new(start.settings_path, saved_text, start.snapshot_file, start_hidden),
            core_link,
            hwnd,
            look: None,
            unseen_error: false,
            exit_request,
            disconnect_ask,
            modals,
            updates,
            core,
            agent,
            tray_layout,
        }
    }

    /// Щелчок по уведомлению Windows: окно уже поднято треем, остаётся открыть «Обновления и откаты» или выбрать
    /// туннель, о котором было уведомление.
    fn handle_toast_click(&mut self, ctx: &egui::Context) {
        match tray::take_click() {
            Some(tray::Clicked::Update) => self.updates.open(),
            Some(tray::Clicked::Tunnel(name)) => tray_menu::select_from_toast(&mut self.s.book, &name, ctx),
            None => {}
        }
    }

    /// Окно обновлений и его подтверждение: поведение — в `UpdatesWindow`, здесь только то, что принадлежит App
    /// (настройки, скрытое в трей окно, диалоги `Modals`).
    fn show_updates_window(&mut self, ctx: &egui::Context) {
        // Сообщение об обновлении (первое и суточные напоминания) — уведомление Windows и в окне, при видимом окне
        // тоже; уведомление Windows подчиняется настройке «уведомления».
        if let Some(text) = self.updates.tick(&mut self.s.update_notified, &mut self.s.update_reminded, monitor::unix_now()) {
            if self.s.notify {
                tray::notify_update(crate::APP_TITLE, &text);
            }
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
            // Отключение — сначала вопрос (пока не ответили «Больше не спрашивать»); то же решение у трея.
            Action::Switch(name, plan) if list::asks_first(plan, &s.hidden_dialogs) => self.modals.open(Modal::Confirm(Confirm::Disconnect(name))),
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
            Action::SaveLog(text) => self.save_log(&text),
            Action::ClearNotice => self.notice.clear(),
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
                            lock(&infos).insert(tunnel.clone(), (info, origin));
                        }
                        Err(e) => error.push(e),
                    }
                    lock(&loading).remove(&tunnel);
                });
            }
            // Сначала задача запуска без UAC: ярлык ведёт на exe, а тот поднимает себя через неё.
            Action::DesktopShortcut => match crate::shortcut::create_on_desktop(crate::APP_TITLE, &tr("about.text")) {
                Ok(path) => self.notice.done(trf("set.shortcut_done", &[&path.display().to_string()])),
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
            Action::CopyDiagnostics => self.copy_diagnostics(),
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
                        self.notice.done(trf("src.watching", &[&path.display().to_string()]));
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
            Action::OpenSettings => self.open_settings(),
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
        self.unseen_error = self.action_error.unseen_after(self.unseen_error, self.s.view.log);
    }

    /// Кадр модальных диалогов. Стек вынут на время кадра: диалог действует через `App` и может открыть
    /// другой (неверный пароль — снова окно пароля); открытые так ложатся поверх оставшихся.
    fn show_modals(&mut self, ctx: &egui::Context) {
        let mut modals = std::mem::take(&mut self.modals);
        modals.run(ctx, |modal, turn| self.show_modal(ctx, modal, turn));
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
            Modal::EditorInvalid(after) => self.show_editor_invalid(ctx, *after, turn),
            Modal::About => self.show_about(ctx, turn),
            Modal::Settings(dlg) => self.show_settings(ctx, dlg, turn),
        }
    }

    /// Подключить, отключить или переподключить (таблица, сведения, клавиши) — тем же `Switcher`, что и меню трея.
    fn switch(&self, name: String, plan: Plan) {
        let switcher = Switcher { shared: self.shared.clone(), core: self.core.clone(), error: self.action_error.clone(), ctx: self.ctx.clone() };
        switcher.switch(name, plan, self.s.multiple);
    }

    /// Отключение, выбранное в трее, ждёт подтверждения: трей уже поднял окно, вопрос — в этом кадре.
    fn check_disconnect_ask(&mut self) {
        let asked = lock(&self.disconnect_ask).take();
        if let Some(tunnel) = asked {
            self.modals.open(Modal::Confirm(Confirm::Disconnect(tunnel)));
        }
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
    /// Всё, что не рисует. eframe зовёт это каждый кадр, а `ui` — только когда окно видно (не свёрнуто):
    /// закрытие, выход из трея, связь с ядром и правки .conf из внешнего редактора не ждут, пока окно развернут.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // eframe показывает окно после первых кадров — прячем после него, иначе он покажет снова.
        if self.window.begin_frame() {
            tray::hide_window();
        }
        self.handle_close(ctx);
        self.handle_toast_click(ctx);
        tray_menu::publish(&self.tray_layout, &self.s);
        self.track_window(ctx);
        self.sync_zoom(ctx);
        self.sync_mode();
        self.apply_look();
        self.watch_core();
        self.drain_errors();
        self.check_exit_request();
        self.check_disconnect_ask();
        self.check_watched();
        // Настройки меняет и ядро (режим) — сохранить, даже если `ui` в этом кадре не позовут.
        self.push_options();
        self.save_settings(ctx);
    }

    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        self.menu_key.hook(raw_input);
    }

    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = &root.ctx().clone();
        // До рисования: Enter и Esc при открытом списке или меню — его, а не диалога (`dialog_keys`).
        dialog::note_popups(ctx);
        let mut actions = Vec::new();
        {
            let shared = self.shared.clone();
            let FrameView { snap, pending, ping, stats, service, agent_down } = shared.frame_view();
            self.refresh_source_info();
            self.refresh_store_info(&snap);
            self.apply_deleted();
            let infos = self.sources.infos_snapshot();
            let info_loading = self.sources.loading_snapshot();
            let notice = self.notice.current();
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
            let popup = egui::Popup::is_any_open(ctx);
            let keys = table_keys(&self.modals, self.updates.is_open(), self.editor.as_ref(), typing, popup);

            let updates_new = self.updates.has_new();
            let lang_dir = self.base_dir.join("lang");
            let bar = menu::MenuBarInput { lang_dir: &lang_dir, updates_new, keyboard: keys.search };
            egui::Panel::top("menu").show(root, |ui| menu_bar(ui, &mut self.menu_nav, &mut self.s, &bar, &mut actions));
            self.core_banner(root, &mut actions);
            egui::Panel::bottom("status").show(root, |ui| {
                let bar = StatusBar {
                    mode: self.s.mode(),
                    service: &service,
                    poll_error: snap.error.as_deref(),
                    unseen_error: self.unseen_error,
                    notice: notice.as_ref(),
                };
                status_bar(ui, &bar, &mut actions);
            });
            // Простой show, не show_collapsible: с 0.35 тот даёт закрыть панель перетаскиванием края или двойным щелчком,
            // а список туннелей не скрывается вовсе, журнал — только из меню «Вид».
            if self.s.view.log {
                let resp = egui::Panel::bottom("log")
                    .resizable(true)
                    .default_size(self.s.log_height)
                    .size_range(60.0..=600.0)
                    .show(root, |ui| event_log(ui, &shared, &mut self.log_filter, self.s.book.tunnel(), &mut actions));
                self.s.log_height = resp.response.rect.height();
            }
            let resp = egui::Panel::left("tunnels")
                .resizable(true)
                .default_size(self.s.left_width)
                .size_range(LEFT_MIN..=left_panel_max(root.available_width()))
                .show(root, |ui| {
                    let list = List { snap: &snap, healths: &healths, stats: &stats, keys };
                    tunnel_list(ui, &mut self.s, &mut self.search, &list, &mut actions)
                });
            self.s.left_width = resp.response.rect.width();
            egui::CentralPanel::default().show(root, |ui| match self.s.book.tunnel().map(str::to_string) {
                Some(name) => {
                    let ctx = Detail {
                        name: &name,
                        live: snap.running.get(&name),
                        health: healths.get(&name).cloned().unwrap_or(Health::new(Level::Off, tr("health.off"))),
                        busy: pending.contains_key(&name),
                        ping: &ping,
                        ping_unavailable: agent_down,
                        stats: &stats,
                        info: infos.get(&name),
                        info_loading: info_loading.contains(&name),
                        retry_slow: snap.retries.get(&name).is_some_and(|r| r.slow),
                        core_lost: snap.core_lost,
                        snap: &snap,
                    };
                    details(ui, &ctx, &mut self.s, &mut self.graph, &mut actions);
                }
                None => {
                    ui.label(tr("empty.no_tunnels"));
                }
            });
        }
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

/// Переключение туннеля — одно для окна и меню трея: пометка «занят», один запрос ядру в своём потоке, ошибка — в
/// журнал окна. Без «несколько сразу» ядро само снимает остальные туннели, как родной клиент.
#[derive(Clone)]
struct Switcher {
    shared: Arc<Shared>,
    core: Core,
    error: ErrorSink,
    ctx: egui::Context,
}

impl Switcher {
    fn switch(&self, name: String, plan: Plan, multiple: bool) {
        let label = match plan {
            Plan::Connect => "busy.connect",
            Plan::Disconnect => "busy.disconnect",
            Plan::Reconnect => "busy.reconnect",
        };
        // Повторное нажатие, пока туннель переключается, — ничего не делать. Пометка снимается охранником —
        // и тогда, когда поток переключения упал.
        let Some(busy) = self.shared.try_pending_guard(&name, label) else { return };
        let (core, error, ctx) = (self.core.clone(), self.error.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            switch_through_core(core.as_ref(), &name, plan, multiple, &error);
            drop(busy);
            ctx.request_repaint();
        });
    }
}

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
        Ok(other) => Some(trf("err.core_unexpected", &[&crate::explain::variant_name(&other)])),
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
        let shared = Arc::new(Shared::new(None, options, None));
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

    /// Карточке туннеля всегда остаётся не меньше `CARD_MIN`; в широком окне таблица не шире 60 %; в узком — не у́же
    /// своего минимума, даже если карточке тогда не хватает (меньше 760 главное окно не бывает).
    #[test]
    fn left_panel_leaves_room_for_the_card() {
        assert_eq!(left_panel_max(760.0), 340.0);
        assert_eq!(left_panel_max(1280.0), 768.0);
        assert_eq!(left_panel_max(500.0), LEFT_MIN);
        assert!(left_panel_max(760.0) + CARD_MIN <= 760.0);
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
        assert_eq!(logged[0].1, trf("err.core_unexpected", &["Text"]), "имя варианта, не дамп ответа");
    }
}
