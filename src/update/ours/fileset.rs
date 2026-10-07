//! Транзакция над набором файлов: новые файлы сначала кладутся рядом (`<имя>.new-<random>`, сброшены на диск), план
//! пишется в журнал (`swap`), затем каждый заменяемый файл отодвигается (`<имя>.old-<random>`) и новый встаёт на его
//! место переименованиями с записью на диск. Ошибка — всё поставленное убирается, отодвинутое возвращается (не
//! вернулось — `<имя>.keep-<random>`), план снимается. Обрыв посреди — план остаётся, и следующий старт доводит набор
//! (`swap::recover`). Поверх неё — установка сборки программы с выкладкой окну и перезапуском ядра.

use std::path::{Path, PathBuf};

use windows_sys::Win32::System::Threading::{CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS};

use super::fs::{Fs, RealFs};
use super::swap::{SwapFile, SwapPlan, SwapRemove, SWAP_JOURNAL, SWAP_PLAN_VERSION};
use super::{InstallTarget, APP_EXE, RESTART_FLAG};
use crate::i18n::trf;
use crate::update::journal::Journal;

impl InstallTarget {
    /// Поставить набор сборки (`remove` — убрать), выложить его окну и перезапустить ядро (через 2 с — успеть
    /// записать историю).
    #[allow(dead_code)]
    pub(super) fn install_app(&self, files: &[(String, PathBuf)], remove: &[&str], version: &str) -> Result<(), String> {
        self.install_app_with(&RealFs, files, remove, || self.publish_for_window(files, version), spawn_restart)
    }

    /// Шаги установки сборки в папку программы. Не удалось выложить окну или запустить перезапуск — набор
    /// откатывается: ядро остаётся на прежней сборке. Перезапуск получает новый exe и весь заменённый набор
    /// (`restart_set`) — путь возврата, если новое ядро не поднимется.
    pub(super) fn install_app_with(
        &self,
        fs: &dyn Fs,
        files: &[(String, PathBuf)],
        remove: &[&str],
        publish: impl FnOnce() -> Result<(), String>,
        restart: impl FnOnce(&Path, &[String]) -> Result<(), String>,
    ) -> Result<(), String> {
        let set = put_set(&self.dir, self.swap_journal(), files, remove, fs)?;
        let exe = self.dir.join(APP_EXE);
        match publish().and_then(|()| restart(&exe, &set.restart_args())) {
            Ok(()) => set.finish(),
            Err(e) => Err(with_rollback(e, set.undo(fs))),
        }
    }

    /// Поставить файлы `(имя, источник)` в папку программы, каждый — отодвинув текущий. Ошибка на любом — всё
    /// поставленное убирается, отодвинутое возвращается на место.
    pub(super) fn install_set(&self, files: &[(String, PathBuf)], fs: &dyn Fs) -> Result<(), String> {
        put_set(&self.dir, self.swap_journal(), files, &[], fs)?.finish()
    }

    /// Журнал замены набора файлов в хранилище обновлений.
    pub(super) fn swap_journal(&self) -> Journal<SwapPlan> {
        Journal::at(self.store.join(SWAP_JOURNAL))
    }
}

/// Как запускается помощник перезапуска: без консоли и группы запустившего и вне его Job object. Агент живёт в задании
/// ядра с `KILL_ON_JOB_CLOSE`: без `CREATE_BREAKAWAY_FROM_JOB` помощник погиб бы вместе с ядром, которое он
/// останавливает (задание выход разрешает — `agent_watch`). Процессу вне задания (ядру) флаг ничего не меняет.
const RESTART_FLAGS: u32 = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB;

/// Запустить `exe --restart-core <заменённый набор>` отдельным процессом.
fn spawn_restart(exe: &Path, set: &[String]) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    std::process::Command::new(exe)
        .arg(RESTART_FLAG)
        .args(set)
        .creation_flags(RESTART_FLAGS)
        .spawn()
        .map(drop)
        .map_err(|e| format!("{RESTART_FLAG}: {e}"))
}

