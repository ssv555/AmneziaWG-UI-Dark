//! План замены набора файлов и его доведение после обрыва. Замена (`fileset`) идёт в три фазы: новые файлы
//! кладутся рядом как `<имя>.new-<random>` и сбрасываются на диск, план записывается в журнал (`update::journal`),
//! затем переименованиями с записью на диск прежний файл уходит в `<имя>.old-<random>`, новый встаёт на место.
//! Обрыв (питание, снятый агент) оставляет файлы в одном из состояний плана, и `recover` на следующем старте ядра,
//! агента или установки доводит набор до согласного: вперёд, если каждый новый файл цел (на месте или в `.new-`),
//! иначе назад к прежним. Не вышло ни то, ни другое — уцелевшие прежние файлы сохраняются как `.keep-` (ядро их не
//! удаляет), чтобы следующее обновление или возврат из копии поставили набор заново.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::fileset::keep_name;
use super::fs::Fs;
use crate::events::Severity;
use crate::i18n::trf;
use crate::update::journal::Journal;

/// Имя файла журнала замены в хранилище обновлений.
pub(crate) const SWAP_JOURNAL: &str = "swap.json";

/// Версия записи плана. Журнал пишет одна сборка, а читает подчас другая (ядро новой сборки после обновления программы,
/// прежняя после возврата): поле есть с самого начала, чтобы читатель мог отличить чужую запись, а новые поля
/// добавлялись с `#[serde(default)]` — нечитаемый план оставляет файлы без присмотра (`Recovery::Unreadable`).
pub(crate) const SWAP_PLAN_VERSION: u32 = 1;

/// План одной замены: папка и файлы; записывается до первого переименования.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub(crate) struct SwapPlan {
    /// `SWAP_PLAN_VERSION`; записи без поля (0) — первой версии.
    #[serde(default)]
    pub version: u32,
    pub dir: PathBuf,
    pub files: Vec<SwapFile>,
    /// Файлы, которые замена убирает, ничего не ставя (манифест при возврате к сборке с вшитыми суммами).
    pub remove: Vec<SwapRemove>,
}

/// Заменяемый файл: имя, где лежит новый (`.new-`), куда уходит прежний (`.old-`; `None` — прежнего не было) и
/// сумма нового — по ней `recover` узнаёт, какой файл стоит на месте.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub(crate) struct SwapFile {
    pub name: String,
    pub new: String,
    pub aside: Option<String>,
    pub sha256: String,
    pub size: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub(crate) struct SwapRemove {
    pub name: String,
    pub aside: String,
}

impl SwapPlan {
    /// Имена файлов плана через запятую — для журнала событий.
    fn names(&self) -> String {
        self.files.iter().map(|f| f.name.as_str()).chain(self.remove.iter().map(|r| r.name.as_str())).collect::<Vec<_>>().join(", ")
    }

    /// Файлы рядом с рабочими (`.new-`, `.old-`), которые план ещё держит: их не удаляет уборка ядра.
    pub(crate) fn side_files(&self) -> Vec<String> {
        let files = self.files.iter().flat_map(|f| [Some(&f.new), f.aside.as_ref()]).flatten();
        files.chain(self.remove.iter().map(|r| &r.aside)).cloned().collect()
    }
}

/// Файлы рядом с рабочими, которые держит незавершённый план из `journal`; нечитаемый или отсутствующий — пусто.
pub(crate) fn protected(journal: &Journal<SwapPlan>) -> Vec<String> {
    journal.pending().ok().flatten().map(|p| p.side_files()).unwrap_or_default()
}

/// Исход доведения.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Recovery {
    /// Плана нет.
    Nothing,
    /// Журнал не читается: отодвинут, файлы не тронуты.
    Unreadable,
    /// Набор доведён до нового.
    Forward,
    /// Набор возвращён к прежнему.
    RolledBack,
    /// Ни туда, ни сюда: прежние файлы сохранены как `.keep-`, план снят.
    Kept,
    /// Не удалось даже сохранить прежние: план остаётся, следующий старт пробует снова.
    Stuck,
}

