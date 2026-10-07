//! Менеджер обновлений в ядре: проверка источников, резервная копия перед каждым обновлением и возвратом,
//! установка, история с «Вернуть», ежедневная проверка. Одна работа за раз — в своём потоке; окно видит её ход
//! в `busy`.
//!
//! Хранилище — `<папка данных>\updates`: `history.json`, `state.json`, `backups\<id>-<компонент>-<версия>\`,
//! `downloads\` (временное, чистится после каждой работы), `logs\` (журналы msiexec).
//! После каждой работы — пределы хранилища: `HISTORY_MAX` строк, `BACKUPS_MAX` копий на компонент, `LOGS_MAX`
//! журналов.

use std::ops::ControlFlow;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::{feed, sign};
use super::clock::{check_due, Clock, SystemClock, TICK};
use super::component::{ComponentOps, Components, Journal, RestoreJob};
use super::core_link::CoreLink;
use super::sources::{GithubSources, OursError, Sources};
use super::{Action, Component, HistoryEntry, NativeUiMark, UpdateOp, UpdatesState, ORDER};
use super::jsonstore::{load_json, load_or_default, rotate_logs, safe_name, save_json, set_aside};
use super::backup::{adopt_orphans, BackupInfo, BACKUP_INFO};
use super::busy::Busy;
use super::history::{entry, History, HISTORY};
use super::restore_target::{offers as restore_offers, resolve as resolve_restore, RestoreBlock};
use super::rows::{
    apply_order, available_rows, check_confirmed, deserialize_results, prune_released, remember_found, remember, rows, to_announce,
    unknown_release_dates, CheckResult, ReleaseDate, UpstreamCheck,
};
pub(super) use super::history::{amend_stored, record_fallback};
use crate::crash::lock;
use crate::events::Severity;
use crate::i18n::{tr, trf};
use crate::monitor::Shared;

/// Журналов msiexec в `logs\` не больше этого.
pub(super) const LOGS_MAX: usize = 20;
const STATE: &str = "state.json";
pub(super) const BACKUPS: &str = "backups";
pub(super) const DOWNLOADS: &str = "downloads";
const LOGS: &str = "logs";
/// Номер строки истории идущего возврата AmneziaWG; остался после остановки ядра — возврат прерван.
const STARTED: &str = "started.json";
/// Компоненты, чей шаг после замены (`after_change`, у движка — `ReconnectEngine` ядру) ещё не принят: отметка
/// ставится до замены файлов и снимается только после ответа ядра. Агент умер между заменой и запросом или ядро
/// перезапускалось — при следующем запуске (на такте планировщика) шаг повторяется.
const OWED: &str = "after_change.json";

/// Что сохраняется между запусками, кроме истории.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
struct Saved {
    checked_at: Option<u64>,
    /// Результат последней проверки (установленные версии здесь не хранятся — их читают заново). Читает и прежний
    /// формат файла, где строка была целой `ComponentState`.
    #[serde(default, deserialize_with = "deserialize_results")]
    components: Vec<CheckResult>,
    /// О каких версиях уже сообщили в журнал событий.
    #[serde(default)]
    announced: Vec<(Component, String)>,
    /// Новейшая метка amneziawg-windows при последней проверке; в `state.json` прежних версий поля нет.
    #[serde(default)]
    upstream: Option<UpstreamCheck>,
    /// Даты выхода версий, замеченные проверками (найденные и установленные сейчас); в `state.json` прежних версий поля нет.
    #[serde(default)]
    released: Vec<ReleaseDate>,
    /// Последняя законченная работа с MSI родного AmneziaWG (окно открывает снова закрытое им окно AmneziaWG); в
    /// `state.json` прежних версий поля нет.
    #[serde(default)]
    native_ui: NativeUiMark,
}

struct Data {
    saved: Saved,
    /// Единственный владелец `history.json` в ядре (см. `History`).
    history: History,
}

/// Ответы источников при проверке.
pub(super) struct Fetched {
    pub(super) native: Result<feed::Release, String>,
    pub(super) ours: Result<(feed::Release, sign::Manifest), OursError>,
}

pub struct Manager {
    pub(super) shared: Arc<Shared>,
    /// Что у каждого компонента своё (в тестах — подделки).
    components: Components,
    pub(super) dir: PathBuf,
    /// Откуда берутся релизы и файлы (в тестах — подделка).
    pub(super) sources: Arc<dyn Sources>,
    /// Часы планировщика и отметки последней проверки.
    clock: Arc<dyn Clock>,
    /// Идущая работа; `Some` — занято. Текст для окна собирается при чтении (`state`).
    busy: Mutex<Option<Busy>>,
    /// Идущая работа запускала MSI родного AmneziaWG и было ли перед ним открыто его окно (`note_native_ui`); в конце
    /// работы становится `Saved::native_ui`.
    native_ui_in_job: Mutex<Option<bool>>,
    data: Mutex<Data>,
    /// Отметка `OWED`; держится на время самого шага, чтобы такт планировщика и работа не слали его вразнобой.
    owed: Mutex<Owed>,
}

/// Невыполненные шаги после замены и было ли уже в журнале, что повтор не удался (пишется один раз до успеха).
struct Owed {
    components: Vec<Component>,
    failure_logged: bool,
}

/// Конец работы (и при панике): загрузки убраны, занятость снята.
struct JobDone<'a>(&'a Manager);

impl Drop for JobDone<'_> {
    fn drop(&mut self) {
        self.0.clean_downloads();
        self.0.finish_native_ui();
        *lock(&self.0.busy) = None;
    }
}

impl Manager {
    /// Менеджер ядра (создаётся один раз) с ежедневной проверкой в фоне; туннели трогает только через `core`.
    pub fn new(shared: Arc<Shared>, core: Arc<dyn CoreLink>) -> Arc<Manager> {
        let components = Components::real(core);
        let m = Arc::new(Manager::open(store_dir(), shared, components, Arc::new(GithubSources), Arc::new(SystemClock::new())));
        let daily = m.clone();
        crate::crash::spawn_named("update-daily", move || daily.daily());
        m
    }

    /// Менеджер для тестов ядра: хранилище во временной папке, без фоновой проверки и без обращений в сеть.
    #[cfg(test)]
    pub(crate) fn for_core_tests(shared: Arc<Shared>) -> Arc<Manager> {
        let dir = std::env::temp_dir().join(format!("awg-core-test-{}", std::process::id()));
        Arc::new(Manager::open(dir, shared, Components::real(Arc::new(super::core_link::fake::RecordingCore::default())), Arc::new(GithubSources), Arc::new(SystemClock::new())))
    }

    /// Менеджер над хранилищем `dir`, без фоновой проверки.
    fn open(dir: PathBuf, shared: Arc<Shared>, components: Components, sources: Arc<dyn Sources>, clock: Arc<dyn Clock>) -> Manager {
        // Испорченная история — файл отодвигается, история пустая, событие в журнале (как `load_or_default`): ядро
        // должно запуститься, а строки с именами копий не должны пропасть под следующей записью.
        let mut history = History::open(&dir).unwrap_or_else(|e| {
            shared.log("", Severity::Bad, &set_aside(&dir.join(HISTORY), &e));
            History::empty(&dir)
        });
        for fixed in history.repair() {
            shared.log("", Severity::Warn, &fixed);
        }
        let data = Data { saved: load_or_default(&dir.join(STATE), &shared), history };
        // Отметка пишется атомарно; испорченная — чужая правка: событие в журнале и пусто (`load_or_default`).
        let owed = Mutex::new(Owed { components: load_or_default(&dir.join(OWED), &shared), failure_logged: false });
        let m = Manager { shared, components, dir, sources, clock, busy: Mutex::new(None), native_ui_in_job: Mutex::new(None), data: Mutex::new(data), owed };
        for sub in [BACKUPS, DOWNLOADS, LOGS] {
            if let Err(e) = std::fs::create_dir_all(m.dir.join(sub)) {
                m.shared.log("", Severity::Bad, &crate::fsutil::io_ctx(m.dir.join(sub), e));
            }
        }
        // Остаток загрузок после сбоя или после перезапуска ядра при обновлении программы.
        m.clean_downloads();
        m.mark_interrupted();
        m.recover_components();
        // До `tidy`: найденные копии подчиняются тем же пределам, что и остальные.
        m.adopt_orphan_backups();
        m.tidy();
        m.announce_owed();
        m
    }

    /// Менеджер над временной папкой `dir` с заданными компонентами (подделками), без сети и без фоновой проверки.
    #[cfg(test)]
    pub(super) fn for_tests(dir: &std::path::Path, components: Components) -> Arc<Manager> {
        let options = crate::monitor::Options { ping: false, ping_host: String::new(), notify: false, tray: false, taskbar: false };
        let shared = Arc::new(Shared::new(None, options, None));
        Arc::new(Manager::open(dir.to_path_buf(), shared, components, Arc::new(super::sources::fake::FakeSources::down()), Arc::new(super::clock::fake::FakeClock::new(1_000))))
    }

    /// Остатки работ, оборванных смертью процесса (у AmneziaWG — туннели возврата, не вернувшиеся в его папку):
    /// каждый компонент доделывает своё, итог — в журнал. Ошибка доделки — событие, не остановка: менеджер нужен и
    /// без неё, а остаток лежит до следующего запуска.
    fn recover_components(&self) {
        for c in ORDER {
            self.recover_component(c);
        }
    }

    fn recover_component(&self, c: Component) {
        match self.ops(c).recover(self) {
            Ok(lines) => lines.iter().for_each(|l| self.shared.log("", Severity::Warn, l)),
            Err(e) => self.shared.log("", Severity::Bad, &trf("updm.recover_failed", &[&self.name(c), &e])),
        }
    }

    /// Доделка, которую компонент отложил при открытии (у AmneziaWG — пока шёл установщик Windows): повтор на такте
    /// планировщика, пока компонент не доделает или не отложит снова. Идёт работа — пропуск: она сама распорядится
    /// остатком.
    fn retry_deferred_recovery(&self) {
        if lock(&self.busy).is_some() {
            return;
        }
        for c in ORDER.into_iter().filter(|c| self.ops(*c).recover_deferred()) {
            self.recover_component(c);
        }
    }

    /// Прошлый запуск не довёл шаг после замены: строка в журнал. Сам повтор — на такте планировщика (`settle_owed`),
    /// не здесь: ядро в этот момент может ещё запускаться.
    fn announce_owed(&self) {
        let names: Vec<String> = lock(&self.owed).components.iter().map(|c| self.name(*c)).collect();
        if !names.is_empty() {
            self.shared.log("", Severity::Warn, &trf("updm.after_change_owed", &[&names.join(", ")]));
        }
    }

    /// Возврат AmneziaWG, оборванный остановкой ядра: его строка истории становится ошибкой «прервано».
    fn mark_interrupted(&self) {
        match load_json::<u64>(&self.dir.join(STARTED)) {
            Ok(id) => {
                let text = tr("updm.interrupted");
                self.amend(id, &text);
                self.shared.log("", Severity::Bad, &trf("updm.restore_failed", &[&self.name(Component::Native), &text]));
            }
            // Нет отметки — прошлый раз всё закончилось; испорченная — строку истории не найти, но сказать об этом надо.
            Err(e) if self.dir.join(STARTED).exists() => self.shared.log("", Severity::Bad, &e),
            Err(_) => {}
        }
        self.clear_started();
    }

    /// Убрать отметку `STARTED`; не убралась — в журнал: при следующем открытии работа будет считаться прерванной.
    fn clear_started(&self) {
        let path = self.dir.join(STARTED);
        if let Err(e) = std::fs::remove_file(&path) {
            if path.exists() {
                self.shared.log("", Severity::Bad, &crate::fsutil::io_ctx(&path, e));
            }
        }
    }

    pub(super) fn ops(&self, c: Component) -> &dyn ComponentOps {
        self.components.get(c)
    }

