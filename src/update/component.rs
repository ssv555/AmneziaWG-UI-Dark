//! Компоненты за одним интерфейсом (стратегия `ComponentOps`): у AmneziaWG, движка и программы своё — установленная
//! версия, найденная проверкой версия, копия, установка, возврат и что сделать после замены. Общий ход работы
//! (сверка подтверждённой версии, копия перед заменой, строки истории, журнал событий) один для всех — в `Manager`.
//! Новый компонент — новая реализация здесь, а не ветки по всему менеджеру.

use std::path::Path;
use std::sync::Arc;

use super::backup::BackupInfo;
use super::busy::Busy;
use super::core_link::CoreLink;
use super::manager::{Fetched, Manager, DOWNLOADS};
use super::{feed, ours, sign, Action, Component};
use crate::i18n::tr;

mod native;

/// Когда пишется строка истории работы.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Journal {
    /// После работы, с её итогом.
    After,
    /// До работы: работа может не вернуться (перезапуск ядра); ошибка, если вернулась, дописывается в ту же строку.
    Before,
    /// До работы и с отметкой `started.json` на время работы: ядро остановилось посередине — при следующем
    /// открытии строка станет ошибкой «прервано».
    Started,
}

/// Возврат к копии: что менеджер подготовил к началу работы.
pub(super) struct RestoreJob<'a> {
    /// Строка истории возврата (имена журналов msiexec).
    pub(super) id: u64,
    /// Имя папки копии и сама папка.
    pub(super) name: &'a str,
    pub(super) dir: &'a Path,
    pub(super) info: &'a BackupInfo,
    /// Копия состояния перед возвратом (имя папки); `None` — компонента не было.
    pub(super) current_backup: Option<&'a str>,
}

/// Что у компонента своё. Ошибка — готовый текст для журнала и строки истории.
pub(super) trait ComponentOps: Send + Sync {
    /// Название для журнала и окна.
    fn name(&self) -> String;
    /// Установленная версия; `None` — не установлен или неизвестна.
    fn installed(&self) -> Option<String>;
    /// Версия, которую нашла проверка `f` (без пояснений — как в истории); источник недоступен — `Err`.
    fn found(&self, f: &Fetched) -> Result<String, String>;
    /// Дата выхода установленной версии `version` (unix, сек), если её можно узнать у источника; `Ok(None)` — источник
    /// такого не даёт (наши компоненты: их даты приходят с найденными проверкой версиями).
    fn release_date(&self, _m: &Manager, _version: &str) -> Result<Option<u64>, String> {
        Ok(None)
    }
    /// Копия установленной версии `version` в готовую пустую папку `dir`.
    fn backup_into(&self, m: &Manager, version: &str, dir: &Path) -> Result<(), String>;
    /// Установка версии `to` (её нашла `found(f)`); `id` — строка истории работы.
    fn update(&self, m: &Manager, f: &Fetched, to: &str, id: u64) -> Result<(), String>;
    /// Возврат к копии `job.dir`.
    fn restore(&self, m: &Manager, job: &RestoreJob) -> Result<(), String>;
    /// Когда писать строку истории работы `action` (обновление или возврат).
    fn journal(&self, _action: Action) -> Journal {
        Journal::After
    }
    /// Компонент заменён (обновлён или возвращён) успешно; ошибка уходит в журнал, замена остаётся удачной.
    fn after_change(&self) -> Result<(), String> {
        Ok(())
    }
    /// `after_change` что-то делает и не должен потеряться: менеджер отмечает его долгом до замены и повторяет после
    /// сбоя или смерти процесса. У программы шага нет (её замена и так перезапускает процесс), у AmneziaWG — тоже.
    fn owes_after_change(&self) -> bool {
        false
    }
    /// Остатки работы этого компонента, оборванной смертью процесса, при открытии менеджера: что доделано —
    /// строки для журнала; не вышло — ошибка. По умолчанию остатков не бывает.
    fn recover(&self, _m: &Manager) -> Result<Vec<String>, String> {
        Ok(Vec::new())
    }
    /// `recover` отложил доделку (шёл установщик Windows — у AmneziaWG): менеджер повторяет её на своём такте.
    fn recover_deferred(&self) -> bool {
        false
    }
}

/// Реализации по компонентам; у каждого компонента ровно одна.
pub(super) struct Components {
    native: Box<dyn ComponentOps>,
    engine: Box<dyn ComponentOps>,
    app: Box<dyn ComponentOps>,
}

impl Components {
    pub(super) fn new(native: Box<dyn ComponentOps>, engine: Box<dyn ComponentOps>, app: Box<dyn ComponentOps>) -> Self {
        Components { native, engine, app }
    }

