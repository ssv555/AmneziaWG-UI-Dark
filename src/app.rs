//! Окно: меню «Вид»/«Настройки», слева таблица туннелей (группы или плоский список),
//! справа статус сверху, итоги, график, детали; снизу журнал событий и строка состояния.
//! Числа — моноширинным шрифтом в ячейках фиксированной ширины: при смене значений ничего не сдвигается.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use crate::conf::{PeerInfo, TunnelInfo};
use crate::events::Severity;
use crate::fmt;
use crate::groups::{self, Agg, Row, Verdict, UNGROUPED};
use crate::health::{self, Health, Level};
use crate::i18n::{self, tr, trf};
use crate::monitor::{self, Live, Options, Shared, Snapshot};
use crate::ping::PingState;
use crate::settings::{Settings, SortKey, WindowRect};
use crate::stats::{self, Stats, TunnelStats};
use crate::{tray, win};

/// Сколько ждём, пока служба туннеля поднимется или исчезнет после команды.
const SWITCH_TIMEOUT: Duration = Duration::from_secs(15);
/// Настройки пишутся, когда изменения затихли на это время (перетаскивание не пишет файл на каждый пиксель).
const SAVE_DELAY: Duration = Duration::from_millis(300);
const ROW_H: f32 = 24.0;
/// Сдвиг строки дерева на каждый уровень вложенности.
const INDENT: f32 = 16.0;
const NUM_FONT: f32 = 13.5;
const GRAPH_MIN_H: f32 = 40.0;
const GRAPH_MAX_H: f32 = 600.0;
const PING_STRIP_H: f32 = 34.0;
const PERIODS: [(u32, &str); 3] = [(120, "period.2m"), (600, "period.10m"), (3600, "period.1h")];

const GREEN: Color32 = Color32::from_rgb(90, 200, 120);
const YELLOW: Color32 = Color32::from_rgb(230, 185, 70);
const RED: Color32 = Color32::from_rgb(235, 95, 85);
const GRAY: Color32 = Color32::from_rgb(120, 120, 120);
const BLUE: Color32 = Color32::from_rgb(90, 160, 240);
const VIOLET: Color32 = Color32::from_rgb(180, 140, 240);

pub struct Start {
    pub settings: Settings,
    pub settings_path: PathBuf,
    /// Папка программы: Settings.ini, Stats.ini, lang.
    pub base_dir: PathBuf,
    /// Запуск из автозапуска: сразу спрятаться в трей.
    pub hidden: bool,
    pub snapshot_file: Option<String>,
    pub demo: bool,
    /// Открыть «О программе» сразу (для снимка окна).
    pub about: bool,
}

/// Открытый в редакторе файл .conf пользователя.
struct Editor {
    path: PathBuf,
    text: String,
    /// Содержимое на диске — для признака «изменён».
    saved: String,
    /// Сообщение под кнопками: текст и признак ошибки.
    note: Option<(String, bool)>,
    /// Закрытие с несохранёнными изменениями уже предупреждено — следующее закрывает без сохранения.
    confirm_close: bool,
}

/// Файл-источник, открытый во внешнем редакторе.
struct Watch {
    tunnel: String,
    path: PathBuf,
    modified: Option<std::time::SystemTime>,
}

enum Dialog {
    /// Новая группа в `parent` (`None` — верхний уровень); `assign` — туннель, который сразу в неё положить.
    NewGroup { parent: Option<String>, name: String, assign: Option<String> },
    /// `old` — полный путь, `name` — новое имя последней части пути.
    Rename { old: String, name: String },
}

/// Что перетаскивают мышью в таблице туннелей.
#[derive(Clone)]
enum Drag {
    Tunnel(String),
    Group(String),
}

#[derive(Clone, Copy, PartialEq)]
enum Plan {
    Connect,
    Disconnect,
    Reconnect,
}

enum Action {
    Select(String),
    Switch(String, Plan),
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
    ClearError,
    ClearNotice,
    DesktopShortcut,
    Confirm(Confirm),
    /// Прочитать сведения о неподключённом туннеле из родного окна.
    ReadNative(String),
}

/// Действия, которые перезаписывают или удаляют данные, — только после подтверждения.
#[derive(Clone)]
enum Confirm {
    /// AmneziaWG → файл-источник.
    ToSource(String),
    /// Файл-источник → AmneziaWG.
    ToNative(String),
    /// Удалить группу (содержимое переходит уровнем выше).
    DeleteGroup(String),
    /// Удалить туннель в AmneziaWG.
    DeleteTunnel(String),
}