/// Довести незавершённую замену из `journal` (см. описание модуля); исход — в `log`. Повторный вызов на уже
/// согласном наборе ничего не меняет.
pub(crate) fn recover(journal: &Journal<SwapPlan>, fs: &dyn Fs, log: &mut dyn FnMut(Severity, &str)) -> Recovery {
    let plan = match journal.pending() {
        Ok(Some(plan)) => plan,
        Ok(None) => return Recovery::Nothing,
        Err(e) => {
            log(Severity::Bad, &trf("updo.swap_journal_bad", &[&e]));
            return Recovery::Unreadable;
        }
    };
    let names = plan.names();
    let mut notes = Vec::new();
    if forward_feasible(&plan) {
        notes = forward(&plan, fs);
        if notes.is_empty() {
            log(Severity::Warn, &trf("updo.swap_forward", &[&names]));
            return finished(journal, Recovery::Forward, log);
        }
    }
    notes.extend(rollback(&plan, fs));
    if notes.is_empty() {
        log(Severity::Warn, &trf("updo.swap_back", &[&names]));
        return finished(journal, Recovery::RolledBack, log);
    }
    let (kept, errors) = keep(&plan, fs);
    if errors.is_empty() {
        let kept = if kept.is_empty() { "-".to_string() } else { kept.join(", ") };
        log(Severity::Bad, &trf("updo.swap_kept", &[&names, &notes.join("; "), &kept]));
        return finished(journal, Recovery::Kept, log);
    }
    notes.extend(errors);
    log(Severity::Bad, &trf("updo.swap_stuck", &[&names, &notes.join("; "), &journal.path().display().to_string()]));
    Recovery::Stuck
}

/// Снять план после доведения; не снялся — в журнал: следующий старт повторит доведение (оно ничего не меняет).
fn finished(journal: &Journal<SwapPlan>, outcome: Recovery, log: &mut dyn FnMut(Severity, &str)) -> Recovery {
    if let Err(e) = journal.finish() {
        log(Severity::Bad, &e);
    }
    outcome
}

/// На месте `path` стоит файл с суммой и размером записи.
fn matches(path: &Path, f: &SwapFile) -> bool {
    crate::update::sign::file_digest(path).is_ok_and(|(hash, size)| hash == f.sha256 && size == f.size)
}

/// Обычный файл (не папка и не ссылка).
fn regular_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file())
}

/// Вперёд можно, если каждый новый файл цел — уже на месте или в `.new-`.
fn forward_feasible(plan: &SwapPlan) -> bool {
    plan.files.iter().all(|f| matches(&plan.dir.join(&f.name), f) || matches(&plan.dir.join(&f.new), f))
}

fn rename(fs: &dyn Fs, from: &Path, to: &Path) -> Option<String> {
    fs.rename(from, to).err().map(|e| crate::fsutil::io_ctx_move(from, to, e))
}

fn remove(fs: &dyn Fs, path: &Path) -> Option<String> {
    match fs.remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Some(crate::fsutil::io_ctx(path, e)),
        _ => None,
    }
}

/// Оба файла на месте — состояние вне плана (переименования идут по одному): трогать нельзя.
fn both(a: &Path, b: &Path) -> String {
    trf("updo.swap_both", &[&a.display().to_string(), &b.display().to_string()])
}