    /// Настоящие компоненты; туннели при замене трогает только ядро (`core`): аренда на время MSI, переподключение
    /// после замены движка.
    pub(super) fn real(core: Arc<dyn CoreLink>) -> Self {
        Components::new(Box::new(native::real(core.clone())), Box::new(EngineOps { core }), Box::new(AppOps))
    }

    pub(super) fn get(&self, c: Component) -> &dyn ComponentOps {
        match c {
            Component::Native => &*self.native,
            Component::Engine => &*self.engine,
            Component::App => &*self.app,
        }
    }
}

/// Релиз и манифест нашей сборки из проверки (движок и программа берутся из одного релиза).
fn ours_release(f: &Fetched) -> Result<&(feed::Release, sign::Manifest), String> {
    f.ours.as_ref().map_err(|e| e.message.clone())
}

/// Движок режима 2 (`tunnel.dll` + `wintun.dll`).
struct EngineOps {
    core: Arc<dyn CoreLink>,
}

impl ComponentOps for EngineOps {
    fn name(&self) -> String {
        tr("updm.engine")
    }

    fn installed(&self) -> Option<String> {
        ours::engine_version()
    }

    fn found(&self, f: &Fetched) -> Result<String, String> {
        ours_release(f).map(|(_, m)| m.engine.version.clone())
    }

    fn backup_into(&self, _m: &Manager, _version: &str, dir: &Path) -> Result<(), String> {
        ours::backup_engine(dir).map(drop)
    }

    fn update(&self, m: &Manager, f: &Fetched, to: &str, _id: u64) -> Result<(), String> {
        let (rel, manifest) = ours_release(f)?;
        m.set_busy(Busy::Install { what: Component::Engine, version: to.to_string() });
        ours::update_engine(&*m.sources, rel, manifest, &m.dir.join(DOWNLOADS), &mut |b| m.set_busy(b))
    }

    fn restore(&self, _m: &Manager, job: &RestoreJob) -> Result<(), String> {
        ours::restore_engine(job.dir)
    }

    /// Переподключает ядро в своём потоке надзора: работа обновления не ждёт туннели и не держит их блокировку.
    fn after_change(&self) -> Result<(), String> {
        self.core.reconnect_engine()
    }

    /// Без переподключения туннели режима 2 так и работают на прежней DLL.
    fn owes_after_change(&self) -> bool {
        true
    }
}

/// Сборка программы (`awg-ui.exe`).
struct AppOps;

impl ComponentOps for AppOps {
    fn name(&self) -> String {
        tr("updm.app")
    }

    fn installed(&self) -> Option<String> {
        Some(ours::app_version())
    }

    fn found(&self, f: &Fetched) -> Result<String, String> {
        ours_release(f).map(|(_, m)| m.version.clone())
    }

    fn backup_into(&self, _m: &Manager, _version: &str, dir: &Path) -> Result<(), String> {
        ours::backup_app(dir).map(drop)
    }

    fn update(&self, m: &Manager, f: &Fetched, to: &str, _id: u64) -> Result<(), String> {
        let (rel, manifest) = ours_release(f)?;
        m.set_busy(Busy::Install { what: Component::App, version: to.to_string() });
        ours::update_app(&*m.sources, rel, manifest, &m.dir.join(DOWNLOADS), &mut |b| m.set_busy(b))
    }

    fn restore(&self, _m: &Manager, job: &RestoreJob) -> Result<(), String> {
        ours::restore_app(job.dir)
    }

    /// `update_app`/`restore_app` перезапускают ядро и могут не вернуться: строка — до работы.
    fn journal(&self, _action: Action) -> Journal {
        Journal::Before
    }
}

#[cfg(test)]
pub(super) mod fake {
    use std::sync::{Arc, Mutex};

    use super::*;

    /// Вызовы всех подделок одного менеджера по порядку: «backup Engine 3.1», «update Engine 3.2», «changed Engine».
    pub(in crate::update) type Calls = Arc<Mutex<Vec<String>>>;

    /// Подделка компонента: установленная версия задана и меняется удачной установкой или возвратом; найденная
    /// проверкой — задана или берётся у настоящей реализации (тогда из `Fetched`); копия пишет метку `fake` в
    /// папку копии; итоги установки и возврата заданы.
    pub(in crate::update) struct FakeOps {
        c: Component,
        installed: Mutex<Option<String>>,
        found: Result<String, String>,
        /// Настоящая реализация: её `found` и `name` вместо заданных.
        real: Option<Box<dyn ComponentOps>>,
        update: Result<(), String>,
        restore: Result<(), String>,
        journal: Journal,
        /// Писать ли `recover` в `calls`: по умолчанию нет — проверки работ сравнивают список вызовов целиком.
        record_recover: bool,
        /// `recover` «отложен» (идёт установщик): менеджер должен повторить его на такте.
        deferred: Arc<std::sync::atomic::AtomicBool>,
        calls: Calls,
    }