    /// Перед заменой `c`: шаг после замены становится долгом, пока его не примут (`after_change`).
    fn owe(&self, c: Component) {
        if !self.ops(c).owes_after_change() {
            return;
        }
        let mut owed = lock(&self.owed);
        if !owed.components.contains(&c) {
            owed.components.push(c);
            self.save(OWED, &owed.components);
        }
    }

    /// Замена `c` вернулась с ошибкой: файлы не заменены или откачены, шаг после замены не нужен.
    fn forgive(&self, c: Component) {
        let mut owed = lock(&self.owed);
        if owed.components.contains(&c) {
            owed.components.retain(|x| *x != c);
            self.save(OWED, &owed.components);
        }
    }

    /// Шаг после удачной замены компонента; не удался — в журнал, долг остаётся и повторяется на такте планировщика
    /// (уже без новой строки в журнале); сама замена остаётся удачной.
    fn after_change(&self, c: Component) {
        let mut owed = lock(&self.owed);
        match self.ops(c).after_change() {
            Ok(()) => self.settled(&mut owed, c),
            Err(e) => {
                self.shared.log("", Severity::Bad, &trf("updm.after_change_failed", &[&self.name(c), &e]));
                owed.failure_logged = true;
            }
        }
    }

    /// Повтор невыполненных шагов после замены (такт планировщика). Идёт работа — пропуск: свою отметку она снимет
    /// сама, а запрос посреди замены файлов снял бы её раньше времени. Неудача пишется в журнал один раз до
    /// ближайшего успеха.
    fn settle_owed(&self) {
        let mut owed = lock(&self.owed);
        if owed.components.is_empty() || lock(&self.busy).is_some() {
            return;
        }
        for c in owed.components.clone() {
            match self.ops(c).after_change() {
                Ok(()) => {
                    self.shared.log("", Severity::Info, &trf("updm.after_change_resent", &[&self.name(c)]));
                    self.settled(&mut owed, c);
                }
                Err(e) if !owed.failure_logged => {
                    self.shared.log("", Severity::Warn, &trf("updm.after_change_retry", &[&self.name(c), &e]));
                    owed.failure_logged = true;
                }
                Err(_) => {}
            }
        }
    }

    /// Шаг после замены `c` принят: долг снят и на диске.
    fn settled(&self, owed: &mut Owed, c: Component) {
        owed.components.retain(|x| *x != c);
        if owed.components.is_empty() {
            owed.failure_logged = false;
        }
        self.save(OWED, &owed.components);
    }

    /// Название компонента для журнала и строки занятости.
    pub(super) fn name(&self, c: Component) -> String {
        self.ops(c).name()
    }

    /// Установленные версии всех компонентов (в порядке `ORDER`).
    fn installed(&self) -> Vec<(Component, Option<String>)> {
        ORDER.iter().map(|c| (*c, self.ops(*c).installed())).collect()
    }

    /// Команда окна: состояние сразу; проверка, обновление и возврат — в фоне (идёт другая работа — отказ).
    pub fn handle(self: &Arc<Self>, op: UpdateOp) -> Result<UpdatesState, String> {
        match op {
            UpdateOp::State => {}
            UpdateOp::Check => self.start(Busy::Checking, |m| {
                m.check();
            })?,
            UpdateOp::Apply(targets) => {
                let order = apply_order(&targets);
                self.start(Busy::Checking, move |m| {
                    m.apply(&order);
                    m.tidy();
                })?
            }
            UpdateOp::Restore(id) => {
                let (name, info) = self.restore_target(id)?;
                self.start(Busy::Restore { what: info.component, version: info.version.clone() }, move |m| {
                    m.restore(&name, &info);
                    m.tidy();
                })?
            }
        }
        Ok(self.state())
    }

    fn state(&self) -> UpdatesState {
        let installed = self.installed();
        let data = lock(&self.data);
        UpdatesState {
            components: rows(&data.saved.components, data.saved.upstream.as_ref(), &installed, data.history.entries(), &data.saved.released),
            history: data.history.entries().to_vec(),
            restores: restore_offers(data.history.entries(), &installed, &|n| self.backup_on_disk(n)),
            checked_at: data.saved.checked_at,
            busy: lock(&self.busy).as_ref().map(|b| b.text(|c| self.name(c))),
        }
    }

    /// Занять менеджер работой; уже занят — `false`.
    fn begin(&self, work: Busy) -> bool {
        let mut busy = lock(&self.busy);
        if busy.is_some() {
            return false;
        }
        *busy = Some(work);
        true
    }

    /// Работа с обновлениями разбирает данные из сети: её паника — ошибка этой работы в журнале, а не остановка
    /// ядра. Иначе одна и та же паника на каждой проверке после перезапуска исчерпала бы перезапуски службы, и ядро,
    /// которое держит туннели, осталось бы лежать из-за источника обновлений.
    fn isolated(&self, work: impl FnOnce()) {
        if let Err(panic) = crate::crash::isolate(work) {
            self.shared.log("", Severity::Bad, &trf("updm.failed", &[&panic]));
        }
    }

    /// Перед MSI родного AmneziaWG: работает ли его окно в сеансе пользователя (`open`). За работу MSI может идти
    /// не один раз (возврат и установка обратно) — окно было открыто, если хоть раз было.
    pub(super) fn note_native_ui(&self, open: bool) {
        let mut seen = lock(&self.native_ui_in_job);
        *seen = Some(seen.unwrap_or(false) || open);
    }

    /// Отметка последней законченной работы с MSI для `State` агента.
    pub fn native_ui(&self) -> NativeUiMark {
        lock(&self.data).saved.native_ui
    }

    /// Конец работы: был MSI — новый номер и было ли окно AmneziaWG открыто, в `state.json`.
    fn finish_native_ui(&self) {
        let Some(was_open) = lock(&self.native_ui_in_job).take() else { return };
        let mut data = lock(&self.data);
        data.saved.native_ui = NativeUiMark { seq: data.saved.native_ui.seq + 1, was_open };
        self.save(STATE, &data.saved);
    }

    pub(super) fn set_busy(&self, work: Busy) {
        *lock(&self.busy) = Some(work);
    }

    /// Работа в своём потоке; занятость снимается по её окончании.
    fn start(self: &Arc<Self>, work: Busy, job: impl FnOnce(&Manager) + Send + 'static) -> Result<(), String> {
        if !self.begin(work) {
            return Err(tr("updm.busy"));
        }
        let m = self.clone();
        crate::crash::spawn_named("update-job", move || {
            let _done = JobDone(&m);
            m.isolated(|| job(&m));
        });
        Ok(())
    }

    /// Фоновый поток: раз в `TICK` решает, пора ли проверять (`daily_tick`).
    fn daily(self: Arc<Self>) {
        self.daily_until(&|| false);
    }