/// Поставить набор (см. описание модуля); поставленное можно откатить и после успеха (`Installed::undo`), а
/// завершить — `finish`. Незавершённый план прошлой замены — отказ: его доводит старт ядра или агента.
fn put_set(dir: &Path, journal: Journal<SwapPlan>, files: &[(String, PathBuf)], remove: &[&str], fs: &dyn Fs) -> Result<Installed, String> {
    if journal.pending()?.is_some() {
        return Err(trf("updo.swap_pending", &[&journal.path().display().to_string()]));
    }
    let plan = stage(dir, files, remove, fs)?;
    let mut set = Installed { plan, journal, done: Vec::new() };
    if let Err(e) = set.journal.begin(&set.plan) {
        return Err(with_rollback(e, set.undo(fs)));
    }
    if let Err(e) = set.place(fs) {
        return Err(with_rollback(e, set.undo(fs)));
    }
    Ok(set)
}

/// Фаза 1: копии новых файлов рядом с рабочими (`.new-`), сброшенные на диск, и план с их суммами. Копия не
/// удалась — положенные убираются, рабочие файлы не тронуты.
fn stage(dir: &Path, files: &[(String, PathBuf)], remove: &[&str], fs: &dyn Fs) -> Result<SwapPlan, String> {
    let mut plan = SwapPlan { version: SWAP_PLAN_VERSION, dir: dir.to_path_buf(), files: Vec::new(), remove: Vec::new() };
    for (name, src) in files {
        let path = dir.join(name);
        let new = new_name(&path);
        let staged = fs.copy(src, &new).map_err(|e| crate::fsutil::io_ctx_move(src, &new, e)).and_then(|_| crate::update::sign::file_digest(&new));
        let (sha256, size) = match staged {
            Ok(digest) => digest,
            Err(e) => return Err(with_rollback(e, remove_staged(&plan, fs))),
        };
        let aside = path.exists().then(|| file_name(&aside_name(&path)));
        plan.files.push(SwapFile { name: name.clone(), new: file_name(&new), aside, sha256, size });
    }
    plan.remove = remove.iter().map(|n| dir.join(n)).filter(|p| p.exists()).map(|p| SwapRemove { name: file_name(&p), aside: file_name(&aside_name(&p)) }).collect();
    Ok(plan)
}

/// Убрать все `.new-` плана; ошибки вместе.
fn remove_staged(plan: &SwapPlan, fs: &dyn Fs) -> Result<(), String> {
    let errors: Vec<String> = plan
        .files
        .iter()
        .map(|f| plan.dir.join(&f.new))
        .filter(|p| p.exists())
        .filter_map(|p| fs.remove_file(&p).err().map(|e| crate::fsutil::io_ctx(&p, e)))
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

fn file_name(path: &Path) -> String {
    path.file_name().unwrap_or_default().to_string_lossy().into_owned()
}

/// Ошибка шага вместе с ошибкой отката, если откат тоже не удался.
fn with_rollback(e: String, undo: Result<(), String>) -> String {
    match undo {
        Ok(()) => e,
        Err(u) => format!("{e}; {u}"),
    }
}

/// Сделанный шаг фазы переименований: индекс в `plan.files` или `plan.remove`.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Step {
    /// Прежний файл отодвинут.
    Aside(usize),
    /// Новый файл встал на место.
    Placed(usize),
    /// Убираемый файл отодвинут.
    Removed(usize),
}

/// Поставленный набор: план, его журнал и сделанные шаги переименований (для отката в обратном порядке).
struct Installed {
    plan: SwapPlan,
    journal: Journal<SwapPlan>,
    done: Vec<Step>,
}

