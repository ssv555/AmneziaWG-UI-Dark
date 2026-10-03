//! Настройки окна и вида — `Settings.ini` рядом с exe. Пишутся при каждом изменении (см. `App::save_settings`).

use std::collections::{BTreeMap, BTreeSet};

use crate::ini::Ini;

pub const DEFAULT_PING_HOST: &str = "1.1.1.1";
pub const DEFAULT_LOG_DIR: &str = "logs";

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct WindowRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
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

/// Ширина числовых колонок таблицы туннелей; колонка имени занимает остаток.
#[derive(Clone, PartialEq, Debug)]
pub struct Columns {
    pub rx: f32,
    pub tx: f32,
    pub peak: f32,
    pub share: f32,
}

impl Default for Columns {
    fn default() -> Self {
        Columns { rx: 96.0, tx: 96.0, peak: 104.0, share: 64.0 }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct Settings {
    pub window: Option<WindowRect>,
    pub maximized: bool,
    pub left_width: f32,
    pub log_height: f32,
    pub graph_height: f32,
    /// Период графика, секунд.
    pub graph_period: u32,
    pub columns: Columns,
    pub view: View,
    pub sort: SortKey,
    pub sort_desc: bool,
    pub multiple: bool,
    pub tray: bool,
    pub notify: bool,
    pub close_to_tray: bool,
    pub ping_host: String,
    /// Код языка ISO 639-2 (eng, rus, …).
    pub language: String,
    /// Папка журнала событий; относительная — от папки программы.
    pub log_dir: String,
    pub selected: Option<String>,
    /// Полные пути групп через «/» («Europe/Amsterdam»); порядок — порядок среди соседей (см. `groups.rs`).
    pub groups: Vec<String>,
    /// Свёрнутые группы (пути).
    pub collapsed: BTreeSet<String>,
    /// туннель → путь группы
    pub assignment: BTreeMap<String, String>,
    /// туннель → незашифрованный .conf-источник пользователя
    pub sources: BTreeMap<String, String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            window: None,
            maximized: false,
            left_width: 520.0,
            log_height: 140.0,
            graph_height: 140.0,
            graph_period: 120,
            columns: Columns::default(),
            view: View::default(),
            sort: SortKey::Name,
            sort_desc: false,
            multiple: false,
            tray: true,
            notify: true,
            close_to_tray: true,
            ping_host: DEFAULT_PING_HOST.to_string(),
            language: crate::i18n::DEFAULT.to_string(),
            log_dir: DEFAULT_LOG_DIR.to_string(),
            selected: None,
            groups: Vec::new(),
            collapsed: BTreeSet::new(),
            assignment: BTreeMap::new(),
            sources: BTreeMap::new(),
        }
    }
}

impl Settings {
    pub fn from_ini(ini: &Ini) -> Settings {
        let d = Settings::default();
        let v = View::default();
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
            graph_height: ini.get_or("layout", "graph_height", d.graph_height),
            graph_period: ini.get_or("layout", "graph_period", d.graph_period),
            columns: Columns {
                rx: ini.get_or("columns", "rx", d.columns.rx),
                tx: ini.get_or("columns", "tx", d.columns.tx),
                peak: ini.get_or("columns", "peak", d.columns.peak),
                share: ini.get_or("columns", "share", d.columns.share),
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
            close_to_tray: ini.get_bool("options", "close_to_tray", d.close_to_tray),
            ping_host: ini.get("options", "ping_host").filter(|h| !h.is_empty()).unwrap_or(DEFAULT_PING_HOST).to_string(),
            language: ini.get("options", "language").filter(|c| crate::i18n::is_code(c)).unwrap_or(crate::i18n::DEFAULT).to_string(),
            log_dir: ini.get("options", "log_dir").filter(|d| !d.is_empty()).unwrap_or(DEFAULT_LOG_DIR).to_string(),
            selected: ini.get("state", "selected").filter(|s| !s.is_empty()).map(str::to_string),
            // Полные пути через «/»; плоские имена прежних версий — группы верхнего уровня.
            groups: crate::groups::normalize(&list("groups")),
            collapsed: list("collapsed").into_iter().collect(),
            assignment: ini.section("assign").iter().cloned().collect(),
            sources: ini.section("sources").iter().cloned().collect(),
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
        ini.set("layout", "graph_height", self.graph_height.round());
        ini.set("layout", "graph_period", self.graph_period);
        ini.set("columns", "rx", self.columns.rx.round());
        ini.set("columns", "tx", self.columns.tx.round());
        ini.set("columns", "peak", self.columns.peak.round());
        ini.set("columns", "share", self.columns.share.round());
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
        ini.set_bool("options", "close_to_tray", self.close_to_tray);
        ini.set("options", "ping_host", &self.ping_host);
        ini.set("options", "language", &self.language);
        ini.set("options", "log_dir", &self.log_dir);
        ini.set("state", "selected", self.selected.as_deref().unwrap_or(""));
        for (i, g) in self.groups.iter().enumerate() {
            ini.set("groups", &(i + 1).to_string(), g);
        }
        for (i, g) in self.collapsed.iter().enumerate() {
            ini.set("collapsed", &(i + 1).to_string(), g);
        }
        for (tunnel, group) in &self.assignment {
            ini.set("assign", tunnel, group);
        }
        for (tunnel, path) in &self.sources {
            ini.set("sources", tunnel, path);
        }
        ini
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_roundtrip() {
        let s = Settings::default();
        assert_eq!(Settings::from_ini(&Ini::parse(&s.to_ini().to_text())), s);
    }

    #[test]
    fn full_roundtrip() {
        let mut s = Settings::default();
        s.window = Some(WindowRect { x: -1200.0, y: 40.0, width: 1300.0, height: 800.0 });
        s.maximized = true;
        s.left_width = 610.0;
        s.graph_period = 3600;
        s.view.groups = false;
        s.view.col_tx = true;
        s.sort = SortKey::Share;
        s.sort_desc = true;
        s.ping_host = "8.8.8.8".into();
        s.language = "deu".into();
        s.log_dir = r"D:\logs".into();
        s.selected = Some("home.nl-ams.full".into());
        s.groups = vec!["Home".into(), "Office".into()];
        s.collapsed.insert("Office".into());
        s.assignment.insert("home.nl-ams.full".into(), "Home".into());
        s.sources.insert("home.nl-ams.full".into(), r"D:\vpn\home.nl-ams.full.conf".into());
        assert_eq!(Settings::from_ini(&Ini::parse(&s.to_ini().to_text())), s);
    }

    #[test]
    fn nested_groups_roundtrip_and_flat_compat() {
        let mut s = Settings::default();
        s.groups = vec!["Europe".into(), "Europe/Netherlands".into(), "Lab".into()];
        s.collapsed.insert("Europe/Netherlands".into());
        s.assignment.insert("nl".into(), "Europe/Netherlands".into());
        assert_eq!(Settings::from_ini(&Ini::parse(&s.to_ini().to_text())), s);
        // Файл прежней версии с плоскими именами и файл, где предок не записан.
        let old = Settings::from_ini(&Ini::parse("[groups]\n1=Home\n2=Office\n3=Travel/Nordics\n[assign]\nt=Home\n"));
        assert_eq!(old.groups, ["Home", "Office", "Travel", "Travel/Nordics"]);
        assert_eq!(old.assignment["t"], "Home");
    }
}