pub struct App {
    s: Settings,
    shared: Arc<Shared>,
    ctx: egui::Context,
    base_dir: PathBuf,
    about_open: bool,
    about_icon: Option<egui::TextureHandle>,
    editor: Option<Editor>,
    /// Источники, открытые на правку: ждём сохранения.
    watched: Vec<Watch>,
    watch_checked: Instant,
    /// Источник сохранён — спросить, добавить ли его в AmneziaWG: (туннель, файл).
    ask_import: Option<(String, PathBuf)>,
    /// Ждёт подтверждения перезаписи или удаления.
    confirm: Option<Confirm>,
    /// Сведения о конфигах неподключённых туннелей: (сведения, откуда). Только открытые данные.
    infos: Arc<Mutex<BTreeMap<String, (TunnelInfo, String)>>>,
    info_loading: Arc<Mutex<BTreeSet<String>>>,
    /// Время изменения разобранного файла-источника — чтобы перечитывать его после правки.
    source_seen: BTreeMap<String, Option<std::time::SystemTime>>,
    /// Туннели, удалённые в AmneziaWG фоновым потоком, — убрать из настроек.
    deleted: Arc<Mutex<Vec<String>>>,
    /// Подсказка пользователю в строке состояния.
    notice: Arc<Mutex<Option<String>>>,
    settings_path: PathBuf,
    saved_text: String,
    changed_at: Option<Instant>,
    /// None — неизвестно (демо-режим).
    autostart: Option<bool>,
    action_error: Arc<Mutex<Option<String>>>,
    dialog: Option<Dialog>,
    /// Диалог только что открыт: поле ввода получает фокус один раз, а не каждый кадр.
    dialog_focus: bool,
    /// Выделенная строка группы; `None` — выделен туннель (`Settings::selected`).
    sel_group: Option<String>,
    search: String,
    hide_on_frame: Option<u64>,
    frame: u64,
    snapshot_file: Option<String>,
    started: Instant,
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
        let hwnd = match cc.window_handle().map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Win32(w)) => w.hwnd.get(),
            _ => 0,
        };
        if hwnd != 0 {
            win::dark_title_bar(hwnd);
            let on_exit_shared = shared.clone();
            let on_exit = Box::new(move || {
                on_exit_shared.save_stats();
                tray::remove();
                std::process::exit(0);
            });
            tray::install(hwnd, ctx.clone(), start.settings.tray && start.snapshot_file.is_none(), on_exit);
        }
        monitor::spawn(shared.clone(), ctx.clone());
        crate::ping::spawn(shared.clone());
        if !start.demo {
            check_service(shared.clone());
        }
        let saved_text = if start.settings_path.exists() { start.settings.to_ini().to_text() } else { String::new() };
        App {
            autostart: (!start.demo).then(win::autostart_enabled),
            hide_on_frame: (start.hidden && start.settings.tray).then_some(5),
            s: start.settings,
            shared,
            ctx,
            base_dir: start.base_dir,
            about_open: start.about,
            about_icon: None,
            editor: None,
            watched: Vec::new(),
            watch_checked: Instant::now(),
            ask_import: None,
            confirm: None,
            infos: Default::default(),
            info_loading: Default::default(),
            source_seen: BTreeMap::new(),
            deleted: Default::default(),
            notice: Default::default(),
            settings_path: start.settings_path,
            saved_text,
            changed_at: None,
            action_error: Default::default(),
            dialog: None,
            dialog_focus: false,
            sel_group: None,
            search: String::new(),
            frame: 0,
            snapshot_file: start.snapshot_file,
            started: Instant::now(),
        }
    }

    fn apply(&mut self, action: Action) {
        let s = &mut self.s;
        match action {
            Action::Select(name) => {
                s.selected = Some(name);
                self.sel_group = None;
            }
            Action::Switch(name, plan) => self.switch(name, plan),
            Action::Assign(tunnel, Some(group)) => {
                s.assignment.insert(tunnel, group);
            }
            Action::Assign(tunnel, None) => {
                s.assignment.remove(&tunnel);
            }
            Action::MoveGroup(group, delta) => groups::move_sibling(s, &group, delta),
            Action::Reparent(group, to) => {
                if groups::reparent(s, &group, to.as_deref()) && self.sel_group.as_deref() == Some(group.as_str()) {
                    self.sel_group = Some(groups::join(to.as_deref(), groups::leaf(&group)));
                }
            }
            Action::DeleteGroup(group) => {
                groups::delete(s, &group);
                if self.sel_group.as_deref() == Some(group.as_str()) {
                    self.sel_group = groups::parent(&group).map(str::to_string);
                }
            }
            Action::SelectGroup(group) => self.sel_group = Some(group),
            Action::ToggleCollapse(group) => {
                if !s.collapsed.remove(&group) {
                    s.collapsed.insert(group);
                }
            }
            Action::Sort(key) => {
                if s.sort == key {
                    s.sort_desc = !s.sort_desc;
                } else {
                    s.sort = key;
                    s.sort_desc = key != SortKey::Name;
                }
            }
            Action::Open(dialog) => {
                self.dialog = Some(dialog);
                self.dialog_focus = true;
            }
            Action::Autostart(on) => {
                if let Err(e) = win::set_autostart(on) {
                    *self.action_error.lock().unwrap() = Some(e);
                }
                self.autostart = Some(win::autostart_enabled());
            }
            Action::OpenOriginal => {
                if let Err(e) = self.shared.backend.open_original() {
                    *self.action_error.lock().unwrap() = Some(e);
                }
            }
            Action::ClearError => *self.action_error.lock().unwrap() = None,
            Action::ClearNotice => *self.notice.lock().unwrap() = None,
            Action::Confirm(c) => self.confirm = Some(c),
            Action::ReadNative(tunnel) => {
                let (shared, infos, loading, error) =
                    (self.shared.clone(), self.infos.clone(), self.info_loading.clone(), self.action_error.clone());
                loading.lock().unwrap().insert(tunnel.clone());
                std::thread::spawn(move || {
                    match shared.backend.native_details(&tunnel) {
                        Ok(info) => {
                            let origin = trf("det.from_native", &[&fmt::local_time(monitor::unix_now(), "%H:%M:%S")]);
                            infos.lock().unwrap().insert(tunnel.clone(), (info, origin));
                        }
                        Err(e) => *error.lock().unwrap() = Some(e),
                    }
                    loading.lock().unwrap().remove(&tunnel);
                });
            }
            // Сначала задача запуска без UAC: ярлык ведёт на exe, а тот поднимает себя через неё.
            Action::DesktopShortcut => match win::ensure_launch_task()
                .and_then(|_| crate::shortcut::create_on_desktop(crate::APP_TITLE, &tr("about.text")))
            {
                Ok(path) => *self.notice.lock().unwrap() = Some(trf("set.shortcut_done", &[&path.display().to_string()])),
                Err(e) => *self.action_error.lock().unwrap() = Some(e),
            },
            Action::Language(code) => {
                i18n::set(&self.base_dir.join("lang"), &code);
                self.s.language = code;
            }
            Action::AddLanguage => {
                // Шаблон с ключами и английским текстом — в блокнот; пользователь сохраняет как <код>.lng.
                let result = i18n::create_template(&self.base_dir.join("lang"))
                    .map_err(|e| e.to_string())
                    .and_then(|path| std::process::Command::new("notepad.exe").arg(path).spawn().map(drop).map_err(|e| e.to_string()));
                if let Err(e) = result {
                    *self.action_error.lock().unwrap() = Some(e);
                }
            }
            Action::OpenLangFolder => {
                let dir = self.base_dir.join("lang");
                let result = std::fs::create_dir_all(&dir)
                    .and_then(|_| std::process::Command::new("explorer.exe").arg(&dir).spawn().map(drop));
                if let Err(e) = result {
                    *self.action_error.lock().unwrap() = Some(format!("{}: {e}", dir.display()));
                }
            }
            Action::About => self.about_open = true,
            Action::AddConf => {
                if let Some(path) = win::pick_conf(false, None) {
                    // Родной клиент называет туннель именем файла без расширения.
                    if let Some(tunnel) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) {
                        self.s.sources.insert(tunnel, path.display().to_string());
                    }
                    self.import_source(path);
                }
            }
            Action::EditSource(tunnel) => {
                let Some(path) = self.s.sources.get(&tunnel).map(PathBuf::from) else { return };
                match win::shell_open(&path) {
                    Ok(()) => {
                        let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
                        self.watched.retain(|w| w.path != path);
                        *self.notice.lock().unwrap() = Some(trf("src.watching", &[&path.display().to_string()]));
                        self.watched.push(Watch { tunnel, path, modified });
                    }
                    Err(e) => *self.action_error.lock().unwrap() = Some(trf("err.open_file", &[&path.display().to_string(), &e])),
                }
            }
            Action::SetSource(tunnel) => {
                let current = self.s.sources.get(&tunnel).map(PathBuf::from);
                if let Some(path) = win::pick_conf(false, current.as_deref()) {
                    self.s.sources.insert(tunnel, path.display().to_string());
                }
            }
            Action::ImportSource(tunnel) => {
                if let Some(path) = self.s.sources.get(&tunnel).map(PathBuf::from) {
                    self.import_source(path);
                }
            }
            Action::Exit => self.exit_now(false),
            Action::ExitWithNative => self.exit_now(true),
            Action::EditNative(name) => {
                let (shared, error) = (self.shared.clone(), self.action_error.clone());
                std::thread::spawn(move || {
                    if let Err(e) = shared.backend.edit_in_native(&name) {
                        *error.lock().unwrap() = Some(e);
                    }
                });
            }
            Action::OpenConf => {
                if let Some(path) = win::pick_conf(false, None) {
                    match std::fs::read_to_string(&path) {
                        Ok(text) => self.editor = Some(Editor { path, saved: text.clone(), text, note: None, confirm_close: false }),
                        Err(e) => *self.action_error.lock().unwrap() = Some(format!("{}: {e}", path.display())),
                    }
                }
            }
        }
    }

    /// Родной импорт с выделенным файлом; «Открыть» нажимает пользователь. Напоминание про дубликат.
    fn import_source(&self, path: PathBuf) {
        *self.notice.lock().unwrap() = Some(tr("src.import_hint"));
        let (shared, error) = (self.shared.clone(), self.action_error.clone());
        std::thread::spawn(move || {
            if let Err(e) = shared.backend.import_in_native(Some(&path)) {
                *error.lock().unwrap() = Some(e);
            }
        });
    }

    /// Раз в секунду: изменился ли файл-источник, открытый на правку.
    fn check_watched(&mut self) {
        if self.watch_checked.elapsed() < Duration::from_secs(1) || self.watched.is_empty() {
            return;
        }
        self.watch_checked = Instant::now();
        for w in &mut self.watched {
            let modified = std::fs::metadata(&w.path).and_then(|m| m.modified()).ok();
            if modified.is_some() && modified != w.modified {
                w.modified = modified;
                if self.ask_import.is_none() {
                    self.ask_import = Some((w.tunnel.clone(), w.path.clone()));
                    tray::show_window();
                }
            }
        }
        self.ctx.request_repaint_after(Duration::from_secs(1));
    }

    /// Подтверждение перезаписи (синхронизация) и удаления (группа, туннель).
    fn show_confirm(&mut self, ctx: &egui::Context) {
        let Some(c) = self.confirm.clone() else { return };
        let (title, text, primary, warning) = self.confirm_texts(&c);
        // Туннель без источника: перед удалением можно сохранить копию конфига в файл.
        let offer_copy = matches!(&c, Confirm::DeleteTunnel(t) if !self.s.sources.contains_key(t));
        let (mut yes, mut no, mut copy) = (false, false, false);
        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_width(500.0);
                ui.add(egui::Label::new(text).wrap());
                if let Some(w) = &warning {
                    ui.add_space(6.0);
                    ui.add(egui::Label::new(RichText::new(w).color(YELLOW)).wrap());
                }
                ui.add_space(10.0);
                if offer_copy {
                    copy = ui.button(tr("del.save_copy")).on_hover_text(tr("del.save_copy_hint")).clicked();
                    ui.add_space(4.0);
                }
                (yes, no) = dialog_buttons(ui, &primary, true, Some(&tr("btn.cancel")));
            });
        let (enter, escape) = dialog_keys(ctx);
        if copy {
            if let Confirm::DeleteTunnel(t) = &c {
                // Отмена выбора файла — диалог остаётся.
                if let Some(path) = win::pick_conf(true, Some(&PathBuf::from(format!("{t}.conf")))) {
                    self.run_delete_tunnel(t.clone(), Some(path));
                    self.confirm = None;
                }
            }
            return;
        }
        if yes || enter {
            match c {
                Confirm::ToSource(_) | Confirm::ToNative(_) => self.run_sync(c),
                Confirm::DeleteGroup(path) => self.apply(Action::DeleteGroup(path)),
                Confirm::DeleteTunnel(t) => self.run_delete_tunnel(t, None),
            }
        }
        if yes || no || enter || escape {
            self.confirm = None;
        }
    }

    /// Заголовок, текст, надпись главной кнопки и предупреждение для подтверждения.
    fn confirm_texts(&self, c: &Confirm) -> (String, String, String, Option<String>) {
        let source = |t: &str| self.s.sources.get(t).cloned().unwrap_or_default();
        match c {
            Confirm::ToSource(t) => (tr("sync.title"), trf("sync.confirm_to_source", &[t, &source(t)]), tr("btn.yes"), None),
            Confirm::ToNative(t) => (tr("sync.title"), trf("sync.confirm_to_native", &[t, &source(t)]), tr("btn.yes"), None),
            Confirm::DeleteGroup(path) => {
                let subgroups = self.s.groups.iter().filter(|g| *g != path && groups::within(g, path)).count();
                let tunnels = self.s.assignment.values().filter(|g| groups::within(g, path)).count();
                let target = groups::parent(path).map(str::to_string).unwrap_or_else(|| tr("app.ungrouped"));
                let text = trf("del.group_text", &[path, &subgroups.to_string(), &tunnels.to_string(), &target]);
                (tr("del.group_title"), text, tr("del.delete"), None)
            }
            Confirm::DeleteTunnel(t) => {
                let running = self.shared.snapshot.lock().unwrap().running.contains_key(t);
                let mut warning = Vec::new();
                if running {
                    warning.push(tr("del.tunnel_running"));
                }
                match self.s.sources.get(t) {
                    Some(src) => warning.push(trf("del.tunnel_has_source", &[src])),
                    None => warning.push(tr("del.tunnel_no_source")),
                }
                (tr("del.tunnel_title"), trf("del.tunnel_text", &[t]), tr("del.delete"), Some(warning.join("\n")))
            }
        }
    }

    /// Удаление туннеля в AmneziaWG в фоне; с `copy` — сначала сохранить его конфиг в файл.
    fn run_delete_tunnel(&self, tunnel: String, copy: Option<PathBuf>) {
        let (shared, error, notice, deleted) =
            (self.shared.clone(), self.action_error.clone(), self.notice.clone(), self.deleted.clone());
        *notice.lock().unwrap() = Some(trf("del.running", &[&tunnel]));
        std::thread::spawn(move || {
            let saved = match &copy {
                Some(path) => shared
                    .backend
                    .read_native_config(&tunnel)
                    .and_then(|text| std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))),
                None => Ok(()),
            };
            // Не удалось сохранить копию — не удаляем.
            let result = saved.and_then(|_| shared.backend.delete_tunnel(&tunnel));
            match result {
                Ok(()) => {
                    let text = match &copy {
                        Some(p) => trf("del.done_copy", &[&tunnel, &p.display().to_string()]),
                        None => trf("del.done", &[&tunnel]),
                    };
                    shared.log(&tunnel, Severity::Warn, &text);
                    *notice.lock().unwrap() = Some(text);
                    deleted.lock().unwrap().push(tunnel);
                }
                Err(e) => {
                    shared.log(&tunnel, Severity::Bad, &e);
                    *notice.lock().unwrap() = None;
                    *error.lock().unwrap() = Some(e);
                }
            }
        });
    }

    /// Убрать удалённые туннели из настроек: группа, источник, выбор, кэш сведений. Файл-источник не трогаем.
    fn apply_deleted(&mut self) {
        let gone: Vec<String> = std::mem::take(&mut *self.deleted.lock().unwrap());
        for t in gone {
            self.s.assignment.remove(&t);
            self.s.sources.remove(&t);
            self.infos.lock().unwrap().remove(&t);
            self.source_seen.remove(&t);
            if self.s.selected.as_deref() == Some(t.as_str()) {
                self.s.selected = None;
            }
        }
    }

    /// Выбранный неподключённый туннель с источником: разобрать файл заново, если он изменился.
    fn refresh_source_info(&mut self) {
        let Some(t) = self.s.selected.clone() else { return };
        let Some(path) = self.s.sources.get(&t).map(PathBuf::from) else { return };
        let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        if self.source_seen.get(&t) == Some(&modified) {
            return;
        }
        self.source_seen.insert(t.clone(), modified);
        if let Ok(text) = std::fs::read_to_string(&path) {
            let origin = trf("det.from_source", &[&path.display().to_string()]);
            self.infos.lock().unwrap().insert(t, (crate::conf::parse(&text), origin));
        }
    }

    /// Синхронизация в фоне: родное окно управляется автоматически, итог — в строке состояния.
    fn run_sync(&self, c: Confirm) {
        let (shared, error, notice) = (self.shared.clone(), self.action_error.clone(), self.notice.clone());
        let (tunnel, to_source) = match c {
            Confirm::ToSource(t) => (t, true),
            Confirm::ToNative(t) => (t, false),
            Confirm::DeleteGroup(_) | Confirm::DeleteTunnel(_) => return,
        };
        let Some(path) = self.s.sources.get(&tunnel).map(PathBuf::from) else { return };
        *notice.lock().unwrap() = Some(trf("sync.running", &[&tunnel]));
        std::thread::spawn(move || {
            let result = if to_source {
                shared
                    .backend
                    .read_native_config(&tunnel)
                    .and_then(|text| std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display())))
            } else {
                std::fs::read_to_string(&path)
                    .map_err(|e| format!("{}: {e}", path.display()))
                    .and_then(|text| shared.backend.write_native_config(&tunnel, &text))
            };
            match result {
                Ok(()) => {
                    let done = if to_source { "sync.done_to_source" } else { "sync.done_to_native" };
                    shared.log(&tunnel, Severity::Info, &trf(done, &[&path.display().to_string()]));
                    *notice.lock().unwrap() = Some(trf(done, &[&path.display().to_string()]));
                }
                Err(e) => {
                    shared.log(&tunnel, Severity::Bad, &e);
                    *notice.lock().unwrap() = None;
                    *error.lock().unwrap() = Some(e);
                }
            }
        });
    }

    fn show_ask_import(&mut self, ctx: &egui::Context) {
        let Some((tunnel, path)) = self.ask_import.clone() else { return };
        let (mut yes, mut no) = (false, false);
        egui::Window::new(tr("src.saved_title"))
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_width(460.0);
                ui.add(egui::Label::new(trf("src.ask", &[&path.display().to_string()])).wrap());
                ui.add_space(10.0);
                (yes, no) = dialog_buttons(ui, &tr("btn.yes"), true, Some(&tr("btn.no")));
            });
        let (enter, escape) = dialog_keys(ctx);
        yes |= enter;
        no |= escape;
        if yes {
            self.apply(Action::ImportSource(tunnel));
        }
        if yes || no {
            self.ask_import = None;
        }
    }

    /// Выход: настройки и статистику — на диск сразу; с `native` — сначала закрыть родное окно AmneziaWG
    /// (процесс интерфейса в этой сессии; служба и туннели продолжают работать).
    fn exit_now(&mut self, native: bool) {
        let _ = self.s.to_ini().save(&self.settings_path);
        self.shared.save_stats();
        if native {
            win::close_session_processes("amneziawg.exe");
        }
        tray::remove();
        std::process::exit(0);
    }

    /// Редактор исходного .conf пользователя: сохранить, сохранить как, импортировать в родной клиент.
    fn show_editor(&mut self, ctx: &egui::Context) {
        let (shared, error) = (self.shared.clone(), self.action_error.clone());
        let Some(ed) = &mut self.editor else { return };
        let dirty = ed.text != ed.saved;
        let file = ed.path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
        let marker = if dirty { format!(" ({})", tr("ed.modified")) } else { String::new() };
        let (mut open, mut save, mut save_as, mut import, mut close) = (true, false, false, false, false);
        egui::Window::new(format!("{} — {file}{marker}", tr("ed.title")))
            .id(egui::Id::new("conf-editor"))
            .open(&mut open)
            .default_size([720.0, 540.0])
            .resizable(true)
            .collapsible(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    save = ui.add_enabled(dirty, egui::Button::new(tr("ed.save")).shortcut_text("Ctrl+S")).clicked();
                    save_as = ui.button(tr("ed.save_as")).clicked();
                    import = ui.button(tr("ed.import")).clicked();
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        close = ui.button(tr("btn.close")).on_hover_text("Esc").clicked();
                    });
                });
                if !ed.text.contains("[Interface]") {
                    ui.colored_label(YELLOW, tr("ed.no_interface"));
                }
                if let Some((note, is_error)) = &ed.note {
                    ui.colored_label(if *is_error { RED } else { GREEN }, note);
                }
                ui.separator();
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    ui.add(egui::TextEdit::multiline(&mut ed.text).code_editor().desired_width(f32::INFINITY).desired_rows(24));
                });
            });
        // Ctrl+S — сохранить, Esc — закрыть (Enter в редакторе — перевод строки, не действие окна).
        let (ctrl_s, escape) = ctx.input_mut(|i| {
            (i.consume_key(egui::Modifiers::COMMAND, egui::Key::S), i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
        });
        save |= ctrl_s && dirty;
        close |= escape;
        if save_as {
            match win::pick_conf(true, Some(&ed.path)) {
                Some(path) => {
                    ed.path = path;
                    save = true;
                }
                None => save = false,
            }
        }
        // Импорт берёт файл с диска — несохранённое сначала сохраняем.
        if save || (import && dirty) {
            match std::fs::write(&ed.path, &ed.text) {
                Ok(()) => {
                    ed.saved = ed.text.clone();
                    ed.confirm_close = false;
                    ed.note = Some((trf("ed.saved", &[&ed.path.display().to_string()]), false));
                }
                Err(e) => {
                    ed.note = Some((format!("{}: {e}", ed.path.display()), true));
                    import = false;
                }
            }
        }
        if import {
            let path = ed.path.display().to_string();
            ctx.copy_text(path.clone());
            ed.note = Some((trf("ed.import_hint", &[&path]), false));
            let file = ed.path.clone();
            std::thread::spawn(move || {
                if let Err(e) = shared.backend.import_in_native(Some(&file)) {
                    *error.lock().unwrap() = Some(e);
                }
            });
        }
        if !open || close {
            // Несохранённое молча не теряем: первое закрытие предупреждает, второе закрывает.
            if ed.text != ed.saved && !ed.confirm_close {
                ed.confirm_close = true;
                ed.note = Some((tr("ed.unsaved"), true));
            } else {
                self.editor = None;
            }
        }
    }

    fn show_about(&mut self, ctx: &egui::Context) {
        if !self.about_open {
            return;
        }
        let icon = self
            .about_icon
            .get_or_insert_with(|| {
                let image = egui::ColorImage::from_rgba_unmultiplied([64, 64], &crate::icon::rgba(64));
                ctx.load_texture("about-icon", image, Default::default())
            })
            .clone();
        let mut open = true;
        let mut close = false;
        egui::Window::new(tr("help.about"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.add(egui::Image::new(&icon).fit_to_exact_size(Vec2::splat(64.0)));
                    ui.vertical(|ui| {
                        ui.heading(crate::APP_TITLE);
                        ui.label(RichText::new(trf("about.version", &[env!("CARGO_PKG_VERSION")])).weak());
                    });
                });
                ui.add_space(8.0);
                ui.set_max_width(460.0);
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
                ui.add(egui::Label::new(RichText::new(trf("about.files", &[&self.base_dir.display().to_string()])).weak()).wrap());
                ui.add_space(8.0);
                close = dialog_buttons(ui, &tr("btn.close"), true, None).0;
            });
        let (enter, escape) = dialog_keys(ctx);
        self.about_open = open && !close && !enter && !escape;
    }

    /// Подключить, отключить или переподключить. Без «несколько сразу» подключение снимает остальные туннели,
    /// как это делает родной клиент.
    fn switch(&self, name: String, plan: Plan) {
        if self.shared.pending.lock().unwrap().contains_key(&name) {
            return;
        }
        let running: Vec<String> = self.shared.snapshot.lock().unwrap().running.keys().cloned().collect();
        let others: Vec<String> = if plan != Plan::Disconnect && !self.s.multiple {
            running.into_iter().filter(|n| *n != name).collect()
        } else {
            vec![]
        };
        let label = match plan {
            Plan::Connect => "busy.connect",
            Plan::Disconnect => "busy.disconnect",
            Plan::Reconnect => "busy.reconnect",
        };
        self.shared.pending.lock().unwrap().insert(name.clone(), label);

        let (shared, error, ctx) = (self.shared.clone(), self.action_error.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let b = &shared.backend;
            let wait = |up: bool| {
                // Держим «Подключение…», пока служба не появится, чтобы статус не мигал.
                let deadline = Instant::now() + SWITCH_TIMEOUT;
                while Instant::now() < deadline {
                    if b.running().map(|r| r.contains(&name) == up).unwrap_or(false) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
            };
            let mut result = others.iter().try_for_each(|o| b.disconnect(o));
            if result.is_ok() && plan != Plan::Connect {
                result = b.disconnect(&name);
                if result.is_ok() {
                    wait(false);
                }
            }
            if result.is_ok() && plan != Plan::Disconnect {
                result = b.connect(&name);
                if result.is_ok() {
                    wait(true);
                }
            }
            if let Err(e) = &result {
                shared.log(&name, Severity::Bad, e);
                *error.lock().unwrap() = Some(e.clone());
            }
            shared.pending.lock().unwrap().remove(&name);
            ctx.request_repaint();
        });
    }

    /// Размер, положение, развёрнутость окна — в настройки (сохраняются вместе с остальным).
    fn track_window(&mut self, ctx: &egui::Context) {
        let (outer, inner, maximized, minimized) = ctx.input(|i| {
            let v = i.viewport();
            (v.outer_rect, v.inner_rect, v.maximized, v.minimized)
        });
        if minimized == Some(true) || self.hide_on_frame.is_some() {
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

    fn handle_close(&mut self, ctx: &egui::Context) {
        if !ctx.input(|i| i.viewport().close_requested()) {
            return;
        }
        if self.s.tray && self.s.close_to_tray && self.snapshot_file.is_none() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            tray::hide_window();
        }
    }

    /// Изменения — в Settings.ini сразу, как только затихли на SAVE_DELAY.
    fn save_settings(&mut self, ctx: &egui::Context) {
        let ini = self.s.to_ini();
        let text = ini.to_text();
        if text == self.saved_text {
            self.changed_at = None;
            return;
        }
        let since = *self.changed_at.get_or_insert_with(Instant::now);
        if since.elapsed() < SAVE_DELAY {
            ctx.request_repaint_after(SAVE_DELAY);
            return;
        }
        match ini.save(&self.settings_path) {
            Ok(()) => {
                self.saved_text = text;
                self.changed_at = None;
            }
            Err(e) => *self.action_error.lock().unwrap() = Some(format!("{}: {e}", self.settings_path.display())),
        }
    }

    /// Окно меняет настройки — фоновым потокам нужны пинг, уведомления, трей.
    fn push_options(&self) {
        let new = Options {
            ping: self.s.view.ping,
            ping_host: self.s.ping_host.trim().to_string(),
            notify: self.s.notify,
            tray: self.s.tray,
        };
        let mut o = self.shared.options.lock().unwrap();
        if o.tray != new.tray {
            tray::set_visible(new.tray);
        }
        if o.ping != new.ping || o.ping_host != new.ping_host || o.notify != new.notify || o.tray != new.tray {
            *o = new;
        }
    }

    fn show_dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &mut self.dialog else { return };
        let (title, text, parent, except) = match dialog {
            Dialog::NewGroup { parent, name, .. } => {
                let title = match parent {
                    Some(p) => trf("dlg.new_subgroup", &[p]),
                    None => tr("dlg.new_group"),
                };
                (title, name, parent.clone(), None)
            }
            Dialog::Rename { old, name } => (tr("dlg.rename_group"), name, groups::parent(old).map(str::to_string), Some(old.clone())),
        };
        let all = &self.s.groups;
        // Фокус и выделение текста — только в кадре открытия; дальше поле живёт как обычно.
        let focus = std::mem::take(&mut self.dialog_focus);
        let (mut ok, mut cancel, mut valid) = (false, false, false);
        egui::Window::new(title)
            .id(egui::Id::new("group-dialog"))
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_width(340.0);
                ui.label(tr("dlg.name"));
                let mut out = egui::TextEdit::singleline(text).desired_width(f32::INFINITY).show(ui);
                if focus {
                    out.response.request_focus();
                    let end = egui::text::CCursor::new(text.chars().count());
                    out.state.cursor.set_char_range(Some(egui::text::CCursorRange::two(egui::text::CCursor::new(0), end)));
                    out.state.store(ui.ctx(), out.response.id);
                }
                let check = groups::check_name(all, parent.as_deref(), text, except.as_deref());
                valid = check.is_ok();
                let error = match check {
                    Err(groups::NameError::BadChar) => tr("dlg.err_slash"),
                    Err(groups::NameError::Exists) => tr("dlg.err_exists"),
                    _ => String::new(),
                };
                // Строка ошибки занимает место всегда — окно не прыгает при наборе.
                ui.add_sized([ui.available_width(), 18.0], egui::Label::new(RichText::new(error).color(RED).small()).truncate());
                ui.add_space(4.0);
                (ok, cancel) = dialog_buttons(ui, &tr("btn.ok"), valid, Some(&tr("btn.cancel")));
            });
        let (enter, escape) = dialog_keys(ctx);
        ok |= enter && valid;
        cancel |= escape;
        if cancel {
            self.dialog = None;
            return;
        }
        if !ok {
            return;
        }
        match self.dialog.take() {
            Some(Dialog::NewGroup { parent, name, assign }) => {
                if let Ok(path) = groups::add(&mut self.s, parent.as_deref(), &name) {
                    // Новую подгруппу видно сразу: родитель раскрывается.
                    if let Some(p) = &parent {
                        self.s.collapsed.remove(p);
                    }
                    match assign {
                        Some(tunnel) => {
                            self.s.assignment.insert(tunnel, path);
                        }
                        None => self.sel_group = Some(path),
                    }
                }
            }
            Some(Dialog::Rename { old, name }) => {
                if let Ok(path) = groups::rename(&mut self.s, &old, &name) {
                    if self.sel_group.as_deref() == Some(old.as_str()) {
                        self.sel_group = Some(path);
                    }
                }
            }
            None => {}
        }
    }

    /// `--snapshot файл.png`: через несколько секунд сохранить кадр окна и выйти.
    fn handle_snapshot(&mut self, ctx: &egui::Context) {
        let Some(path) = self.snapshot_file.clone() else { return };
        ctx.request_repaint();
        if self.started.elapsed() > Duration::from_secs(6) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
            self.started += Duration::from_secs(3600);
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

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.frame += 1;
        // eframe показывает окно после первых кадров — прячем после него, иначе он покажет снова.
        if self.hide_on_frame.is_some_and(|f| self.frame >= f) {
            self.hide_on_frame = None;
            tray::hide_window();
        }
        self.handle_close(ctx);
        self.track_window(ctx);

        let mut actions = Vec::new();
        {
            let shared = self.shared.clone();
            let snap = shared.snapshot.lock().unwrap();
            let pending = shared.pending.lock().unwrap().clone();
            let ping = shared.ping.lock().unwrap().clone();
            let stats = shared.stats.lock().unwrap().clone();
            self.refresh_source_info();
            self.apply_deleted();
            let infos = self.infos.lock().unwrap().clone();
            let info_loading = self.info_loading.lock().unwrap().clone();
            let service = shared.service.lock().unwrap().clone();
            let action_error = self.action_error.lock().unwrap().clone();
            let notice = self.notice.lock().unwrap().clone();
            let ping_ref = self.s.view.ping.then_some(&ping);
            let healths: BTreeMap<String, Health> = snap
                .tunnels
                .iter()
                .map(|t| (t.clone(), health::health(snap.running.get(t), pending.get(t).copied(), ping_ref)))
                .collect();
            if self.s.selected.as_ref().is_none_or(|t| !snap.tunnels.contains(t)) && !snap.tunnels.is_empty() {
                self.s.selected = snap.running.keys().next().or(snap.tunnels.first()).cloned();
            }
            if self.sel_group.as_ref().is_some_and(|g| !self.s.view.groups || (g != UNGROUPED && !self.s.groups.contains(g))) {
                self.sel_group = None;
            }
            // Клавиши таблицы — только когда нет диалогов, открытых меню и ввода текста.
            let typing = ctx.memory(|m| m.focused()).is_some_and(|id| egui::TextEdit::load_state(ctx, id).is_some());
            let modal = self.dialog.is_some() || self.about_open || self.ask_import.is_some() || self.confirm.is_some();
            let keys = Keys { list: !modal && !typing && !ctx.memory(|m| m.any_popup_open()), search: !modal };

            egui::TopBottomPanel::top("menu").show(ctx, |ui| menu_bar(ui, &mut self.s, self.autostart, &self.base_dir.join("lang"), &mut actions));
            egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
                status_bar(ui, &service, snap.error.as_deref(), action_error.as_deref(), notice.as_deref(), &mut actions)
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
                    let list = List { snap: &snap, healths: &healths, stats: &stats, sel_group: self.sel_group.as_deref(), keys };
                    tunnel_list(ui, &mut self.s, &mut self.search, &list, &mut actions)
                });
            self.s.left_width = resp.response.rect.width();
            egui::CentralPanel::default().show(ctx, |ui| match self.s.selected.clone() {
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
                    };
                    details(ui, &ctx, &mut self.s, &mut actions)
                }
                None => {
                    ui.label(tr("empty.no_tunnels"));
                }
            });
        }
        // Порядок важен: Enter и Esc забирает верхний диалог, нижние их уже не видят.
        self.show_dialog(ctx);
        self.check_watched();
        self.show_ask_import(ctx);
        self.show_confirm(ctx);
        self.show_about(ctx);
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

