//! Модель истории обновлений: `History` — строки, их номера, пределы (`HISTORY_MAX`, `BACKUPS_MAX`), копия «точка
//! отката», чтение и запись `history.json`. Вторая запись в тот же файл — из процесса `--restart-core`, пока ядро
//! стоит (`record_fallback`, `amend_stored`): тот же объект и тот же код, чтобы строки не расходились.

use std::path::{Path, PathBuf};

use super::backup::StoredBackup;
use super::jsonstore::{load_json, save_json};
use super::{Action, Component, HistoryEntry, ORDER};
use crate::i18n::{tr, trf};
use crate::monitor::unix_now;

/// Файл истории в хранилище.
pub(super) const HISTORY: &str = "history.json";
/// Строк истории не больше этого (кроме последнего обновления каждого компонента и его точки отката, см. `trim`);
/// папки копий отброшенных строк удаляются.
pub(super) const HISTORY_MAX: usize = 200;
/// Копий одного компонента не больше этого (новые по номеру строки), кроме копии перед последним обновлением
/// или возвратом.
pub(super) const BACKUPS_MAX: usize = 5;

/// Строки истории и их номера; единственный владелец `history.json` в своём процессе. Новые строки сверху.
pub(super) struct History {
    path: PathBuf,
    entries: Vec<HistoryEntry>,
    /// Следующий номер строки. Растёт только вперёд: номер, выданный `allocate`, не достанется второй раз, даже
    /// пока его строку не записали.
    next: u64,
}

impl History {
    /// Пустая история над файлом хранилища `dir` (файл не читается и не создаётся).
    pub(super) fn empty(dir: &Path) -> History {
        History { path: dir.join(HISTORY), entries: Vec::new(), next: 1 }
    }

    /// История хранилища `dir`. Нет файла — пусто; испорчен — ошибка: потеря истории не должна пройти молча, а
    /// перезапись испорченного файла её закрепила бы (что делать дальше, решает вызывающий).
    pub(super) fn open(dir: &Path) -> Result<History, String> {
        let mut history = History::empty(dir);
        if history.path.exists() {
            history.entries = load_json(&history.path)?;
        }
        history.next = history.entries.iter().map(|e| e.id).max().unwrap_or(0) + 1;
        Ok(history)
    }

    /// Строки, новые сверху.
    pub(super) fn entries(&self) -> &[HistoryEntry] {
        &self.entries
    }

    /// Занять номер строки: пока строка не записана, следующий вызов выдаст уже другой номер.
    pub(super) fn allocate(&mut self) -> u64 {
        let id = self.next;
        self.next += 1;
        id
    }

    /// Новая строка сверху; лишние отбрасывает `trim` в конце работы.
    pub(super) fn push(&mut self, entry: HistoryEntry) {
        self.next = self.next.max(entry.id + 1);
        self.entries.insert(0, entry);
    }

    /// Строка `id` становится ошибкой `error`. Такой строки нет (отброшена пределом или номер чужой) — ошибка с
    /// текстом `error` внутри: вызывающий пишет её в журнал, иначе ошибка работы пропала бы без следа.
    pub(super) fn amend(&mut self, id: u64, error: &str) -> Result<(), String> {
        let Some(e) = self.entries.iter_mut().find(|e| e.id == id) else {
            return Err(trf("updm.amend_unknown", &[&id.to_string(), error]));
        };
        e.finish(Err(error.to_string()));
        Ok(())
    }

    /// Есть строка, у которой копия — папка `name`.
    pub(super) fn references(&self, name: &str) -> bool {
        self.entries.iter().any(|e| e.backup.as_deref() == Some(name))
    }

    /// Номера до `id` включительно больше не выдаются: номер есть в имени папки копии на диске, и новая копия с тем
    /// же компонентом и версией попала бы в чужую папку (а при сбое удалила бы её).
    pub(super) fn reserve_through(&mut self, id: u64) {
        self.next = self.next.max(id.saturating_add(1));
    }

