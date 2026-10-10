//! Загрузка релиза и установка из него: движок и сборка программы скачиваются, проверяются по подписанному
//! манифесту и ставятся одним набором. Сеть — через `Sources`, файлы замены — через `Fs`: успех и отказы
//! проверяются тестами без сети и без настоящей установки.

use std::path::{Path, PathBuf};

use super::fs::Fs;
use super::{manifest_files, InstallTarget, APP_EXE, ENGINE_FILES, FILE_MAX, MANIFEST, MANIFEST_MAX, MANIFEST_SIG};
use crate::i18n::trf;
use crate::update::busy::{Busy, Release};
use crate::update::sources::Sources;
use crate::update::{feed, sign};

/// Итог проверки скачанного манифеста по подписи: разобранный манифест или причина отказа.
type Checked = Result<sign::Manifest, String>;

/// Манифест релиза, скачанный заново, обязан совпасть с тем, что проверен при поиске обновления, — целиком, не
/// только версией: файлы выше проверены по записям `m`, а ставится этот манифест (по нему движку доверяют). Один
/// путь для движка и программы: у переподписанного манифеста с теми же версиями другие суммы DLL, и поставленные
/// файлы перестали бы ему соответствовать. Подпись — над точными байтами файла, поэтому ставятся скачанные байты.
fn refetch_same_manifest(
    rel: &feed::Release,
    m: &sign::Manifest,
    get: &dyn Fn(&str) -> Result<Vec<u8>, String>,
    check: &dyn Fn(&[u8], &str) -> Checked,
) -> Result<(Vec<u8>, String), String> {
    let (json, sig) = manifest_files(rel, get).map_err(|e| e.message)?;
    let fresh = check(&json, &sig)?;
    // Не сериализуется — «не то же»: два `None` нельзя считать равными.
    let same = match (serde_json::to_value(&fresh), serde_json::to_value(m)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    if !same {
        return Err(trf("updo.manifest_changed", &[&rel.version]));
    }
    Ok((json, sig))
}

/// Манифест и подпись релиза (после `refetch_same_manifest`) как файлы набора в `work`.
fn manifest_set(src: &dyn Sources, rel: &feed::Release, m: &sign::Manifest, work: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    let (json, sig) = refetch_same_manifest(rel, m, &|url| src.get(url, MANIFEST_MAX), &|j, s| src.verify_manifest(j, s))?;
    let mut files = Vec::new();
    for (name, data) in [(MANIFEST, json.as_slice()), (MANIFEST_SIG, sig.as_bytes())] {
        let path = work.join(name);
        std::fs::write(&path, data).map_err(|e| crate::fsutil::io_ctx(&path, e))?;
        files.push((name.to_string(), path));
    }
    Ok(files)
}

impl InstallTarget {
    /// Скачать файлы движка из релиза в `work`, проверить по манифесту и поставить в Program Files.
    pub(super) fn update_engine(&self, src: &dyn Sources, fs: &dyn Fs, rel: &feed::Release, m: &sign::Manifest, work: &Path, busy: &mut dyn FnMut(Busy)) -> Result<(), String> {
        std::fs::create_dir_all(work).map_err(|e| crate::fsutil::io_ctx(work, e))?;
        let mut files = Vec::new();
        for entry in &m.engine.files {
            files.push((entry.name.clone(), fetch(src, rel, entry, work, busy)?));
        }
        files.extend(manifest_set(src, rel, m, work)?);
        busy(Busy::SetInstall(Release::Engine(m.engine.version.clone())));
        self.install_set(&files, fs)
    }

    /// Скачать exe из релиза, проверить по манифесту, поставить вместо exe ядра, отдать окну и перезапустить ядро.
    /// Новый exe доверяет DLL движка только по своим вшитым суммам или по локальному манифесту: если стоящие DLL не
    /// из манифеста релиза, они скачиваются и ставятся одним набором с exe; манифест с подписью ставится всегда.
    pub(super) fn update_app(&self, src: &dyn Sources, rel: &feed::Release, m: &sign::Manifest, work: &Path, busy: &mut dyn FnMut(Busy)) -> Result<(), String> {
        let files = self.app_set(src, rel, m, work, busy)?;
        busy(Busy::SetInstall(Release::App(m.version.clone())));
        self.install_app(&files, &[], &m.version)
    }

    /// Набор сборки программы, скачанный и проверенный в `work`; до установки ничего не трогает в папке программы.
    fn app_set(&self, src: &dyn Sources, rel: &feed::Release, m: &sign::Manifest, work: &Path, busy: &mut dyn FnMut(Busy)) -> Result<Vec<(String, PathBuf)>, String> {
        std::fs::create_dir_all(work).map_err(|e| crate::fsutil::io_ctx(work, e))?;
        let mut files = vec![(APP_EXE.to_string(), fetch(src, rel, &m.app, work, busy)?)];
        if self.dir.join(ENGINE_FILES[0]).exists() && !engine_matches(&self.dir, m) {
            for entry in &m.engine.files {
                files.push((entry.name.clone(), fetch(src, rel, entry, work, busy)?));
            }
        }
        files.extend(manifest_set(src, rel, m, work)?);
        Ok(files)
    }
}

/// Обе DLL движка в `dir` совпадают (размер и SHA-256) с записями манифеста.
fn engine_matches(dir: &Path, m: &sign::Manifest) -> bool {
    ENGINE_FILES
        .iter()
        .all(|n| m.engine.files.iter().any(|f| f.name == *n && sign::check_file(&dir.join(n), f).is_ok()))
}

/// Скачать файл релиза в `work` и проверить по записи манифеста; прогресс — в `busy`.
fn fetch(src: &dyn Sources, rel: &feed::Release, entry: &sign::FileEntry, work: &Path, busy: &mut dyn FnMut(Busy)) -> Result<PathBuf, String> {
    let asset = rel.asset(&entry.name).ok_or_else(|| trf("updo.no_asset", &[&entry.name]))?;
    let dest = work.join(&entry.name);
    let mut last = None;
    src.download(&asset.url, &dest, FILE_MAX.min(entry.size), &mut |got, total| {
        let pct = (got * 100).checked_div(total.unwrap_or(entry.size)).unwrap_or(0);
        if last != Some(pct) {
            last = Some(pct);
            busy(Busy::FileDownload { file: entry.name.clone(), percent: pct });
        }
    })?;
    sign::check_file(&dest, entry)?;
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::ours::fs::RealFs;
    use crate::update::ours::testutil::*;
    use crate::update::sources::fake::FakeSources;

    #[test]
    fn refetch_rejects_same_version_manifest_with_other_hashes() {
        let json = r#"{"tag_name":"v0.4.0","assets":[
            {"name":"update-manifest.json","browser_download_url":"https://h/m","size":1},
            {"name":"update-manifest.json.sig","browser_download_url":"https://h/s","size":1}]}"#;
        let rel = feed::parse_release(json).unwrap();
        let get = |url: &str| Ok(if url.ends_with("/m") { JSON.to_vec() } else { SIG.as_bytes().to_vec() });
        let checked = test_key(JSON, SIG).unwrap();
        // Тот же манифест — принят, ставятся скачанные байты.
        let (j, s) = refetch_same_manifest(&rel, &checked, &get, &test_key).unwrap();
        assert_eq!((j.as_slice(), s.as_str()), (JSON, SIG));
        // Версии те же, сумма DLL другая (переподписанный релиз): общий путь отказывает и для движка, и для программы.
        let mut other = checked.clone();
        other.engine.files[0].sha256 = "0".repeat(64);
        let e = refetch_same_manifest(&rel, &other, &get, &test_key).unwrap_err();
        assert_eq!(e, trf("updo.manifest_changed", &["0.4.0"]));
        // Сменилась только сумма exe.
        let mut other = checked.clone();
        other.app.sha256 = "0".repeat(64);
        assert!(refetch_same_manifest(&rel, &other, &get, &test_key).is_err());
        // Подпись не сошлась или загрузка не удалась — ошибка источника, не «тот же».
        let bad = |_: &[u8], _: &str| -> Result<sign::Manifest, String> { Err("sig".into()) };
        assert_eq!(refetch_same_manifest(&rel, &checked, &get, &bad).unwrap_err(), "sig");
        let down = |_: &str| -> Result<Vec<u8>, String> { Err("net".into()) };
        assert_eq!(refetch_same_manifest(&rel, &checked, &down, &test_key).unwrap_err(), "net");
    }

    // Релиз для установки: новые файлы, подписанный тестовым ключом манифест и всё это за `FakeSources`.
    const OLD: [(&str, &str); 4] = [("tunnel.dll", "old tunnel"), ("wintun.dll", "old wintun"), (MANIFEST, "old manifest"), (MANIFEST_SIG, "old sig")];
    const NEW_EXE: &[u8] = b"new exe";
    const NEW_TUNNEL: &[u8] = b"new tunnel";
    const NEW_WINTUN: &[u8] = b"new wintun";

    fn sha(data: &[u8]) -> String {
        sign::sha256_hex(data)
    }

    /// Манифест 0.4.0 / движок 3.2.0 на эти три файла.
    fn manifest_json(exe: &[u8], tunnel: &[u8], wintun: &[u8]) -> Vec<u8> {
        let entry = |name: &str, d: &[u8]| format!(r#"{{"name":"{name}","sha256":"{}","size":{}}}"#, sha(d), d.len());
        format!(
            r#"{{"version":"0.4.0","published":"2026-10-05T12:00:00Z","app":{},"engine":{{"version":"3.2.0","wintun":"0.14.1","files":[{},{}]}}}}"#,
            entry("awg-ui.exe", exe),
            entry("tunnel.dll", tunnel),
            entry("wintun.dll", wintun)
        )
        .into_bytes()
    }

    fn url(name: &str) -> String {
        format!("https://h/{name}")
    }

    struct Published {
        rel: feed::Release,
        m: sign::Manifest,
        src: FakeSources,
    }

    fn published() -> Published {
        let assets: Vec<String> = ["awg-ui.exe", "tunnel.dll", "wintun.dll", MANIFEST, MANIFEST_SIG]
            .iter()
            .map(|n| format!(r#"{{"name":"{n}","browser_download_url":"{}","size":1}}"#, url(n)))
            .collect();
        let rel = feed::parse_release(&format!(r#"{{"tag_name":"v0.4.0","assets":[{}]}}"#, assets.join(","))).unwrap();
        let json = manifest_json(NEW_EXE, NEW_TUNNEL, NEW_WINTUN);
        let sig = sign_as_test_signer(&json);
        let m = signed_by_test_signer(&json, &sig).unwrap();
        let src = FakeSources::down();
        for (name, data) in [("awg-ui.exe", NEW_EXE), ("tunnel.dll", NEW_TUNNEL), ("wintun.dll", NEW_WINTUN)] {
            src.serve(&url(name), data);
        }
        src.serve(&url(MANIFEST), &json);
        src.serve(&url(MANIFEST_SIG), sig.as_bytes());
        Published { rel, m, src }
    }

    /// Папка программы со старыми файлами (`<base>/inst`) и рядом ещё не созданная папка загрузок.
    fn installed(tag: &str, files: &[(&str, &str)]) -> (PathBuf, PathBuf, InstallTarget) {
        let base = temp(tag);
        let (dir, work) = (base.join("inst"), base.join("work"));
        write_all(&dir, files);
        let t = target(&dir);
        (base, work, t)
    }

    /// Файлы в `dir` и их содержимое: после отказа должно остаться ровно то, что было (в том числе без `.old-`).
    fn snapshot(dir: &Path) -> Vec<(String, String)> {
        names(dir).into_iter().map(|n| (read(dir, &n), n)).map(|(c, n)| (n, c)).collect()
    }

    #[test]
    fn engine_update_installs_downloaded_files_and_the_signed_manifest() {
        let p = published();
        let (base, work, t) = installed("rel-engine-ok", &OLD);
        let mut log = Vec::new();
        t.update_engine(&p.src, &RealFs, &p.rel, &p.m, &work, &mut |b| log.push(b.text(|c| format!("{c:?}")))).unwrap();
        let inst = base.join("inst");
        assert_eq!(read(&inst, "tunnel.dll").as_bytes(), NEW_TUNNEL);
        assert_eq!(read(&inst, "wintun.dll").as_bytes(), NEW_WINTUN);
        // Ставятся скачанные байты манифеста: по ним подпись сходится у нового exe.
        let json = manifest_json(NEW_EXE, NEW_TUNNEL, NEW_WINTUN);
        assert_eq!(std::fs::read(inst.join(MANIFEST)).unwrap(), json);
        assert_eq!(read(&inst, MANIFEST_SIG), sign_as_test_signer(&json));
        // Прежние файлы отодвинуты (их удалит ядро); в `busy` — прогресс загрузки и шаг установки.
        assert_eq!(names(&inst).iter().filter(|n| n.contains(".old-")).count(), 4);
        assert!(log.iter().any(|s| s.contains("100")), "{log:?}");
        assert!(log.last().is_some_and(|s| s.contains("3.2.0")), "{log:?}");
        assert!(!names(&inst).iter().any(|n| n == "awg-ui.exe"), "exe не входит в набор движка");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn engine_update_refuses_manifest_whose_signature_does_not_match() {
        let (base, work, t) = installed("rel-engine-sig", &OLD);
        let inst = base.join("inst");
        let before = snapshot(&inst);
        // Подпись от других байтов, затем подпись другого ключа: оба раза установка отказывает, набор цел.
        for (sig, want) in [(sign_as_test_signer(b"other manifest"), "не сходится"), (SIG.to_string(), "другим ключом")] {
            let p = published();
            p.src.serve(&url(MANIFEST_SIG), sig.as_bytes());
            let e = t.update_engine(&p.src, &RealFs, &p.rel, &p.m, &work, &mut |_| {}).unwrap_err();
            assert!(e.contains(want), "{e}");
            assert_eq!(snapshot(&inst), before);
        }
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn engine_update_with_failed_download_leaves_the_installed_set_untouched() {
        let (base, work, t) = installed("rel-engine-net", &OLD);
        let inst = base.join("inst");
        let before = snapshot(&inst);
        // Обрыв на второй DLL: первая уже скачана в `work`, но в папку программы ничего не попало.
        let p = published();
        p.src.break_url(&url("wintun.dll"));
        let e = t.update_engine(&p.src, &RealFs, &p.rel, &p.m, &work, &mut |_| {}).unwrap_err();
        assert!(e.contains("connection lost"), "{e}");
        assert_eq!(snapshot(&inst), before);
        // Скачалось не то (сумма не сходится) — тоже отказ до установки.
        let p = published();
        p.src.serve(&url("wintun.dll"), b"new wintux");
        let e = t.update_engine(&p.src, &RealFs, &p.rel, &p.m, &work, &mut |_| {}).unwrap_err();
        assert!(e.contains("SHA-256"), "{e}");
        assert_eq!(snapshot(&inst), before);
        // Файла нет среди ассетов релиза.
        let mut p = published();
        p.rel.assets.retain(|a| a.name != "wintun.dll");
        let e = t.update_engine(&p.src, &RealFs, &p.rel, &p.m, &work, &mut |_| {}).unwrap_err();
        assert_eq!(e, trf("updo.no_asset", &["wintun.dll"]));
        assert_eq!(snapshot(&inst), before);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn resigned_manifest_with_other_hashes_is_refused_for_engine_and_app() {
        // D7: релиз переподписан между проверкой и установкой — версии те же, сумма tunnel.dll другая, подпись верна.
        let p = published();
        let json = manifest_json(NEW_EXE, b"another tunnel", NEW_WINTUN);
        p.src.serve(&url(MANIFEST), &json);
        p.src.serve(&url(MANIFEST_SIG), sign_as_test_signer(&json).as_bytes());
        let want = trf("updo.manifest_changed", &["0.4.0"]);

        let (base, work, t) = installed("rel-resigned", &OLD);
        let inst = base.join("inst");
        let before = snapshot(&inst);
        assert_eq!(t.update_engine(&p.src, &RealFs, &p.rel, &p.m, &work, &mut |_| {}).unwrap_err(), want);
        assert_eq!(snapshot(&inst), before);
        assert_eq!(t.app_set(&p.src, &p.rel, &p.m, &work, &mut |_| {}).unwrap_err(), want);
        assert_eq!(snapshot(&inst), before);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn engine_update_rolls_back_when_a_file_cannot_be_put_in_place() {
        let p = published();
        let (base, work, t) = installed("rel-engine-fs", &OLD);
        let inst = base.join("inst");
        let before = snapshot(&inst);
        // Копия идёт в `wintun.dll.new-<random>` — отказывает она.
        let fs = FaultFs::new(|op, _, to| op == Op::Copy && to.file_name().is_some_and(|n| n.to_string_lossy().starts_with("wintun.dll.new-")));
        let e = t.update_engine(&p.src, &fs, &p.rel, &p.m, &work, &mut |_| {}).unwrap_err();
        assert!(e.contains("injected"), "{e}");
        assert_eq!(snapshot(&inst), before);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn app_set_downloads_the_exe_and_dlls_only_when_the_installed_ones_differ() {
        let downloaded = |p: &Published| -> Vec<String> { p.src.downloads.lock().unwrap().iter().map(|u| u.rsplit('/').next().unwrap().to_string()).collect() };
        let in_set = |files: &[(String, PathBuf)]| -> Vec<String> { files.iter().map(|(n, _)| n.clone()).collect() };
        // DLL стоят из манифеста релиза — достаточно exe и манифеста с подписью.
        let p = published();
        let current = [("tunnel.dll", "new tunnel"), ("wintun.dll", "new wintun"), ("awg-ui.exe", "old exe")];
        let (base, work, t) = installed("rel-app-same", &current);
        let files = t.app_set(&p.src, &p.rel, &p.m, &work, &mut |_| {}).unwrap();
        assert_eq!(in_set(&files), ["awg-ui.exe", MANIFEST, MANIFEST_SIG]);
        assert_eq!(downloaded(&p), ["awg-ui.exe"]);
        std::fs::remove_dir_all(&base).unwrap();
        // Стоят другие DLL — ставятся вместе с exe, иначе новый exe им не доверит.
        let p = published();
        let (base, work, t) = installed("rel-app-stale", &OLD);
        let files = t.app_set(&p.src, &p.rel, &p.m, &work, &mut |_| {}).unwrap();
        assert_eq!(in_set(&files), ["awg-ui.exe", "tunnel.dll", "wintun.dll", MANIFEST, MANIFEST_SIG]);
        assert_eq!(downloaded(&p), ["awg-ui.exe", "tunnel.dll", "wintun.dll"]);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn app_update_installs_the_set_and_hands_it_to_the_restart_or_rolls_back() {
        let old = [("awg-ui.exe", "old exe"), ("tunnel.dll", "old tunnel"), ("wintun.dll", "old wintun"), (MANIFEST, "old manifest"), (MANIFEST_SIG, "old sig")];
        let p = published();
        let (base, work, t) = installed("rel-app-ok", &old);
        let inst = base.join("inst");
        let files = t.app_set(&p.src, &p.rel, &p.m, &work, &mut |_| {}).unwrap();
        let mut restarted = Vec::new();
        t.install_app_with(&RealFs, &files, &[], || Ok(()), |exe, set| {
            restarted.push((exe.to_path_buf(), set.len()));
            Ok(())
        })
        .unwrap();
        assert_eq!(read(&inst, "awg-ui.exe").as_bytes(), NEW_EXE);
        assert_eq!(read(&inst, "tunnel.dll").as_bytes(), NEW_TUNNEL);
        assert_eq!(restarted, [(inst.join("awg-ui.exe"), 5)]);

        // Перезапуск не получился — набор откатывается, ядро остаётся на прежней сборке.
        let (base2, work2, t2) = installed("rel-app-fail", &old);
        let inst2 = base2.join("inst");
        let before = snapshot(&inst2);
        let files = t2.app_set(&p.src, &p.rel, &p.m, &work2, &mut |_| {}).unwrap();
        let e = t2.install_app_with(&RealFs, &files, &[], || Ok(()), |_, _| Err("restart".into())).unwrap_err();
        assert_eq!(e, "restart");
        assert_eq!(snapshot(&inst2), before);
        std::fs::remove_dir_all(&base).unwrap();
        std::fs::remove_dir_all(&base2).unwrap();
    }

    #[test]
    fn app_download_failure_leaves_the_program_folder_untouched() {
        let old = [("awg-ui.exe", "old exe"), ("tunnel.dll", "old tunnel"), ("wintun.dll", "old wintun")];
        let p = published();
        p.src.break_url(&url("awg-ui.exe"));
        let (base, work, t) = installed("rel-app-net", &old);
        let before = snapshot(&base.join("inst"));
        // Прямо `update_app`: до установки (и перезапуска ядра) дело не доходит.
        let e = t.update_app(&p.src, &p.rel, &p.m, &work, &mut |_| {}).unwrap_err();
        assert!(e.contains("connection lost"), "{e}");
        assert_eq!(snapshot(&base.join("inst")), before);
        std::fs::remove_dir_all(&base).unwrap();
    }
}