fn check_service(shared: Arc<Shared>) {
    const NAME: &str = crate::backend::MANAGER_SERVICE;
    std::thread::spawn(move || {
        let text = match win::ensure_service(NAME) {
            Ok(false) => trf("service.running", &[NAME]),
            Ok(true) => {
                shared.log("", Severity::Info, &trf("service.started", &[NAME]));
                trf("service.started", &[NAME])
            }
            Err(e) => {
                shared.log("", Severity::Bad, &e);
                e
            }
        };
        *shared.service.lock().unwrap() = text;
    });
}

fn level_color(level: Level) -> Color32 {
    match level {
        Level::Off => GRAY,
        Level::Ok => GREEN,
        Level::Busy | Level::Warn => YELLOW,
        Level::Bad => RED,
    }
}

fn severity_color(s: Severity) -> Color32 {
    match s {
        Severity::Info => GREEN,
        Severity::Warn => YELLOW,
        Severity::Bad => RED,
    }
}

fn dot(ui: &mut Ui, color: Color32, radius: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(radius * 2.0 + 4.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), radius, color);
}

fn mono(text: impl Into<String>, color: Color32) -> RichText {
    RichText::new(text).font(FontId::monospace(NUM_FONT)).color(color)
}

// ───────────────────────────── меню и строка состояния ─────────────────────────────

