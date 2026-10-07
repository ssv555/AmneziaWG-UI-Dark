//! Настройки окна и вида — `Settings.ini` рядом с exe. Пишутся при каждом изменении (см. `App::save_settings`).

use std::collections::{BTreeMap, BTreeSet};

use crate::groups::TunnelBook;
use crate::ini::Ini;

pub const DEFAULT_PING_HOST: &str = "1.1.1.1";
pub const DEFAULT_LOG_DIR: &str = "logs";
/// Пределы множителя масштаба интерфейса.
pub const MIN_SCALE: f32 = 0.5;
pub const MAX_SCALE: f32 = 3.0;
/// Наименьший размер главного окна в точках egui: столько места нужно таблице и карточке туннеля рядом. Окно держит
/// его при любом масштабе интерфейса (`WindowState::min_size_due`), поэтому раскладка проверяется при этом размере.
pub const MIN_WINDOW: [f32; 2] = [760.0, 480.0];

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct WindowRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// Режим работы программы.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum Mode {
    /// Надстройка над установленным AmneziaWG (по умолчанию).
    Overlay,
    /// Встроенный движок: свои службы туннелей и своё хранилище конфигов.
    Engine,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Overlay => "overlay",
            Mode::Engine => "engine",
        }
    }

    fn parse(s: &str) -> Option<Mode> {
        [Mode::Overlay, Mode::Engine].into_iter().find(|m| m.as_str() == s)
    }
}

/// Тема окна. `System` — светлая или тёмная вслед за Windows; какая именно, решает окно (`app::theme::resolve`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Theme {
    /// Тёмная, прежний вид (по умолчанию).
    Graphite,
    /// Мягкая тёмная сине-серая.
    Slate,
    /// Светлая.
    Daylight,
    /// Как в Windows.
    System,
}

impl Theme {
    pub const ALL: [Theme; 4] = [Theme::Graphite, Theme::Slate, Theme::Daylight, Theme::System];

    /// Имя в INI. Не менять: оно записано в файлах пользователей.
    pub fn as_str(self) -> &'static str {
        match self {
            Theme::Graphite => "graphite",
            Theme::Slate => "slate",
            Theme::Daylight => "daylight",
            Theme::System => "system",
        }
    }

    fn parse(s: &str) -> Option<Theme> {
        Theme::ALL.into_iter().find(|t| t.as_str() == s)
    }
}

/// Масштаб графика скорости. Первые три — замеры окна в памяти, последние три — история, которую копит агент
/// (`daemon::agent::history`); какой источник у масштаба, решает окно (`app::graph::source`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum GraphRange {
    #[default]
    Min2,
    Min10,
    Hour1,
    Day,
    Month,
    Year,
}

impl GraphRange {
    pub const ALL: [GraphRange; 6] = [GraphRange::Min2, GraphRange::Min10, GraphRange::Hour1, GraphRange::Day, GraphRange::Month, GraphRange::Year];

    /// Имя в INI. Не менять: оно записано в файлах пользователей.
    pub fn as_str(self) -> &'static str {
        match self {
            GraphRange::Min2 => "2m",
            GraphRange::Min10 => "10m",
            GraphRange::Hour1 => "1h",
            GraphRange::Day => "day",
            GraphRange::Month => "month",
            GraphRange::Year => "year",
        }
    }

    fn parse(s: &str) -> Option<GraphRange> {
        GraphRange::ALL.into_iter().find(|r| r.as_str() == s)
    }

    /// Прежний ключ `[layout] graph_period` (до 0.5.4): период в секундах.
    fn from_period(secs: &str) -> Option<GraphRange> {
        match secs.trim() {
            "120" => Some(GraphRange::Min2),
            "600" => Some(GraphRange::Min10),
            "3600" => Some(GraphRange::Hour1),
            _ => None,
        }
    }
}

