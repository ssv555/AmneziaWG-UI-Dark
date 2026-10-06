//! Транзакция над набором файлов: каждый заменяемый файл отодвигается (`<имя>.old-<random>`), новый копируется на
//! его место; ошибка — всё поставленное убирается, отодвинутое возвращается (не вернулось — `<имя>.keep-<random>`).
//! Поверх неё — установка сборки программы с выкладкой окну и перезапуском ядра.

use std::path::{Path, PathBuf};

use windows_sys::Win32::System::Threading::{CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS};

use super::fs::{Fs, RealFs};
use super::{InstallTarget, APP_EXE, RESTART_FLAG};
use crate::i18n::trf;

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
        let mut set = put_set(&self.dir, files, fs)?;
        let exe = self.dir.join(APP_EXE);
        let result = remove
            .iter()
            .try_for_each(|n| set.set_aside(&self.dir.join(n), fs))
            .and_then(|()| publish())
            .and_then(|()| restart(&exe, &set.restart_args()));
        match result {
            Ok(()) => Ok(()),
            Err(e) => Err(with_rollback(e, set.undo(fs))),
        }
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

/// Поставить файлы `(имя, источник)` в `dst`, каждый — отодвинув текущий. Ошибка на любом — всё поставленное
/// убирается, отодвинутое возвращается на место.
pub(super) fn install_set(dst: &Path, files: &[(String, PathBuf)], fs: &dyn Fs) -> Result<(), String> {
    put_set(dst, files, fs).map(drop)
}

/// Как `install_set`, но поставленное можно откатить и после успеха.
fn put_set(dst: &Path, files: &[(String, PathBuf)], fs: &dyn Fs) -> Result<Installed, String> {
    let mut done = Installed(Vec::new());
    for (name, src) in files {
        let path = dst.join(name);
        match replace(src, &path, fs) {
            Ok(aside) => done.0.push((path, aside)),
            Err(e) => return Err(with_rollback(e, done.undo(fs))),
        }
    }
    Ok(done)
}

/// Ошибка шага вместе с ошибкой отката, если откат тоже не удался.
fn with_rollback(e: String, undo: Result<(), String>) -> String {
    match undo {
        Ok(()) => e,
        Err(u) => format!("{e}; {u}"),
    }
}

/// Поставленные файлы: `(путь, отодвинутый прежний)`.
struct Installed(Vec<(PathBuf, Option<PathBuf>)>);

impl Installed {
    /// Отодвинуть `path` (если есть), ничего не ставя на его место; откат вернёт.
    fn set_aside(&mut self, path: &Path, fs: &dyn Fs) -> Result<(), String> {
        if !path.exists() {
            return Ok(());
        }
        let aside = aside_name(path);
        fs.rename(path, &aside).map_err(|e| crate::fsutil::io_ctx(&path, e))?;
        self.0.push((path.to_path_buf(), Some(aside)));
        Ok(())
    }

    /// Аргументы перезапуска (`restart_set`): по файлу — имя отодвинутого прежнего или, если прежнего не было,
    /// голое имя поставленного.
    fn restart_args(&self) -> Vec<String> {
        let name = |p: &Path| p.file_name().unwrap_or_default().to_string_lossy().into_owned();
        self.0.iter().map(|(path, aside)| name(aside.as_deref().unwrap_or(path))).collect()
    }

    /// Убрать поставленное и вернуть отодвинутое, в обратном порядке; ошибки всех шагов — вместе.
    fn undo(self, fs: &dyn Fs) -> Result<(), String> {
        let errors: Vec<String> =
            self.0.into_iter().rev().filter_map(|(path, aside)| restore(&path, aside.as_deref(), fs).err()).collect();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}

/// Убрать поставленный `path` и вернуть на его место отодвинутый `aside`. Не вернулся — прежний файл
/// переименовывается в `<имя>.keep-<random>` (такие ядро не удаляет), ошибка называет его: под именем
/// `.old-` его удалило бы ядро при старте.
fn restore(path: &Path, aside: Option<&Path>, fs: &dyn Fs) -> Result<(), String> {
    let removed = match fs.remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(crate::fsutil::io_ctx(&path, e)),
        _ => Ok(()),
    };
    let Some(aside) = aside else { return removed };
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

fn suffixed(path: &Path, tag: &str) -> PathBuf {
    path.with_file_name(format!("{}.{tag}-{}", path.file_name().unwrap_or_default().to_string_lossy(), crate::store::random_hex()))
}

/// Отодвинуть `path` (если есть) и скопировать `src` на его место; не скопировалось — вернуть отодвинутый.
/// Возвращает имя отодвинутого файла.
fn replace(src: &Path, path: &Path, fs: &dyn Fs) -> Result<Option<PathBuf>, String> {
    let aside = if path.exists() {
        let aside = aside_name(path);
        fs.rename(path, &aside).map_err(|e| crate::fsutil::io_ctx(&path, e))?;
        Some(aside)
    } else {
        None
    };
    if let Err(e) = fs.copy(src, path) {
        let e = crate::fsutil::io_ctx_move(&src, &path, e);
        return Err(with_rollback(e, restore(path, aside.as_deref(), fs)));
    }
    Ok(aside)
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
        assert!(install_set(&dst, &files, &RealFs).is_err());
        assert_eq!(std::fs::read_to_string(dst.join("a.dll")).unwrap(), "old a");
        assert_eq!(std::fs::read_to_string(dst.join("b.dll")).unwrap(), "old b");
        assert_eq!(std::fs::read_dir(&dst).unwrap().count(), 2);

        std::fs::write(src.join("b.dll"), "new b").unwrap();
        install_set(&dst, &files, &RealFs).unwrap();
        assert_eq!(std::fs::read_to_string(dst.join("a.dll")).unwrap(), "new a");
        assert_eq!(std::fs::read_to_string(dst.join("b.dll")).unwrap(), "new b");
        let aside = std::fs::read_dir(&dst).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().contains(".old-")).count();
        assert_eq!(aside, 2);
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
            assert_eq!(names(&dst), ["a.dll", "b.dll", "c.dll"], "{what}: отодвинутых не остаётся");
        };
        // Копирование третьего файла отказало: первые два уже стоят — оба возвращены.
        let e = install_set(&dst, &files, &FaultFs::new(|op, _, to| op == Op::Copy && name(to) == "c.dll")).unwrap_err();
        assert!(e.contains("c.dll") && e.contains("injected"), "{e}");
        unchanged("copy");
        // Отодвинуть второй файл не вышло — то же.
        let e = install_set(&dst, &files, &FaultFs::new(|op, from, _| op == Op::Rename && name(from) == "b.dll")).unwrap_err();
        assert!(e.contains("b.dll") && e.contains("injected"), "{e}");
        unchanged("rename");
        // Отказал и откат: ошибка называет обе причины, прежний файл не пропал (`.keep-`).
        let fs = FaultFs::new(|op, from, to| {
            (op == Op::Copy && name(to) == "b.dll") || (op == Op::Rename && name(to) == "a.dll" && name(from).starts_with("a.dll.old-"))
        });
        let e = install_set(&dst, &files, &fs).unwrap_err();
        assert!(e.contains("injected") && e.contains("b.dll") && e.contains("a.dll.keep-"), "{e}");
        assert_eq!(read(&dst, "b.dll"), "old b");
        let all = names(&dst);
        let keep = all.iter().find(|n| n.starts_with("a.dll.keep-")).unwrap_or_else(|| panic!("{all:?}"));
        assert_eq!(read(&dst, keep), "old a");
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
        let set = put_set(&dst, &files, &RealFs).unwrap();
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
}