fn menu_bar(ui: &mut Ui, s: &mut Settings, autostart: Option<bool>, lang_dir: &std::path::Path, actions: &mut Vec<Action>) {
    egui::menu::bar(ui, |ui| {
        ui.menu_button(tr("menu.file"), |ui| {
            if ui.button(tr("file.open_conf")).clicked() {
                actions.push(Action::OpenConf);
                ui.close_menu();
            }
            ui.separator();
            if ui.button(tr("file.exit")).clicked() {
                actions.push(Action::Exit);
                ui.close_menu();
            }
            if ui.button(tr("file.exit_native")).clicked() {
                actions.push(Action::ExitWithNative);
                ui.close_menu();
            }
        });
        ui.menu_button(tr("menu.view"), |ui| {
            let v = &mut s.view;
            ui.checkbox(&mut v.groups, tr("view.groups"));
            ui.checkbox(&mut v.search, tr("view.search"));
            ui.separator();
            ui.weak(tr("view.columns"));
            ui.checkbox(&mut v.col_rx, tr("view.col_rx"));
            ui.checkbox(&mut v.col_tx, tr("view.col_tx"));
            ui.checkbox(&mut v.col_peak, tr("view.col_peak"));
            ui.checkbox(&mut v.col_share, tr("view.col_share"));
            ui.separator();
            ui.weak(tr("view.right"));
            ui.checkbox(&mut v.totals, tr("view.totals"));
            ui.checkbox(&mut v.graph, tr("view.graph"));
            ui.checkbox(&mut v.ping, tr("view.ping"));
            ui.checkbox(&mut v.reconnect, tr("view.reconnect"));
            ui.checkbox(&mut v.details, tr("view.details"));
            ui.separator();
            ui.checkbox(&mut v.log, tr("view.log"));
        });
        ui.menu_button(tr("menu.settings"), |ui| {
            ui.checkbox(&mut s.multiple, tr("set.multiple"));
            ui.checkbox(&mut s.tray, tr("set.tray"));
            ui.add_enabled_ui(s.tray, |ui| {
                ui.checkbox(&mut s.notify, tr("set.notify"));
                ui.checkbox(&mut s.close_to_tray, tr("set.close_to_tray"));
            });
            if let Some(on) = autostart {
                let mut want = on;
                if ui.checkbox(&mut want, tr("set.autostart")).changed() {
                    actions.push(Action::Autostart(want));
                }
            }
            ui.horizontal(|ui| {
                ui.label(tr("set.ping_to"));
                ui.add(egui::TextEdit::singleline(&mut s.ping_host).desired_width(140.0));
            });
            ui.separator();
            if ui.button(tr("set.shortcut")).clicked() {
                actions.push(Action::DesktopShortcut);
                ui.close_menu();
            }
            if ui.button(tr("set.original")).clicked() {
                actions.push(Action::OpenOriginal);
                ui.close_menu();
            }
        });
        ui.menu_button(tr("menu.language"), |ui| {
            let current = i18n::current_code();
            for (code, name) in i18n::available(lang_dir) {
                if ui.radio(code == current, format!("{name} ({code})")).clicked() {
                    actions.push(Action::Language(code));
                    ui.close_menu();
                }
            }
            ui.separator();
            if ui.button(tr("lang.add")).clicked() {
                actions.push(Action::AddLanguage);
                ui.close_menu();
            }
            if ui.button(tr("lang.folder")).clicked() {
                actions.push(Action::OpenLangFolder);
                ui.close_menu();
            }
        });
        ui.menu_button(tr("menu.help"), |ui| {
            if ui.button(tr("help.about")).clicked() {
                actions.push(Action::About);
                ui.close_menu();
            }
        });
    });
}

/// Шрифты Windows как запасные: встроенный шрифт egui знает только латиницу и кириллицу,
/// а пользовательский язык может быть любым (CJK, арабский, иврит, греческий …).
fn add_fallback_fonts(ctx: &egui::Context) {
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

fn status_bar(
    ui: &mut Ui,
    service: &str,
    poll_error: Option<&str>,
    action_error: Option<&str>,
    notice: Option<&str>,
    actions: &mut Vec<Action>,
) {
    ui.horizontal(|ui| {
        ui.weak(service);
        if let Some(e) = poll_error {
            ui.colored_label(RED, trf("status.poll_error", &[e]));
        }
        if let Some(n) = notice {
            if ui.small_button("×").on_hover_text(tr("btn.dismiss")).clicked() {
                actions.push(Action::ClearNotice);
            }
            ui.colored_label(GREEN, n);
        }
        if let Some(e) = action_error {
            if ui.small_button("×").on_hover_text(tr("btn.dismiss")).clicked() {
                actions.push(Action::ClearError);
            }
            ui.colored_label(RED, e);
        }
    });
}

fn event_log(ui: &mut Ui, shared: &Shared) {
    ui.add_space(4.0);
    ui.strong(tr("log.title"));
    egui::ScrollArea::vertical().auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
        let events = shared.events.lock().unwrap();
        if events.items.is_empty() {
            ui.weak(tr("log.empty"));
        }
        for e in &events.items {
            ui.horizontal(|ui| {
                ui.label(mono(fmt::local_time(e.at, &tr("fmt.log_time")), GRAY));
                dot(ui, severity_color(e.severity), 4.0);
                if !e.tunnel.is_empty() {
                    ui.strong(&e.tunnel);
                }
                ui.label(&e.text);
            });
        }
    });
}

// ───────────────────────────── таблица туннелей ─────────────────────────────

/// Числовые колонки слева направо: ключ сортировки, заголовок, ширина из настроек.
fn numeric_columns(s: &Settings) -> Vec<(SortKey, &'static str, f32)> {
    let v = &s.view;
    let c = &s.columns;
    [
        (v.col_rx, SortKey::Rx, "col.rx", c.rx),
        (v.col_tx, SortKey::Tx, "col.tx", c.tx),
        (v.col_peak, SortKey::Peak, "col.peak", c.peak),
        (v.col_share, SortKey::Share, "col.share", c.share),
    ]
    .into_iter()
    .filter(|(on, ..)| *on)
    .map(|(_, k, t, w)| (k, t, w))
    .collect()
}

fn column_width(s: &mut Settings, key: SortKey) -> Option<&mut f32> {
    match key {
        SortKey::Rx => Some(&mut s.columns.rx),
        SortKey::Tx => Some(&mut s.columns.tx),
        SortKey::Peak => Some(&mut s.columns.peak),
        SortKey::Share => Some(&mut s.columns.share),
        SortKey::Name => None,
    }
}