impl Installed {
    /// Фаза 2: переименования по плану; первая ошибка — наружу, сделанные шаги остаются в `done` для `undo`.
    fn place(&mut self, fs: &dyn Fs) -> Result<(), String> {
        let dir = self.plan.dir.clone();
        for (i, f) in self.plan.files.iter().enumerate() {
            let path = dir.join(&f.name);
            if let Some(aside) = &f.aside {
                fs.rename(&path, &dir.join(aside)).map_err(|e| crate::fsutil::io_ctx(&path, e))?;
                self.done.push(Step::Aside(i));
            }
            let new = dir.join(&f.new);
            fs.rename(&new, &path).map_err(|e| crate::fsutil::io_ctx_move(&new, &path, e))?;
            self.done.push(Step::Placed(i));
        }
        for (j, r) in self.plan.remove.iter().enumerate() {
            let path = dir.join(&r.name);
            fs.rename(&path, &dir.join(&r.aside)).map_err(|e| crate::fsutil::io_ctx(&path, e))?;
            self.done.push(Step::Removed(j));
        }
        Ok(())
    }

    /// Аргументы перезапуска (`restart_set`): по файлу — имя отодвинутого прежнего или, если прежнего не было,
    /// голое имя поставленного.
    fn restart_args(&self) -> Vec<String> {
        let files = self.plan.files.iter().map(|f| f.aside.clone().unwrap_or_else(|| f.name.clone()));
        files.chain(self.plan.remove.iter().map(|r| r.aside.clone())).collect()
    }

    /// Набор стоит: снять план.
    fn finish(self) -> Result<(), String> {
        self.journal.finish()
    }

    /// Убрать поставленное и вернуть отодвинутое, в обратном порядке; не положенные `.new-` убрать; план снять.
    /// Ошибки всех шагов — вместе.
    fn undo(self, fs: &dyn Fs) -> Result<(), String> {
        let dir = &self.plan.dir;
        let mut errors = Vec::new();
        for step in self.done.iter().rev() {
            let result = match *step {
                Step::Placed(i) => match fs.remove_file(&dir.join(&self.plan.files[i].name)) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(crate::fsutil::io_ctx(dir.join(&self.plan.files[i].name), e)),
                    _ => Ok(()),
                },
                Step::Aside(i) => {
                    let f = &self.plan.files[i];
                    put_back(&dir.join(&f.name), &dir.join(f.aside.as_deref().unwrap_or_default()), fs)
                }
                Step::Removed(j) => put_back(&dir.join(&self.plan.remove[j].name), &dir.join(&self.plan.remove[j].aside), fs),
            };
            errors.extend(result.err());
        }
        errors.extend(remove_staged(&self.plan, fs).err());
        errors.extend(self.journal.finish().err());
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}

/// Вернуть отодвинутый `aside` на место `path`. Не вернулся — прежний файл переименовывается в `<имя>.keep-<random>`
/// (такие ядро не удаляет), ошибка называет его: под именем `.old-` его удалило бы ядро при старте.
fn put_back(path: &Path, aside: &Path, fs: &dyn Fs) -> Result<(), String> {
    let Err(e) = fs.rename(aside, path) else { return Ok(()) };
    let (shown, e) = (path.display().to_string(), e.to_string());
    let keep = keep_name(path);
    match fs.rename(aside, &keep) {
        Ok(()) => Err(trf("updo.rollback_kept", &[&shown, &e, &keep.display().to_string()])),
        Err(e2) => Err(trf("updo.rollback_lost", &[&shown, &e, &aside.display().to_string(), &e2.to_string()])),
    }
}

/// `<имя>.old-<random>` рядом с `path`; такие файлы удаляет ядро, когда новая версия готова.
pub(super) fn aside_name(path: &Path) -> PathBuf {
    suffixed(path, "old")
}

/// `<имя>.keep-<random>` рядом с `path`: прежний файл, который не удалось вернуть; ядро его не трогает.
pub(super) fn keep_name(path: &Path) -> PathBuf {
    suffixed(path, "keep")
}