    /// Цикл планировщика — вторичный (`crash::nonfatal_loop`): паника шага вне самой проверки (часы, чтение
    /// состояния, уборка загрузок) — запись в журнал и пауза, а не остановка ядра. `done` — только для проверок:
    /// в ядре цикл идёт, пока жив процесс.
    fn daily_until(&self, done: &dyn Fn() -> bool) {
        let mut first = true;
        let report = |panic: &str, wait: Duration| self.shared.report_secondary_panic(panic, wait);
        crate::crash::nonfatal_loop(TICK, &|d| self.clock.sleep(d), &report, || {
            self.daily_tick(&mut first);
            if done() {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        });
    }

    /// Один шаг планировщика: первая проверка — через `FIRST_CHECK` после запуска, дальше — когда с последней прошли
    /// сутки. Занято другой работой — шаг пропускается, `first` не меняется. Возвращает, была ли проверка.
    fn daily_tick(&self, first: &mut bool) -> bool {
        self.settle_owed();
        self.retry_deferred_recovery();
        let checked = lock(&self.data).saved.checked_at;
        if !check_due(*first, self.clock.uptime(), checked, self.clock.now()) || !self.begin(Busy::Checking) {
            return false;
        }
        let _done = JobDone(self);
        self.isolated(|| drop(self.check()));
        *first = false;
        true
    }

    /// Сбой проверки компонента: в таблице окна — фраза (`upd.st_failed`), технический текст — здесь, в журнале.
    /// Один источник на два компонента (наш релиз: движок и программа) — одна запись с обоими именами.
    fn log_check_failures(&self, results: &[CheckResult]) {
        let mut failed: Vec<(&str, Vec<String>)> = Vec::new();
        for r in results.iter().filter(|r| !r.manual_only) {
            let Some(e) = r.error.as_deref() else { continue };
            match failed.iter_mut().find(|(seen, _)| *seen == e) {
                Some((_, names)) => names.push(self.name(r.component)),
                None => failed.push((e, vec![self.name(r.component)])),
            }
        }
        for (e, names) in failed {
            self.shared.log("", Severity::Warn, &crate::explain::log_line(&trf("updm.check_failed", &[&names.join(", ")]), e));
        }
    }

    /// Запрос источников; результат — в `state.json`, о новых версиях — по одному событию в журнал.
    fn check(&self) -> Fetched {
        self.set_busy(Busy::Checking);
        let fetched = Fetched { native: self.sources.latest_native(), ours: self.sources.latest_ours() };
        let upstream = UpstreamCheck::from(self.sources.latest_engine_tag());
        // Сбой этой проверки в таблице — одно нейтральное «Не удалось проверить»; причина — только здесь.
        if let Some(e) = &upstream.error {
            self.shared.log("", Severity::Warn, &trf("updm.upstream_failed", &[e]));
        }
        let installed = self.installed();
        let results = available_rows(&fetched);
        self.log_check_failures(&results);
        let released = self.release_dates(&fetched, &results, &installed);
        let news = {
            let mut data = lock(&self.data);
            data.saved.components = results;
            data.saved.released = released;
            data.saved.upstream = Some(upstream);
            data.saved.checked_at = Some(self.clock.now());
            let current = rows(&data.saved.components, data.saved.upstream.as_ref(), &installed, data.history.entries(), &data.saved.released);
            let news = to_announce(&current, &mut data.saved.announced);
            self.save(STATE, &data.saved);
            news
        };
        for (c, v) in news {
            self.shared.log("", Severity::Info, &trf("updm.available", &[&self.name(c), &v]));
        }
        fetched
    }

    /// Даты выхода для `state.json`: найденных проверкой версий и установленных сейчас. У установленной версии, которой
    /// нет среди найденных и запомненных, дату спрашивает у источника сам компонент (AmneziaWG — релиз по метке);
    /// сбой запроса — предупреждение в журнале, дата останется неизвестной до следующей проверки. Источник вообще
    /// не ответил (сети нет) — не спрашиваем: причина уже в таблице.
    fn release_dates(&self, fetched: &Fetched, results: &[CheckResult], installed: &[(Component, Option<String>)]) -> Vec<ReleaseDate> {
        let mut released = lock(&self.data).saved.released.clone();
        remember_found(&mut released, results);
        if fetched.native.is_ok() {
            for (c, version) in unknown_release_dates(&released, results, installed) {
                match self.ops(c).release_date(self, &version) {
                    Ok(Some(at)) if at > 0 => remember(&mut released, c, &version, at),
                    Ok(_) => {}
                    Err(e) => self.shared.log("", Severity::Warn, &trf("updm.release_date_failed", &[&self.name(c), &version, &e])),
                }
            }
        }
        prune_released(&mut released, results, installed);
        released
    }

    /// Свежая проверка, затем выбранные компоненты с обновлением — по порядку `ORDER`, каждый только до версии,
    /// которую подтвердил пользователь.
    fn apply(&self, targets: &[(Component, String)]) {
        let fetched = self.check();
        for (c, confirmed) in targets {
            self.apply_one(*c, &fetched, confirmed);
        }
    }

    /// Свежая проверка нашла ту же версию, что подтвердил пользователь. Нет — строка обновления с ошибкой,
    /// компонент не трогается.
    fn confirmed(&self, c: Component, from: &str, found: &str, confirmed: &str) -> bool {
        let Err(e) = check_confirmed(found, confirmed) else { return true };
        self.fail_update(c, Some(from), confirmed, e);
        false
    }

    /// Обновление не началось (источник недоступен, компонента нет): строка истории с ошибкой и запись в журнал.
    /// Без неё подтверждённое пользователем обновление исчезало молча — ошибка видна была только в таблице компонентов.
    fn fail_update(&self, c: Component, from: Option<&str>, confirmed: &str, e: String) {
        let mut x = entry(self.next_id(), c, Action::Update, from.map(str::to_string), Some(confirmed.to_string()));
        x.finish(Err(e.clone()));
        self.record(x);
        self.shared.log("", Severity::Bad, &trf("updm.update_failed", &[&self.name(c), &e]));
    }

    /// Уже стоит версия не ниже найденной (поставили, пока окно ждало подтверждения): не сбой, строки истории нет —
    /// только запись в журнал.
    fn log_up_to_date(&self, c: Component, installed: &str) {
        self.shared.log("", Severity::Info, &format!("{}: {} ({installed})", self.name(c), tr("upd.st_ok")));
    }

    /// Обновление компонента `c`: источник ответил, компонент установлен, версия та, что подтвердил пользователь, и
    /// новее установленной — копия текущей версии, установка, строка истории, событие в журнале.
    fn apply_one(&self, c: Component, f: &Fetched, confirmed: &str) {
        let ops = self.ops(c);
        let installed = ops.installed();
        let to = match ops.found(f) {
            Ok(to) => to,
            Err(e) => return self.fail_update(c, installed.as_deref(), confirmed, e),
        };
        let Some(from) = installed else { return self.fail_update(c, None, confirmed, tr("upd.not_installed")) };
        if !self.confirmed(c, &from, &to, confirmed) {
            return;
        }
        if !feed::newer(&to, &from) {
            return self.log_up_to_date(c, &from);
        }
        // Неудача копии записана самой `backup` (строка и журнал).
        let Some((backup_id, _)) = self.backup(c, &from) else { return };
        let id = self.next_id();
        let mut entry = entry(id, c, Action::Update, Some(from.clone()), Some(to.clone()));
        entry.prior_backup = Some(backup_id);
        self.owe(c);
        let result = self.run_journaled(ops.journal(Action::Update), entry, || ops.update(self, f, &to, id));
        match result {
            Ok(()) => {
                self.shared.log("", Severity::Info, &trf("updm.updated", &[&ops.name(), &from, &to]));
                self.after_change(c);
            }
            Err(e) => {
                self.forgive(c);
                self.shared.log("", Severity::Bad, &trf("updm.update_failed", &[&ops.name(), &e]));
            }
        }
    }

    /// Папка копии на диске: имя безопасно (не путь) и папка есть.
    fn backup_on_disk(&self, name: &str) -> bool {
        safe_name(name) && self.dir.join(BACKUPS).join(name).is_dir()
    }

    /// Цель `Restore(id)`: папка копии и её описание (из самой папки, не из запроса окна). Цель выбирает то же
    /// правило, что показывает кнопке окно (`restore_offers`).
    fn restore_target(&self, id: u64) -> Result<(String, BackupInfo), String> {
        let component = lock(&self.data).history.entries().iter().find(|e| e.id == id).map(|e| e.component);
        let installed = component.and_then(|c| self.ops(c).installed());
        let resolved = {
            let data = lock(&self.data);
            resolve_restore(data.history.entries(), id, installed.as_deref(), &|n| self.backup_on_disk(n))
        };
        let name = match resolved {
            Some(Ok(target)) => target.backup,
            Some(Err(RestoreBlock::Installed)) => return Err(tr("updm.already_installed")),
            Some(Err(RestoreBlock::TooOld)) => return Err(trf("upd.restore_too_old", &[super::restore_target::MIN_APP_RESTORE])),
            Some(Err(RestoreBlock::NoCopy)) | None => return Err(tr("updm.no_backup")),
        };
        let info: BackupInfo = load_json(&self.dir.join(BACKUPS).join(&name).join(BACKUP_INFO))
            .map_err(|e| trf("updm.bad_backup", &[&name, &e]))?;
        Ok((name, info))
    }

    /// Возврат к копии `name`: сначала копия текущего состояния (чтобы можно было вернуться обратно).
    fn restore(&self, name: &str, info: &BackupInfo) {
        let c = info.component;
        let ops = self.ops(c);
        let dir = self.dir.join(BACKUPS).join(name);
        let current = ops.installed();
        let mut current_backup = None;
        if let Some(cur) = &current {
            match self.backup(c, cur) {
                Some(made) => current_backup = Some(made),
                None => return,
            }
        }
        let id = self.next_id();
        self.set_busy(Busy::Restore { what: info.component, version: info.version.clone() });
        let mut entry = entry(id, c, Action::Restore, current, Some(info.version.clone()));
        entry.prior_backup = current_backup.as_ref().map(|(backup_id, _)| *backup_id);
        let job = RestoreJob { id, name, dir: &dir, info, current_backup: current_backup.as_ref().map(|(_, n)| n.as_str()) };
        self.owe(c);
        match self.run_journaled(ops.journal(Action::Restore), entry, || ops.restore(self, &job)) {
            Ok(()) => {
                self.shared.log("", Severity::Info, &trf("updm.restored", &[&ops.name(), &info.version]));
                self.after_change(c);
            }
            Err(e) => {
                self.forgive(c);
                self.shared.log("", Severity::Bad, &trf("updm.restore_failed", &[&ops.name(), &e]));
            }
        }
    }

    /// Строка истории вокруг `work`, записанная тогда, когда велит `journal` (см. `Journal`).
    fn run_journaled(&self, journal: Journal, entry: HistoryEntry, work: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
        match journal {
            Journal::After => {
                let mut entry = entry;
                let result = work();
                entry.finish(result.clone());
                self.record(entry);
                result
            }
            Journal::Before => {
                let id = entry.id;
                self.record(entry);
                let result = work();
                if let Err(e) = &result {
                    self.amend(id, e);
                }
                result
            }
            Journal::Started => self.run_started(entry, work),
        }
    }

    /// Строка истории до работы и отметка `STARTED` на время работы: ядро остановилось посередине — при следующем
    /// открытии строка станет ошибкой «прервано». Ошибка работы дописывается в ту же строку.
    fn run_started(&self, entry: HistoryEntry, work: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
        let id = entry.id;
        self.record(entry);
        self.save(STARTED, &id);
        let result = work();
        if let Err(e) = &result {
            self.amend(id, e);
        }
        self.clear_started();
        result
    }

    /// Занять номер строки истории (см. `History::allocate`); строку после этого записывает `record`.
    pub(super) fn next_id(&self) -> u64 {
        lock(&self.data).history.allocate()
    }

    pub(super) fn record(&self, entry: HistoryEntry) {
        let mut data = lock(&self.data);
        data.history.push(entry);
        self.save_history(&data.history);
    }

    /// Дописать ошибку в строку `id`; такой строки нет — предупреждение в журнал (ошибка работы иначе пропала бы).
    fn amend(&self, id: u64, error: &str) {
        let mut data = lock(&self.data);
        match data.history.amend(id, error) {
            Ok(()) => self.save_history(&data.history),
            Err(e) => self.shared.log("", Severity::Warn, &e),
        }
    }

    /// Папки копий без строки истории — снова строки (`adopt_orphans`), сохраняются сразу: следующий запуск видит их
    /// уже строками.
    fn adopt_orphan_backups(&self) {
        let mut data = lock(&self.data);
        if adopt_orphans(&mut data.history, &self.dir.join(BACKUPS), &|c| self.name(c), &self.shared) {
            self.save_history(&data.history);
        }
    }

    fn save_history(&self, history: &History) {
        if let Err(e) = history.save() {
            self.shared.log("", Severity::Bad, &e);
        }
    }

    fn save<T: Serialize>(&self, file: &str, value: &T) {
        if let Err(e) = save_json(&self.dir.join(file), value) {
            self.shared.log("", Severity::Bad, &e);
        }
    }

    pub(super) fn log_path(&self, id: u64, what: &str) -> PathBuf {
        self.dir.join(LOGS).join(format!("{id}-{what}.log"))
    }

    /// Пределы хранилища (после каждой работы и при открытии): строки истории сверх `HISTORY_MAX` и копии сверх
    /// `BACKUPS_MAX` — сначала из истории (у строк пропадает «Вернуть»), затем их папки; журналы сверх `LOGS_MAX`.
    fn tidy(&self) {
        let pruned = {
            let mut data = lock(&self.data);
            let len = data.history.entries().len();
            let pruned = data.history.trim();
            if !pruned.is_empty() || data.history.entries().len() != len {
                self.save_history(&data.history);
            }
            pruned
        };
        for name in pruned.iter().filter(|n| safe_name(n)) {
            let dir = self.dir.join(BACKUPS).join(name);
            if let Err(e) = std::fs::remove_dir_all(&dir) {
                if dir.exists() {
                    self.shared.log("", Severity::Bad, &crate::fsutil::io_ctx(&dir, e));
                }
            }
        }
        for e in rotate_logs(&self.dir.join(LOGS), LOGS_MAX) {
            self.shared.log("", Severity::Bad, &e);
        }
    }

    /// Недокачанное и отброшенное из `downloads`. Что не удалилось, остаётся до следующего запуска (`tidy`) и мешает
    /// только местом на диске: имена загрузок не повторяются.
    fn clean_downloads(&self) {
        let dir = self.dir.join(DOWNLOADS);
        if let Ok(list) = std::fs::read_dir(&dir) {
            for e in list.flatten() {
                let p = e.path();
                let _ = if p.is_dir() { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) };
            }
        }
    }
}