/// Границы ячеек строки: имя занимает остаток ширины.
struct Cells {
    name: Rect,
    nums: Vec<(SortKey, Rect)>,
}

fn cells(row: Rect, cols: &[(SortKey, &str, f32)]) -> Cells {
    let mut right = row.right();
    let mut nums = Vec::new();
    for (key, _, w) in cols.iter().rev() {
        nums.push((*key, Rect::from_x_y_ranges(right - w..=right, row.y_range())));
        right -= w;
    }
    nums.reverse();
    Cells { name: Rect::from_x_y_ranges(row.left()..=right, row.y_range()), nums }
}

fn cell_value(key: SortKey, st: &TunnelStats, share: f64) -> String {
    match key {
        SortKey::Rx => fmt::bytes(st.rx as f64),
        SortKey::Tx => fmt::bytes(st.tx as f64),
        SortKey::Peak => fmt::rate(st.peak_rx),
        SortKey::Share => fmt::percent(share),
        SortKey::Name => String::new(),
    }
}

fn sort_value(key: SortKey, st: Option<&TunnelStats>, share: f64) -> f64 {
    let st = st.cloned().unwrap_or_default();
    match key {
        SortKey::Rx => st.rx as f64,
        SortKey::Tx => st.tx as f64,
        SortKey::Peak => st.peak_rx,
        SortKey::Share => share,
        SortKey::Name => 0.0,
    }
}

/// Какие клавиши таблица может забрать в этом кадре.
#[derive(Clone, Copy)]
struct Keys {
    /// Стрелки, Enter, F2, Delete — нет диалогов, меню и ввода текста.
    list: bool,
    /// Ctrl+F — нет диалогов.
    search: bool,
}

/// Состояние для таблицы туннелей на этот кадр.
struct List<'a> {
    snap: &'a Snapshot,
    healths: &'a BTreeMap<String, Health>,
    stats: &'a Stats,
    sel_group: Option<&'a str>,
    keys: Keys,
}

/// Метка в памяти egui: выделение сменили клавишами — прокрутить к нему.
fn scroll_flag() -> egui::Id {
    egui::Id::new("tunnel-list-scroll")
}

fn tunnel_list(ui: &mut Ui, s: &mut Settings, search: &mut String, l: &List, actions: &mut Vec<Action>) {
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        ui.checkbox(&mut s.view.groups, tr("view.groups"));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if s.view.groups && ui.button(tr("list.add_group")).on_hover_text(tr("list.add_group_tip")).clicked() {
                actions.push(Action::Open(Dialog::NewGroup { parent: None, name: String::new(), assign: None }));
            }
        });
    });
    if s.view.search {
        let id = egui::Id::new("tunnel-search");
        if l.keys.search && ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::F)) {
            ui.memory_mut(|m| m.request_focus(id));
        }
        let resp = ui.add(egui::TextEdit::singleline(search).id(id).hint_text(tr("list.search_hint")).desired_width(f32::INFINITY));
        // Esc в поле поиска — очистить.
        if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            search.clear();
        }
    }
    ui.add_space(2.0);

    let needle = search.trim().to_lowercase();
    let stats = l.stats;
    let mut visible: Vec<&String> =
        l.snap.tunnels.iter().filter(|t| needle.is_empty() || t.to_lowercase().contains(&needle)).collect();
    let share = |t: &str| stats::share(stats, t);
    let (key, desc) = (s.sort, s.sort_desc);
    visible.sort_by(|a, b| {
        let ord = match key {
            SortKey::Name => a.to_lowercase().cmp(&b.to_lowercase()),
            _ => sort_value(key, stats.get(*a), share(a)).total_cmp(&sort_value(key, stats.get(*b), share(b))),
        };
        if desc { ord.reverse() } else { ord }
    });

    let dragging = egui::DragAndDrop::has_payload_of_type::<Drag>(ui.ctx());
    let rows: Vec<Row> = if s.view.groups {
        groups::rows(s, &visible, !needle.is_empty(), dragging)
    } else {
        visible.iter().map(|t| Row::Tunnel { name: t.to_string(), depth: 0, group: None }).collect()
    };
    if l.keys.list && !dragging {
        list_keys(ui, &rows, s, l, actions);
    }

    header(ui, s, actions);
    let cols = numeric_columns(s);
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        for row in &rows {
            match row {
                Row::Group { path, depth, collapsed, members } => {
                    let g = GroupLine { key: path, depth: *depth, collapsed: *collapsed, members, selected: l.sel_group == Some(path.as_str()) };
                    group_row(ui, &g, &cols, s, l, actions);
                }
                Row::Ungrouped { collapsed, members } => {
                    let g = GroupLine { key: UNGROUPED, depth: 0, collapsed: *collapsed, members, selected: l.sel_group == Some(UNGROUPED) };
                    group_row(ui, &g, &cols, s, l, actions);
                }
                Row::Tunnel { name, depth, group } => {
                    tunnel_row(ui, name, *depth, group.as_deref(), &cols, s, l, actions);
                }
            }
        }
        // Пустое место под списком — тоже цель перетаскивания: группа на верхний уровень, туннель — из группы.
        if s.view.groups {
            let height = ui.available_height().max(ROW_H * 2.0);
            let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::hover());
            drop_target(ui, &resp, rect, None, s, actions);
        }
    });
    drag_preview(ui.ctx());
}

/// Клавиши на выделенной строке: ↑/↓ — соседняя строка, ←/→ — свернуть/раскрыть или к родителю,
/// Enter — подключить/отключить туннель или свернуть группу, F2 — переименовать группу, Delete — удалить группу.
fn list_keys(ui: &Ui, rows: &[Row], s: &Settings, l: &List, actions: &mut Vec<Action>) {
    use egui::Key;
    let (up, down, left, right, enter, f2, delete) = ui.input_mut(|i| {
        let mut k = |key| i.consume_key(egui::Modifiers::NONE, key);
        (k(Key::ArrowUp), k(Key::ArrowDown), k(Key::ArrowLeft), k(Key::ArrowRight), k(Key::Enter), k(Key::F2), k(Key::Delete))
    });
    if rows.is_empty() || !(up || down || left || right || enter || f2 || delete) {
        return;
    }
    let current = rows.iter().position(|r| match r {
        Row::Group { path, .. } => l.sel_group == Some(path.as_str()),
        Row::Ungrouped { .. } => l.sel_group == Some(UNGROUPED),
        Row::Tunnel { name, .. } => l.sel_group.is_none() && s.selected.as_ref() == Some(name),
    });
    let select = |i: usize, actions: &mut Vec<Action>| {
        actions.push(match &rows[i] {
            Row::Group { path, .. } => Action::SelectGroup(path.clone()),
            Row::Ungrouped { .. } => Action::SelectGroup(UNGROUPED.to_string()),
            Row::Tunnel { name, .. } => Action::Select(name.clone()),
        });
        ui.ctx().data_mut(|d| d.insert_temp(scroll_flag(), true));
    };
    if up || down {
        let next = match current {
            None => 0,
            Some(c) if up => c.saturating_sub(1),
            Some(c) => (c + 1).min(rows.len() - 1),
        };
        select(next, actions);
        return;
    }
    let Some(c) = current else { return };
    match &rows[c] {
        Row::Group { path, collapsed, .. } => {
            if (left && !collapsed) || (right && *collapsed) || enter {
                actions.push(Action::ToggleCollapse(path.clone()));
            } else if left {
                if let Some(p) = groups::parent(path) {
                    actions.push(Action::SelectGroup(p.to_string()));
                }
            }
            if f2 {
                actions.push(Action::Open(Dialog::Rename { old: path.clone(), name: groups::leaf(path).to_string() }));
            }
            if delete {
                actions.push(Action::Confirm(Confirm::DeleteGroup(path.clone())));
            }
        }
        Row::Ungrouped { collapsed, .. } => {
            if (left && !collapsed) || (right && *collapsed) || enter {
                actions.push(Action::ToggleCollapse(UNGROUPED.to_string()));
            }
        }
        Row::Tunnel { name, group, .. } => {
            let level = l.healths.get(name).map_or(Level::Off, |h| h.level);
            if enter && level != Level::Busy {
                let plan = if level == Level::Off { Plan::Connect } else { Plan::Disconnect };
                actions.push(Action::Switch(name.clone(), plan));
            }
            if delete {
                actions.push(Action::Confirm(Confirm::DeleteTunnel(name.clone())));
            }
            if left && s.view.groups {
                actions.push(Action::SelectGroup(group.clone().unwrap_or_else(|| UNGROUPED.to_string())));
            }
        }
    }
}

/// Подсветка цели под перетаскиваемым и сам перенос при отпускании. `target` — группа (`None` — верхний
/// уровень / «Без группы»). Недопустимое (группа в своего потомка, занятое имя) — красная рамка и запрещающий курсор.
fn drop_target(ui: &Ui, resp: &egui::Response, rect: Rect, target: Option<&str>, s: &Settings, actions: &mut Vec<Action>) {
    let Some(drag) = resp.dnd_hover_payload::<Drag>() else { return };
    let verdict = match &*drag {
        Drag::Tunnel(t) => groups::check_assign(s, t, target),
        Drag::Group(g) => groups::check_reparent(&s.groups, g, target),
    };
    let area = rect.shrink(1.0);
    match verdict {
        Verdict::Noop => {}
        Verdict::Invalid => {
            ui.painter().rect_stroke(area, 3.0, Stroke::new(1.5, RED), egui::StrokeKind::Inside);
            ui.ctx().set_cursor_icon(egui::CursorIcon::NotAllowed);
        }
        Verdict::Valid => {
            ui.painter().rect_filled(area, 3.0, BLUE.gamma_multiply(0.15));
            ui.painter().rect_stroke(area, 3.0, Stroke::new(1.5, BLUE), egui::StrokeKind::Inside);
            if resp.dnd_release_payload::<Drag>().is_some() {
                let to = target.map(str::to_string);
                actions.push(match &*drag {
                    Drag::Tunnel(t) => Action::Assign(t.clone(), to),
                    Drag::Group(g) => Action::Reparent(g.clone(), to),
                });
            }
        }
    }
}

/// Что тащим — подпись у курсора.
fn drag_preview(ctx: &egui::Context) {
    let Some(drag) = egui::DragAndDrop::payload::<Drag>(ctx) else { return };
    let Some(pos) = ctx.pointer_latest_pos() else { return };
    let text = match &*drag {
        Drag::Tunnel(t) => format!("● {t}"),
        Drag::Group(g) => format!("▶ {}", groups::leaf(g)),
    };
    egui::Area::new(egui::Id::new("drag-preview"))
        .order(egui::Order::Tooltip)
        .fixed_pos(pos + Vec2::new(16.0, 8.0))
        .interactable(false)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.label(RichText::new(text).strong());
            });
        });
    if ctx.output(|o| o.cursor_icon) == egui::CursorIcon::Default {
        ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
    }
}