    impl FakeOps {
        pub(in crate::update) fn new(c: Component, installed: Option<&str>, found: &str, calls: &Calls) -> FakeOps {
            FakeOps {
                c,
                installed: Mutex::new(installed.map(str::to_string)),
                found: Ok(found.to_string()),
                real: None,
                update: Ok(()),
                restore: Ok(()),
                journal: Journal::After,
                record_recover: false,
                deferred: Arc::default(),
                calls: calls.clone(),
            }
        }

        /// Версию из проверки находит настоящая реализация компонента `c`; установленная — `installed`.
        pub(in crate::update) fn real(c: Component, installed: Option<&str>, calls: &Calls) -> FakeOps {
            let Components { native, engine, app } = Components::real(Arc::new(crate::update::core_link::fake::RecordingCore::default()));
            let real = match c {
                Component::Native => native,
                Component::Engine => engine,
                Component::App => app,
            };
            FakeOps { real: Some(real), ..FakeOps::new(c, installed, "", calls) }
        }

        pub(in crate::update) fn update_err(mut self, e: &str) -> FakeOps {
            self.update = Err(e.to_string());
            self
        }

        pub(in crate::update) fn restore_err(mut self, e: &str) -> FakeOps {
            self.restore = Err(e.to_string());
            self
        }

        pub(in crate::update) fn journal(mut self, journal: Journal) -> FakeOps {
            self.journal = journal;
            self
        }

        pub(in crate::update) fn record_recover(mut self) -> FakeOps {
            self.record_recover = true;
            self
        }

        /// Доделка «отложена», пока проверка держит `flag` поднятым: так проверяется повтор на такте менеджера.
        pub(in crate::update) fn deferred_by(mut self, flag: Arc<std::sync::atomic::AtomicBool>) -> FakeOps {
            self.deferred = flag;
            self
        }

        fn call(&self, what: String) {
            self.calls.lock().unwrap().push(what);
        }

        fn replaced(&self, result: &Result<(), String>, version: &str) -> Result<(), String> {
            if result.is_ok() {
                *self.installed.lock().unwrap() = Some(version.to_string());
            }
            result.clone()
        }
    }

    impl ComponentOps for FakeOps {
        fn name(&self) -> String {
            self.real.as_ref().map_or_else(|| format!("{:?}", self.c), |r| r.name())
        }

        fn installed(&self) -> Option<String> {
            self.installed.lock().unwrap().clone()
        }

        fn found(&self, f: &Fetched) -> Result<String, String> {
            self.real.as_ref().map_or_else(|| self.found.clone(), |r| r.found(f))
        }

        fn release_date(&self, m: &Manager, version: &str) -> Result<Option<u64>, String> {
            self.real.as_ref().map_or(Ok(None), |r| r.release_date(m, version))
        }

        fn backup_into(&self, _m: &Manager, version: &str, dir: &Path) -> Result<(), String> {
            self.call(format!("backup {:?} {version}", self.c));
            std::fs::write(dir.join("fake"), version).map_err(|e| e.to_string())
        }

        fn update(&self, _m: &Manager, _f: &Fetched, to: &str, _id: u64) -> Result<(), String> {
            self.call(format!("update {:?} {to}", self.c));
            self.replaced(&self.update, to)
        }

        fn restore(&self, _m: &Manager, job: &RestoreJob) -> Result<(), String> {
            self.call(format!("restore {:?} {}", self.c, job.info.version));
            let mark = std::fs::read_to_string(job.dir.join("fake")).map_err(|e| format!("{}: {e}", job.dir.display()))?;
            assert_eq!(mark, job.info.version, "возврат — из папки своей копии");
            self.replaced(&self.restore, &job.info.version)
        }

        fn journal(&self, _action: Action) -> Journal {
            self.journal
        }

        fn after_change(&self) -> Result<(), String> {
            self.call(format!("changed {:?}", self.c));
            Ok(())
        }

        fn recover(&self, _m: &Manager) -> Result<Vec<String>, String> {
            if self.record_recover {
                self.call(format!("recover {:?}", self.c));
            }
            Ok(Vec::new())
        }

        fn recover_deferred(&self) -> bool {
            self.deferred.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    pub(in crate::update) fn components(native: FakeOps, engine: FakeOps, app: FakeOps) -> Components {
        Components::new(Box::new(native), Box::new(engine), Box::new(app))
    }
}