/// Доделать оставшиеся переименования; ошибки шагов (остальные шаги всё равно делаются).
fn forward(plan: &SwapPlan, fs: &dyn Fs) -> Vec<String> {
    let mut errors = Vec::new();
    for f in &plan.files {
        let (target, new) = (plan.dir.join(&f.name), plan.dir.join(&f.new));
        if matches(&target, f) {
            continue;
        }
        if regular_file(&target) {
            match f.aside.as_ref().map(|a| plan.dir.join(a)) {
                Some(aside) if !aside.exists() => errors.extend(rename(fs, &target, &aside)),
                Some(aside) => {
                    errors.push(both(&target, &aside));
                    continue;
                }
                // Прежнего не было, а на месте нового стоит чужой файл: его не затираем.
                None => {
                    errors.push(both(&target, &new));
                    continue;
                }
            }
        }
        if !target.exists() {
            errors.extend(rename(fs, &new, &target));
        }
    }
    for r in &plan.remove {
        let (target, aside) = (plan.dir.join(&r.name), plan.dir.join(&r.aside));
        if regular_file(&target) {
            if aside.exists() {
                errors.push(both(&target, &aside));
            } else {
                errors.extend(rename(fs, &target, &aside));
            }
        }
    }
    errors
}

/// Вернуть прежние файлы: новый на месте убирается, отодвинутый возвращается, `.new-` удаляется; ошибки шагов.
fn rollback(plan: &SwapPlan, fs: &dyn Fs) -> Vec<String> {
    let mut errors = Vec::new();
    for f in plan.files.iter().rev() {
        let (target, new) = (plan.dir.join(&f.name), plan.dir.join(&f.new));
        if matches(&target, f) {
            errors.extend(remove(fs, &target));
        }
        if let Some(aside) = f.aside.as_ref().map(|a| plan.dir.join(a)) {
            if regular_file(&aside) {
                if target.exists() {
                    errors.push(both(&target, &aside));
                } else {
                    errors.extend(rename(fs, &aside, &target));
                }
            } else if !regular_file(&target) {
                errors.push(trf("updo.rollback_missing", &[&target.display().to_string(), &aside.display().to_string()]));
            }
        }
        if new.exists() {
            errors.extend(remove(fs, &new));
        }
    }
    for r in plan.remove.iter().rev() {
        let (target, aside) = (plan.dir.join(&r.name), plan.dir.join(&r.aside));
        if regular_file(&aside) {
            if target.exists() {
                errors.push(both(&target, &aside));
            } else {
                errors.extend(rename(fs, &aside, &target));
            }
        } else if !regular_file(&target) {
            errors.push(trf("updo.rollback_missing", &[&target.display().to_string(), &aside.display().to_string()]));
        }
    }
    errors
}