/// Хранилище обновлений ядра (`<папка данных>\updates`).
pub fn store_dir() -> PathBuf {
    crate::daemon::data_dir().join("updates")
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::jsonstore::{backup_name, temp};
    use super::super::history::{hist, BACKUPS_MAX};
    use super::super::clock::fake::FakeClock;
    use super::super::clock::{CHECK_EVERY, FIRST_CHECK};
    use super::super::sources::fake::FakeSources;
    use super::super::component::fake::{components, Calls, FakeOps};
    use super::super::core_link::fake::RecordingCore;
    use super::super::ours;
    use super::super::Available;
    use std::path::Path;
    use std::time::Duration;
    use crate::monitor::Options;

    fn manager(dir: &Path) -> Arc<Manager> {
        with_ops(dir, Components::real(Arc::new(RecordingCore::default())))
    }

    /// Менеджер с заданными компонентами (обычно подделками `FakeOps`); источники недоступны.
    fn with_ops(dir: &Path, components: Components) -> Arc<Manager> {
        Manager::for_tests(dir, components)
    }

    /// Компоненты, у которых версию из проверки находит настоящая реализация, а установлены — `installed`
    /// (AmneziaWG, движок, программа).
    fn real_found(installed: [Option<&str>; 3]) -> Components {
        let calls = Calls::default();
        let [n, e, a] = installed;
        components(FakeOps::real(Component::Native, n, &calls), FakeOps::real(Component::Engine, e, &calls), FakeOps::real(Component::App, a, &calls))
    }

    /// `state.json` прежних версий хранил целую строку окна (установленная версия, `update`) и строку без компонента;
    /// новый формат — только результат проверки. Прежний читается, новый не содержит устаревающих полей.
    /// `state.json` с версией движка в прежнем виде («версия · wintun x» одной строкой) читается раздельно, а
    /// «есть обновление» и объявление считаются по голой версии.
    #[test]
    fn state_file_with_annotated_engine_version_is_split_on_load() {
        let dir = temp("state-annotated");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(STATE),
            r#"{"checked_at":42,"components":[
                {"component":"Engine","available":{"version":"3.2 · wintun 0.14.1","published":1,"notes":"n"},"error":null}],
              "announced":[]}"#,
        )
        .unwrap();
        let m = manager(&dir);
        let a = row(&m, Component::Engine).available.unwrap();
        assert_eq!((a.version.as_str(), a.wintun.as_deref()), ("3.2", Some("0.14.1")));
        m.save(STATE, &lock(&m.data).saved);
        let text = std::fs::read_to_string(dir.join(STATE)).unwrap();
        assert!(!text.contains("3.2 ·") && text.contains("0.14.1"), "сохраняется раздельно: {text}");
        let again = row(&manager(&dir), Component::Engine).available.unwrap();
        assert_eq!((again.version.as_str(), again.wintun.as_deref()), ("3.2", Some("0.14.1")), "новый файл читается");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_state_file_loads_and_new_one_has_only_the_check_result() {
        let dir = temp("state-old-file");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(STATE),
            r#"{"checked_at":42,"components":[
                {"component":"Native","installed":"1.0.4","installed_at":7,"available":{"version":"1.0.5","published":1,"notes":"n"},"update":true,"error":null},
                {"component":null,"installed":null,"installed_at":null,"available":null,"update":false,"error":"x"},
                {"component":"App","installed":"0.1","installed_at":null,"available":null,"update":false,"error":"offline"}],
              "announced":[["Native","1.0.5"]]}"#,
        )
        .unwrap();
        let m = manager(&dir);
        {
            let data = lock(&m.data);
            assert_eq!(data.saved.checked_at, Some(42));
            assert_eq!(data.saved.announced, vec![(Component::Native, "1.0.5".to_string())]);
            assert_eq!(data.saved.components.len(), 2, "строка без компонента отброшена");
        }
        assert_eq!(row(&m, Component::Native).available.map(|a| a.version), Some("1.0.5".to_string()));
        assert_eq!(row(&m, Component::App).error.as_deref(), Some("offline"));
        m.save(STATE, &lock(&m.data).saved);
        let text = std::fs::read_to_string(dir.join(STATE)).unwrap();
        assert!(!text.contains("installed") && !text.contains("\"update\""), "{text}");
        let again = manager(&dir);
        assert_eq!(lock(&again.data).saved.components.len(), 2, "новый файл читается");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn history_and_state_survive_reopen() {
        let dir = temp("persist");
        {
            let m = manager(&dir);
            m.record(hist(1, Component::Native, Action::Backup, None, Some("1-native-1.0")));
            m.record(hist(2, Component::Native, Action::Update, Some("1.1"), None));
            m.amend(2, "boom");
            let mut data = lock(&m.data);
            data.saved.checked_at = Some(42);
            data.saved.announced = vec![(Component::App, "0.4.0".into())];
            m.save(STATE, &data.saved);
        }
        let m = manager(&dir);
        let data = lock(&m.data);
        assert_eq!(data.history.entries().iter().map(|e| e.id).collect::<Vec<_>>(), vec![2, 1], "новые сверху");
        assert_eq!((data.history.entries()[0].ok, data.history.entries()[0].error.as_deref()), (false, Some("boom")));
        assert_eq!(data.history.entries()[1].backup.as_deref(), Some("1-native-1.0"));
        assert_eq!(data.saved.checked_at, Some(42));
        assert_eq!(data.saved.announced, vec![(Component::App, "0.4.0".to_string())]);
        assert!(dir.join(BACKUPS).is_dir() && dir.join(LOGS).is_dir() && dir.join(DOWNLOADS).is_dir());
        drop(data);
        let _ = std::fs::remove_dir_all(&dir);
    }


    fn manifest(version: &str, engine: &str) -> sign::Manifest {
        let file = |name: &str| sign::FileEntry { name: name.into(), sha256: "0".repeat(64), size: 1 };
        sign::Manifest {
            version: version.into(),
            published: "2026-01-01T00:00:00Z".into(),
            app: file("awg-ui.exe"),
            engine: sign::Engine { version: engine.into(), wintun: "0.14".into(), files: vec![file("tunnel.dll"), file("wintun.dll")] },
        }
    }

    /// Менеджер с подделками источников и часов (в `Clock` — unix-время `at`).
    fn faked(dir: &Path, sources: FakeSources, at: u64) -> (Arc<Manager>, Arc<FakeSources>, Arc<FakeClock>) {
        let options = Options { ping: false, ping_host: String::new(), notify: false, tray: false, taskbar: false };
        let shared = Arc::new(Shared::new(None, options, None));
        let (sources, clock) = (Arc::new(sources), Arc::new(FakeClock::new(at)));
        let m = Manager::open(dir.to_path_buf(), shared, Components::real(Arc::new(RecordingCore::default())), sources.clone(), clock.clone());
        (Arc::new(m), sources, clock)
    }

    fn logged(m: &Manager) -> Vec<(Severity, String)> {
        m.shared.events_since(0).into_iter().map(|(_, e)| (e.severity, e.text)).collect()
    }

    fn row(m: &Manager, c: Component) -> CheckResult {
        lock(&m.data).saved.components.iter().find(|r| r.component == c).cloned().unwrap()
    }

    #[test]
    fn check_with_unreachable_sources_keeps_errors_and_announces_nothing() {
        let dir = temp("check-down");
        let (m, _, _) = faked(&dir, FakeSources::down(), 1_000);
        let fetched = m.check();
        assert_eq!((fetched.native.clone().err(), fetched.ours.clone().err().map(|e| e.message)), (Some("offline".to_string()), Some("offline".to_string())));
        for c in [Component::Native, Component::Engine, Component::App] {
            let r = row(&m, c);
            assert_eq!((r.error.as_deref(), r.available.is_none()), (Some("offline"), true), "{c:?}");
        }
        assert_eq!(lock(&m.data).saved.checked_at, Some(1_000), "проверка с ошибками — тоже проверка: повтор не раньше чем через сутки");
        // В таблице окна — фраза «Не удалось проверить», технический текст — в журнале: одна запись на источник
        // (все компоненты с той же ошибкой — в одной), плюс сбой проверки метки движка.
        let offline: Vec<(Severity, String)> = logged(&m).into_iter().filter(|(_, t)| t.contains("offline")).collect();
        assert_eq!(offline.len(), 2, "{offline:?}");
        assert!(offline.contains(&(Severity::Warn, trf("updm.upstream_failed", &["offline"]))), "{offline:?}");
        let prefix = trf("updm.check_failed", &[""]);
        assert!(offline.iter().any(|(s, t)| *s == Severity::Warn && t.starts_with(&prefix) && t.ends_with("(offline)")), "{offline:?}");
        assert!(lock(&m.busy).is_some(), "check сам занятость не снимает — это делает JobDone");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_finds_update_and_announces_it_once() {
        let dir = temp("check-update");
        let ours = (release("99.0.0"), manifest("99.0.0", "99.1"));
        let (m, _, clock) = faked(&dir, FakeSources::new(Err("offline".into()), Ok(ours)), 5_000);
        m.check();
        let (app, engine, native) = (row(&m, Component::App), row(&m, Component::Engine), row(&m, Component::Native));
        assert_eq!(app.available.as_ref().map(|a| a.version.as_str()), Some("99.0.0"));
        assert_eq!(engine.available.as_ref().map(|a| a.version.as_str()), Some("99.1"), "версия движка — без пояснения");
        assert_eq!(engine.available.as_ref().map(Available::shown).as_deref(), Some("99.1 · wintun 0.14"), "для показа пояснение добавляется");
        assert_eq!((app.error, native.error.as_deref()), (None, Some("offline")), "ошибка одного источника не мешает другому");
        // Строки окна считают «есть обновление» по установленной версии: у программы она известна всегда.
        assert!(m.state().components.iter().find(|r| r.component == Some(Component::App)).unwrap().update);
        let announce = trf("updm.available", &[&m.name(Component::App), "99.0.0"]);
        let count = |m: &Manager| logged(m).iter().filter(|(s, t)| *s == Severity::Info && *t == announce).count();
        assert_eq!(count(&m), 1);
        clock.sleep(Duration::from_secs(60));
        m.check();
        assert_eq!(count(&m), 1, "о той же версии второй раз не сообщается");
        assert_eq!(lock(&m.data).saved.checked_at, Some(5_060));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_when_already_current_has_no_update_and_no_news() {
        let dir = temp("check-current");
        let current = ours::app_version();
        let (m, _, _) = faked(&dir, FakeSources::new(Err("offline".into()), Ok((release(&current), manifest(&current, "0.0")))), 1);
        m.check();
        let app = m.state().components.into_iter().find(|r| r.component == Some(Component::App)).unwrap();
        assert_eq!((app.update, app.available.map(|a| a.version)), (false, Some(current)));
        assert!(logged(&m).iter().all(|(_, t)| !t.contains(&m.name(Component::App))), "{:?}", logged(&m));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Менеджер с подделками источников и компонентов: движок установлен `3.1.20260814`, программа `0.3.0`.
    fn faked_engine(dir: &Path, sources: FakeSources) -> (Arc<Manager>, Arc<FakeSources>) {
        let options = Options { ping: false, ping_host: String::new(), notify: false, tray: false, taskbar: false };
        let shared = Arc::new(Shared::new(None, options, None));
        let calls = Calls::default();
        let comps = components(
            FakeOps::new(Component::Native, Some("3.1.0"), "3.1.0", &calls),
            FakeOps::new(Component::Engine, Some("3.1.20260814"), "3.1.20260814", &calls),
            FakeOps::new(Component::App, Some("0.3.0"), "0.3.0", &calls),
        );
        let sources = Arc::new(sources);
        (Arc::new(Manager::open(dir.to_path_buf(), shared, comps, sources.clone(), Arc::new(FakeClock::new(1_000)))), sources)
    }

    fn engine_row(m: &Manager) -> crate::update::ComponentState {
        m.state().components.into_iter().find(|r| r.component == Some(Component::Engine)).unwrap()
    }

    fn sources_with_engine_tag(tag: Result<&str, &str>) -> FakeSources {
        let s = FakeSources::down();
        *s.engine_tag.lock().unwrap() = tag.map(String::from).map_err(String::from);
        s
    }

    #[test]
    fn engine_equal_to_newest_amnezia_tag_is_current_and_nothing_is_logged() {
        use crate::update::EngineUpstream as Up;
        let dir = temp("upstream-equal");
        let (m, _) = faked_engine(&dir, sources_with_engine_tag(Ok("v3.1.20260814")));
        m.check();
        assert_eq!(engine_row(&m).upstream, Some(Up::Current("v3.1.20260814".into())));
        assert!(logged(&m).iter().all(|(_, t)| !t.contains("amneziawg-windows")), "{:?}", logged(&m));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn newer_amnezia_tag_is_reported_for_the_engine_row_only() {
        use crate::update::EngineUpstream as Up;
        let dir = temp("upstream-newer");
        let (m, _) = faked_engine(&dir, sources_with_engine_tag(Ok("v3.1.20260901")));
        m.check();
        let rows = m.state().components;
        assert_eq!(rows.iter().map(|r| r.upstream.clone()).collect::<Vec<_>>(), [None, Some(Up::Newer("v3.1.20260901".into())), None]);
        assert!(rows.iter().all(|r| !r.update), "новая метка Amnezia — не обновление, которое можно поставить");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn upstream_network_failure_is_unchecked_with_details_in_the_event_log() {
        use crate::update::EngineUpstream as Up;
        let dir = temp("upstream-down");
        let (m, sources) = faked_engine(&dir, sources_with_engine_tag(Err("HTTP 403 rate limit")));
        m.check();
        assert_eq!(engine_row(&m).upstream, Some(Up::Unchecked));
        assert!(logged(&m).contains(&(Severity::Warn, trf("updm.upstream_failed", &["HTTP 403 rate limit"]))), "{:?}", logged(&m));
        // Сбой не стирает результат навсегда: следующая удачная проверка возвращает метку.
        *sources.engine_tag.lock().unwrap() = Ok("v3.1.20260814".into());
        m.check();
        assert_eq!(engine_row(&m).upstream, Some(Up::Current("v3.1.20260814".into())));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn release_without_manifest_is_manual_only_not_an_error_state() {
        let dir = temp("no-manifest");
        let s = FakeSources::down();
        *s.ours.lock().unwrap() = Err(OursError::no_manifest("Release 0.3.0 has no signed update manifest".into()));
        *s.engine_tag.lock().unwrap() = Ok("v3.1.20260814".into());
        let (m, _) = faked_engine(&dir, s);
        m.check();
        let state = m.state().components;
        let (engine, app) = (&state[1], &state[2]);
        assert_eq!((app.manual_only, app.error.as_deref()), (true, Some("Release 0.3.0 has no signed update manifest")));
        assert!(engine.manual_only && engine.upstream.is_some());
        assert!(!state[0].manual_only, "сбой AmneziaWG — настоящий");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `state.json` до сверки метки движка: ни `upstream`, ни `manual_only` — читается, строка движка без вывода о
    /// метке (проверки ещё не было), прежние поля на месте.
    #[test]
    fn state_file_before_the_upstream_check_loads_without_it() {
        let dir = temp("state-pre-upstream");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(STATE),
            r#"{"checked_at":42,"components":[
                {"component":"Engine","available":{"version":"3.1.20260814","published":1,"notes":""},"error":null},
                {"component":"App","available":null,"error":"Release 0.3.0 has no signed update manifest"}],
              "announced":[]}"#,
        )
        .unwrap();
        let (m, _) = faked_engine(&dir, FakeSources::down());
        assert_eq!(lock(&m.data).saved.upstream, None);
        assert!(lock(&m.data).saved.released.is_empty(), "в прежнем state.json дат выхода нет");
        let engine = engine_row(&m);
        assert_eq!((engine.upstream, engine.manual_only, engine.available.map(|a| a.version)), (None, false, Some("3.1.20260814".to_string())));
        let app = m.state().components.into_iter().find(|r| r.component == Some(Component::App)).unwrap();
        assert!(!app.manual_only, "в старом файле признака нет: показывается как прежде (текст ошибки)");
        assert!(app.error.is_some());
        // Новая запись содержит метку и читается обратно.
        m.check();
        let (again, _) = faked_engine(&dir, FakeSources::down());
        assert_eq!(lock(&again.data).saved.upstream, lock(&m.data).saved.upstream);
        assert!(lock(&again.data).saved.upstream.as_ref().is_some_and(|u| u.error.as_deref() == Some("offline")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn native_release(tag: &str, published: u64) -> feed::Release {
        feed::Release { tag: tag.into(), version: tag.trim_start_matches('v').into(), published, notes: String::new(), assets: vec![] }
    }

    /// Менеджер, у которого AmneziaWG — настоящая реализация с установленной версией `installed` (ищет релиз по
    /// метке через `FakeSources`); движок `3.1.20260814`, программа `0.3.0`.
    fn with_native(dir: &Path, installed: &str, sources: FakeSources) -> (Arc<Manager>, Arc<FakeSources>) {
        let options = Options { ping: false, ping_host: String::new(), notify: false, tray: false, taskbar: false };
        let shared = Arc::new(Shared::new(None, options, None));
        let calls = Calls::default();
        let comps = components(
            FakeOps::real(Component::Native, Some(installed), &calls),
            FakeOps::new(Component::Engine, Some("3.1.20260814"), "3.1.20260814", &calls),
            FakeOps::new(Component::App, Some("0.3.0"), "0.3.0", &calls),
        );
        let sources = Arc::new(sources);
        (Arc::new(Manager::open(dir.to_path_buf(), shared, comps, sources.clone(), Arc::new(FakeClock::new(1_000)))), sources)
    }

    fn native_row(m: &Manager) -> crate::update::ComponentState {
        m.state().components.into_iter().find(|r| r.component == Some(Component::Native)).unwrap()
    }

    /// Установленная 3.1.0 старше последней 3.2.0: её дата выхода берётся из релиза по метке, а не равна дате
    /// установки; запоминается в `state.json`, и следующая проверка за ней в сеть не ходит.
    #[test]
    fn installed_native_release_date_comes_from_its_tag_and_is_cached() {
        let dir = temp("released-native");
        let sources = FakeSources::new(Ok(native_release("v3.2.0", 2_000)), Err("offline".into()));
        sources.tags.lock().unwrap().push(native_release("v3.1.0", 1_000));
        let (m, sources) = with_native(&dir, "3.1.0", sources);
        m.check();
        let row = native_row(&m);
        assert_eq!((row.released, row.available.map(|a| a.published)), (Some(1_000), Some(2_000)));
        assert_eq!(*sources.tag_calls.lock().unwrap(), ["3.1.0", "v3.1.0"], "метка без v не нашлась — с v");
        m.check();
        assert_eq!(sources.tag_calls.lock().unwrap().len(), 2, "дата запомнена: второй проверке релиз по метке не нужен");
        let (again, _) = with_native(&dir, "3.1.0", FakeSources::down());
        assert_eq!(native_row(&again).released, Some(1_000), "после перезапуска ядра дата читается из state.json");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Стоит последняя версия: дата — из самой проверки (та же, что в «Доступно»), запроса по метке нет.
    #[test]
    fn installed_latest_native_needs_no_tag_lookup() {
        let dir = temp("released-latest");
        let (m, sources) = with_native(&dir, "3.2.0", FakeSources::new(Ok(native_release("v3.2.0", 2_000)), Err("offline".into())));
        m.check();
        let row = native_row(&m);
        assert_eq!((row.released, row.available.map(|a| a.published), row.update), (Some(2_000), Some(2_000), false));
        assert!(sources.tag_calls.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Релиз по метке не нашёлся: даты нет (не выдумывается), в журнале предупреждение, при следующей проверке — новая попытка.
    #[test]
    fn failed_tag_lookup_leaves_the_date_unknown_and_warns() {
        let dir = temp("released-failed");
        let (m, sources) = with_native(&dir, "3.1.0", FakeSources::new(Ok(native_release("v3.2.0", 2_000)), Err("offline".into())));
        m.check();
        assert_eq!(native_row(&m).released, None);
        let want = trf("updm.release_date_failed", &[&m.name(Component::Native), "3.1.0", "fake: no release 3.1.0"]);
        assert!(logged(&m).contains(&(Severity::Warn, want)), "{:?}", logged(&m));
        m.check();
        assert_eq!(sources.tag_calls.lock().unwrap().len(), 4, "повтор при следующей проверке");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Источник AmneziaWG недоступен (сети нет): релиз по метке не запрашивается, предупреждений нет.
    #[test]
    fn offline_check_does_not_look_up_tags() {
        let dir = temp("released-offline");
        let (m, sources) = with_native(&dir, "3.1.0", FakeSources::down());
        m.check();
        assert!(sources.tag_calls.lock().unwrap().is_empty());
        assert!(logged(&m).iter().all(|(_, t)| !t.contains("3.1.0")), "{:?}", logged(&m));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Наши компоненты: дата установленной версии — из найденного проверкой, без запросов; другая версия — неизвестна.
    #[test]
    fn app_release_date_is_the_found_one_only_for_the_same_version() {
        let dir = temp("released-ours");
        let current = ours::app_version();
        let mut rel = release(&current);
        rel.published = 7_000;
        let (m, _, _) = faked(&dir, FakeSources::new(Err("offline".into()), Ok((rel, manifest(&current, "0.0")))), 1);
        m.check();
        let app = m.state().components.into_iter().find(|r| r.component == Some(Component::App)).unwrap();
        assert_eq!((app.released, app.available.map(|a| a.published)), (Some(7_000), Some(7_000)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn daily_first_check_waits_then_runs_once_a_day() {
        let dir = temp("daily");
        let (m, sources, clock) = faked(&dir, FakeSources::down(), 1_000_000);
        let calls = || *sources.latest_calls.lock().unwrap();
        let mut first = true;
        assert!(!m.daily_tick(&mut first) && calls() == 0, "сразу после запуска — рано");
        clock.sleep(FIRST_CHECK);
        assert!(m.daily_tick(&mut first) && calls() == 1 && !first, "первая проверка после FIRST_CHECK");
        assert!(lock(&m.busy).is_none(), "занятость снята");
        assert!(!m.daily_tick(&mut first) && calls() == 1, "свежая проверка — повтор не нужен");
        clock.sleep(Duration::from_secs(CHECK_EVERY - 1));
        assert!(!m.daily_tick(&mut first) && calls() == 1, "без секунды сутки");
        clock.sleep(Duration::from_secs(1));
        assert!(m.daily_tick(&mut first) && calls() == 2, "сутки прошли");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn daily_tick_skips_while_another_job_runs_and_keeps_the_first_check() {
        let dir = temp("daily-busy");
        let (m, sources, clock) = faked(&dir, FakeSources::down(), 1_000_000);
        clock.sleep(FIRST_CHECK);
        let mut first = true;
        assert!(m.begin(Busy::Backup { what: Component::App, version: "other job".into() }));
        assert!(!m.daily_tick(&mut first) && first, "занято — шаг пропущен, первая проверка не потеряна");
        assert_eq!(*sources.latest_calls.lock().unwrap(), 0);
        assert_eq!(lock(&m.busy).as_ref(), Some(&Busy::Backup { what: Component::App, version: "other job".into() }), "чужую занятость не снимает");
        drop(JobDone(&m));
        assert!(m.daily_tick(&mut first), "освободилось — проверка идёт");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Паника шага планировщика вне самой проверки (здесь — часы) — запись в журнал и следующий шаг, а не сбой ядра:
    /// первая проверка всё равно проходит по расписанию.
    #[test]
    fn daily_loop_survives_a_panic_and_still_checks() {
        let dir = temp("daily-panic");
        let (m, sources, clock) = faked(&dir, FakeSources::down(), 1_000_000);
        *clock.panics.lock().unwrap() = 1;
        let calls = || *sources.latest_calls.lock().unwrap();
        m.daily_until(&|| calls() >= 1);
        assert_eq!(calls(), 1);
        assert!(crate::crash::core_failure().is_none());
        assert!(logged(&m).iter().any(|(s, t)| *s == Severity::Bad && t.contains("clock edge")), "{:?}", logged(&m));
        assert!(lock(&m.busy).is_none(), "занятость снята");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn amend_of_unknown_entry_warns_in_the_event_log() {
        let dir = temp("amend-unknown");
        let m = manager(&dir);
        m.record(hist(1, Component::Engine, Action::Update, Some("2"), None));
        m.amend(42, "boom");
        let warned: Vec<_> = logged(&m).into_iter().filter(|(s, _)| *s == Severity::Warn).collect();
        assert_eq!(warned.len(), 1, "{warned:?}");
        assert!(warned[0].1.contains("42") && warned[0].1.contains("boom"), "{warned:?}");
        assert!(lock(&m.data).history.entries()[0].ok, "известная строка не тронута");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn failed_rows(m: &Manager) -> Vec<HistoryEntry> {
        m.data.lock().unwrap().history.entries().iter().filter(|e| !e.ok).cloned().collect()
    }

    fn release(version: &str) -> feed::Release {
        feed::parse_release(&format!(r#"{{"tag_name":"v{version}","assets":[]}}"#)).unwrap()
    }

    #[test]
    fn unreachable_source_records_failed_update() {
        let dir = temp("apply-source");
        let m = with_ops(&dir, real_found([Some("1.0"), Some("3.1"), Some("0.4.0")]));
        let fetched = Fetched { native: Err("HTTP 503".into()), ours: Err("DNS".into()) };
        m.apply_one(Component::Native, &fetched, "1.1");
        m.apply_one(Component::Engine, &fetched, "3.2");
        m.apply_one(Component::App, &fetched, "0.5.0");
        let rows = failed_rows(&m);
        assert_eq!(rows.len(), 3, "каждое подтверждённое обновление оставляет строку: {rows:?}");
        let by = |c| rows.iter().find(|e| e.component == c).unwrap();
        assert_eq!((by(Component::Native).error.as_deref(), by(Component::Native).from.as_deref(), by(Component::Native).to.as_deref()), (Some("HTTP 503"), Some("1.0"), Some("1.1")));
        assert_eq!(by(Component::Engine).error.as_deref(), Some("DNS"));
        assert_eq!(by(Component::Engine).to.as_deref(), Some("3.2"), "версия без пояснения");
        assert!(rows.iter().all(|e| e.action == Action::Update));
        // Строки переживают перезапуск ядра.
        drop(m);
        assert_eq!(failed_rows(&manager(&dir)).len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_component_records_failed_update() {
        let dir = temp("apply-missing");
        let m = with_ops(&dir, real_found([None, Some("3.1"), Some("0.4.0")]));
        m.apply_one(Component::Native, &Fetched { native: Ok(release("1.1")), ours: Err(OursError::from(String::new())) }, "1.1");
        let rows = failed_rows(&m);
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].component, rows[0].error.as_deref(), rows[0].from.as_deref()), (Component::Native, Some(tr("upd.not_installed").as_str()), None));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn already_current_is_logged_not_failed() {
        let dir = temp("apply-current");
        let m = with_ops(&dir, real_found([Some("1.1"), Some("3.1"), Some("0.4.0")]));
        let fetched = Fetched { native: Ok(release("1.1")), ours: Err(OursError::from(String::new())) };
        // Пока окно ждало подтверждения, поставили ту же версию: не сбой и не копия, работа не начинается.
        m.apply_one(Component::Native, &fetched, "1.1");
        assert!(m.data.lock().unwrap().history.entries().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Подделки трёх компонентов с общим журналом вызовов: установлены `from`, проверка находит `to`.
    fn fakes(calls: &Calls) -> [FakeOps; 3] {
        [
            FakeOps::new(Component::Native, Some("1.0"), "1.1", calls),
            FakeOps::new(Component::Engine, Some("3.1"), "3.2", calls),
            FakeOps::new(Component::App, Some("0.4.0"), "0.5.0", calls),
        ]
    }

    fn components_of([n, e, a]: [FakeOps; 3]) -> Components {
        components(n, e, a)
    }

    fn rows_of(m: &Manager, c: Component, action: Action) -> Vec<HistoryEntry> {
        lock(&m.data).history.entries().iter().filter(|e| e.component == c && e.action == action).cloned().collect()
    }

    fn took(calls: &Calls) -> Vec<String> {
        std::mem::take(&mut *calls.lock().unwrap())
    }

    /// Строка «Резервная копия» версии `version`, как её пишет `Manager::backup` (версия — в «было»).
    fn backup_row(id: u64, version: &str, name: &str) -> HistoryEntry {
        let mut e = entry(id, Component::Engine, Action::Backup, Some(version.to_string()), None);
        e.backup = Some(name.to_string());
        e
    }

    #[test]
    fn update_and_restore_rows_link_to_the_copy_made_before_them_and_restore_to_it() {
        let dir = temp("restore-link");
        let calls = Calls::default();
        let [n, e, a] = fakes(&calls);
        let m = with_ops(&dir, components(n, e, a));
        m.apply(&[(Component::Engine, "3.2".into())]);
        let update = rows_of(&m, Component::Engine, Action::Update).remove(0);
        let before = rows_of(&m, Component::Engine, Action::Backup).remove(0);
        assert_eq!(update.prior_backup, Some(before.id), "обновление ссылается на копию перед собой");
        // Строка обновления возвращает к 3.1 — той же копией, что и её строка «Резервная копия».
        let name = before.backup.clone().unwrap();
        let info = BackupInfo { component: Component::Engine, version: "3.1".into() };
        assert_eq!(m.restore_target(update.id), Ok((name.clone(), info.clone())));
        assert_eq!(m.restore_target(before.id), Ok((name.clone(), info.clone())));
        // Возврат ставит 3.1 и сохраняет заменённую 3.2: строка возврата ссылается на её копию.
        m.restore(&name, &info);
        let restore = rows_of(&m, Component::Engine, Action::Restore).remove(0);
        let replaced = rows_of(&m, Component::Engine, Action::Backup).into_iter().find(|b| b.from.as_deref() == Some("3.2")).unwrap();
        assert_eq!(restore.prior_backup, Some(replaced.id));
        assert_eq!(m.restore_target(restore.id).map(|(n, i)| (n, i.version)), Ok((replaced.backup.clone().unwrap(), "3.2".to_string())));
        // Стоит 3.1: к ней вернуть уже нельзя — ни строкой обновления, ни её копией; окну это видно в состоянии.
        assert_eq!(m.handle(UpdateOp::Restore(update.id)).err(), Some(tr("updm.already_installed")));
        assert_eq!(m.handle(UpdateOp::Restore(before.id)).err(), Some(tr("updm.already_installed")));
        let offers = m.state().restores;
        let blocked = |id| offers.iter().find(|o| o.id == id).unwrap().blocked;
        assert_eq!((blocked(update.id), blocked(before.id), blocked(restore.id)), (Some(RestoreBlock::Installed), Some(RestoreBlock::Installed), None));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pruned_copy_is_refused_by_the_core() {
        let dir = temp("restore-pruned");
        let calls = Calls::default();
        let [n, e, a] = fakes(&calls);
        let m = with_ops(&dir, components(n, e, a));
        m.apply(&[(Component::Engine, "3.2".into())]);
        let update = rows_of(&m, Component::Engine, Action::Update).remove(0);
        let name = rows_of(&m, Component::Engine, Action::Backup).remove(0).backup.unwrap();
        std::fs::remove_dir_all(dir.join(BACKUPS).join(&name)).unwrap();
        assert_eq!(m.handle(UpdateOp::Restore(update.id)).err(), Some(tr("updm.no_backup")));
        assert_eq!(m.state().restores.iter().find(|o| o.id == update.id).unwrap().blocked, Some(RestoreBlock::NoCopy));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_goes_in_order_with_a_backup_before_each_update() {
        let dir = temp("apply-order");
        let calls = Calls::default();
        let [n, e, a] = fakes(&calls);
        let m = with_ops(&dir, components(n, e, a));
        let t = |c, v: &str| (c, v.to_string());
        m.apply(&apply_order(&[t(Component::App, "0.5.0"), t(Component::Native, "1.1"), t(Component::Engine, "3.2")]));
        assert_eq!(
            took(&calls),
            [
                "backup Native 1.0", "update Native 1.1", "changed Native",
                "backup Engine 3.1", "update Engine 3.2", "changed Engine",
                "backup App 0.4.0", "update App 0.5.0", "changed App",
            ]
        );
        for (c, from, to) in [(Component::Native, "1.0", "1.1"), (Component::Engine, "3.1", "3.2"), (Component::App, "0.4.0", "0.5.0")] {
            let backup = rows_of(&m, c, Action::Backup);
            assert_eq!(backup.len(), 1, "{c:?}");
            let name = backup[0].backup.clone().unwrap();
            assert_eq!(load_json::<BackupInfo>(&dir.join(BACKUPS).join(&name).join(BACKUP_INFO)), Ok(BackupInfo { component: c, version: from.into() }));
            let update = rows_of(&m, c, Action::Update);
            assert_eq!(update.len(), 1, "{c:?}");
            assert_eq!((update[0].ok, update[0].from.as_deref(), update[0].to.as_deref()), (true, Some(from), Some(to)), "{c:?}");
            assert!(update[0].id > backup[0].id, "копия — до обновления");
            assert_eq!(m.ops(c).installed().as_deref(), Some(to));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_update_keeps_its_backup_and_history_row_and_the_rest_goes_on() {
        let dir = temp("apply-fail");
        let calls = Calls::default();
        let [n, e, a] = fakes(&calls);
        let m = with_ops(&dir, components(n, e.update_err("boom"), a));
        m.apply(&[(Component::Engine, "3.2".into()), (Component::App, "0.5.0".into())]);
        assert_eq!(took(&calls), ["backup Engine 3.1", "update Engine 3.2", "backup App 0.4.0", "update App 0.5.0", "changed App"], "неудача — без after_change");
        let backup = rows_of(&m, Component::Engine, Action::Backup);
        let name = backup[0].backup.clone().expect("копия перед неудачным обновлением остаётся с «Вернуть»");
        assert!(dir.join(BACKUPS).join(&name).join("fake").is_file());
        let update = rows_of(&m, Component::Engine, Action::Update);
        assert_eq!((update.len(), update[0].ok, update[0].error.as_deref()), (1, false, Some("boom")));
        assert_eq!(m.ops(Component::Engine).installed().as_deref(), Some("3.1"));
        let failed = trf("updm.update_failed", &[&m.name(Component::Engine), "boom"]);
        assert!(logged(&m).contains(&(Severity::Bad, failed)), "{:?}", logged(&m));
        // Строка и копия переживают перезапуск ядра.
        drop(m);
        let [n, e, a] = fakes(&calls);
        let m = with_ops(&dir, components(n, e, a));
        assert_eq!(rows_of(&m, Component::Engine, Action::Update)[0].error.as_deref(), Some("boom"));
        assert_eq!(rows_of(&m, Component::Engine, Action::Backup)[0].backup.as_deref(), Some(name.as_str()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn update_journaled_before_work_keeps_one_row_with_the_error() {
        let dir = temp("apply-before");
        let calls = Calls::default();
        let [n, e, a] = fakes(&calls);
        let m = with_ops(&dir, components(n, e, a.journal(Journal::Before).update_err("restart failed")));
        m.apply(&[(Component::App, "0.5.0".into())]);
        let update = rows_of(&m, Component::App, Action::Update);
        assert_eq!(update.len(), 1, "строка пишется до работы, ошибка дописывается в неё же: {update:?}");
        assert_eq!((update[0].ok, update[0].error.as_deref(), update[0].to.as_deref()), (false, Some("restart failed"), Some("0.5.0")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_backs_up_current_state_then_calls_after_change() {
        let dir = temp("restore-ok");
        let calls = Calls::default();
        let [n, e, a] = fakes(&calls);
        let m = with_ops(&dir, components(n, e, a));
        let old = m.backup(Component::Engine, "3.0").unwrap().1;
        took(&calls);
        let info = BackupInfo { component: Component::Engine, version: "3.0".into() };
        m.restore(&old, &info);
        assert_eq!(took(&calls), ["backup Engine 3.1", "restore Engine 3.0", "changed Engine"]);
        assert_eq!(m.ops(Component::Engine).installed().as_deref(), Some("3.0"));
        let restore = rows_of(&m, Component::Engine, Action::Restore);
        assert_eq!((restore.len(), restore[0].ok, restore[0].from.as_deref(), restore[0].to.as_deref()), (1, true, Some("3.1"), Some("3.0")));
        let backups: Vec<_> = rows_of(&m, Component::Engine, Action::Backup).iter().filter_map(|e| e.from.clone()).collect();
        assert_eq!(backups, ["3.1", "3.0"], "состояние перед возвратом тоже сохранено (новые сверху)");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_restore_records_the_error_and_skips_after_change() {
        let dir = temp("restore-fail");
        let calls = Calls::default();
        let [n, e, a] = fakes(&calls);
        let m = with_ops(&dir, components(n, e.restore_err("locked"), a));
        let old = m.backup(Component::Engine, "3.0").unwrap().1;
        took(&calls);
        m.restore(&old, &BackupInfo { component: Component::Engine, version: "3.0".into() });
        assert_eq!(took(&calls), ["backup Engine 3.1", "restore Engine 3.0"]);
        let restore = rows_of(&m, Component::Engine, Action::Restore);
        assert_eq!((restore.len(), restore[0].ok, restore[0].error.as_deref()), (1, false, Some("locked")));
        assert_eq!(m.ops(Component::Engine).installed().as_deref(), Some("3.1"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tidy_removes_pruned_backup_dirs() {
        let dir = temp("tidy");
        let m = manager(&dir);
        for id in 1..=7 {
            let name = backup_name(id, Component::Engine, "v");
            std::fs::create_dir_all(dir.join(BACKUPS).join(&name)).unwrap();
            m.record(hist(id, Component::Engine, Action::Backup, None, Some(&name)));
        }
        m.tidy();
        for id in 1..=7 {
            assert_eq!(dir.join(BACKUPS).join(backup_name(id, Component::Engine, "v")).is_dir(), id > 2, "{id}");
        }
        drop(m);
        let m = manager(&dir);
        assert_eq!(lock(&m.data).history.entries().iter().filter(|e| e.backup.is_some()).count(), BACKUPS_MAX, "история сохранена");
        drop(m);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn second_job_is_rejected_while_busy() {
        let dir = temp("busy");
        let m = manager(&dir);
        assert!(m.begin(Busy::Checking));
        assert!(!m.begin(Busy::Checking));
        assert_eq!(m.handle(UpdateOp::Check).err(), Some(tr("updm.busy")));
        assert_eq!(m.handle(UpdateOp::Apply(vec![(Component::App, "0.0.0".into())])).err(), Some(tr("updm.busy")));
        drop(JobDone(&m));
        assert!(lock(&m.busy).is_none(), "конец работы снимает занятость");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Отметка окна AmneziaWG: снимок перед MSI, новый номер в конце работы, переживает перезапуск агента.
    #[test]
    fn native_ui_mark_is_taken_before_the_msi_and_survives_a_restart() {
        let dir = temp("native-ui");
        let m = manager(&dir);
        assert_eq!(m.native_ui(), NativeUiMark::default());
        assert!(m.begin(Busy::Checking));
        drop(JobDone(&m));
        assert_eq!(m.native_ui(), NativeUiMark::default(), "работа без MSI отметку не трогает");
        // Окно было открыто перед MSI; второй снимок (установка обратно после неудачи) его уже не видит — MSI закрыл.
        assert!(m.begin(Busy::Restore { what: Component::Native, version: "2.0.1".into() }));
        m.note_native_ui(true);
        m.note_native_ui(false);
        assert_eq!(m.native_ui(), NativeUiMark::default(), "пока работа идёт, отметка прежняя");
        drop(JobDone(&m));
        assert_eq!(m.native_ui(), NativeUiMark { seq: 1, was_open: true });
        assert!(m.begin(Busy::Install { what: Component::Native, version: "3.1.0".into() }));
        m.note_native_ui(false);
        drop(JobDone(&m));
        assert_eq!(m.native_ui(), NativeUiMark { seq: 2, was_open: false }, "окно было закрыто — открывать нечего");
        drop(m);
        assert_eq!(manager(&dir).native_ui(), NativeUiMark { seq: 2, was_open: false }, "из state.json");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_needs_known_entry_with_backup() {
        let dir = temp("restore");
        let m = manager(&dir);
        m.record(hist(1, Component::Engine, Action::Update, Some("2"), None));
        m.record(backup_row(2, "1", "2-engine-1"));
        m.record(backup_row(3, "1", r"..\..\Windows"));
        assert_eq!(m.handle(UpdateOp::Restore(99)).err(), Some(tr("updm.no_backup")), "неизвестная строка");
        assert_eq!(m.handle(UpdateOp::Restore(1)).err(), Some(tr("updm.no_backup")), "строка без копии");
        assert_eq!(m.handle(UpdateOp::Restore(3)).err(), Some(tr("updm.no_backup")), "путь вместо имени");
        assert!(m.handle(UpdateOp::Restore(2)).is_err(), "папки копии нет");
        std::fs::create_dir_all(dir.join(BACKUPS).join("2-engine-1")).unwrap();
        save_json(&dir.join(BACKUPS).join("2-engine-1").join(BACKUP_INFO), &BackupInfo { component: Component::Engine, version: "1".into() }).unwrap();
        assert_eq!(m.restore_target(2), Ok(("2-engine-1".to_string(), BackupInfo { component: Component::Engine, version: "1".into() })));
        assert_eq!(m.restore_target(2), m.restore_target(2), "правило одно");
        assert!(lock(&m.busy).is_none(), "отказ не занимает менеджер");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Нечитаемая `history.json` (другой формат после возврата к другой сборке, сбой диска) не затирается следующей
    /// записью: файл отодвигается в `history.json.unreadable-<дата>` с событием в журнале, история начинается пустой;
    /// строки с именами копий остаются в отодвинутом файле. То же с `state.json`.
    #[test]
    fn unreadable_history_and_state_are_moved_aside_not_overwritten() {
        let dir = temp("history-corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(HISTORY), "{broken").unwrap();
        std::fs::write(dir.join(STATE), "[nope").unwrap();
        let m = manager(&dir);
        m.record(hist(1, Component::Engine, Action::Update, Some("2"), None));
        let aside = |name: &str| -> Vec<PathBuf> {
            std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.path()).filter(|p| p.file_name().unwrap().to_string_lossy().starts_with(&format!("{name}.unreadable-"))).collect()
        };
        let (h, s) = (aside(HISTORY), aside(STATE));
        assert_eq!((h.len(), s.len()), (1, 1), "{:?}", names_in(&dir));
        assert_eq!(std::fs::read_to_string(&h[0]).unwrap(), "{broken", "исходное содержимое цело");
        assert_eq!(std::fs::read_to_string(&s[0]).unwrap(), "[nope");
        assert_eq!(History::open(&dir).unwrap().entries().len(), 1, "новая история — только новая строка");
        let logged = logged(&m);
        for p in [&h[0], &s[0]] {
            let name = p.display().to_string();
            assert!(logged.iter().any(|(sev, t)| *sev == Severity::Bad && t.contains(&name)), "{name}: {logged:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Папка копии `name` в хранилище `dir` с описанием `info` (`None` — без `component.json`, `Some(Err)` — испорченный).
    fn stored_copy(dir: &Path, name: &str, info: Option<Result<(Component, &str), &str>>) -> PathBuf {
        let copy = dir.join(BACKUPS).join(name);
        std::fs::create_dir_all(&copy).unwrap();
        std::fs::write(copy.join("payload"), b"12345").unwrap();
        match info {
            Some(Ok((component, version))) => save_json(&copy.join(BACKUP_INFO), &BackupInfo { component, version: version.into() }).unwrap(),
            Some(Err(text)) => std::fs::write(copy.join(BACKUP_INFO), text).unwrap(),
            None => {}
        }
        copy
    }

    /// Папки копий, на которые не ссылается ни одна строка (история отодвинута как нечитаемая или строки потеряны
    /// иначе), при открытии снова становятся строками «Резервная копия» с «Вернуть»; строки сохраняются, повторное
    /// открытие их не удваивает. Папка без годного `component.json` остаётся на диске, строкой не становится, в
    /// журнале — предупреждение с путём.
    #[test]
    fn orphan_backup_folders_become_restorable_rows_once() {
        let dir = temp("orphans");
        std::fs::create_dir_all(&dir).unwrap();
        let mut known = hist(7, Component::Engine, Action::Backup, None, Some("7-engine-3.0"));
        known.from = Some("3.0".into());
        save_json(&dir.join(HISTORY), &vec![hist(8, Component::Engine, Action::Update, Some("3.1"), None), known]).unwrap();
        stored_copy(&dir, "7-engine-3.0", Some(Ok((Component::Engine, "3.0"))));
        stored_copy(&dir, "34-native-3.1.0", Some(Ok((Component::Native, "3.1.0"))));
        stored_copy(&dir, "5-app-0.4.9", Some(Ok((Component::App, "0.4.9"))));
        let missing = stored_copy(&dir, "40-engine-2.0", None);
        let broken = stored_copy(&dir, "41-engine-2.1", Some(Err("{broken")));
        std::fs::write(dir.join(BACKUPS).join("note.txt"), b"not a copy").unwrap();
        let calls = Calls::default();
        let m = with_ops(&dir, components_of(fakes(&calls)));
        let ids = |m: &Manager| lock(&m.data).history.entries().iter().map(|e| e.id).collect::<Vec<_>>();
        assert_eq!(ids(&m), vec![34, 8, 7, 5], "строки встают по номеру из имени папки, новые сверху");
        let native = rows_of(&m, Component::Native, Action::Backup);
        assert_eq!(native.len(), 1);
        let n = &native[0];
        assert_eq!((n.from.as_deref(), n.to.as_deref(), n.backup.as_deref(), n.ok), (Some("3.1.0"), None, Some("34-native-3.1.0"), true));
        assert!(n.backup_size >= 5, "размер папки: {}", n.backup_size);
        assert_eq!(rows_of(&m, Component::Engine, Action::Backup).len(), 1, "папка со строкой не удваивается");
        assert_eq!(m.restore_target(34), Ok(("34-native-3.1.0".to_string(), BackupInfo { component: Component::Native, version: "3.1.0".into() })));
        assert_eq!(m.restore_target(5).err(), Some(trf("upd.restore_too_old", &[super::super::restore_target::MIN_APP_RESTORE])), "правила возврата прежние");
        let offered = m.state().restores;
        assert!(offered.iter().any(|o| o.id == 34 && o.blocked.is_none()), "{offered:?}");
        let log = logged(&m);
        for name in ["34-native-3.1.0", "5-app-0.4.9"] {
            assert_eq!(log.iter().filter(|(sev, t)| *sev == Severity::Warn && t.contains(name)).count(), 1, "{name}: {log:?}");
        }
        for path in [&missing, &broken] {
            let path = path.display().to_string();
            assert_eq!(log.iter().filter(|(sev, t)| *sev == Severity::Warn && t.contains(&path)).count(), 1, "{path}: {log:?}");
            assert!(Path::new(&path).is_dir(), "папку без годного описания не удаляем: {path}");
        }
        assert!(!log.iter().any(|(_, t)| t.contains("7-engine-3.0") || t.contains("note.txt")), "{log:?}");
        assert_eq!(m.next_id(), 42, "новые строки и папки не займут номера найденных папок");
        drop(m);
        assert_eq!(History::open(&dir).unwrap().entries().iter().map(|e| e.id).collect::<Vec<_>>(), vec![34, 8, 7, 5], "строки сохранены");
        let again = with_ops(&dir, components_of(fakes(&calls)));
        assert_eq!(ids(&again), vec![34, 8, 7, 5], "повторное открытие не удваивает");
        assert!(!logged(&again).iter().any(|(_, t)| t.contains("34-native-3.1.0")), "и не пишет о возврате строк снова");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Номер из имени найденной папки уже занят строкой новой истории — строка получает свободный номер; найденные
    /// копии подчиняются обычному пределу копий на компонент.
    #[test]
    fn orphan_rows_take_a_free_id_and_follow_the_backup_limit() {
        let dir = temp("orphans-limit");
        std::fs::create_dir_all(&dir).unwrap();
        save_json(&dir.join(HISTORY), &vec![hist(2, Component::Native, Action::Update, Some("1.0"), None)]).unwrap();
        for id in 1..=7 {
            stored_copy(&dir, &backup_name(id, Component::Engine, &format!("3.{id}")), Some(Ok((Component::Engine, &format!("3.{id}")))));
        }
        let calls = Calls::default();
        let m = with_ops(&dir, components_of(fakes(&calls)));
        let engine = rows_of(&m, Component::Engine, Action::Backup);
        let kept: Vec<_> = engine.iter().filter_map(|e| e.backup.clone()).collect();
        assert_eq!(kept.len(), BACKUPS_MAX, "{engine:?}");
        assert!(engine.iter().all(|e| e.id != 2), "номер 2 занят строкой AmneziaWG: {engine:?}");
        let gone: Vec<_> = (1..=7).filter(|id| !dir.join(BACKUPS).join(backup_name(*id, Component::Engine, &format!("3.{id}"))).exists()).collect();
        assert_eq!(gone.len(), 7 - BACKUPS_MAX, "лишние копии удалены пределом: {gone:?}");
        for name in &kept {
            assert!(dir.join(BACKUPS).join(name).is_dir(), "{name}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn names_in(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()
    }

    /// При открытии каждый компонент доделывает остатки своей оборванной работы (`recover`), после отметки «прервано».
    #[test]
    fn open_lets_every_component_recover_its_leftovers() {
        let dir = temp("recover");
        let calls = Calls::default();
        let comps = components(
            FakeOps::new(Component::Native, Some("1"), "1", &calls).record_recover(),
            FakeOps::new(Component::Engine, Some("1"), "1", &calls).record_recover(),
            FakeOps::new(Component::App, Some("1"), "1", &calls).record_recover(),
        );
        let _m = with_ops(&dir, comps);
        assert_eq!(*calls.lock().unwrap(), ["recover Native", "recover Engine", "recover App"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Доделку, отложенную компонентом при открытии (у AmneziaWG — шёл установщик Windows), такт планировщика
    /// повторяет, пока компонент держит отметку; другие компоненты не трогаются, идущая работа — пропуск.
    #[test]
    fn deferred_recovery_is_retried_on_the_tick() {
        let dir = temp("recover-deferred");
        let calls = Calls::default();
        let deferred = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let comps = components(
            FakeOps::new(Component::Native, Some("1"), "1", &calls).record_recover().deferred_by(deferred.clone()),
            FakeOps::new(Component::Engine, Some("1"), "1", &calls).record_recover(),
            FakeOps::new(Component::App, Some("1"), "1", &calls).record_recover(),
        );
        let m = with_ops(&dir, comps);
        let recovers = || calls.lock().unwrap().iter().filter(|c| c.starts_with("recover ")).cloned().collect::<Vec<_>>();
        assert_eq!(recovers(), ["recover Native", "recover Engine", "recover App"]);
        let mut first = true;
        m.daily_tick(&mut first);
        assert_eq!(recovers(), ["recover Native", "recover Engine", "recover App", "recover Native"], "повтор — только отложенного");
        assert!(m.begin(Busy::Checking));
        m.daily_tick(&mut first);
        assert_eq!(recovers().len(), 4, "идёт работа — повтора нет");
        lock(&m.busy).take();
        deferred.store(false, std::sync::atomic::Ordering::SeqCst);
        m.daily_tick(&mut first);
        assert_eq!(recovers().len(), 4, "доделано — повторов больше нет");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_started_marker_is_reported_and_removed() {
        let dir = temp("started-corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(STARTED), "not json").unwrap();
        let m = manager(&dir);
        assert!(!dir.join(STARTED).exists());
        assert!(logged(&m).iter().any(|(s, t)| *s == Severity::Bad && t.contains(STARTED)), "{:?}", logged(&m));
    }

    #[test]
    fn started_restore_is_marked_interrupted_on_open() {
        let dir = temp("started");
        {
            let m = manager(&dir);
            m.record(hist(4, Component::Native, Action::Restore, Some("1.0"), None));
            m.save(STARTED, &4u64);
            m.record(hist(5, Component::Native, Action::Backup, None, Some("5-native-1.0")));
        }
        let m = manager(&dir);
        {
            let data = lock(&m.data);
            let e = data.history.entries().iter().find(|e| e.id == 4).unwrap();
            assert_eq!((e.ok, e.error.clone()), (false, Some(tr("updm.interrupted"))));
            assert!(data.history.entries().iter().find(|e| e.id == 5).unwrap().ok, "другие строки не трогаются");
        }
        assert!(!dir.join(STARTED).exists());
        // Законченная работа отметку убирает, ошибка дописывается в ту же строку.
        assert_eq!(m.run_started(hist(6, Component::Native, Action::Restore, Some("1.0"), None), || Ok(())), Ok(()));
        assert!(!dir.join(STARTED).exists());
        let r = m.run_started(hist(7, Component::Native, Action::Restore, Some("1.0"), None), || {
            assert!(dir.join(STARTED).exists(), "отметка стоит, пока идёт работа");
            Err("boom".into())
        });
        assert_eq!(r, Err("boom".to_string()));
        assert!(!dir.join(STARTED).exists());
        drop(m);
        let m = manager(&dir);
        let data = lock(&m.data);
        let state = |id| data.history.entries().iter().find(|e| e.id == id).map(|e| (e.ok, e.error.clone())).unwrap();
        assert_eq!((state(6), state(7)), ((true, None), (false, Some("boom".to_string()))));
        drop(data);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Движок с настоящим шагом после замены (`ReconnectEngine` в `core`); `dies` — процесс обрывается посреди замены
    /// файлов (паника вместо возврата), `fails` — замена вернулась с ошибкой.
    struct EngineFake {
        core: Arc<RecordingCore>,
        dies: bool,
        fails: bool,
    }

    impl ComponentOps for EngineFake {
        fn name(&self) -> String {
            "engine".into()
        }
        fn installed(&self) -> Option<String> {
            Some("3.1".into())
        }
        fn found(&self, _f: &Fetched) -> Result<String, String> {
            Ok("3.2".into())
        }
        fn backup_into(&self, _m: &Manager, _version: &str, _dir: &Path) -> Result<(), String> {
            Ok(())
        }
        fn update(&self, m: &Manager, _f: &Fetched, _to: &str, _id: u64) -> Result<(), String> {
            assert_eq!(owed_on_disk(&m.dir), [Component::Engine], "долг записан до замены файлов");
            if self.dies {
                panic!("agent killed while replacing the engine");
            }
            if self.fails {
                return Err("locked".into());
            }
            Ok(())
        }
        fn restore(&self, _m: &Manager, _job: &RestoreJob) -> Result<(), String> {
            Ok(())
        }
        fn after_change(&self) -> Result<(), String> {
            self.core.reconnect_engine()
        }
        fn owes_after_change(&self) -> bool {
            true
        }
    }

    fn with_engine(dir: &Path, core: &Arc<RecordingCore>, dies: bool, fails: bool) -> Arc<Manager> {
        let calls = Calls::default();
        let engine = EngineFake { core: core.clone(), dies, fails };
        let native = FakeOps::new(Component::Native, Some("1.0"), "1.0", &calls);
        let app = FakeOps::new(Component::App, Some("0.4.0"), "0.4.0", &calls);
        with_ops(dir, Components::new(Box::new(native), Box::new(engine), Box::new(app)))
    }

    fn owed_on_disk(dir: &Path) -> Vec<Component> {
        load_json(&dir.join(OWED)).unwrap()
    }

    fn update_engine(m: &Manager) {
        m.apply_one(Component::Engine, &Fetched { native: Err("off".into()), ours: Err("off".into()) }, "3.2");
    }

    fn count(m: &Manager, severity: Severity, text: &str) -> usize {
        logged(m).iter().filter(|(s, t)| *s == severity && t == text).count()
    }

    /// Агент умер после замены файлов движка, до `ReconnectEngine`: отметка на диске; следующий запуск пишет об этом
    /// в журнал и на такте планировщика шлёт запрос один раз, затем снимает отметку.
    #[test]
    fn agent_killed_after_engine_replace_resends_reconnect_once_on_next_start() {
        let dir = temp("owed-killed");
        let first = Arc::new(RecordingCore::default());
        let m = with_engine(&dir, &first, true, false);
        let killed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| update_engine(&m)));
        assert!(killed.is_err());
        assert!(first.calls().is_empty(), "запрос так и не ушёл");
        drop(m);

        let core = Arc::new(RecordingCore::default());
        let m = with_engine(&dir, &core, false, false);
        assert_eq!(count(&m, Severity::Warn, &trf("updm.after_change_owed", &["engine"])), 1, "{:?}", logged(&m));
        assert!(core.calls().is_empty(), "при открытии не шлёт: ядро может ещё запускаться");
        assert!(!m.daily_tick(&mut true), "проверке ещё рано");
        assert_eq!(core.calls(), ["reconnect engine"], "такт планировщика повторил запрос");
        assert!(owed_on_disk(&dir).is_empty());
        assert_eq!(count(&m, Severity::Info, &trf("updm.after_change_resent", &["engine"])), 1);
        m.settle_owed();
        assert_eq!(core.calls().len(), 1, "принятый запрос не повторяется");
        drop(m);
        let m = with_engine(&dir, &core, false, false);
        assert!(logged(&m).is_empty(), "долга больше нет: {:?}", logged(&m));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Ядро недоступно: отметка остаётся, повтор на каждом такте, в журнале — одна строка до успеха; идёт работа —
    /// повтор ждёт её конца.
    #[test]
    fn unreachable_core_keeps_the_record_retries_and_logs_once() {
        let dir = temp("owed-down");
        std::fs::create_dir_all(&dir).unwrap();
        save_json(&dir.join(OWED), &[Component::Engine]).unwrap();
        let core = Arc::new(RecordingCore::default());
        core.refuse_reconnect.store(true, std::sync::atomic::Ordering::SeqCst);
        let m = with_engine(&dir, &core, false, false);
        for _ in 0..3 {
            m.settle_owed();
        }
        assert_eq!(core.calls().len(), 3);
        assert_eq!(owed_on_disk(&dir), [Component::Engine], "не принято — отметка остаётся");
        let retry = trf("updm.after_change_retry", &["engine", "core: pipe unavailable"]);
        assert_eq!(count(&m, Severity::Warn, &retry), 1, "{:?}", logged(&m));

        core.refuse_reconnect.store(false, std::sync::atomic::Ordering::SeqCst);
        assert!(m.begin(Busy::Checking));
        m.settle_owed();
        assert_eq!(core.calls().len(), 3, "идёт работа — повтор ждёт");
        drop(JobDone(&m));
        m.settle_owed();
        assert_eq!(core.calls().len(), 4);
        assert!(owed_on_disk(&dir).is_empty());
        m.settle_owed();
        assert_eq!(core.calls().len(), 4);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Обычная работа: удачная замена с принятым запросом и неудачная замена долга не оставляют; запрос после удачной
    /// замены не дошёл — одна строка `after_change_failed`, отметка остаётся до повтора без новых строк.
    #[test]
    fn engine_update_clears_its_record_only_after_the_core_accepted() {
        let dir = temp("owed-job");
        let core = Arc::new(RecordingCore::default());
        update_engine(&with_engine(&dir, &core, false, true));
        assert!(owed_on_disk(&dir).is_empty() && core.calls().is_empty(), "неудачная замена — шаг не нужен");
        update_engine(&with_engine(&dir, &core, false, false));
        assert!(owed_on_disk(&dir).is_empty());
        assert_eq!(core.calls(), ["reconnect engine"]);

        core.refuse_reconnect.store(true, std::sync::atomic::Ordering::SeqCst);
        let m = with_engine(&dir, &core, false, false);
        update_engine(&m);
        assert_eq!(owed_on_disk(&dir), [Component::Engine]);
        // Конец работы (занятость снята) — дальше такты планировщика.
        drop(JobDone(&m));
        m.settle_owed();
        assert_eq!(core.calls().len(), 3, "повтор был");
        let failed = trf("updm.after_change_failed", &["engine", "core: pipe unavailable"]);
        assert_eq!(count(&m, Severity::Bad, &failed), 1);
        assert!(!logged(&m).iter().any(|(s, _)| *s == Severity::Warn), "повтор не пишет второй раз: {:?}", logged(&m));
        core.refuse_reconnect.store(false, std::sync::atomic::Ordering::SeqCst);
        m.settle_owed();
        assert!(owed_on_disk(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