/// Заголовок таблицы: клик — сортировка, перетаскивание левой границы числовой колонки — её ширина.
fn header(ui: &mut Ui, s: &mut Settings, actions: &mut Vec<Action>) {
    let (row, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H), Sense::hover());
    let painter = ui.painter_at(row);
    let weak = ui.visuals().weak_text_color();
    painter.line_segment([row.left_bottom(), row.right_bottom()], Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color));
    let cols = numeric_columns(s);
    let c = cells(row, &cols);
    let mut titled: Vec<(SortKey, &str, Rect)> = vec![(SortKey::Name, "col.tunnel", c.name)];
    titled.extend(cols.iter().zip(&c.nums).map(|((k, t, _), (_, r))| (*k, *t, *r)));
    for (key, title, rect) in titled {
        let id = ui.id().with(("hdr", title));
        let resp = ui.interact(rect, id, Sense::click()).on_hover_text(tr("col.sort_hint"));
        if resp.clicked() {
            actions.push(Action::Sort(key));
        }
        let color = if resp.hovered() { ui.visuals().strong_text_color() } else { weak };
        let (align, pos) = if key == SortKey::Name {
            (Align2::LEFT_CENTER, rect.left_center() + Vec2::new(22.0, 0.0))
        } else {
            (Align2::RIGHT_CENTER, rect.right_center() - Vec2::new(8.0, 0.0))
        };
        let text_rect = painter.text(pos, align, tr(title), FontId::proportional(13.0), color);
        if s.sort == key {
            let x = if key == SortKey::Name { text_rect.right() + 8.0 } else { text_rect.left() - 8.0 };
            triangle(&painter, Pos2::new(x, row.center().y), 4.0, !s.sort_desc, color);
        }
        if key != SortKey::Name {
            let handle = Rect::from_x_y_ranges(rect.left() - 3.0..=rect.left() + 3.0, row.y_range());
            let drag = ui.interact(handle, id.with("resize"), Sense::drag());
            if drag.hovered() || drag.dragged() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                painter.vline(rect.left(), row.y_range(), Stroke::new(1.0, weak));
            }
            if let Some(w) = column_width(s, key) {
                *w = (*w - drag.drag_delta().x).clamp(50.0, 320.0);
            }
        }
    }
}

/// Треугольник: вершиной вверх (`up`) или вниз; вправо — для свёрнутой группы.
fn triangle(painter: &egui::Painter, c: Pos2, r: f32, up: bool, color: Color32) {
    let pts = if up {
        vec![Pos2::new(c.x - r, c.y + r * 0.6), Pos2::new(c.x + r, c.y + r * 0.6), Pos2::new(c.x, c.y - r * 0.7)]
    } else {
        vec![Pos2::new(c.x - r, c.y - r * 0.6), Pos2::new(c.x + r, c.y - r * 0.6), Pos2::new(c.x, c.y + r * 0.7)]
    };
    painter.add(egui::Shape::convex_polygon(pts, color, Stroke::NONE));
}

fn triangle_right(painter: &egui::Painter, c: Pos2, r: f32, color: Color32) {
    let pts = vec![Pos2::new(c.x - r * 0.6, c.y - r), Pos2::new(c.x - r * 0.6, c.y + r), Pos2::new(c.x + r * 0.7, c.y)];
    painter.add(egui::Shape::convex_polygon(pts, color, Stroke::NONE));
}

fn truncated(ui: &Ui, text: &str, font: FontId, color: Color32, width: f32) -> Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::single_section(text.to_string(), egui::TextFormat::simple(font, color));
    job.wrap = egui::text::TextWrapping::truncate_at_width(width.max(10.0));
    ui.fonts(|f| f.layout_job(job))
}

/// Строка группы (или «Без группы») в дереве.
struct GroupLine<'a> {
    key: &'a str,
    depth: usize,
    collapsed: bool,
    /// Видимые туннели всего поддерева — для итогов.
    members: &'a [String],
    selected: bool,
}

/// Тонкие вертикальные линии дерева под треугольниками предков.
fn guides(ui: &Ui, painter: &egui::Painter, row: Rect, depth: usize) {
    let color = ui.visuals().widgets.noninteractive.bg_stroke.color;
    for k in 0..depth {
        let x = row.left() + k as f32 * INDENT + 10.0;
        painter.vline(x, row.y_range(), Stroke::new(1.0, color));
    }
}

/// Строка стала выделенной клавишами — прокрутить к ней.
fn scroll_if_flagged(ui: &Ui, row: Rect) {
    if ui.ctx().data_mut(|d| d.remove_temp::<bool>(scroll_flag())).is_some() {
        ui.scroll_to_rect(row, None);
    }
}

fn group_row(ui: &mut Ui, g: &GroupLine, cols: &[(SortKey, &str, f32)], s: &Settings, l: &List, actions: &mut Vec<Action>) {
    let real = g.key != UNGROUPED;
    let sense = if real { Sense::click_and_drag() } else { Sense::click() };
    let (row, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H + 2.0), sense);
    if g.selected {
        scroll_if_flagged(ui, row);
    }
    let painter = ui.painter_at(row);
    if g.selected {
        painter.rect_filled(row, 2.0, ui.visuals().selection.bg_fill);
    } else if resp.hovered() {
        painter.rect_filled(row, 2.0, ui.visuals().widgets.hovered.weak_bg_fill);
    }
    let strong = if g.selected { ui.visuals().selection.stroke.color } else { ui.visuals().strong_text_color() };
    let weak = if g.selected { strong } else { ui.visuals().weak_text_color() };
    guides(ui, &painter, row, g.depth);
    let c = cells(row, cols);
    let x0 = row.left() + g.depth as f32 * INDENT;
    let arrow = Pos2::new(x0 + 10.0, row.center().y);
    if g.collapsed {
        triangle_right(&painter, arrow, 4.5, strong);
    } else {
        triangle(&painter, arrow, 4.5, false, strong);
    }

    // Итоги по всему поддереву: активные/всего, сумма скачанного и отданного, пик — максимум, доля — сумма.
    let mut agg = Agg::default();
    for t in g.members {
        let st = l.stats.get(t).cloned().unwrap_or_default();
        let active = l.healths.get(t).is_some_and(|h| h.level != Level::Off);
        agg.add(active, st.rx, st.tx, st.peak_rx, stats::share(l.stats, t));
    }
    let shown = if real { groups::leaf(g.key).to_string() } else { tr("app.ungrouped") };
    let count = format!("{}/{}", agg.active, agg.total);
    let count_w = 12.0 + 9.0 * count.chars().count() as f32;
    let name_left = x0 + 22.0;
    let galley = truncated(ui, &shown, FontId::proportional(15.0), strong, c.name.right() - name_left - count_w - 8.0);
    let name_w = galley.size().x;
    painter.galley(Pos2::new(name_left, row.center().y - galley.size().y / 2.0), galley, strong);
    painter.text(Pos2::new(name_left + name_w + 12.0, row.center().y), Align2::LEFT_CENTER, count, FontId::monospace(12.5), weak);
    let mut sum = TunnelStats::default();
    sum.rx = agg.rx;
    sum.tx = agg.tx;
    sum.peak_rx = agg.peak;
    for (key, rect) in &c.nums {
        let text = cell_value(*key, &sum, agg.share);
        painter.text(rect.right_center() - Vec2::new(8.0, 0.0), Align2::RIGHT_CENTER, text, FontId::monospace(NUM_FONT), strong);
    }

    if real {
        resp.dnd_set_drag_payload(Drag::Group(g.key.to_string()));
    }
    drop_target(ui, &resp, row, real.then_some(g.key), s, actions);
    if resp.clicked() {
        actions.push(Action::ToggleCollapse(g.key.to_string()));
        actions.push(Action::SelectGroup(g.key.to_string()));
    }
    if !real {
        return;
    }
    let path = g.key;
    resp.context_menu(|ui| {
        if ui.button(tr("grp.new_sub")).clicked() {
            actions.push(Action::Open(Dialog::NewGroup { parent: Some(path.to_string()), name: String::new(), assign: None }));
            ui.close_menu();
        }
        if ui.add(egui::Button::new(tr("grp.rename")).shortcut_text("F2")).clicked() {
            actions.push(Action::Open(Dialog::Rename { old: path.to_string(), name: groups::leaf(path).to_string() }));
            ui.close_menu();
        }
        ui.separator();
        if ui.add_enabled(groups::can_move(&s.groups, path, -1), egui::Button::new(tr("grp.up"))).clicked() {
            actions.push(Action::MoveGroup(path.to_string(), -1));
            ui.close_menu();
        }
        if ui.add_enabled(groups::can_move(&s.groups, path, 1), egui::Button::new(tr("grp.down"))).clicked() {
            actions.push(Action::MoveGroup(path.to_string(), 1));
            ui.close_menu();
        }
        if g.depth > 0 {
            let verdict = groups::check_reparent(&s.groups, path, None);
            let top = ui.add_enabled(verdict == Verdict::Valid, egui::Button::new(tr("grp.to_top")));
            if top.on_disabled_hover_text(tr("grp.to_top_taken")).clicked() {
                actions.push(Action::Reparent(path.to_string(), None));
                ui.close_menu();
            }
        }
        ui.separator();
        if ui.add(egui::Button::new(tr("grp.delete")).shortcut_text("Del")).clicked() {
            actions.push(Action::Confirm(Confirm::DeleteGroup(path.to_string())));
            ui.close_menu();
        }
    });
}

