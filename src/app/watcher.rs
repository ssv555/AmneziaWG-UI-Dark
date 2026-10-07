//! Файлы-источники туннелей, открытые на правку, и кэш сведений о конфигах: что изменилось на диске — окну знать,
//! чтобы спросить про импорт и перечитать сведения.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use crate::conf::TunnelInfo;
use crate::crash::lock;
use crate::i18n::trf;

/// Как часто смотреть на время изменения файлов, открытых на правку.
const POLL_EVERY: Duration = Duration::from_secs(1);

/// Сведения о конфигах неподключённых туннелей: (сведения, откуда). Только открытые данные. Общие с фоновыми потоками.
pub(super) type Infos = Mutex<BTreeMap<String, (TunnelInfo, String)>>;
/// Туннели, сведения которых сейчас запрашиваются.
pub(super) type Loading = Mutex<BTreeSet<String>>;

/// Файл-источник, открытый во внешнем редакторе.
struct Watch {
    tunnel: String,
    path: PathBuf,
    modified: Option<SystemTime>,
}

/// Файл, который сохранили в редакторе после того, как его открыли на правку.
pub(super) struct Saved {
    pub(super) tunnel: String,
    pub(super) path: PathBuf,
}

/// Наблюдатель за файлами-источниками. Владеет списком открытых на правку, временем изменения разобранных файлов
/// и кэшем сведений, который делит с потоками, читающими конфиги у ядра.
#[derive(Default)]
pub(super) struct SourceWatcher {
    watched: Vec<Watch>,
    /// Когда файлы смотрели в последний раз.
    checked: Option<Instant>,
    /// Время изменения разобранного файла-источника — чтобы перечитывать его после правки.
    seen: BTreeMap<String, Option<SystemTime>>,
    infos: Arc<Infos>,
    loading: Arc<Loading>,
}