/// Диалог с «Больше не показывать». Имя в `[hidden_dialogs]` — `ini_name`, оно записано в файлах пользователей и
/// от переводов не зависит: переименование ключа в `i18n` не должно сбрасывать запомненный выбор.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum DialogId {
    /// Справка о режиме 1.
    ModeOverlay,
    /// Справка о режиме 2.
    ModeEngine,
    /// Выход: оставить туннели подключёнными.
    ExitKeep,
    /// Выход: отключить туннели.
    ExitDisconnect,
    /// Подтверждение отключения туннеля (кнопка, меню строки, трей).
    Disconnect,
}

impl DialogId {
    const ALL: [DialogId; 5] = [DialogId::ModeOverlay, DialogId::ModeEngine, DialogId::ExitKeep, DialogId::ExitDisconnect, DialogId::Disconnect];

    /// Имя в INI. Не менять: файлы прежних версий хранят именно эти строки.
    pub fn ini_name(self) -> &'static str {
        match self {
            DialogId::ModeOverlay => "mode.overlay",
            DialogId::ModeEngine => "mode.engine",
            DialogId::ExitKeep => "exit.keep",
            DialogId::ExitDisconnect => "exit.disconnect",
            DialogId::Disconnect => "tunnel.disconnect",
        }
    }

    fn parse(name: &str) -> Option<DialogId> {
        DialogId::ALL.into_iter().find(|d| d.ini_name() == name)
    }

    /// Справка о переходе в этот режим.
    pub fn mode_help(mode: Mode) -> DialogId {
        match mode {
            Mode::Overlay => DialogId::ModeOverlay,
            Mode::Engine => DialogId::ModeEngine,
        }
    }
}

/// Ключи `[state]` для выбранного туннеля: (текущего режима, другого режима).
fn selected_keys(mode: Mode) -> (&'static str, &'static str) {
    match mode {
        Mode::Overlay => ("selected", "engine_selected"),
        Mode::Engine => ("engine_selected", "selected"),
    }
}