/// `<имя>.new-<random>` рядом с `path`: новый файл до того, как встал на место; без плана такие удаляет ядро.
fn new_name(path: &Path) -> PathBuf {
    suffixed(path, "new")
}

fn suffixed(path: &Path, tag: &str) -> PathBuf {
    path.with_file_name(format!("{}.{tag}-{}", path.file_name().unwrap_or_default().to_string_lossy(), crate::store::random_hex()))
}

/// Скопировать файл (папка назначения создаётся); размер.
pub(super) fn copy(from: &Path, to: &Path) -> Result<u64, String> {
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir).map_err(|e| crate::fsutil::io_ctx(&dir, e))?;
    }
    std::fs::copy(from, to).map_err(|e| crate::fsutil::io_ctx_move(&from, &to, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::ours::fallback::restart_set;
    use crate::update::ours::swap::{recover, Recovery};
    use crate::update::ours::testutil::*;
    use crate::update::ours::{MANIFEST, MANIFEST_SIG};

    /// Помощник перезапуска, запущенный агентом, выходит из задания ядра: иначе остановка ядра убила бы и его.
    #[test]
    fn restart_helper_breaks_away_from_the_job() {
        assert_eq!(RESTART_FLAGS & CREATE_BREAKAWAY_FROM_JOB, CREATE_BREAKAWAY_FROM_JOB);
        assert_eq!(RESTART_FLAGS & DETACHED_PROCESS, DETACHED_PROCESS, "без консоли запустившего");
    }

    #[test]
    fn install_set_replaces_and_rolls_back() {
        let dir = temp("install");
        let (dst, src) = (dir.join("dst"), dir.join("src"));
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(dst.join("a.dll"), "old a").unwrap();
        std::fs::write(dst.join("b.dll"), "old b").unwrap();
        std::fs::write(src.join("a.dll"), "new a").unwrap();
        // Второго источника нет — ошибка, первый файл возвращается, отодвинутых не остаётся.
        let files = vec![("a.dll".to_string(), src.join("a.dll")), ("b.dll".to_string(), src.join("b.dll"))];
        let t = target(&dst);
        assert!(t.install_set(&files, &RealFs).is_err());
        assert_eq!(std::fs::read_to_string(dst.join("a.dll")).unwrap(), "old a");
        assert_eq!(std::fs::read_to_string(dst.join("b.dll")).unwrap(), "old b");
        assert_eq!(std::fs::read_dir(&dst).unwrap().count(), 2);

        std::fs::write(src.join("b.dll"), "new b").unwrap();
        t.install_set(&files, &RealFs).unwrap();
        assert_eq!(std::fs::read_to_string(dst.join("a.dll")).unwrap(), "new a");
        assert_eq!(std::fs::read_to_string(dst.join("b.dll")).unwrap(), "new b");
        let aside = std::fs::read_dir(&dst).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().contains(".old-")).count();
        assert_eq!(aside, 2);
        assert!(!names(&dst).iter().any(|n| n.contains(".new-")), "{:?}", names(&dst));
        assert!(t.swap_journal().pending().unwrap().is_none(), "план снят после успеха");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn app_install_rolls_back_when_publish_or_restart_fails() {
        let dir = temp("rollback");
        let (inst, src) = (dir.join("inst"), dir.join("src"));
        let old = [(APP_EXE, "exe1"), ("tunnel.dll", "t1"), ("wintun.dll", "w1"), (MANIFEST, "m1"), (MANIFEST_SIG, "s1")];
        write_all(&inst, &old);
        write_all(&src, &[(APP_EXE, "exe2"), ("tunnel.dll", "t2"), ("wintun.dll", "w2")]);
        let files: Vec<(String, PathBuf)> = [APP_EXE, "tunnel.dll", "wintun.dll"].iter().map(|n| (n.to_string(), src.join(n))).collect();
        let remove = [MANIFEST, MANIFEST_SIG];
        let before = names(&inst);
        let unchanged = |what: &str| {
            for (n, d) in old {
                assert_eq!(read(&inst, n), d, "{what}: {n}");
            }
            assert_eq!(names(&inst), before, "{what}: отодвинутых не остаётся");
            assert!(target(&inst).swap_journal().pending().unwrap().is_none(), "{what}: план снят");
        };
        let e = target(&inst).install_app_with(&RealFs, &files, &remove, || Err("publish".into()), |_, _| panic!("no restart after failed publish"));
        assert_eq!(e.unwrap_err(), "publish");
        unchanged("publish");
        let e = target(&inst).install_app_with(&RealFs, &files, &remove, || Ok(()), |_, _| Err("restart".into()));
        assert_eq!(e.unwrap_err(), "restart");
        unchanged("restart");
        // Удачно: перезапуск получает новый exe и весь отодвинутый набор, манифест убран.
        let mut started = None;
        target(&inst).install_app_with(&RealFs, &files, &remove, || Ok(()), |exe, set| {
            started = Some((std::fs::read_to_string(exe).unwrap(), set.to_vec()));
            Ok(())
        })
        .unwrap();
        let (exe, args) = started.unwrap();
        assert_eq!(exe, "exe2");
        let set = restart_set([RESTART_FLAG.to_string()].into_iter().chain(args)).unwrap();
        let mut got: Vec<(&str, String)> = set.iter().map(|r| (r.name, read(&inst, r.aside.as_ref().unwrap()))).collect();
        got.sort();
        let mut want: Vec<(&str, String)> = old.iter().map(|(n, d)| (*n, d.to_string())).collect();
        want.sort();
        assert_eq!(got, want, "каждый заменённый файл — со своим прежним");
        assert_eq!(read(&inst, "tunnel.dll"), "t2");
        assert!(!inst.join(MANIFEST).exists());
        assert!(target(&inst).swap_journal().pending().unwrap().is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn fs_failure_mid_set_rolls_back_everything_placed() {
        let dir = temp("fs-fault");
        let (dst, src) = (dir.join("dst"), dir.join("src"));
        let old = [("a.dll", "old a"), ("b.dll", "old b"), ("c.dll", "old c")];
        write_all(&dst, &old);
        write_all(&src, &[("a.dll", "new a"), ("b.dll", "new b"), ("c.dll", "new c")]);
        let files: Vec<(String, PathBuf)> = ["a.dll", "b.dll", "c.dll"].iter().map(|n| (n.to_string(), src.join(n))).collect();
        let name = |p: &Path| p.file_name().unwrap().to_string_lossy().into_owned();
        let unchanged = |what: &str| {
            for (n, d) in old {
                assert_eq!(read(&dst, n), d, "{what}: {n}");
            }
            assert_eq!(names(&dst), ["a.dll", "b.dll", "c.dll"], "{what}: ни отодвинутых, ни `.new-` не остаётся");
            assert!(target(&dst).swap_journal().pending().unwrap().is_none(), "{what}: план снят");
        };
        // Копия третьего файла отказала: первые два лежат в `.new-` — убраны, рабочие файлы не тронуты.
        let e = target(&dst).install_set(&files, &FaultFs::new(|op, _, to| op == Op::Copy && name(to).starts_with("c.dll.new-"))).unwrap_err();
        assert!(e.contains("c.dll") && e.contains("injected"), "{e}");
        unchanged("copy");
        // Отодвинуть второй файл не вышло — первый уже стоит и возвращается.
        let e = target(&dst).install_set(&files, &FaultFs::new(|op, from, _| op == Op::Rename && name(from) == "b.dll")).unwrap_err();
        assert!(e.contains("b.dll") && e.contains("injected"), "{e}");
        unchanged("rename");
        // Отказал и откат: новый `b` не встал, прежний `a` не возвращается — ошибка называет обе причины, прежний
        // файл не пропал (`.keep-`).
        let fs = FaultFs::new(|op, from, to| {
            op == Op::Rename && ((name(from).starts_with("b.dll.new-") && name(to) == "b.dll") || (name(to) == "a.dll" && name(from).starts_with("a.dll.old-")))
        });
        let e = target(&dst).install_set(&files, &fs).unwrap_err();
        assert!(e.contains("injected") && e.contains("b.dll") && e.contains("a.dll.keep-"), "{e}");
        assert_eq!(read(&dst, "b.dll"), "old b");
        let all = names(&dst);
        let keep = all.iter().find(|n| n.starts_with("a.dll.keep-")).unwrap_or_else(|| panic!("{all:?}"));
        assert_eq!(read(&dst, keep), "old a");
        assert!(!all.iter().any(|n| n.contains(".new-") || n.contains(".old-")), "{all:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn app_install_rolls_back_when_removing_a_file_fails() {
        let dir = temp("fs-fault-app");
        let (inst, src) = (dir.join("inst"), dir.join("src"));
        let old = [(APP_EXE, "exe1"), ("tunnel.dll", "t1"), ("wintun.dll", "w1"), (MANIFEST, "m1"), (MANIFEST_SIG, "s1")];
        write_all(&inst, &old);
        write_all(&src, &[(APP_EXE, "exe2")]);
        let files = vec![(APP_EXE.to_string(), src.join(APP_EXE))];
        // Новый exe уже стоит, манифест с подписью отодвигается; на подписи отказ — exe и манифест возвращены.
        let fs = FaultFs::new(|op, from, _| op == Op::Rename && from.file_name().is_some_and(|n| n == MANIFEST_SIG));
        let e = target(&inst).install_app_with(&fs, &files, &[MANIFEST, MANIFEST_SIG], || Ok(()), |_, _| panic!("no restart")).unwrap_err();
        assert!(e.contains(MANIFEST_SIG) && e.contains("injected"), "{e}");
        for (n, d) in old {
            assert_eq!(read(&inst, n), d, "{n}");
        }
        assert_eq!(names(&inst).len(), old.len(), "отодвинутых не остаётся");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rollback_keeps_original_that_could_not_be_put_back() {
        let dir = temp("keep");
        let (dst, src) = (dir.join("dst"), dir.join("src"));
        write_all(&dst, &[("a.dll", "old a"), ("b.dll", "old b")]);
        write_all(&src, &[("a.dll", "new a"), ("b.dll", "new b")]);
        let files = vec![("a.dll".to_string(), src.join("a.dll")), ("b.dll".to_string(), src.join("b.dll"))];
        let set = put_set(&dst, target(&dst).swap_journal(), &files, &[], &RealFs).unwrap();
        // Возврат b.dll на место не удаётся.
        let e = set
            .undo(&RenameFs::new(|from, to| {
                if to.file_name().is_some_and(|n| n == "b.dll") {
                    Err(std::io::Error::other("busy"))
                } else {
                    std::fs::rename(from, to)
                }
            }))
            .unwrap_err();
        assert_eq!(read(&dst, "a.dll"), "old a");
        let all = names(&dst);
        assert_eq!(all.len(), 2, "{all:?}");
        let keep = all.iter().find(|n| n.starts_with("b.dll.keep-")).expect("keep file");
        assert_eq!(read(&dst, keep), "old b");
        assert!(!all.iter().any(|n| n.contains(".old-")), "{all:?}");
        assert!(e.contains(keep.as_str()) && e.contains("busy"), "{e}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Файловая система, которая «гибнет» на `at`-й операции: паника вместо результата. Разматывание проходит
    /// через `put_set`, не трогая отката, — как снятый процесс или пропавшее питание (журнал плана остаётся).
    struct CrashFs {
        at: usize,
        seen: std::cell::Cell<usize>,
    }

    impl CrashFs {
        fn tick(&self) {
            let n = self.seen.get();
            if n == self.at {
                std::panic::panic_any(CRASH);
            }
            self.seen.set(n + 1);
        }
    }

    const CRASH: &str = "simulated crash";

    impl Fs for CrashFs {
        fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
            self.tick();
            RealFs.rename(from, to)
        }
        fn remove_file(&self, path: &Path) -> std::io::Result<()> {
            self.tick();
            RealFs.remove_file(path)
        }
        fn copy(&self, from: &Path, to: &Path) -> std::io::Result<u64> {
            self.tick();
            RealFs.copy(from, to)
        }
    }

    /// Обрыв после каждой файловой операции замены набора: доведение на следующем старте (`swap::recover`) и уборка
    /// ядра оставляют либо целиком прежний, либо целиком новый набор — без `.new-`, без потерянных файлов.
    #[test]
    fn crash_at_every_step_ends_in_a_complete_set_after_recovery() {
        let old = [(APP_EXE, "exe1"), ("tunnel.dll", "t1"), ("wintun.dll", "w1"), (MANIFEST, "m1"), (MANIFEST_SIG, "s1")];
        let new = [(APP_EXE, "exe2"), ("tunnel.dll", "t2"), ("wintun.dll", "w2")];
        // Операций: 3 копии, по 2 переименования на файл, 2 на убираемые = 11; `at` = 11 — обрыва нет.
        let total = 3 + 3 * 2 + 2;
        let (mut forward, mut back) = (0, 0);
        let quiet = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        for at in 0..=total {
            let dir = temp(&format!("crash-{at}"));
            let (inst, src) = (dir.join("inst"), dir.join("src"));
            write_all(&inst, &old);
            write_all(&src, &new);
            let files: Vec<(String, PathBuf)> = new.iter().map(|(n, _)| (n.to_string(), src.join(n))).collect();
            let fs = CrashFs { at, seen: std::cell::Cell::new(0) };
            let t = target(&inst);
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                put_set(&inst, t.swap_journal(), &files, &[MANIFEST, MANIFEST_SIG], &fs).and_then(|set| set.finish())
            }));
            match &outcome {
                Err(p) => assert_eq!(p.downcast_ref::<&str>(), Some(&CRASH), "at {at}: чужая паника"),
                Ok(r) => assert!(at == total && r.is_ok(), "at {at}: {r:?}"),
            }
            // Следующий старт: доведение, затем уборка `.old-`/`.new-` без плана.
            let mut log = Vec::new();
            let recovered = recover(&t.swap_journal(), &RealFs, &mut |s, text| log.push((s, text.to_string())));
            crate::daemon::install::remove_old_copies_except(&inst, &crate::update::ours::swap::protected(&t.swap_journal()));
            let all = names(&inst);
            let is_new = read(&inst, APP_EXE) == "exe2";
            let want: Vec<(&str, &str)> = if is_new { new.to_vec() } else { old.to_vec() };
            for (n, d) in &want {
                assert_eq!(read(&inst, n), *d, "at {at} ({recovered:?}): {n}");
            }
            let mut want_names: Vec<String> = want.iter().map(|(n, _)| n.to_string()).collect();
            want_names.sort();
            assert_eq!(all, want_names, "at {at} ({recovered:?}): лишние или потерянные файлы");
            assert!(t.swap_journal().pending().unwrap().is_none(), "at {at}: план снят");
            match recovered {
                Recovery::Forward => forward += 1,
                Recovery::RolledBack => back += 1,
                Recovery::Nothing => {}
                other => panic!("at {at}: {other:?} {log:?}"),
            }
            std::fs::remove_dir_all(&dir).unwrap();
        }
        std::panic::set_hook(quiet);
        // Обрывы после плана доводятся вперёд (новые файлы целы); до плана — ничего, кроме уборки.
        assert!(forward >= total - 3, "forward {forward}, back {back}");
    }
}