/// Подменю «В группу»: дерево групп вложенными меню; текущая группа отмечена.
fn group_menu(ui: &mut Ui, s: &Settings, of: Option<&str>, tunnel: &str, actions: &mut Vec<Action>) {
    let current = groups::group_of(s, tunnel);
    for g in groups::children(&s.groups, of) {
        let here = current == Some(g.as_str());
        let mut assign = false;
        if groups::children(&s.groups, Some(g)).is_empty() {
            assign = ui.add(egui::Button::new(groups::leaf(g)).selected(here)).clicked();
        } else {
            let inside = current.is_some_and(|c| groups::within(c, g));
            let title = if inside { RichText::new(groups::leaf(g)).strong() } else { RichText::new(groups::leaf(g)) };
            ui.menu_button(title, |ui| {
                assign = ui.add(egui::Button::new(trf("act.into_group", &[groups::leaf(g)])).selected(here)).clicked();
                ui.separator();
                group_menu(ui, s, Some(g), tunnel, actions);
            });
        }
        if assign {
            actions.push(Action::Assign(tunnel.to_string(), Some(g.clone())));
            ui.close_menu();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn tunnel_row(
    ui: &mut Ui,
    name: &String,
    depth: usize,
    group: Option<&str>,
    cols: &[(SortKey, &str, f32)],
    s: &Settings,
    l: &List,
    actions: &mut Vec<Action>,
) {
    let h = l.healths.get(name).cloned().unwrap_or(Health { level: Level::Off, text: String::new() });
    let sense = if s.view.groups { Sense::click_and_drag() } else { Sense::click() };
    let (row, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H), sense);
    let selected = l.sel_group.is_none() && s.selected.as_ref() == Some(name);
    if selected {
        scroll_if_flagged(ui, row);
    }
    let painter = ui.painter_at(row);
    if selected {
        painter.rect_filled(row, 2.0, ui.visuals().selection.bg_fill);
    } else if resp.hovered() {
        painter.rect_filled(row, 2.0, ui.visuals().widgets.hovered.weak_bg_fill);
    }
    let text_color = if selected { ui.visuals().selection.stroke.color } else { ui.visuals().text_color() };
    guides(ui, &painter, row, depth);
    let indent = depth as f32 * INDENT + 6.0;
    let c = cells(row, cols);
    painter.circle_filled(Pos2::new(c.name.left() + indent + 5.0, row.center().y), 5.0, level_color(h.level));
    let galley = truncated(ui, name, FontId::proportional(15.0), text_color, c.name.width() - indent - 22.0);
    painter.galley(Pos2::new(c.name.left() + indent + 16.0, row.center().y - galley.size().y / 2.0), galley, text_color);
    let st = l.stats.get(name).cloned().unwrap_or_default();
    let share = stats::share(l.stats, name);
    for (key, rect) in &c.nums {
        painter.text(
            rect.right_center() - Vec2::new(8.0, 0.0),
            Align2::RIGHT_CENTER,
            cell_value(*key, &st, share),
            FontId::monospace(NUM_FONT),
            text_color,
        );
    }
    if s.view.groups {
        resp.dnd_set_drag_payload(Drag::Tunnel(name.clone()));
        // Брошенное на туннель попадает в его группу.
        drop_target(ui, &resp, row, group, s, actions);
    }
    let resp = resp.on_hover_text(format!("{name}\n{}", h.text));
    if resp.clicked() {
        actions.push(Action::Select(name.clone()));
    }
    let running = h.level != Level::Off;
    let busy = h.level == Level::Busy;
    let plan = if running { Plan::Disconnect } else { Plan::Connect };
    if resp.double_clicked() && !busy {
        actions.push(Action::Switch(name.clone(), plan));
    }
    resp.context_menu(|ui| {
        let main = egui::Button::new(tr(if running { "act.disconnect" } else { "act.connect" })).shortcut_text("Enter");
        if ui.add_enabled(!busy, main).clicked() {
            actions.push(Action::Switch(name.clone(), plan));
            ui.close_menu();
        }
        if running && ui.add_enabled(!busy, egui::Button::new(tr("act.reconnect"))).clicked() {
            actions.push(Action::Switch(name.clone(), Plan::Reconnect));
            ui.close_menu();
        }
        ui.menu_button(tr("act.to_group"), |ui| {
            group_menu(ui, s, None, name, actions);
            if !s.groups.is_empty() {
                ui.separator();
            }
            let loose = groups::group_of(s, name).is_none();
            if ui.add(egui::Button::new(tr("app.ungrouped")).selected(loose)).clicked() {
                actions.push(Action::Assign(name.clone(), None));
                ui.close_menu();
            }
            if ui.button(tr("act.new_group")).clicked() {
                actions.push(Action::Open(Dialog::NewGroup { parent: None, name: String::new(), assign: Some(name.clone()) }));
                ui.close_menu();
            }
        });
        if ui.button(tr("act.edit_native")).clicked() {
            actions.push(Action::EditNative(name.clone()));
            ui.close_menu();
        }
        ui.separator();
        // Источники — незашифрованные .conf пользователя, из которых туннели импортированы в AmneziaWG.
        if ui.button(tr("act.add_conf")).clicked() {
            actions.push(Action::AddConf);
            ui.close_menu();
        }
        let source = s.sources.get(name);
        let edit = ui.add_enabled(source.is_some(), egui::Button::new(tr("act.edit_source")));
        let edit = match source {
            Some(path) => edit.on_hover_text(path),
            None => edit.on_disabled_hover_text(tr("src.none")),
        };
        if edit.clicked() {
            actions.push(Action::EditSource(name.clone()));
            ui.close_menu();
        }
        if source.is_none() {
            if ui.button(tr("act.set_source")).clicked() {
                actions.push(Action::SetSource(name.clone()));
                ui.close_menu();
            }
        } else {
            // Источник привязан — пункт становится подменю синхронизации.
            ui.menu_button(tr("act.set_source"), |ui| {
                if ui.button(tr("sync.to_source")).on_hover_text(tr("sync.to_source_hint")).clicked() {
                    actions.push(Action::Confirm(Confirm::ToSource(name.clone())));
                    ui.close_menu();
                }
                if ui.button(tr("sync.to_native")).on_hover_text(tr("sync.to_native_hint")).clicked() {
                    actions.push(Action::Confirm(Confirm::ToNative(name.clone())));
                    ui.close_menu();
                }
                ui.separator();
                if ui.button(tr("sync.other_file")).clicked() {
                    actions.push(Action::SetSource(name.clone()));
                    ui.close_menu();
                }
            });
        }
        ui.separator();
        let delete = egui::Button::new(RichText::new(tr("del.tunnel_menu")).color(RED)).shortcut_text("Del");
        if ui.add(delete).clicked() {
            actions.push(Action::Confirm(Confirm::DeleteTunnel(name.clone())));
            ui.close_menu();
        }
    });
}

/// Кнопки диалога по правому краю в порядке Windows: главная первой, «Отмена» последней.
/// Возвращает (главная нажата, отмена нажата).
fn dialog_buttons(ui: &mut Ui, primary: &str, enabled: bool, cancel: Option<&str>) -> (bool, bool) {
    let (mut ok, mut no) = (false, false);
    let size = Vec2::new(88.0, 26.0);
    // Полоса высотой в кнопку: `with_layout` занял бы всю высоту окна.
    let layout = egui::Layout::right_to_left(egui::Align::Center);
    ui.allocate_ui_with_layout(Vec2::new(ui.available_width(), size.y), layout, |ui| {
        // Справа налево: сначала «Отмена», левее — главная.
        if let Some(c) = cancel {
            no = ui.add(egui::Button::new(c).min_size(size)).on_hover_text("Esc").clicked();
        }
        ok = ui.add_enabled(enabled, egui::Button::new(primary).min_size(size)).on_hover_text("Enter").clicked();
    });
    (ok, no)
}

/// Enter и Esc для верхнего диалога: забираются из ввода, чтобы их не увидели окна под ним.
fn dialog_keys(ctx: &egui::Context) -> (bool, bool) {
    ctx.input_mut(|i| {
        (i.consume_key(egui::Modifiers::NONE, egui::Key::Enter), i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
    })
}

// ───────────────────────────── правая часть ─────────────────────────────

struct Detail<'a> {
    name: &'a str,
    live: Option<&'a Live>,
    health: Health,
    busy: bool,
    ping: &'a PingState,
    stats: &'a Stats,
    /// Сведения о конфиге из источника или родного окна и откуда они.
    info: Option<&'a (TunnelInfo, String)>,
    info_loading: bool,
}

/// Подключённый туннель: сведения из службы (UAPI), адреса/DNS/MTU — из конфига, если он известен.
fn info_from_status(st: &crate::uapi::Status, conf: Option<&TunnelInfo>) -> TunnelInfo {
    let base = conf.cloned().unwrap_or_default();
    TunnelInfo {
        public_key: st.public_key.clone(),
        listen_port: st.listen_port.to_string(),
        mtu: base.mtu,
        addresses: base.addresses,
        dns: base.dns,
        awg: st.awg_params.clone(),
        peers: st
            .peers
            .iter()
            .map(|p| PeerInfo {
                public_key: p.public_key.clone(),
                preshared: conf.and_then(|c| c.peers.iter().find(|cp| cp.public_key == p.public_key)).is_some_and(|cp| cp.preshared),
                endpoint: p.endpoint.clone(),
                allowed_ips: p.allowed_ips.clone(),
                keepalive: if p.keepalive == 0 { String::new() } else { p.keepalive.to_string() },
            })
            .collect(),
    }
}

/// Сетка из 4 колонок равной ширины: подпись, значение, подпись, значение. Ширина не зависит от текста.
/// Подписи — по левому краю, значения — по правому краю своей колонки.
fn fixed_grid(ui: &mut Ui, id: &str, rows: &[[RichText; 4]]) {
    let spacing = 24.0;
    let col = ((ui.available_width() - spacing * 3.0) / 4.0).max(60.0);
    egui::Grid::new(id).num_columns(4).spacing([spacing, 6.0]).min_col_width(col).max_col_width(col).show(ui, |ui| {
        for row in rows {
            for (i, cell) in row.iter().enumerate() {
                let layout = if i % 2 == 1 {
                    egui::Layout::right_to_left(egui::Align::Center)
                } else {
                    egui::Layout::left_to_right(egui::Align::Center)
                };
                ui.allocate_ui_with_layout(Vec2::new(col, ROW_H - 4.0), layout, |ui| {
                    ui.add(egui::Label::new(cell.clone()).truncate());
                });
            }
            ui.end_row();
        }
    });
}

fn label(text: &str) -> RichText {
    RichText::new(text).weak()
}

fn details(ui: &mut Ui, d: &Detail, s: &mut Settings, actions: &mut Vec<Action>) {
    let status = d.live.and_then(|l| l.status.as_ref());
    let running = d.live.is_some();
    egui::Frame::group(ui.style()).inner_margin(egui::Margin::same(12)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(RichText::new(d.name).size(18.0).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let text = tr(if running { "act.disconnect" } else { "act.connect" });
                let main = egui::Button::new(RichText::new(text).size(16.0)).min_size(Vec2::new(140.0, 32.0));
                if ui.add_enabled(!d.busy, main).clicked() {
                    let plan = if running { Plan::Disconnect } else { Plan::Connect };
                    actions.push(Action::Switch(d.name.to_string(), plan));
                }
                if s.view.reconnect && running {
                    let re = egui::Button::new(RichText::new(tr("act.reconnect")).size(16.0)).min_size(Vec2::new(150.0, 32.0));
                    if ui.add_enabled(!d.busy, re).clicked() {
                        actions.push(Action::Switch(d.name.to_string(), Plan::Reconnect));
                    }
                }
            });
        });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            dot(ui, level_color(d.health.level), 9.0);
            ui.label(RichText::new(&d.health.text).size(22.0).strong().color(level_color(d.health.level)));
        });
        ui.add_space(8.0);

        let mut rows: Vec<[RichText; 4]> = Vec::new();
        if let (Some(live), Some(st)) = (d.live, status) {
            let (rx_rate, tx_rate) = live.rate(3.0);
            let hs = st.last_handshake_sec();
            let age = if hs == 0 { tr("st.never") } else { fmt::ago(monitor::unix_now().saturating_sub(hs)) };
            let endpoint = st.peers.first().map(|p| p.endpoint.clone()).unwrap_or_else(|| "—".into());
            rows.push([label(&tr("st.handshake")), mono(age, Color32::WHITE), label(&tr("st.endpoint")), mono(endpoint, Color32::WHITE)]);
            rows.push([
                label(&tr("st.received")),
                mono(fmt::bytes(st.rx_bytes() as f64), GREEN),
                label(&tr("st.sent")),
                mono(fmt::bytes(st.tx_bytes() as f64), BLUE),
            ]);
            rows.push([
                label(&tr("st.rx_rate")),
                mono(fmt::rate(rx_rate), GREEN),
                label(&tr("st.tx_rate")),
                mono(fmt::rate(tx_rate), BLUE),
            ]);
            if s.view.ping {
                let value = match &d.ping.last {
                    None => mono("…", GRAY),
                    Some(Ok(ms)) => mono(trf("unit.ms", &[&ms.to_string()]), VIOLET),
                    Some(Err(e)) => mono(format!("{}: {e}", tr("st.no_reply")), RED),
                };
                rows.push([label(&trf("st.ping_to", &[&d.ping.host])), value, label(""), label("")]);
            }
        }
        if s.view.totals {
            if let Some(t) = d.stats.get(d.name) {
                let share = stats::share(d.stats, d.name);
                rows.push([
                    label(&tr("tot.rx")),
                    mono(fmt::bytes(t.rx as f64), GREEN),
                    label(&tr("tot.tx")),
                    mono(fmt::bytes(t.tx as f64), BLUE),
                ]);
                rows.push([
                    label(&tr("tot.peak")),
                    mono(fmt::rate(t.peak_rx), GREEN),
                    label(&tr("tot.time")),
                    mono(format!("{} ({})", fmt::duration(t.seconds), fmt::percent(share)), Color32::WHITE),
                ]);
                rows.push([label(&tr("tot.since")), mono(fmt::local_time(t.since, &tr("fmt.date_time")), GRAY), label(""), label("")]);
            }
        }
        fixed_grid(ui, "status-grid", &rows);

        if let (true, Some(live)) = (s.view.graph, d.live) {
            ui.add_space(8.0);
            graph(ui, live, s.view.ping.then_some(d.ping), s);
        }
    });

    if !s.view.details {
        return;
    }
    ui.add_space(8.0);
    // Подключён — данные службы; нет — конфиг из источника или из родного окна.
    let (info, origin) = match (status, d.info) {
        (Some(st), conf) => (Some(info_from_status(st, conf.map(|c| &c.0))), None),
        (None, Some((info, origin))) => (Some(info.clone()), Some(origin.as_str())),
        (None, None) => (None, None),
    };
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| match info {
        Some(info) => config_sections(ui, &info, origin),
        None => {
            ui.weak(tr("det.not_loaded"));
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let read = egui::Button::new(tr("det.read_native"));
                if ui.add_enabled(!d.info_loading, read).on_hover_text(tr("det.read_native_hint")).clicked() {
                    actions.push(Action::ReadNative(d.name.to_string()));
                }
                if d.info_loading {
                    ui.spinner();
                }
            });
        }
    });
}