    /// Папка копии `b`, на которую не ссылается ни одна строка, снова становится строкой «Резервная копия» — с
    /// «Вернуть» и под пределами `trim`, как любая копия. Номер — из имени папки (`backup_name`), если свободен: так
    /// строка встаёт на своё место по времени; занят (история начата заново) — новый. Строки остаются упорядочены
    /// по номеру, новые сверху. Возвращает номер строки; на папку уже ссылается строка — `None`.
    pub(super) fn adopt(&mut self, b: StoredBackup) -> Option<u64> {
        if self.references(&b.name) {
            return None;
        }
        let id = match backup_id(&b.name) {
            Some(id) if id > 0 && !self.entries.iter().any(|e| e.id == id) => id,
            _ => self.allocate(),
        };
        self.reserve_through(id);
        let mut row = entry(id, b.info.component, Action::Backup, Some(b.info.version), None).with_backup(b.name, b.size);
        row.at = b.at;
        let at = self.entries.iter().position(|e| e.id < id).unwrap_or(self.entries.len());
        self.entries.insert(at, row);
        Some(id)
    }

    /// Привести прочитанные строки к допустимым сочетаниям полей (см. `HistoryEntry::repair`); по записи на каждую
    /// исправленную строку — для журнала ядра. Формат `history.json` прежний: исправляется только содержимое.
    pub(super) fn repair(&mut self) -> Vec<String> {
        self.entries.iter_mut().flat_map(|e| e.repair()).collect()
    }

    /// Запись через временный файл: оборванная запись не портит прежний `history.json`.
    pub(super) fn save(&self) -> Result<(), String> {
        save_json(&self.path, &self.entries)
    }

    /// Пределы истории: строки сверх `HISTORY_MAX` отбрасываются, у копий сверх `BACKUPS_MAX` снимается `backup`.
    /// Возвращает папки копий для удаления. Последнее обновление или возврат компонента и копия перед ним (точка
    /// отката) остаются строками и за `HISTORY_MAX`: папка без строки стала бы сиротой, которую `adopt` при каждом
    /// открытии возвращал бы снова, а «Вернуть» у последнего обновления пропала бы.
    pub(super) fn trim(&mut self) -> Vec<String> {
        let keep: Vec<u64> = ORDER.iter().flat_map(|c| rollback_rows(&self.entries, *c)).collect();
        let (mut kept, mut dropped) = (Vec::new(), Vec::new());
        for (i, e) in std::mem::take(&mut self.entries).into_iter().enumerate() {
            if i < HISTORY_MAX || keep.contains(&e.id) {
                kept.push(e);
            } else {
                dropped.push(e);
            }
        }
        let mut pruned: Vec<String> =
            dropped.iter().filter_map(|e| e.backup.clone()).filter(|b| !kept.iter().any(|e| e.backup.as_ref() == Some(b))).collect();
        self.entries = kept;
        let excess = excess_backups(&self.entries);
        for e in self.entries.iter_mut() {
            if e.backup.as_ref().is_some_and(|b| excess.contains(b)) {
                e.backup = None;
            }
        }
        pruned.extend(excess);
        pruned
    }

    /// Последняя строка обновления или возврата программы (не копии) к версии `version`: её номер и прежняя версия.
    fn last_app_change_to(&self, version: &str) -> Option<(u64, Option<String>)> {
        self.entries
            .iter()
            .filter(|e| e.component == Component::App && e.action != Action::Backup)
            .max_by_key(|e| e.id)
            .filter(|e| e.to.as_deref() == Some(version))
            .map(|e| (e.id, e.from.clone()))
    }
}

/// Автоматический возврат прежней сборки процессом `--restart-core`. Ядро в это время стоит, поэтому запись идёт
/// прямо в `history.json` хранилища `dir`, тем же форматом: последняя строка обновления или возврата программы
/// к `version` становится ошибкой `failed`; был возврат (`back` — его исход) — сверху строка «Возврат» от
/// `version` к прежней версии. Возвращает номер строки возврата — дописать, если прежняя сборка не запустится.
/// Испорченная история не перезаписывается (ошибка): потеря истории не должна пройти молча.
pub(super) fn record_fallback(dir: &Path, version: &str, failed: &str, back: Option<Result<(), String>>) -> Result<Option<u64>, String> {
    let mut history = History::open(dir)?;
    let previous = history.last_app_change_to(version);
    if let Some((id, _)) = &previous {
        history.amend(*id, failed)?;
    }
    let id = back.map(|result| {
        let from = previous.and_then(|(_, from)| from);
        let mut e = entry(history.allocate(), Component::App, Action::Restore, Some(version.to_string()), from);
        e.finish(result);
        let id = e.id;
        history.push(e);
        id
    });
    history.save()?;
    Ok(id)
}