/// `None` и для файла, которого нет: отметка нужна только для сравнения, а причину сбоя чтения даст сам `read_to_string`.
fn modified_of(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

impl SourceWatcher {
    /// Файл открыт на правку: следить, пока его не сохранят. Повторное открытие того же файла начинает наблюдение заново.
    pub(super) fn watch(&mut self, tunnel: String, path: PathBuf) {
        let modified = modified_of(&path);
        self.watched.retain(|w| w.path != path);
        self.watched.push(Watch { tunnel, path, modified });
    }

    /// Раз в секунду: какие из открытых на правку файлов изменились. `None` — смотреть ещё рано или нечего;
    /// `Some` — проверка была (даже с пустым итогом), и сохранённые файлы названы по одному разу.
    pub(super) fn poll(&mut self, now: Instant) -> Option<Vec<Saved>> {
        if self.watched.is_empty() || self.checked.is_some_and(|t| now.duration_since(t) < POLL_EVERY) {
            return None;
        }
        self.checked = Some(now);
        let mut saved = Vec::new();
        for w in &mut self.watched {
            let modified = modified_of(&w.path);
            if modified.is_some() && modified != w.modified {
                w.modified = modified;
                saved.push(Saved { tunnel: w.tunnel.clone(), path: w.path.clone() });
            }
        }
        Some(saved)
    }

    /// Туннель с источником выбран: разобрать файл заново, если он изменился с прошлого раза. Файл не читается —
    /// ошибка возвращается один раз на это время изменения (отметка запоминается и при сбое), кадры её не повторяют.
    pub(super) fn refresh_source(&mut self, tunnel: &str, path: &Path) -> std::io::Result<()> {
        let modified = modified_of(path);
        if self.seen.get(tunnel) == Some(&modified) {
            return Ok(());
        }
        self.seen.insert(tunnel.to_string(), modified);
        let text = std::fs::read_to_string(path)?;
        let origin = trf("det.from_source", &[&path.display().to_string()]);
        lock(&self.infos).insert(tunnel.to_string(), (crate::conf::parse(&text), origin));
        Ok(())
    }

    /// Туннель удалён: сведения, наблюдение и отметка о файле не нужны.
    pub(super) fn forget(&mut self, tunnel: &str) {
        lock(&self.infos).remove(tunnel);
        self.seen.remove(tunnel);
        self.watched.retain(|w| w.tunnel != tunnel);
    }

    /// Туннель переименован: сведения устарели, наблюдение идёт за новым именем.
    pub(super) fn rename(&mut self, old: &str, new: &str) {
        lock(&self.infos).remove(old);
        self.seen.remove(old);
        for w in self.watched.iter_mut().filter(|w| w.tunnel == old) {
            w.tunnel = new.to_string();
        }
    }

    /// Режим сменился — сведения прежнего режима не годятся.
    pub(super) fn clear_infos(&self) {
        lock(&self.infos).clear();
    }

    pub(super) fn has_info(&self, tunnel: &str) -> bool {
        lock(&self.infos).contains_key(tunnel)
    }

    /// Занять запрос сведений: `false` — этот туннель уже запрашивается.
    pub(super) fn begin_load(&self, tunnel: &str) -> bool {
        lock(&self.loading).insert(tunnel.to_string())
    }

    pub(super) fn infos_snapshot(&self) -> BTreeMap<String, (TunnelInfo, String)> {
        lock(&self.infos).clone()
    }

    pub(super) fn loading_snapshot(&self) -> BTreeSet<String> {
        lock(&self.loading).clone()
    }

    /// Ручки для фонового потока: он кладёт сведения и снимает пометку запроса.
    pub(super) fn infos_handle(&self) -> Arc<Infos> {
        self.infos.clone()
    }

    pub(super) fn loading_handle(&self) -> Arc<Loading> {
        self.loading.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("awg-ui-watcher-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Записать файл и выставить время изменения: файловые системы с грубой отметкой времени не отличили бы две записи подряд.
    fn write_at(path: &Path, text: &str, secs: u64) {
        std::fs::write(path, text).unwrap();
        let file = std::fs::File::options().write(true).open(path).unwrap();
        file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs)).unwrap();
    }

    #[test]
    fn a_saved_file_is_reported_once_and_only_after_the_poll_interval() {
        let dir = temp_dir("poll");
        let path = dir.join("office.conf");
        write_at(&path, "[Interface]\n", 1000);
        let mut w = SourceWatcher::default();
        w.watch("office".into(), path.clone());
        let t0 = Instant::now();
        // Файл не менялся с открытия: проверка была, сообщать нечего.
        assert!(w.poll(t0).unwrap().is_empty());
        write_at(&path, "[Interface]\nMTU = 1280\n", 2000);
        // Слишком рано — файл не смотрят.
        assert!(w.poll(t0 + POLL_EVERY / 2).is_none());
        let saved = w.poll(t0 + POLL_EVERY).unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!((saved[0].tunnel.as_str(), &saved[0].path), ("office", &path));
        // Та же правка во второй раз не сообщается.
        assert!(w.poll(t0 + POLL_EVERY * 2).unwrap().is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_vanished_file_is_not_a_save_and_nothing_watched_is_not_polled() {
        let dir = temp_dir("gone");
        let path = dir.join("office.conf");
        write_at(&path, "x", 1000);
        let mut w = SourceWatcher::default();
        let t0 = Instant::now();
        assert!(w.poll(t0).is_none());
        w.watch("office".into(), path.clone());
        std::fs::remove_file(&path).unwrap();
        // Редактор пересоздаёт файл при сохранении: пропажа — не «сохранили», дождёмся появления.
        assert!(w.poll(t0).unwrap().is_empty());
        write_at(&path, "y", 3000);
        assert_eq!(w.poll(t0 + POLL_EVERY * 2).unwrap().len(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rename_and_forget_redirect_or_drop_the_watch() {
        let dir = temp_dir("rename");
        let path = dir.join("office.conf");
        write_at(&path, "x", 1000);
        let mut w = SourceWatcher::default();
        w.watch("office".into(), path.clone());
        w.rename("office", "hq");
        write_at(&path, "y", 2000);
        let t0 = Instant::now();
        let saved = w.poll(t0).unwrap();
        assert_eq!(saved[0].tunnel, "hq");
        w.forget("hq");
        assert!(w.poll(t0 + POLL_EVERY * 2).is_none(), "наблюдать больше не за чем");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn source_info_is_parsed_again_only_after_the_file_changed() {
        let dir = temp_dir("info");
        let path = dir.join("office.conf");
        write_at(&path, "[Interface]\nAddress = 10.0.0.2/32\n", 1000);
        let mut w = SourceWatcher::default();
        assert!(!w.has_info("office"));
        w.refresh_source("office", &path).unwrap();
        assert!(w.has_info("office"));
        // Сведения сброшены, файл тот же — заново не разбирается.
        w.infos_handle().lock().unwrap().remove("office");
        w.refresh_source("office", &path).unwrap();
        assert!(!w.has_info("office"));
        // Файл изменился — разобран снова.
        write_at(&path, "[Interface]\nAddress = 10.0.0.3/32\n", 2000);
        w.refresh_source("office", &path).unwrap();
        assert!(w.has_info("office"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unreadable_source_is_reported_once_per_modification() {
        let dir = temp_dir("unreadable");
        let path = dir.join("gone.conf");
        let mut w = SourceWatcher::default();
        let first = w.refresh_source("office", &path).unwrap_err();
        assert_eq!(first.kind(), std::io::ErrorKind::NotFound);
        assert!(!w.has_info("office"));
        w.refresh_source("office", &path).unwrap();
        // Появился и читается — сведения разобраны.
        write_at(&path, "[Interface]\nAddress = 10.0.0.2/32\n", 1000);
        w.refresh_source("office", &path).unwrap();
        assert!(w.has_info("office"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn loading_is_claimed_once_per_tunnel() {
        let w = SourceWatcher::default();
        assert!(w.begin_load("office"));
        assert!(!w.begin_load("office"), "запрос уже идёт");
        assert!(w.loading_snapshot().contains("office"));
        w.loading_handle().lock().unwrap().remove("office");
        assert!(w.begin_load("office"));
    }
}