/// Разделы «Интерфейс» и «Пир»; `origin` — откуда сведения, если туннель не подключён.
fn config_sections(ui: &mut Ui, info: &TunnelInfo, origin: Option<&str>) {
    if let Some(o) = origin {
        ui.weak(o);
        ui.add_space(4.0);
    }
    let row = |ui: &mut Ui, name: String, value: &str| {
        if !value.is_empty() {
            ui.weak(name);
            ui.add(egui::Label::new(RichText::new(value).monospace()).wrap());
            ui.end_row();
        }
    };
    egui::CollapsingHeader::new(RichText::new(tr("det.interface")).strong()).default_open(true).show(ui, |ui| {
        egui::Grid::new("iface").num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
            row(ui, tr("det.pubkey"), &info.public_key);
            row(ui, tr("det.addresses"), &info.addresses.join(", "));
            row(ui, tr("det.dns"), &info.dns.join(", "));
            row(ui, tr("det.port"), &info.listen_port);
            row(ui, tr("det.mtu"), &info.mtu);
            let awg: Vec<String> = info.awg.iter().map(|(k, v)| format!("{k}={v}")).collect();
            row(ui, "AWG".to_string(), &awg.join("  "));
        });
    });
    for (i, peer) in info.peers.iter().enumerate() {
        egui::CollapsingHeader::new(RichText::new(tr("det.peer")).strong()).id_salt(("peer", i)).default_open(true).show(ui, |ui| {
            egui::Grid::new(("peer-grid", i)).num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
                row(ui, tr("det.pubkey"), &peer.public_key);
                row(ui, tr("st.endpoint"), &peer.endpoint);
                let keepalive = match peer.keepalive.parse::<f64>() {
                    Ok(k) if k > 0.0 => fmt::duration(k),
                    _ => tr("det.off"),
                };
                row(ui, tr("det.keepalive"), &keepalive);
                row(ui, tr("det.preshared"), &tr(if peer.preshared { "det.yes" } else { "det.no" }));
            });
            egui::CollapsingHeader::new(trf("det.allowed", &[&peer.allowed_ips.len().to_string()]))
                .id_salt(("ips", i))
                .default_open(false)
                .show(ui, |ui| {
                    ui.add(egui::Label::new(RichText::new(peer.allowed_ips.join(", ")).monospace()).wrap());
                });
        });
    }
}

/// Максимум по корзинам — пики не теряются при сжатии часа в ширину графика.
pub fn bucket_max(series: &[(f64, f64)], buckets: usize) -> Vec<(f64, f64)> {
    if buckets == 0 || series.len() <= buckets {
        return series.to_vec();
    }
    let k = series.len().div_ceil(buckets);
    series.chunks(k).map(|c| c.iter().fold((0.0f64, 0.0f64), |a, s| (a.0.max(s.0), a.1.max(s.1)))).collect()
}

/// График скорости (вход — зелёный, выход — синий), под ним полоса пинга. Высота тянется за нижний край.
fn graph(ui: &mut Ui, live: &Live, ping: Option<&PingState>, s: &mut Settings) {
    ui.horizontal(|ui| {
        ui.weak(tr("gr.speed"));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            for (secs, title) in PERIODS.iter().rev() {
                ui.selectable_value(&mut s.graph_period, *secs, tr(title));
            }
        });
    });
    let period = s.graph_period.max(60) as f64;
    let height = s.graph_height.clamp(GRAPH_MIN_H, GRAPH_MAX_H);
    let (rect, hover) = ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, egui::CornerRadius::same(4), ui.visuals().extreme_bg_color);
    painter.hline(rect.x_range(), rect.center().y, Stroke::new(1.0, ui.visuals().faint_bg_color));
    let series = live.rate_series(period);
    let points = bucket_max(&series, rect.width() as usize);
    let max = points.iter().map(|(r, t)| r.max(*t)).fold(1.0, f64::max);
    // Замер раз в секунду; после сжатия одна точка покрывает `per_point` секунд.
    let per_point = if series.len() > points.len() { series.len().div_ceil(points.len().max(1)) } else { 1 };
    let step = rect.width() / period as f32 * per_point as f32;
    let line = |pick: fn(&(f64, f64)) -> f64| -> Vec<Pos2> {
        points
            .iter()
            .rev()
            .enumerate()
            .map(|(i, p)| Pos2::new(rect.right() - i as f32 * step, rect.bottom() - 4.0 - (pick(p) / max) as f32 * (rect.height() - 8.0)))
            .collect()
    };
    if points.len() >= 2 {
        painter.add(egui::Shape::line(line(|p| p.1), Stroke::new(1.5, BLUE)));
        painter.add(egui::Shape::line(line(|p| p.0), Stroke::new(1.5, GREEN)));
    }
    painter.text(
        rect.left_top() + Vec2::new(6.0, 4.0),
        Align2::LEFT_TOP,
        trf("gr.peak", &[&fmt::rate(max)]),
        FontId::proportional(12.0),
        ui.visuals().weak_text_color(),
    );

    // Наведение: ближайшая точка — вертикальная линия, точки на кривых, подсказка со значениями.
    if let Some(pos) = hover.hover_pos() {
        if let Some(i) = nearest_from_right(rect.right() - pos.x, step, points.len()) {
            let p = points[points.len() - 1 - i];
            let x = rect.right() - i as f32 * step;
            let y = |v: f64| rect.bottom() - 4.0 - (v / max) as f32 * (rect.height() - 8.0);
            painter.vline(x, rect.y_range(), Stroke::new(1.0, ui.visuals().weak_text_color()));
            painter.circle_filled(Pos2::new(x, y(p.0)), 3.5, GREEN);
            painter.circle_filled(Pos2::new(x, y(p.1)), 3.5, BLUE);
            let when = fmt::ago((i * per_point) as u64);
            hover.on_hover_ui_at_pointer(|ui| {
                ui.label(RichText::new(when).weak());
                if per_point > 1 {
                    ui.label(RichText::new(trf("gr.bucket", &[&fmt::duration(per_point as f64)])).weak());
                }
                egui::Grid::new("graph-tip").num_columns(2).spacing([16.0, 2.0]).show(ui, |ui| {
                    ui.label(&tr("st.rx_rate"));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| ui.label(mono(fmt::rate(p.0), GREEN)));
                    ui.end_row();
                    ui.label(&tr("st.tx_rate"));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| ui.label(mono(fmt::rate(p.1), BLUE)));
                    ui.end_row();
                });
            });
        }
    }

    if let Some(ping) = ping {
        ui.add_space(4.0);
        ping_strip(ui, ping, period);
    }

    // Ручка изменения высоты под графиком.
    let (handle, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 8.0), Sense::drag());
    if resp.hovered() || resp.dragged() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
        ui.painter().hline(handle.x_range(), handle.center().y, Stroke::new(2.0, ui.visuals().weak_text_color()));
    }
    if resp.dragged() {
        s.graph_height = (s.graph_height + resp.drag_delta().y).clamp(GRAPH_MIN_H, GRAPH_MAX_H);
    }
}

fn ping_strip(ui: &mut Ui, ping: &PingState, period: f64) {
    let (rect, hover) = ui.allocate_exact_size(Vec2::new(ui.available_width(), PING_STRIP_H), Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, egui::CornerRadius::same(4), ui.visuals().extreme_bg_color);
    let recent: Vec<(f64, Option<u32>)> = ping
        .history
        .iter()
        .map(|(at, ms)| (at.elapsed().as_secs_f64(), *ms))
        .filter(|(age, _)| *age <= period)
        .collect();
    let max = recent.iter().filter_map(|(_, ms)| *ms).max().unwrap_or(1).max(1) as f32;
    for (age, ms) in &recent {
        let x = rect.right() - (*age / period) as f32 * rect.width();
        match ms {
            Some(ms) => {
                let y = rect.bottom() - 4.0 - (*ms as f32 / max) * (rect.height() - 8.0);
                painter.circle_filled(Pos2::new(x, y), 2.5, VIOLET);
            }
            None => {
                painter.vline(x, rect.y_range(), Stroke::new(2.0, RED));
            }
        }
    }
    painter.text(
        rect.left_top() + Vec2::new(6.0, 3.0),
        Align2::LEFT_TOP,
        trf("gr.ping_legend", &[&trf("unit.ms", &[&format!("{max:.0}")])]),
        FontId::proportional(12.0),
        ui.visuals().weak_text_color(),
    );

    // Наведение: ближайший замер по времени.
    let Some(pos) = hover.hover_pos() else { return };
    let age_at_cursor = (rect.right() - pos.x) as f64 / rect.width() as f64 * period;
    let Some((age, ms)) = recent.iter().min_by(|a, b| (a.0 - age_at_cursor).abs().total_cmp(&(b.0 - age_at_cursor).abs())) else {
        return;
    };
    let x = rect.right() - (*age / period) as f32 * rect.width();
    painter.vline(x, rect.y_range(), Stroke::new(1.0, ui.visuals().weak_text_color()));
    let (text, color) = match ms {
        Some(ms) => (trf("unit.ms", &[&ms.to_string()]), VIOLET),
        None => (tr("st.no_reply"), RED),
    };
    let when = fmt::ago(*age as u64);
    hover.on_hover_ui_at_pointer(|ui| {
        ui.label(RichText::new(when).weak());
        ui.horizontal(|ui| {
            ui.label(trf("st.ping_to", &[&ping.host]));
            ui.label(mono(text, color));
        });
    });
}

/// Индекс точки (отсчёт от правого края), ближайшей к курсору на расстоянии `dx` от правого края.
fn nearest_from_right(dx: f32, step: f32, len: usize) -> Option<usize> {
    if len == 0 || step <= 0.0 || dx < -step / 2.0 {
        return None;
    }
    let i = (dx / step).round().max(0.0) as usize;
    (i < len).then_some(i)
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

    #[test]
    fn bucket_keeps_peaks() {
        let series: Vec<(f64, f64)> = (0..100).map(|i| (if i == 37 { 1000.0 } else { 1.0 }, 0.0)).collect();
        let b = bucket_max(&series, 10);
        assert_eq!(b.len(), 10);
        assert_eq!(b.iter().map(|p| p.0).fold(0.0, f64::max), 1000.0);
        assert_eq!(bucket_max(&series[..5], 10).len(), 5);
    }

    #[test]
    fn hover_picks_nearest_point() {
        assert_eq!(nearest_from_right(0.0, 10.0, 5), Some(0));
        assert_eq!(nearest_from_right(14.0, 10.0, 5), Some(1));
        assert_eq!(nearest_from_right(16.0, 10.0, 5), Some(2));
        // Левее самой старой точки — подсказки нет.
        assert_eq!(nearest_from_right(100.0, 10.0, 5), None);
        assert_eq!(nearest_from_right(5.0, 10.0, 0), None);
    }

    #[test]
    fn cells_fill_row() {
        let row = Rect::from_min_size(Pos2::ZERO, Vec2::new(500.0, 24.0));
        let cols = [(SortKey::Rx, "a", 100.0), (SortKey::Share, "b", 60.0)];
        let c = cells(row, &cols);
        assert_eq!(c.name.width(), 340.0);
        assert_eq!(c.nums[0].1.left(), 340.0);
        assert_eq!(c.nums[1].1.right(), 500.0);
    }
}
