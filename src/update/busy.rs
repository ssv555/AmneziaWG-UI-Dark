//! Идущая работа менеджера обновлений. Хранится то, что делается, а не текст: текст для окна собирается при чтении
//! состояния на языке ядра в этот момент (смена языка посреди работы не оставляет старую надпись), а «занято» — это
//! наличие значения, не разбор строки.

use super::Component;
use crate::i18n::{tr, trf};

/// Что делает менеджер сейчас.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Busy {
    Checking,
    Backup { what: Component, version: String },
    /// `progress` — «45 %» или «1.2 MB» (не переводится).
    Download { what: Component, version: String, progress: String },
    Install { what: Component, version: String },
    Restore { what: Component, version: String },
    /// Файл набора нашего релиза.
    FileDownload { file: String, percent: u64 },
    /// Установка набора нашего релиза (замена файлов).
    SetInstall(Release),
}

/// Набор нашего релиза и его версия.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Release {
    Engine(String),
    App(String),
}

impl Busy {
    /// Текст для окна; `name` — название компонента на текущем языке.
    pub(super) fn text(&self, name: impl Fn(Component) -> String) -> String {
        match self {
            Busy::Checking => tr("updm.checking"),
            Busy::Backup { what, version } => trf("updm.backing_up", &[&name(*what), version]),
            Busy::Download { what, version, progress } => trf("updm.downloading", &[&name(*what), version, progress]),
            Busy::Install { what, version } => trf("updm.installing", &[&name(*what), version]),
            Busy::Restore { what, version } => trf("updm.restoring", &[&name(*what), version]),
            Busy::FileDownload { file, percent } => trf("updo.downloading", &[file, &percent.to_string()]),
            Busy::SetInstall(Release::Engine(version)) => trf("updo.installing", &[&format!("{} {version}", tr("updo.engine"))]),
            Busy::SetInstall(Release::App(version)) => trf("updo.installing", &[&format!("{} {version}", crate::APP_TITLE)]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_built_from_the_state_at_read_time() {
        let name = |c: Component| format!("<{c:?}>");
        assert_eq!(Busy::Checking.text(name), tr("updm.checking"));
        let install = Busy::Install { what: Component::Engine, version: "3".into() };
        assert_eq!(install.text(name), trf("updm.installing", &["<Engine>", "3"]));
        let download = Busy::Download { what: Component::Native, version: "1".into(), progress: "45 %".into() };
        assert_eq!(download.text(name), trf("updm.downloading", &["<Native>", "1", "45 %"]));
        assert_eq!(Busy::FileDownload { file: "a.dll".into(), percent: 7 }.text(name), trf("updo.downloading", &["a.dll", "7"]));
        assert!(Busy::SetInstall(Release::App("0.9".into())).text(name).contains("0.9"));
    }
}