/// Дописать ошибку в строку `id` истории хранилища `dir` (ядро стоит, см. `record_fallback`).
pub(super) fn amend_stored(dir: &Path, id: u64, error: &str) -> Result<(), String> {
    let mut history = History::open(dir)?;
    history.amend(id, error)?;
    history.save()
}

/// Строка истории хранится так же, как раньше (поля публичны: формат `history.json` и обмен с окном), но
/// собирается и меняется только этими методами, чтобы допустимые сочетания полей не зависели от внимательности
/// вызывающего: исход — `finish`, копия — `with_backup`; прочитанное из файла приводит в порядок `repair`.
impl HistoryEntry {
    /// Исход работы: `ok` и `error` меняются только вместе (успех — без текста ошибки).
    pub(super) fn finish(&mut self, result: Result<(), String>) {
        self.ok = result.is_ok();
        self.error = result.err();
    }

    /// Строка «Резервная копия» с готовой копией `name` размером `size`.
    pub(super) fn with_backup(mut self, name: String, size: u64) -> HistoryEntry {
        self.backup = Some(name);
        self.backup_size = size;
        self
    }

    /// Недопустимые сочетания полей (из файла, правленного вручную, или записанного прежней версией) — исправить.
    /// 1. `ok` при тексте ошибки: ошибка есть — значит, не удалось (так же поступает `History::amend`).
    /// 2. Копия: у строки «Резервная копия» нет «версии после» (`to`), а копия есть только у строки «Резервная копия».
    ///
    /// Возвращает описание каждого исправления.
    fn repair(&mut self) -> Vec<String> {
        let mut fixed = Vec::new();
        let mut note = |why: &str| fixed.push(trf("updm.history_repaired", &[&self.id.to_string(), &tr(why)]));
        if self.ok && self.error.is_some() {
            self.ok = false;
            note("updm.fix_ok_with_error");
        }
        if self.action == Action::Backup && self.to.take().is_some() {
            note("updm.fix_backup_with_to");
        }
        if self.action != Action::Backup && self.backup.take().is_some() {
            self.backup_size = 0;
            note("updm.fix_backup_on_non_backup");
        }
        fixed
    }
}

pub(super) fn entry(id: u64, component: Component, action: Action, from: Option<String>, to: Option<String>) -> HistoryEntry {
    HistoryEntry { id, at: unix_now(), component, action, from, to, backup: None, backup_size: 0, ok: true, error: None, prior_backup: None }
}

/// Копии сверх `BACKUPS_MAX` у каждого компонента — старые по номеру строки, кроме копии перед последним
/// обновлением или возвратом.
fn excess_backups(history: &[HistoryEntry]) -> Vec<String> {
    let mut out = Vec::new();
    for c in ORDER {
        let keep = backup_in_use(history, c);
        let mut list: Vec<(u64, &String)> =
            history.iter().filter(|e| e.component == c).filter_map(|e| e.backup.as_ref().map(|b| (e.id, b))).collect();
        list.sort_by_key(|e| std::cmp::Reverse(e.0));
        out.extend(list.into_iter().skip(BACKUPS_MAX).filter(|(_, b)| Some(b.as_str()) != keep).map(|(_, b)| b.clone()));
    }
    out
}

/// Копия, сделанная перед последним обновлением или возвратом компонента (последняя копия с меньшим номером
/// строки), — точка отката к прежней версии.
fn backup_in_use(history: &[HistoryEntry], c: Component) -> Option<&str> {
    rollback_point(history, c).and_then(|e| e.backup.as_deref())
}

/// Строка копии `backup_in_use`.
fn rollback_point(history: &[HistoryEntry], c: Component) -> Option<&HistoryEntry> {
    let last = last_change(history, c)?;
    history.iter().filter(|e| e.component == c && e.id < last && e.backup.is_some()).max_by_key(|e| e.id)
}

/// Номер последней строки обновления или возврата компонента.
fn last_change(history: &[HistoryEntry], c: Component) -> Option<u64> {
    history.iter().filter(|e| e.component == c && e.action != Action::Backup).map(|e| e.id).max()
}

/// Строки компонента, которые `trim` не отбрасывает и за `HISTORY_MAX`: последнее обновление или возврат и его точка
/// отката.
fn rollback_rows(history: &[HistoryEntry], c: Component) -> Vec<u64> {
    last_change(history, c).into_iter().chain(rollback_point(history, c).map(|e| e.id)).collect()
}

/// Номер строки из имени папки копии (`backup_name`: `<номер>-<компонент>-<версия>`); имя не того вида — `None`.
pub(super) fn backup_id(name: &str) -> Option<u64> {
    name.split('-').next().and_then(|n| n.parse().ok())
}