/// Уцелевшие прежние файлы — в `.keep-` (ядро их не удаляет), `.new-` убрать; имена сохранённых и ошибки.
fn keep(plan: &SwapPlan, fs: &dyn Fs) -> (Vec<String>, Vec<String>) {
    let (mut kept, mut errors) = (Vec::new(), Vec::new());
    let asides = plan.files.iter().filter_map(|f| f.aside.as_ref().map(|a| (&f.name, a))).chain(plan.remove.iter().map(|r| (&r.name, &r.aside)));
    for (name, aside) in asides {
        let aside = plan.dir.join(aside);
        if !regular_file(&aside) {
            continue;
        }
        let to = keep_name(&plan.dir.join(name));
        match rename(fs, &aside, &to) {
            None => kept.push(to.display().to_string()),
            Some(e) => errors.push(e),
        }
    }
    for f in &plan.files {
        errors.extend(remove(fs, &plan.dir.join(&f.new)));
    }
    (kept, errors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::ours::fs::RealFs;
    use crate::update::ours::testutil::*;

    fn entry(dir: &Path, name: &str, new_data: &str, aside: Option<&str>) -> SwapFile {
        let new = format!("{name}.new-t");
        std::fs::write(dir.join(&new), new_data).unwrap();
        let (sha256, size) = crate::update::sign::file_digest(&dir.join(&new)).unwrap();
        SwapFile { name: name.into(), new, aside: aside.map(str::to_string), sha256, size }
    }

    /// Папка с прежними `a`, `b`, новыми в `.new-` и записанным планом: ни одного переименования ещё не было.
    fn staged(tag: &str) -> (PathBuf, Journal<SwapPlan>, SwapPlan) {
        let base = temp(tag);
        let dir = base.join("inst");
        write_all(&dir, &[("a.dll", "old a"), ("b.dll", "old b"), ("m.json", "old m")]);
        let plan = SwapPlan {
            version: SWAP_PLAN_VERSION,
            dir: dir.clone(),
            files: vec![entry(&dir, "a.dll", "new a", Some("a.dll.old-t")), entry(&dir, "b.dll", "new b", Some("b.dll.old-t")), entry(&dir, "c.dll", "new c", None)],
            remove: vec![SwapRemove { name: "m.json".into(), aside: "m.json.old-t".into() }],
        };
        let journal = Journal::at(base.join("store").join(SWAP_JOURNAL));
        journal.begin(&plan).unwrap();
        (base, journal, plan)
    }

    fn logged() -> (std::rc::Rc<std::cell::RefCell<Vec<(Severity, String)>>>, impl FnMut(Severity, &str)) {
        let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let sink = log.clone();
        (log, move |s: Severity, t: &str| sink.borrow_mut().push((s, t.to_string())))
    }

    const NEW_SET: [(&str, &str); 3] = [("a.dll", "new a"), ("b.dll", "new b"), ("c.dll", "new c")];
    const OLD_SET: [(&str, &str); 3] = [("a.dll", "old a"), ("b.dll", "old b"), ("m.json", "old m")];

    fn assert_set(dir: &Path, set: &[(&str, &str)], what: &str) {
        for (n, d) in set {
            assert_eq!(read(dir, n), *d, "{what}: {n}");
        }
        let all = names(dir);
        assert!(!all.iter().any(|n| n.contains(".new-")), "{what}: staged leftovers {all:?}");
    }

    #[test]
    fn staged_plan_goes_forward_and_a_corrupt_staged_file_sends_it_back() {
        let (base, journal, _) = staged("swap-fwd");
        let dir = base.join("inst");
        let (log, mut sink) = logged();
        assert_eq!(recover(&journal, &RealFs, &mut sink), Recovery::Forward);
        assert_set(&dir, &NEW_SET, "forward");
        assert!(!dir.join("m.json").exists(), "убираемый файл отодвинут");
        assert_eq!(names(&dir).iter().filter(|n| n.contains(".old-")).count(), 3, "{:?}", names(&dir));
        assert!(journal.pending().unwrap().is_none(), "план снят");
        assert_eq!(log.borrow().len(), 1);
        assert!(log.borrow()[0].1.contains("a.dll, b.dll, c.dll, m.json"), "{:?}", log.borrow());
        // Повтор на согласном наборе — ничего.
        assert_eq!(recover(&journal, &RealFs, &mut sink), Recovery::Nothing);
        std::fs::remove_dir_all(&base).unwrap();

        // Новый `b` испорчен (не дописан): вперёд нельзя, прежние остаются, `.new-` убраны.
        let (base, journal, _) = staged("swap-corrupt");
        let dir = base.join("inst");
        std::fs::write(dir.join("b.dll.new-t"), "new").unwrap();
        let (log, mut sink) = logged();
        assert_eq!(recover(&journal, &RealFs, &mut sink), Recovery::RolledBack);
        assert_set(&dir, &OLD_SET, "corrupt");
        assert_eq!(names(&dir), ["a.dll", "b.dll", "m.json"], "ни `.old-`, ни `c.dll`");
        assert!(journal.pending().unwrap().is_none());
        assert_eq!(log.borrow()[0].0, Severity::Warn);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn half_renamed_set_is_finished_forward_from_every_state() {
        // Все промежуточные состояния фазы переименований: после каждого шага — обрыв, затем доведение.
        let (base, _, plan) = staged("swap-steps");
        std::fs::remove_dir_all(&base).unwrap();
        // Переименования фазы по порядку, как их делает `fileset`: прежний в сторону, новый на место, убираемые в конце.
        let mut renames: Vec<(String, String)> = Vec::new();
        for f in &plan.files {
            if let Some(aside) = &f.aside {
                renames.push((f.name.clone(), aside.clone()));
            }
            renames.push((f.new.clone(), f.name.clone()));
        }
        renames.extend(plan.remove.iter().map(|r| (r.name.clone(), r.aside.clone())));
        assert_eq!(renames.len(), 6);
        for stop in 0..=renames.len() {
            let (base, journal, _) = staged("swap-steps");
            let dir = base.join("inst");
            for (from, to) in &renames[..stop] {
                std::fs::rename(dir.join(from), dir.join(to)).unwrap();
            }
            let (_, mut sink) = logged();
            let outcome = recover(&journal, &RealFs, &mut sink);
            assert!(outcome == Recovery::Forward, "stop {stop}: {outcome:?}");
            assert_set(&dir, &NEW_SET, &format!("stop {stop}"));
            assert!(!dir.join("m.json").exists(), "stop {stop}");
            assert!(journal.pending().unwrap().is_none(), "stop {stop}");
            std::fs::remove_dir_all(&base).unwrap();
        }
    }

    #[test]
    fn lost_new_file_rolls_back_and_lost_both_keeps_the_rest() {
        // Новый `c` пропал после части переименований: назад, прежние на месте.
        let (base, journal, _) = staged("swap-back");
        let dir = base.join("inst");
        std::fs::rename(dir.join("a.dll"), dir.join("a.dll.old-t")).unwrap();
        std::fs::rename(dir.join("a.dll.new-t"), dir.join("a.dll")).unwrap();
        std::fs::rename(dir.join("b.dll"), dir.join("b.dll.old-t")).unwrap();
        std::fs::remove_file(dir.join("c.dll.new-t")).unwrap();
        let (log, mut sink) = logged();
        assert_eq!(recover(&journal, &RealFs, &mut sink), Recovery::RolledBack);
        assert_set(&dir, &OLD_SET, "back");
        assert_eq!(names(&dir), ["a.dll", "b.dll", "m.json"]);
        assert_eq!(log.borrow().len(), 1, "{:?}", log.borrow());
        std::fs::remove_dir_all(&base).unwrap();

        // Ни вперёд (новый `c` пропал), ни назад (прежний `b` пропал, прежний `a` не встаёт на место — занят):
        // уцелевший прежний `a` — в `.keep-`, план снят, следующее обновление поставит набор заново.
        let (base, journal, _) = staged("swap-keep");
        let dir = base.join("inst");
        std::fs::rename(dir.join("a.dll"), dir.join("a.dll.old-t")).unwrap();
        std::fs::rename(dir.join("a.dll.new-t"), dir.join("a.dll")).unwrap();
        std::fs::rename(dir.join("b.dll"), dir.join("b.dll.old-t")).unwrap();
        std::fs::remove_file(dir.join("b.dll.old-t")).unwrap();
        std::fs::remove_file(dir.join("c.dll.new-t")).unwrap();
        let busy = FaultFs::new(|op, from, to| op == Op::Rename && from.file_name().is_some_and(|n| n == "a.dll.old-t") && to.file_name().is_some_and(|n| n == "a.dll"));
        let (log, mut sink) = logged();
        assert_eq!(recover(&journal, &busy, &mut sink), Recovery::Kept);
        let all = names(&dir);
        let keep = all.iter().find(|n| n.starts_with("a.dll.keep-")).unwrap_or_else(|| panic!("{all:?}"));
        assert_eq!(read(&dir, keep), "old a");
        assert!(!all.iter().any(|n| n.contains(".old-") || n.contains(".new-")), "{all:?}");
        assert_eq!(read(&dir, "m.json"), "old m", "неубранный файл не тронут");
        assert!(journal.pending().unwrap().is_none());
        let (sev, text) = log.borrow()[0].clone();
        assert_eq!(sev, Severity::Bad);
        assert!(text.contains(keep.as_str()) && text.contains("b.dll") && text.contains("injected"), "{text}");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn stuck_plan_stays_and_protects_its_files() {
        // Вперёд нельзя (новый `c` пропал), назад нельзя (прежний `b` пропал, прежний `a` занят), и даже сохранить
        // прежний `a` в `.keep-` не даёт файловая система: план остаётся, его файлы под защитой от уборки.
        let (base, journal, plan) = staged("swap-stuck");
        let dir = base.join("inst");
        std::fs::rename(dir.join("a.dll"), dir.join("a.dll.old-t")).unwrap();
        std::fs::rename(dir.join("a.dll.new-t"), dir.join("a.dll")).unwrap();
        std::fs::remove_file(dir.join("c.dll.new-t")).unwrap();
        std::fs::rename(dir.join("b.dll"), dir.join("b.dll.old-t")).unwrap();
        std::fs::remove_file(dir.join("b.dll.old-t")).unwrap();
        let fs = FaultFs::new(|op, from, to| {
            op == Op::Rename && from.file_name().is_some_and(|n| n == "a.dll.old-t") && to.file_name().is_some_and(|n| n == "a.dll" || n.to_string_lossy().contains(".keep-"))
        });
        let (log, mut sink) = logged();
        assert_eq!(recover(&journal, &fs, &mut sink), Recovery::Stuck);
        assert_eq!(journal.pending().unwrap(), Some(plan.clone()), "план остаётся до следующего старта");
        assert_eq!(read(&dir, "a.dll.old-t"), "old a", "прежний файл цел под защитой плана");
        let text = &log.borrow()[0].1;
        assert!(text.contains(SWAP_JOURNAL) && text.contains("injected"), "{text}");
        let mut side = protected(&journal);
        side.sort();
        assert_eq!(side, ["a.dll.new-t", "a.dll.old-t", "b.dll.new-t", "b.dll.old-t", "c.dll.new-t", "m.json.old-t"]);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn unreadable_journal_is_reported_and_files_untouched() {
        let base = temp("swap-unreadable");
        let dir = base.join("inst");
        write_all(&dir, &[("a.dll", "old a"), ("a.dll.old-t", "x")]);
        let journal: Journal<SwapPlan> = Journal::at(base.join("store").join(SWAP_JOURNAL));
        std::fs::create_dir_all(base.join("store")).unwrap();
        std::fs::write(journal.path(), "nonsense").unwrap();
        let (log, mut sink) = logged();
        assert_eq!(recover(&journal, &RealFs, &mut sink), Recovery::Unreadable);
        assert_eq!(names(&dir), ["a.dll", "a.dll.old-t"]);
        assert_eq!(log.borrow()[0].0, Severity::Bad);
        // Журнал отодвинут — защищать уборке нечего, поэтому уборка в этот старт не идёт вовсе: `.old-` может быть
        // единственной целой копией (замена встала между «прежний отодвинут» и «новый на месте»).
        assert!(protected(&journal).is_empty());
        crate::daemon::install::remove_old_copies_after(&dir, Recovery::Unreadable);
        assert_eq!(names(&dir), ["a.dll", "a.dll.old-t"], "уборка после нечитаемого плана не трогает `.old-`");
        assert_eq!(recover(&journal, &RealFs, &mut sink), Recovery::Nothing);
        crate::daemon::install::remove_old_copies_after(&dir, Recovery::Nothing);
        assert_eq!(names(&dir), ["a.dll"], "следующий старт без плана убирает");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// Запись без поля `version` (первая сборка с журналом) читается как первая версия; новая запись несёт версию.
    #[test]
    fn plan_version_defaults_to_the_first_one() {
        let text = r#"{"dir":"C:\\x","files":[],"remove":[]}"#;
        let plan: SwapPlan = serde_json::from_str(text).unwrap();
        assert_eq!(plan.version, 0);
        let written = SwapPlan { version: SWAP_PLAN_VERSION, dir: PathBuf::from("x"), files: vec![], remove: vec![] };
        let back: SwapPlan = serde_json::from_str(&serde_json::to_string(&written).unwrap()).unwrap();
        assert_eq!(back.version, SWAP_PLAN_VERSION);
    }
}