impl Settings {
    /// Текущий режим. Меняется только через `switch_mode`: вместе с ним меняется и выбранный туннель.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Перейти в другой режим: выбранный туннель меняется местами с запомненным для того режима.
    pub fn switch_mode(&mut self, to: Mode) {
        if self.mode != to {
            self.mode = to;
            self.book.swap_mode_selection();
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SortKey {
    Name,
    Rx,
    Tx,
    Peak,
    Share,
}

impl SortKey {
    const ALL: [(SortKey, &'static str); 5] =
        [(SortKey::Name, "name"), (SortKey::Rx, "rx"), (SortKey::Tx, "tx"), (SortKey::Peak, "peak"), (SortKey::Share, "share")];

    fn as_str(self) -> &'static str {
        Self::ALL.iter().find(|(k, _)| *k == self).map(|(_, s)| *s).unwrap_or("name")
    }

    fn parse(s: &str) -> SortKey {
        Self::ALL.iter().find(|(_, n)| *n == s).map(|(k, _)| *k).unwrap_or(SortKey::Name)
    }
}

/// Что показывать. Каждый элемент интерфейса включается своей галкой в меню «Вид».
#[derive(Clone, PartialEq, Debug)]
pub struct View {
    pub groups: bool,
    pub search: bool,
    pub col_rx: bool,
    pub col_tx: bool,
    pub col_peak: bool,
    pub col_share: bool,
    pub totals: bool,
    pub graph: bool,
    pub ping: bool,
    pub details: bool,
    pub reconnect: bool,
    pub log: bool,
}

impl Default for View {
    fn default() -> Self {
        View {
            groups: true,
            search: true,
            col_rx: true,
            col_tx: false,
            col_peak: true,
            col_share: true,
            totals: true,
            graph: true,
            ping: true,
            details: true,
            reconnect: true,
            log: true,
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct Settings {
    pub window: Option<WindowRect>,
    pub maximized: bool,
    pub left_width: f32,
    pub log_height: f32,
    /// Множитель масштаба интерфейса поверх масштаба Windows (1.0 = как в системе).
    pub ui_scale: f32,
    pub graph_height: f32,
    /// Масштаб графика; выбор в окне пишется сразу, как прочий вид.
    pub graph_range: GraphRange,
    pub view: View,
    pub sort: SortKey,
    pub sort_desc: bool,
    pub multiple: bool,
    pub tray: bool,
    pub notify: bool,
    /// Точка состояния на значке окна (панель задач, заголовок).
    pub taskbar: bool,
    pub close_to_tray: bool,
    /// Режим работы; меняется только с перезапуском программы, и только через `switch_mode`.
    mode: Mode,
    /// Тема окна; неизвестное имя в файле — Графит.
    pub theme: Theme,
    pub ping_host: String,
    /// Код языка ISO 639-2 (eng, rus, …).
    pub language: String,
    /// Папка журнала событий; относительная — от папки программы.
    pub log_dir: String,
    /// Группы, назначения, источники и выбор туннеля (по режимам) — см. `groups.rs`.
    pub book: TunnelBook,
    /// Диалоги, отмеченные «Больше не показывать».
    pub hidden_dialogs: BTreeSet<DialogId>,
    /// О каких версиях обновлений уже сообщали окном: ключ компонента → версия. Новая версия сообщается снова.
    pub update_notified: BTreeMap<String, String>,
    /// Когда в последний раз сообщали о неустановленном обновлении (unix-секунды); следующее напоминание — не раньше
    /// чем через сутки. Нигде не показывается, поэтому число, а не дата. «Позже» на уведомлении сдвигает его вперёд.
    pub update_reminded: Option<u64>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            window: None,
            maximized: false,
            left_width: 520.0,
            log_height: 140.0,
            ui_scale: 1.0,
            graph_height: 140.0,
            graph_range: GraphRange::default(),
            view: View::default(),
            sort: SortKey::Name,
            sort_desc: false,
            multiple: false,
            tray: true,
            notify: true,
            taskbar: true,
            close_to_tray: true,
            mode: Mode::Overlay,
            theme: Theme::Graphite,
            ping_host: DEFAULT_PING_HOST.to_string(),
            language: crate::i18n::DEFAULT.to_string(),
            log_dir: DEFAULT_LOG_DIR.to_string(),
            book: TunnelBook::default(),
            hidden_dialogs: BTreeSet::new(),
            update_notified: BTreeMap::new(),
            update_reminded: None,
        }
    }
}

impl Settings {
    pub fn from_ini(ini: &Ini) -> Settings {
        let d = Settings::default();
        let v = View::default();
        let mode = ini.get("options", "mode").and_then(Mode::parse).unwrap_or(d.mode);
        let window = match (ini.get("window", "x"), ini.get("window", "width")) {
            (Some(_), Some(_)) => Some(WindowRect {
                x: ini.get_or("window", "x", 0.0),
                y: ini.get_or("window", "y", 0.0),
                width: ini.get_or("window", "width", 1100.0),
                height: ini.get_or("window", "height", 720.0),
            }),
            _ => None,
        };
        let list = |section: &str| -> Vec<String> { ini.section(section).iter().map(|(_, v)| v.clone()).collect() };
        Settings {
            window,
            maximized: ini.get_bool("window", "maximized", false),
            left_width: ini.get_or("layout", "left_width", d.left_width),
            log_height: ini.get_or("layout", "log_height", d.log_height),
            ui_scale: ini.get_or("layout", "ui_scale", d.ui_scale).clamp(MIN_SCALE, MAX_SCALE),
            graph_height: ini.get_or("layout", "graph_height", d.graph_height),
            // Неизвестное имя (опечатка, файл новой версии) — 2 мин: это вид, не данные. Ключа нет — файл прежней версии
            // с периодом в секундах.
            graph_range: match ini.get("layout", "graph_range") {
                Some(name) => GraphRange::parse(name).unwrap_or(d.graph_range),
                None => ini.get("layout", "graph_period").and_then(GraphRange::from_period).unwrap_or(d.graph_range),
            },
            view: View {
                groups: ini.get_bool("view", "groups", v.groups),
                search: ini.get_bool("view", "search", v.search),
                col_rx: ini.get_bool("view", "col_rx", v.col_rx),
                col_tx: ini.get_bool("view", "col_tx", v.col_tx),
                col_peak: ini.get_bool("view", "col_peak", v.col_peak),
                col_share: ini.get_bool("view", "col_share", v.col_share),
                totals: ini.get_bool("view", "totals", v.totals),
                graph: ini.get_bool("view", "graph", v.graph),
                ping: ini.get_bool("view", "ping", v.ping),
                details: ini.get_bool("view", "details", v.details),
                reconnect: ini.get_bool("view", "reconnect", v.reconnect),
                log: ini.get_bool("view", "log", v.log),
            },
            sort: SortKey::parse(ini.get("view", "sort").unwrap_or("name")),
            sort_desc: ini.get_bool("view", "sort_desc", false),
            multiple: ini.get_bool("options", "multiple", d.multiple),
            tray: ini.get_bool("options", "tray", d.tray),
            notify: ini.get_bool("options", "notify", d.notify),
            taskbar: ini.get_bool("options", "taskbar_state", d.taskbar),
            close_to_tray: ini.get_bool("options", "close_to_tray", d.close_to_tray),
            mode,
            // Неизвестное имя (опечатка, другой регистр, файл новой версии) — тема по умолчанию: это вид, не данные.
            theme: ini.get("options", "theme").and_then(Theme::parse).unwrap_or(d.theme),
            ping_host: ini.get("options", "ping_host").filter(|h| !h.is_empty()).unwrap_or(DEFAULT_PING_HOST).to_string(),
            language: ini.get("options", "language").filter(|c| crate::i18n::is_code(c)).unwrap_or(crate::i18n::DEFAULT).to_string(),
            log_dir: ini.get("options", "log_dir").filter(|d| !d.is_empty()).unwrap_or(DEFAULT_LOG_DIR).to_string(),
            // Полные пути групп через «/»; плоские имена прежних версий — группы верхнего уровня (`from_stored`).
            book: TunnelBook::from_stored(
                &list("groups"),
                list("collapsed").into_iter().collect(),
                ini.section("assign").iter().cloned().collect(),
                ini.section("sources").iter().cloned().collect(),
                ini.get("state", selected_keys(mode).0).filter(|s| !s.is_empty()).map(str::to_string),
                ini.get("state", selected_keys(mode).1).filter(|s| !s.is_empty()).map(str::to_string),
            ),
            // Неизвестное имя (файл новой версии) отбрасывается: показать диалог лишний раз безопаснее, чем держать
            // выбор, смысл которого эта версия не знает.
            hidden_dialogs: list("hidden_dialogs").iter().filter_map(|n| DialogId::parse(n)).collect(),
            update_notified: ini.section("update_notified").iter().cloned().collect(),
            update_reminded: ini.get("update_reminder", "last").and_then(|t| t.parse().ok()),
        }
    }

    pub fn to_ini(&self) -> Ini {
        let mut ini = Ini::default();
        if let Some(w) = self.window {
            ini.set("window", "x", w.x.round());
            ini.set("window", "y", w.y.round());
            ini.set("window", "width", w.width.round());
            ini.set("window", "height", w.height.round());
        }
        ini.set_bool("window", "maximized", self.maximized);
        ini.set("layout", "left_width", self.left_width.round());
        ini.set("layout", "log_height", self.log_height.round());
        ini.set("layout", "ui_scale", (self.ui_scale * 100.0).round() / 100.0);
        ini.set("layout", "graph_height", self.graph_height.round());
        ini.set("layout", "graph_range", self.graph_range.as_str());
        let v = &self.view;
        for (key, on) in [
            ("groups", v.groups),
            ("search", v.search),
            ("col_rx", v.col_rx),
            ("col_tx", v.col_tx),
            ("col_peak", v.col_peak),
            ("col_share", v.col_share),
            ("totals", v.totals),
            ("graph", v.graph),
            ("ping", v.ping),
            ("details", v.details),
            ("reconnect", v.reconnect),
            ("log", v.log),
        ] {
            ini.set_bool("view", key, on);
        }
        ini.set("view", "sort", self.sort.as_str());
        ini.set_bool("view", "sort_desc", self.sort_desc);
        ini.set_bool("options", "multiple", self.multiple);
        ini.set_bool("options", "tray", self.tray);
        ini.set_bool("options", "notify", self.notify);
        ini.set_bool("options", "taskbar_state", self.taskbar);
        ini.set_bool("options", "close_to_tray", self.close_to_tray);
        ini.set("options", "mode", self.mode.as_str());
        ini.set("options", "theme", self.theme.as_str());
        ini.set("options", "ping_host", &self.ping_host);
        ini.set("options", "language", &self.language);
        ini.set("options", "log_dir", &self.log_dir);
        let (mine, other) = selected_keys(self.mode);
        ini.set("state", mine, self.book.tunnel().unwrap_or(""));
        ini.set("state", other, self.book.other_tunnel().unwrap_or(""));
        for (i, g) in self.book.groups().iter().enumerate() {
            ini.set("groups", &(i + 1).to_string(), g);
        }
        for (i, g) in self.book.collapsed().enumerate() {
            ini.set("collapsed", &(i + 1).to_string(), g);
        }
        for (i, d) in self.hidden_dialogs.iter().enumerate() {
            ini.set("hidden_dialogs", &(i + 1).to_string(), d.ini_name());
        }
        for (component, version) in &self.update_notified {
            ini.set("update_notified", component, version);
        }
        if let Some(t) = self.update_reminded {
            ini.set("update_reminder", "last", t);
        }
        for (tunnel, group) in self.book.assignments() {
            ini.set("assign", tunnel, group);
        }
        for (tunnel, path) in self.book.sources() {
            ini.set("sources", tunnel, path);
        }
        ini
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strs(items: &[&str]) -> Vec<String> {
        items.iter().map(|x| x.to_string()).collect()
    }

    fn pairs(items: &[(&str, &str)]) -> BTreeMap<String, String> {
        items.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    fn reload(settings: &Settings) -> Settings {
        Settings::from_ini(&Ini::parse(&settings.to_ini().to_text()))
    }

    #[test]
    fn default_roundtrip() {
        let s = Settings::default();
        assert_eq!(reload(&s), s);
    }

    #[test]
    fn full_roundtrip() {
        let mut s = Settings::default();
        s.window = Some(WindowRect { x: -1200.0, y: 40.0, width: 1300.0, height: 800.0 });
        s.maximized = true;
        s.left_width = 610.0;
        s.graph_range = GraphRange::Month;
        s.view.groups = false;
        s.view.col_tx = true;
        s.sort = SortKey::Share;
        s.sort_desc = true;
        s.ping_host = "8.8.8.8".into();
        s.language = "deu".into();
        s.log_dir = r"D:\logs".into();
        s.book = TunnelBook::from_stored(
            &strs(&["Home", "Office"]),
            ["Office".to_string()].into(),
            pairs(&[("home.nl-ams.full", "Home")]),
            pairs(&[("home.nl-ams.full", r"D:\vpn\home.nl-ams.full.conf")]),
            Some("home.nl-ams.full".into()),
            Some("lab.sg.v4".into()),
        );
        s.hidden_dialogs.insert(DialogId::ModeEngine);
        s.update_notified.insert("app".into(), "0.4.0".into());
        s.update_notified.insert("native".into(), "1.2.3".into());
        s.update_reminded = Some(1_800_000_000);
        assert_eq!(reload(&s), s);
    }

    #[test]
    fn selection_is_kept_per_mode() {
        let mut s = Settings::default();
        s.book.select_tunnel("native-a");
        s.switch_mode(Mode::Engine);
        assert_eq!(s.book.tunnel(), None, "в режиме 2 своих туннелей ещё не выбирали");
        s.book.select_tunnel("own-b");
        let mut back = reload(&s);
        assert_eq!((back.mode(), back.book.tunnel()), (Mode::Engine, Some("own-b")));
        back.switch_mode(Mode::Overlay);
        assert_eq!(back.book.tunnel(), Some("native-a"), "выбор режима 1 вернулся");
    }

    #[test]
    fn remembered_dialogs_keep_their_ini_names() {
        // Файл прежней версии: значения — прежние ключи; лишнее имя не мешает загрузке остального.
        let old = Settings::from_ini(&Ini::parse("[hidden_dialogs]
1=exit.keep
2=mode.engine
3=from.the.future
"));
        assert_eq!(old.hidden_dialogs, BTreeSet::from([DialogId::ExitKeep, DialogId::ModeEngine]));
        let out = old.to_ini();
        let mut names: Vec<String> = out.section("hidden_dialogs").iter().map(|(_, v)| v.clone()).collect();
        names.sort();
        assert_eq!(names, ["exit.keep", "mode.engine"]);
        // Имена закреплены: это формат файла, а не ключи перевода.
        let all: Vec<_> = DialogId::ALL.iter().map(|d| d.ini_name()).collect();
        assert_eq!(all, ["mode.overlay", "mode.engine", "exit.keep", "exit.disconnect", "tunnel.disconnect"]);
        assert_eq!(DialogId::mode_help(Mode::Engine), DialogId::ModeEngine);
    }

    #[test]
    fn theme_roundtrips_under_its_pinned_ini_name() {
        // Имена закреплены: это формат файла.
        assert_eq!(Theme::ALL.map(Theme::as_str), ["graphite", "slate", "daylight", "system"]);
        for theme in Theme::ALL {
            let mut s = Settings::default();
            s.theme = theme;
            assert_eq!(s.to_ini().get("options", "theme"), Some(theme.as_str()));
            assert_eq!(reload(&s), s, "{theme:?}");
        }
    }

    #[test]
    fn graph_range_roundtrips_under_its_pinned_ini_name() {
        // Имена закреплены: это формат файла.
        let all: Vec<_> = GraphRange::ALL.iter().map(|r| r.as_str()).collect();
        assert_eq!(all, ["2m", "10m", "1h", "day", "month", "year"]);
        for range in GraphRange::ALL {
            let mut s = Settings::default();
            s.graph_range = range;
            assert_eq!(s.to_ini().get("layout", "graph_range"), Some(range.as_str()));
            assert_eq!(reload(&s), s, "{range:?}");
        }
    }

    #[test]
    fn missing_or_unknown_graph_range_falls_back_to_two_minutes() {
        assert_eq!(Settings::default().graph_range, GraphRange::Min2);
        let range = |text: &str| Settings::from_ini(&Ini::parse(text)).graph_range;
        assert_eq!(range(""), GraphRange::Min2);
        for value in ["", "week", "Day", "3600"] {
            assert_eq!(range(&format!("[layout]\ngraph_range={value}\n")), GraphRange::Min2, "graph_range={value:?}");
        }
        // Файл 0.5.3 и раньше: период в секундах переносится; незнакомое число — 2 мин.
        assert_eq!(range("[layout]\ngraph_period=3600\n"), GraphRange::Hour1);
        assert_eq!(range("[layout]\ngraph_period=600\n"), GraphRange::Min10);
        assert_eq!(range("[layout]\ngraph_period=90\n"), GraphRange::Min2);
        // Новый ключ важнее прежнего; прежний больше не пишется.
        let both = Settings::from_ini(&Ini::parse("[layout]\ngraph_range=year\ngraph_period=600\n"));
        assert_eq!(both.graph_range, GraphRange::Year);
        assert_eq!(both.to_ini().get("layout", "graph_period"), None);
    }

    #[test]
    fn missing_or_unknown_theme_falls_back_to_graphite() {
        assert_eq!(Settings::default().theme, Theme::Graphite);
        // Файл прежней версии: ключа нет, остальное грузится как было.
        let old = Settings::from_ini(&Ini::parse("[options]\nmode=engine\n"));
        assert_eq!((old.theme, old.mode()), (Theme::Graphite, Mode::Engine));
        for value in ["neon", "", "Slate", "SYSTEM", "day light"] {
            let s = Settings::from_ini(&Ini::parse(&format!("[options]\ntheme={value}\nmode=engine\n")));
            assert_eq!((s.theme, s.mode()), (Theme::Graphite, Mode::Engine), "theme={value:?}");
        }
    }

    #[test]
    fn nested_groups_roundtrip_and_flat_compat() {
        let mut s = Settings::default();
        s.book = TunnelBook::from_stored(
            &strs(&["Europe", "Europe/Netherlands", "Lab"]),
            ["Europe/Netherlands".to_string()].into(),
            pairs(&[("nl", "Europe/Netherlands")]),
            BTreeMap::new(),
            None,
            None,
        );
        assert_eq!(reload(&s), s);
        // Файл прежней версии с плоскими именами и файл, где предок не записан.
        let old = Settings::from_ini(&Ini::parse("[groups]\n1=Home\n2=Office\n3=Travel/Nordics\n[assign]\nt=Home\n"));
        assert_eq!(old.book.groups(), ["Home", "Office", "Travel", "Travel/Nordics"]);
        assert_eq!(old.book.group_of("t"), Some("Home"));
    }

    /// Файл, записанный прежней версией (до `TunnelBook`): грузится так же и пишется теми же секциями и ключами.
    const SAMPLE: &str = r"[options]
mode=engine
[state]
engine_selected=own-b
selected=native-a
[groups]
1=Europe
2=Europe/Netherlands
3=Lab
[collapsed]
1=Europe/Netherlands
[assign]
nl=Europe/Netherlands
lost=Gone
own-b=Lab
[sources]
nl=D:\vpn\nl.conf
";

    #[test]
    fn sample_ini_loads_identically_and_rewrites_the_same_sections() {
        let loaded = Settings::from_ini(&Ini::parse(SAMPLE));
        let b = &loaded.book;
        assert_eq!(loaded.mode(), Mode::Engine);
        // В режиме 2 текущий выбор — engine_selected, а `selected` — запомненный выбор режима 1.
        assert_eq!((b.tunnel(), b.other_tunnel()), (Some("own-b"), Some("native-a")));
        assert_eq!(b.groups(), ["Europe", "Europe/Netherlands", "Lab"]);
        assert_eq!(b.collapsed().collect::<Vec<_>>(), ["Europe/Netherlands"]);
        assert_eq!(b.group_of("nl"), Some("Europe/Netherlands"));
        assert_eq!(b.group_of("own-b"), Some("Lab"));
        assert_eq!(b.group_of("lost"), None, "группы нет — туннель «Без группы»");
        assert_eq!(b.source("nl"), Some(r"D:\vpn\nl.conf"));

        // Запись: те же секции и ключи; назначение в исчезнувшую группу не теряется молча.
        let out = loaded.to_ini();
        for (section, expected) in [
            ("groups", vec![("1", "Europe"), ("2", "Europe/Netherlands"), ("3", "Lab")]),
            ("collapsed", vec![("1", "Europe/Netherlands")]),
            ("assign", vec![("lost", "Gone"), ("nl", "Europe/Netherlands"), ("own-b", "Lab")]),
            ("sources", vec![("nl", r"D:\vpn\nl.conf")]),
            ("state", vec![("engine_selected", "own-b"), ("selected", "native-a")]),
        ] {
            let mut got: Vec<(&str, &str)> = out.section(section).iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            got.sort();
            assert_eq!(got, expected, "[{section}]");
        }
        assert_eq!(reload(&loaded), loaded);
    }
}