#[cfg(test)]
pub(super) fn hist(id: u64, c: Component, action: Action, to: Option<&str>, backup: Option<&str>) -> HistoryEntry {
    let mut e = entry(id, c, action, None, to.map(str::to_string));
    e.backup = backup.map(str::to_string);
    e
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::jsonstore::{backup_name, temp};

    #[test]
    fn ids_grow_and_history_is_capped() {
        let mut h = History::empty(Path::new("none"));
        assert_eq!(h.allocate(), 1);
        let mut h = History::empty(Path::new("none"));
        // Старые строки: копия программы перед её обновлением и копия AmneziaWG без обновления; дальше только движок.
        h.push(hist(1, Component::App, Action::Backup, None, Some("1-app-0.1")));
        h.push(hist(2, Component::App, Action::Update, Some("0.2"), None));
        h.push(hist(3, Component::Native, Action::Backup, None, Some("3-native-1")));
        for _ in 0..HISTORY_MAX + 2 {
            let id = h.allocate();
            h.push(hist(id, Component::Engine, Action::Update, None, None));
        }
        let pruned = h.trim();
        // Последнее обновление программы и его точка отката остаются строками и за пределом: иначе папка копии
        // осталась бы без строки (сирота, которую открытие возвращало бы каждый раз), а «Вернуть» у обновления пропала бы.
        assert_eq!(h.entries().len(), HISTORY_MAX + 2);
        let tail: Vec<u64> = h.entries()[HISTORY_MAX..].iter().map(|e| e.id).collect();
        assert_eq!(tail, vec![2, 1]);
        assert_eq!(h.entries()[0].id, (HISTORY_MAX + 5) as u64, "новые сверху");
        assert_eq!(h.allocate(), (HISTORY_MAX + 6) as u64);
        assert_eq!(pruned, vec!["3-native-1".to_string()], "папка отброшенной строки удаляется");
        assert!(!pruned.contains(&"1-app-0.1".to_string()), "копия перед последним обновлением программы — никогда");
        assert!(h.trim().is_empty(), "второй проход ничего не удаляет");
        assert_eq!(h.entries().len(), HISTORY_MAX + 2, "и строк не отбрасывает");
    }

    /// Найденная папка без строки становится строкой «Резервная копия» на месте по номеру из имени; номер занят или
    /// имя не того вида — новый номер; папка, на которую есть строка, не удваивается.
    #[test]
    fn adopted_backup_rows_keep_id_order_and_skip_referenced_folders() {
        let stored = |name: &str, component, version: &str| StoredBackup {
            name: name.into(),
            info: crate::update::backup::BackupInfo { component, version: version.into() },
            at: 77,
            size: 5,
        };
        let mut h = History::empty(Path::new("none"));
        let mut known = hist(3, Component::Engine, Action::Backup, None, Some("3-engine-1"));
        known.from = Some("1".into());
        h.push(known);
        h.push(hist(9, Component::Engine, Action::Update, Some("2"), None));
        assert_eq!(h.adopt(stored("3-engine-1", Component::Engine, "1")), None, "на папку есть строка");
        assert_eq!(h.adopt(stored("5-native-1.0", Component::Native, "1.0")), Some(5));
        assert_eq!(h.adopt(stored("9-app-0.5.1", Component::App, "0.5.1")), Some(10), "номер занят — следующий свободный");
        assert_eq!(h.adopt(stored("copy", Component::App, "0.5.2")), Some(11), "имя без номера — новый номер");
        assert_eq!(h.adopt(stored("5-native-1.0", Component::Native, "1.0")), None, "второй раз не добавляется");
        assert_eq!(h.entries().iter().map(|e| e.id).collect::<Vec<_>>(), vec![11, 10, 9, 5, 3], "по номеру, новые сверху");
        let row = h.entries().iter().find(|e| e.id == 5).unwrap().clone();
        assert_eq!(
            (row.component, row.action, row.from.as_deref(), row.to.as_deref(), row.backup.as_deref(), row.backup_size, row.at, row.ok),
            (Component::Native, Action::Backup, Some("1.0"), None, Some("5-native-1.0"), 5, 77, true)
        );
        assert!(h.repair().is_empty(), "строка собрана как обычная строка копии");
        assert_eq!(h.allocate(), 12);
        h.reserve_through(40);
        assert_eq!(h.allocate(), 41, "номера папок на диске не выдаются");
        assert_eq!(backup_id("34-native-3.1.0"), Some(34));
        assert_eq!(backup_id("native"), None);
    }

    #[test]
    fn allocate_reserves_the_id_until_the_row_is_recorded() {
        let mut h = History::empty(Path::new("none"));
        h.push(hist(4, Component::Engine, Action::Update, None, None));
        let (a, b) = (h.allocate(), h.allocate());
        assert_eq!((a, b), (5, 6), "второй номер не повторяет первый, пока строки нет");
        h.push(hist(b, Component::Engine, Action::Update, None, None));
        assert_eq!(h.allocate(), 7);
        h.push(hist(20, Component::Engine, Action::Update, None, None));
        assert_eq!(h.allocate(), 21, "строка с чужим номером сдвигает счётчик");
    }

    #[test]
    fn backups_over_cap_are_pruned_except_rollback_point() {
        let b = |id: u64, c| {
            let name = backup_name(id, c, "v");
            hist(id, c, Action::Backup, None, Some(&name))
        };
        // Программа: копии 1..=7, последнее обновление — строка 2 (после копии 1); движок: две копии.
        let mut h = History::empty(Path::new("none"));
        for e in [b(1, Component::App), hist(2, Component::App, Action::Update, Some("0.2"), None)] {
            h.push(e);
        }
        for id in 3..=8 {
            h.push(b(id, Component::App));
        }
        h.push(b(9, Component::Engine));
        h.push(b(10, Component::Engine));
        let excess = excess_backups(h.entries());
        assert_eq!(excess, vec![backup_name(3, Component::App, "v")], "старше пяти новых, кроме копии перед обновлением");
        assert_eq!(backup_in_use(h.entries(), Component::App), Some(backup_name(1, Component::App, "v").as_str()));
        assert_eq!(backup_in_use(h.entries(), Component::Engine), None, "обновлений не было");
        let pruned = h.trim();
        assert_eq!(pruned, excess);
        assert_eq!(h.entries().iter().filter(|e| e.backup.is_some()).count(), 8, "у строки удалённой копии «Вернуть» пропадает");
        assert!(h.entries().iter().find(|e| e.id == 3).unwrap().backup.is_none());
        assert!(h.trim().is_empty(), "второй проход ничего не трогает");
    }

    #[test]
    fn open_save_roundtrip_and_next_id_from_file() {
        let dir = temp("hist-roundtrip");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(History::open(&dir).unwrap().entries().is_empty(), "нет файла — пусто");
        let mut h = History::open(&dir).unwrap();
        h.push(hist(7, Component::Engine, Action::Update, Some("2"), None));
        h.push(hist(9, Component::Engine, Action::Update, Some("3"), None));
        h.amend(7, "boom").unwrap();
        h.save().unwrap();
        assert!(!dir.join("history.json.tmp").exists(), "временный файл переименован");
        let mut h = History::open(&dir).unwrap();
        assert_eq!(h.entries().iter().map(|e| e.id).collect::<Vec<_>>(), vec![9, 7], "новые сверху");
        assert_eq!(h.entries()[1].error.as_deref(), Some("boom"));
        assert_eq!(h.allocate(), 10, "номер продолжает файл");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `history.json` прежних версий: допустимые строки и три недопустимых сочетания.
    const OLD_FILE: &str = r#"[
        {"id":5,"at":50,"component":"Engine","action":"Update","from":"1","to":"2","backup":"3-engine-1","backup_size":9,"ok":true,"error":"boom"},
        {"id":4,"at":40,"component":"App","action":"Backup","from":"0.1","to":"0.2","backup":"4-app-0.1","backup_size":7,"ok":true,"error":null},
        {"id":3,"at":30,"component":"Native","action":"Restore","from":"2","to":"1","backup":null,"backup_size":0,"ok":false,"error":"x"},
        {"id":2,"at":20,"component":"Native","action":"Backup","from":"1","to":null,"backup":"2-native-1","backup_size":5,"ok":true,"error":null}
    ]"#;

    #[test]
    fn old_history_file_loads_and_incoherent_rows_are_repaired_with_a_note_each() {
        let dir = temp("hist-old-file");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(HISTORY), OLD_FILE).unwrap();
        let mut h = History::open(&dir).unwrap();
        assert_eq!(h.entries().len(), 4, "прежний формат читается");
        let notes = h.repair();
        assert_eq!(notes.len(), 3, "{notes:?}");
        assert!(notes[0].contains('5') && notes.iter().any(|n| n.contains('4')), "{notes:?}");
        let by = |id| h.entries().iter().find(|e| e.id == id).unwrap().clone();
        let e5 = by(5);
        assert_eq!((e5.ok, e5.error.as_deref(), e5.backup.as_deref(), e5.backup_size), (false, Some("boom"), None, 0), "ошибка важнее ok; копия у строки обновления снята");
        assert_eq!(by(4).to, None, "у копии нет «версии после»");
        let (e3, e2) = (by(3), by(2));
        assert_eq!((e3.ok, e3.to.as_deref()), (false, Some("1")), "допустимые строки не тронуты");
        assert_eq!((e2.backup.as_deref(), e2.backup_size), (Some("2-native-1"), 5));
        assert!(h.repair().is_empty(), "второй проход ничего не меняет");
        // Формат файла прежний: те же ключи, что у строки до изменений.
        h.save().unwrap();
        let text = std::fs::read_to_string(dir.join(HISTORY)).unwrap();
        for key in ["id", "at", "component", "action", "from", "to", "backup", "backup_size", "ok", "error"] {
            assert!(text.contains(&format!("\"{key}\"")), "{key}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn finish_and_with_backup_keep_the_fields_coherent() {
        let mut e = entry(1, Component::App, Action::Update, Some("1".into()), Some("2".into()));
        e.finish(Err("boom".into()));
        assert_eq!((e.ok, e.error.as_deref()), (false, Some("boom")));
        e.finish(Ok(()));
        assert_eq!((e.ok, e.error), (true, None), "успех снимает текст ошибки");
        let b = entry(2, Component::App, Action::Backup, Some("1".into()), None).with_backup("2-app-1".into(), 10);
        assert_eq!((b.backup.as_deref(), b.backup_size, b.to.clone()), (Some("2-app-1"), 10, None));
        let mut coherent = b.clone();
        assert!(coherent.repair().is_empty(), "собранное конструкторами исправлять нечего");
    }

    #[test]
    fn amend_of_unknown_id_is_an_error_not_silence() {
        let mut h = History::empty(Path::new("none"));
        h.push(hist(1, Component::Engine, Action::Update, None, None));
        let e = h.amend(9, "boom").unwrap_err();
        assert!(e.contains('9') && e.contains("boom"), "{e}");
        assert!(h.entries()[0].ok, "чужую строку не трогает");
        let dir = temp("hist-amend-unknown");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(amend_stored(&dir, 9, "x").is_err(), "второй писатель тоже не молчит");
        assert!(!dir.join(HISTORY).exists(), "и пустую историю не создаёт");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_file_is_an_error_and_is_left_alone() {
        let dir = temp("hist-corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(HISTORY), "{broken").unwrap();
        assert!(History::open(&dir).is_err());
        assert!(record_fallback(&dir, "1", "x", None).is_err());
        assert!(amend_stored(&dir, 1, "x").is_err());
        assert_eq!(std::fs::read_to_string(dir.join(HISTORY)).unwrap(), "{broken");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fallback_row_continues_ids_of_the_core_writer() {
        // Ядро записало строку обновления и вышло; второй писатель (`--restart-core`) берёт следующий номер из файла,
        // а потом ядро, открыв файл снова, продолжает после строки возврата, а не поверх неё.
        let dir = temp("hist-fallback");
        std::fs::create_dir_all(&dir).unwrap();
        let mut core = History::open(&dir).unwrap();
        let mut update = hist(core.allocate(), Component::App, Action::Update, Some("0.5"), None);
        update.from = Some("0.4".into());
        core.push(update);
        core.save().unwrap();
        let id = record_fallback(&dir, "0.5", "не запустилась", Some(Ok(()))).unwrap();
        assert_eq!(id, Some(2));
        amend_stored(&dir, 2, "и прежняя тоже").unwrap();
        let mut reopened = History::open(&dir).unwrap();
        let rows = reopened.entries();
        assert_eq!((rows[0].id, rows[0].ok, rows[0].error.as_deref()), (2, false, Some("и прежняя тоже")));
        assert_eq!((rows[0].from.as_deref(), rows[0].to.as_deref()), (Some("0.5"), Some("0.4")));
        assert_eq!((rows[1].id, rows[1].ok, rows[1].error.as_deref()), (1, false, Some("не запустилась")));
        assert_eq!(reopened.allocate(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
