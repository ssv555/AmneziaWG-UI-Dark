//! Окно и ядро: полоса «установить / обновить ядро», установка через один запрос UAC, режим от ядра, вид окна по режиму и теме.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{self, RichText};

use super::theme::{self, ThemeId};
use super::{Action, App, Core, ErrorSink};
use crate::events::Severity;
use crate::settings::{Mode, Theme};
use crate::elevated::Outcome;
use crate::i18n::{self, tr, trf};

/// Последняя попытка окна обновить себя до версии ядра: версия и закончилась ли она ошибкой.
static SELF_UPDATE: Mutex<Option<(String, bool)>> = Mutex::new(None);

/// Окно уже пробовало обновиться до этой версии ядра, и попытка не удалась.
fn self_update_failed(core_version: &str) -> bool {
    SELF_UPDATE.lock().unwrap().as_ref().is_some_and(|(v, failed)| *failed && v == core_version)
}

/// Связь окна с ядром — для полосы «установить / обновить ядро».
#[derive(Clone, Debug, PartialEq)]
pub(super) enum LinkState {
    Checking,
    Ok,
    /// Ядро не установлено.
    Missing,
    /// Ядро другой версии (её номер).
    Outdated(String),
    /// Служба есть, но не отвечает.
    Down(String),
    /// Идёт установка (запрос UAC).
    Installing,
}

/// Состояние связи, общее с потоками, которые её проверяют.
type LinkCell = Arc<Mutex<LinkState>>;

/// Связь окна с ядром: состояние, когда ядро сверяли в последний раз и была ли ошибка канала на прошлом кадре.
/// Окно только спрашивает (`state`, `due`) и командует (`check`, `set_installing`); состояние меняют проверки в фоне.
pub(super) struct CoreLink {
    cell: LinkCell,
    /// Когда ядро сверяли в последний раз (`due`).
    checked: Instant,
    /// Канал к ядру на прошлом кадре был с ошибкой — признак «только что восстановился».
    poll_failed: bool,
}

impl CoreLink {
    /// Ядро ещё не спрашивали.
    pub(super) fn checking() -> Self {
        Self::with_state(LinkState::Checking)
    }

    /// Демо: ядра нет, говорить не с кем, связь считается в порядке.
    pub(super) fn demo() -> Self {
        Self::with_state(LinkState::Ok)
    }

    fn with_state(state: LinkState) -> Self {
        CoreLink { cell: Arc::new(Mutex::new(state)), checked: Instant::now(), poll_failed: false }
    }

    pub(super) fn state(&self) -> LinkState {
        self.cell.lock().unwrap().clone()
    }

    fn set_installing(&self) {
        *self.cell.lock().unwrap() = LinkState::Installing;
    }

    /// Спросить ядро о версии в фоне; итог ляжет в состояние.
    pub(super) fn check(&self, core: Core, ctx: egui::Context, error: ErrorSink, probe: Probe) {
        check_core(core, ctx, self.cell.clone(), error, probe);
    }

    /// Нужна ли сверка на этом кадре; `poll_failed` — канал к ядру сейчас с ошибкой. Канал снова заработал после ошибки —
    /// сверка сразу, ядро могли перезапустить. Вызывается каждый кадр: помнит ошибку прошлого.
    pub(super) fn due(&mut self, poll_failed: bool, now: Instant) -> Option<Probe> {
        let reconnected = self.poll_failed && !poll_failed;
        self.poll_failed = poll_failed;
        if !recheck_due(&self.state(), now.duration_since(self.checked), reconnected) {
            return None;
        }
        self.checked = now;
        Some(Probe::Recheck { reconnected })
    }
}

fn check_core(core: Core, ctx: egui::Context, link: LinkCell, error: ErrorSink, probe: Probe) {
    // Сверка зависшего ядра не должна плодить потоки: пока одна не вернулась, следующая не начинается.
    let guard = match probe {
        Probe::Recheck { .. } => match RecheckGuard::take() {
            Some(g) => Some(g),
            None => return,
        },
        Probe::Start => None,
    };
    std::thread::spawn(move || {
        let _guard = guard;
        let found = core_state(core.hello().map(|(v, _)| v), crate::daemon::install::installed);
        let mut current = link.lock().unwrap();
        let state = settle(&current, found, probe);
        // Ядро забывает язык окна при перезапуске: шлём заново, когда связь только появилась или прерывалась.
        let reconnected = matches!(probe, Probe::Recheck { reconnected: true });
        if state == LinkState::Ok && (*current != LinkState::Ok || reconnected) {
            send_language(core.clone(), i18n::current_code(), error);
        }
        *current = state;
        drop(current);
        ctx.request_repaint();
    });
}

/// Откуда взялась проверка ядра.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Probe {
    /// Запуск окна или конец установки: результат берётся как есть.
    Start,
    /// Фоновая сверка; `reconnected` — канал к ядру только что снова заработал после ошибки.
    Recheck { reconnected: bool },
}

/// Единственная фоновая сверка за раз; освобождается, когда поток закончил (в том числе паникой).
struct RecheckGuard;

static RECHECKING: AtomicBool = AtomicBool::new(false);

impl RecheckGuard {
    fn take() -> Option<RecheckGuard> {
        (!RECHECKING.swap(true, std::sync::atomic::Ordering::AcqRel)).then_some(RecheckGuard)
    }
}

impl Drop for RecheckGuard {
    fn drop(&mut self) {
        RECHECKING.store(false, std::sync::atomic::Ordering::Release);
    }
}

/// Как часто сверять ядро, пока оно в порядке (ловит обновление ядра под работающим окном) и пока нет
/// (ложная полоса «переустановить ядро» сама исчезает, когда ядро поднялось).
const CORE_RECHECK_OK: Duration = Duration::from_secs(60);
const CORE_RECHECK_BAD: Duration = Duration::from_secs(10);

/// Состояние связи по ответу ядра на `Hello` (версия).
fn core_state(hello: Result<String, String>, installed: impl FnOnce() -> bool) -> LinkState {
    match hello {
        Ok(version) if version == env!("CARGO_PKG_VERSION") => LinkState::Ok,
        Ok(version) => LinkState::Outdated(version),
        Err(_) if !installed() => LinkState::Missing,
        Err(e) => LinkState::Down(e),
    }
}

/// Что показать после проверки. Фоновая сверка, начатая до щелчка «Установить», не должна стереть «Идёт установка»:
/// установка сама проверит ядро, когда закончится.
fn settle(current: &LinkState, found: LinkState, probe: Probe) -> LinkState {
    match (current, probe) {
        (LinkState::Installing, Probe::Recheck { .. }) => LinkState::Installing,
        _ => found,
    }
}

/// Пора ли сверить ядро заново; `reconnected` — канал только что восстановился, ядро могли перезапустить.
fn recheck_due(link: &LinkState, since_last: Duration, reconnected: bool) -> bool {
    match link {
        LinkState::Checking | LinkState::Installing => false,
        _ if reconnected => true,
        LinkState::Ok => since_last >= CORE_RECHECK_OK,
        _ => since_last >= CORE_RECHECK_BAD,
    }
}

/// Ядро пишет журнал на языке окна.
pub(super) fn send_language(core: Core, code: String, error: ErrorSink) {
    std::thread::spawn(move || {
        if let Err(e) = core.ok(crate::daemon::proto::Request::SetLanguage(code)) {
            error.push(e);
        }
    });
}

impl App {
    /// Установить, обновить или удалить ядро (`flag`) — отдельный процесс с правами администратора.
    pub(super) fn run_core_setup(&self, flag: &'static str) {
        self.core_link.set_installing();
        let (core, link, ctx, error, notice) = (self.core.clone(), self.core_link.cell.clone(), self.ctx.clone(), self.action_error.clone(), self.notice.clone());
        std::thread::spawn(move || {
            // Владелец ядра — эта учётная запись, даже если UAC подтвердит другой администратор.
            let args = match crate::win::current_user_sid() {
                Ok(sid) => format!("{flag} --owner {sid}"),
                Err(e) => {
                    error.push(e);
                    check_core(core, ctx, link, error.clone(), Probe::Start);
                    return;
                }
            };
            match crate::elevated::run(&args, "core.setup_failed") {
                Outcome::Done(note) => {
                    let done = if flag == crate::daemon::install::INSTALL_FLAG { "core.installed" } else { "core.uninstalled" };
                    *notice.lock().unwrap() = Some(tr(done));
                    // У прежней версии был автозапуск через задачу планировщика — включаем его по-новому (ключ Run).
                    if note == crate::daemon::install::AUTOSTART_NOTE {
                        if let Err(e) = crate::win::set_autostart(true) {
                            error.push(e);
                        }
                    }
                }
                Outcome::Failed(e) => error.push(e),
                // Отмена UAC — выбор пользователя, а не сбой: как у «Вернуть».
                Outcome::Cancelled => *notice.lock().unwrap() = Some(tr("core.uac_declined")),
            }
            // Ядру нужно мгновение, чтобы открыть канал.
            std::thread::sleep(Duration::from_millis(500));
            check_core(core, ctx, link, error.clone(), Probe::Start);
        });
    }

    /// Сверять ядро заново, а не один раз при запуске: ядро поднимается позже окна (вход в систему, перезапуск
    /// после обновления) и может смениться под работающим окном. Канал снова заработал после ошибки — сверка сразу.
    pub(super) fn watch_core(&mut self) {
        if !self.shared.mirrors_core() {
            return;
        }
        let Some(probe) = self.core_link.due(self.shared.has_poll_error(), Instant::now()) else { return };
        self.core_link.check(self.core.clone(), self.ctx.clone(), self.action_error.clone(), probe);
    }

    /// Полоса над таблицей, пока ядро не готово: что с ним и кнопка, которая это исправит.
    pub(super) fn core_banner(&self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        let link = self.core_link.state();
        if let LinkState::Outdated(v) = &link {
            self.self_update(v, false);
        }
        let own = env!("CARGO_PKG_VERSION");
        // Ядро новее окна: установка из папки окна откатила бы его назад, поэтому вместо «Обновить ядро» — окно
        // обновляется само, а после неудачной попытки её можно повторить.
        let (text, button, retry) = match &link {
            LinkState::Ok | LinkState::Checking => return,
            LinkState::Missing => (tr("core.missing"), Some(tr("core.install")), None),
            LinkState::Outdated(v) if crate::update::feed::newer(v, own) => {
                (trf("core.newer", &[v, own]), None, self_update_failed(v).then(|| v.clone()))
            }
            LinkState::Outdated(v) => (trf("core.outdated", &[v, own]), Some(tr("core.update")), None),
            LinkState::Down(e) => (trf("core.down", &[e]), Some(tr("core.reinstall")), None),
            LinkState::Installing => (tr("core.installing"), None, None),
        };
        let mut retry_clicked = false;
        egui::Panel::top("core-banner").show(ui, |ui| {
            ui.horizontal(|ui| {
                if matches!(link, LinkState::Installing) {
                    ui.spinner();
                }
                ui.label(RichText::new(text).color(theme::palette().warning));
                if let Some(b) = button {
                    if ui.button(b).on_hover_text(tr("core.uac_hint")).clicked() {
                        actions.push(Action::InstallCore);
                    }
                }
                if retry.is_some() && ui.button(tr("core.retry")).clicked() {
                    retry_clicked = true;
                }
            });
        });
        if let (true, Some(v)) = (retry_clicked, &retry) {
            self.self_update(v, true);
        }
    }

    /// Ядро новее окна и выложило свою сборку — окно ставит её себе и перезапускается. Само пробует один раз на
    /// каждую версию ядра (`again` — повтор по кнопке «Повторить»); не вышло — ошибка в окне и кнопка повтора.
    fn self_update(&self, core_version: &str, again: bool) {
        {
            let mut last = SELF_UPDATE.lock().unwrap();
            if !again && last.as_ref().is_some_and(|(v, _)| v == core_version) {
                return;
            }
            *last = Some((core_version.to_string(), false));
        }
        match crate::update::ours::self_update_window(core_version) {
            Ok(false) => {}
            Ok(true) => {
                self.save_before_exit();
                crate::tray::remove();
                std::process::exit(0);
            }
            Err(e) => {
                *SELF_UPDATE.lock().unwrap() = Some((core_version.to_string(), true));
                self.action_error.push(trf("updo.self_update_failed", &[core_version, &e]));
            }
        }
    }

    /// Вид окна по режиму и теме. Режим виден сразу: в режиме 2 иконки (трей, панель задач, заголовок) жёлтые,
    /// разделители и рамки — цвета `mode2_frame` темы, рамка окна Windows 11 — тоже. Тема — палитра egui и
    /// заголовок окна Windows (тёмный или светлый). Зовётся каждый кадр до рисования; работа — только при смене.
    pub(super) fn apply_look(&mut self) {
        let engine = self.s.mode() == Mode::Engine;
        // Трей ставится после первого кадра — звать каждый кадр, повтор ничего не делает.
        crate::tray::set_engine(engine);
        let (look, registry_error) =
            Look::wanted(self.s.mode(), self.s.theme, self.ctx.system_theme(), crate::win::apps_use_light_theme);
        let focus_changed =
            self.ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::WindowFocused(_))));
        match look.step(self.look, focus_changed) {
            LookStep::Keep => return,
            LookStep::MarkTitleBar => {
                self.mark_title_bar(look.theme.palette().dark);
                return;
            }
            LookStep::Apply => {}
        }
        self.look = Some(look);
        self.about_icon = None;
        // Ошибка реестра пишется только при смене вида: читается он каждый кадр, а ключ вида от неё не меняется.
        if let Some(e) = registry_error {
            self.shared.log("", Severity::Warn, &e);
        }
        theme::set_active(look.theme);
        let palette = look.theme.palette();
        // Тема egui задана явно: при «как в системе» egui сам менял бы стиль вслед за Windows мимо палитры.
        let egui_theme = if palette.dark { egui::Theme::Dark } else { egui::Theme::Light };
        self.ctx.set_theme(egui_theme);
        self.ctx.set_visuals_of(egui_theme, palette.visuals(engine));
        if self.hwnd == 0 {
            return;
        }
        let frame = palette.mode2_frame;
        // В режиме 1 рамка системная (`None`): Windows подбирает её под тёмный или светлый заголовок, и Графит
        // остаётся ровно прежним.
        let border = engine.then_some([frame.r(), frame.g(), frame.b()]);
        for result in [crate::win::title_bar_dark(self.hwnd, palette.dark), crate::win::border_color(self.hwnd, border)] {
            if let Err(e) = result {
                self.shared.log("", Severity::Warn, &e);
            }
        }
    }

    /// Заголовок снова в цвет темы, без перерисовки рамки (см. `LookStep::MarkTitleBar`).
    fn mark_title_bar(&self, dark: bool) {
        if self.hwnd == 0 {
            return;
        }
        if let Err(e) = crate::win::title_bar_dark_mark(self.hwnd, dark) {
            self.shared.log("", Severity::Warn, &e);
        }
    }

    /// Режим, о котором сообщило ядро; поменялся — выбор туннеля берётся тот, что запомнен для нового режима.
    pub(super) fn sync_mode(&mut self) {
        let Some(mode) = self.shared.core_mode() else { return };
        if mode != self.s.mode() {
            self.s.switch_mode(mode);
            self.sources.clear_infos();
        }
    }
}

/// Выставленный вид окна: режим, тема и тема Windows, как её сообщил egui (от winit). Тема Windows — в ключе
/// и при явной теме окна: при смене темы Windows winit сам перекрашивает заголовок под систему, и его нужно
/// вернуть к теме окна.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Look {
    mode: Mode,
    theme: ThemeId,
    system: Option<egui::Theme>,
}

impl Look {
    /// Вид по настройкам. «Как в Windows» берёт тему, о которой сообщил egui; не сообщил — `registry`
    /// (`AppsUseLightTheme`); не прочлось и там — тёмная, с текстом ошибки для журнала.
    fn wanted(
        mode: Mode,
        theme: Theme,
        system: Option<egui::Theme>,
        registry: impl FnOnce() -> Result<bool, String>,
    ) -> (Look, Option<String>) {
        let (system_is_light, error) = match (theme, system) {
            (_, Some(reported)) => (reported == egui::Theme::Light, None),
            // Явная тема от Windows не зависит — реестр не читаем.
            (Theme::Graphite | Theme::Slate | Theme::Daylight, None) => (false, None),
            (Theme::System, None) => match registry() {
                Ok(light) => (light, None),
                Err(e) => (false, Some(format!("Windows theme: {e}; using the dark theme"))),
            },
        };
        (Look { mode, theme: theme::resolve(theme, system_is_light), system }, error)
    }

    /// Выставлять ли вид заново: впервые или что-то в ключе поменялось.
    fn differs_from(self, applied: Option<Look>) -> bool {
        applied != Some(self)
    }

    /// Что сделать с видом в этом кадре. `focus_changed` — окно получило или потеряло фокус.
    fn step(self, applied: Option<Look>, focus_changed: bool) -> LookStep {
        if self.differs_from(applied) {
            LookStep::Apply
        } else if focus_changed {
            LookStep::MarkTitleBar
        } else {
            LookStep::Keep
        }
    }
}

/// Шаг вида окна в кадре.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LookStep {
    /// Вид уже выставлен.
    Keep,
    /// Вид тот же, но заголовок мог перекраситься мимо нас: winit на любое `WM_SETTINGCHANGE` (обои, переменные
    /// среды, не только тема) ставит заголовку тему Windows, и событие приходит, только если сменилась сама тема
    /// Windows. Само сообщение окну не видно, поэтому повод — смена фокуса: настройки меняют в другой программе,
    /// и к окну возвращаются щелчком. Отметка у DWM дешёвая, кадров с фокусом мало.
    MarkTitleBar,
    /// Выставить вид целиком: палитра, заголовок с перерисовкой, рамка.
    Apply,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::fake::FakeCore;
    use crate::daemon::CoreApi;
    use crate::daemon::proto::Response;

    #[test]
    fn unreachable_core_is_down_when_installed_and_missing_when_not() {
        let core = FakeCore::unreachable("core unavailable");
        assert_eq!(core_state(core.hello().map(|(v, _)| v), || true), LinkState::Down("core unavailable".into()));
        assert_eq!(core_state(core.hello().map(|(v, _)| v), || false), LinkState::Missing);
        // Ядро ответило не на тот вопрос — тоже «не отвечает», с текстом ответа, а не тишина.
        let wrong = FakeCore::new(|_| Ok(Response::Ok));
        assert!(matches!(core_state(wrong.hello().map(|(v, _)| v), || true), LinkState::Down(e) if e.contains("unexpected")));
        let other = FakeCore::new(|_| Ok(Response::Hello { version: "0.0.1".into(), mode: Mode::Overlay }));
        assert_eq!(core_state(other.hello().map(|(v, _)| v), || true), LinkState::Outdated("0.0.1".into()));
    }

    #[test]
    fn core_state_follows_hello() {
        let own = env!("CARGO_PKG_VERSION").to_string();
        assert_eq!(core_state(Ok(own), || panic!("не нужно")), LinkState::Ok);
        assert_eq!(core_state(Ok("0.0.1".into()), || panic!("не нужно")), LinkState::Outdated("0.0.1".into()));
        assert_eq!(core_state(Err("x".into()), || false), LinkState::Missing);
        assert_eq!(core_state(Err("x".into()), || true), LinkState::Down("x".into()));
    }

    #[test]
    fn false_down_clears_when_core_comes_up() {
        // Окно запустилось раньше ядра: Down; через сверку ядро ответило — полоса «переустановить» уходит.
        let down = LinkState::Down("pipe".into());
        let recheck = Probe::Recheck { reconnected: false };
        assert_eq!(settle(&down, LinkState::Ok, recheck), LinkState::Ok);
        // Версия ядра сменилась под окном — видна на следующей сверке.
        assert_eq!(settle(&LinkState::Ok, LinkState::Outdated("9".into()), recheck), LinkState::Outdated("9".into()));
    }

    #[test]
    fn recheck_does_not_erase_installing() {
        let recheck = Probe::Recheck { reconnected: false };
        assert_eq!(settle(&LinkState::Installing, LinkState::Down("x".into()), recheck), LinkState::Installing);
        // Конец установки проверяет ядро сам и берёт результат как есть.
        assert_eq!(settle(&LinkState::Installing, LinkState::Ok, Probe::Start), LinkState::Ok);
    }

    #[test]
    fn recheck_schedule() {
        let down = LinkState::Down("x".into());
        let s = Duration::from_secs;
        assert!(!recheck_due(&down, s(5), false));
        assert!(recheck_due(&down, s(10), false));
        assert!(recheck_due(&LinkState::Missing, s(10), false));
        assert!(!recheck_due(&LinkState::Ok, s(30), false));
        assert!(recheck_due(&LinkState::Ok, s(60), false));
        // Канал восстановился — сразу, в любом состоянии, кроме идущих проверки и установки.
        assert!(recheck_due(&LinkState::Ok, s(0), true));
        assert!(!recheck_due(&LinkState::Installing, s(999), true));
        assert!(!recheck_due(&LinkState::Checking, s(999), true));
    }

    #[test]
    fn link_asks_for_a_recheck_on_schedule_and_when_the_channel_recovers() {
        let mut link = CoreLink::demo();
        let t0 = link.checked;
        let s = Duration::from_secs;
        assert_eq!(link.due(false, t0 + s(30)), None);
        assert_eq!(link.due(false, t0 + s(60)), Some(Probe::Recheck { reconnected: false }));
        // Сверка только что была: отсчёт пошёл заново.
        assert_eq!(link.due(false, t0 + s(70)), None);
        // Канал упал и поднялся — сверка сразу, с пометкой «восстановился».
        assert_eq!(link.due(true, t0 + s(71)), None);
        assert_eq!(link.due(false, t0 + s(72)), Some(Probe::Recheck { reconnected: true }));
        // Во время установки сверка не нужна, как бы давно ни спрашивали.
        link.set_installing();
        assert_eq!(link.due(false, t0 + s(9999)), None);
        assert_eq!(link.state(), LinkState::Installing);
    }

    fn no_registry() -> Result<bool, String> {
        panic!("реестр читается только для «Как в Windows», когда egui не сообщил тему")
    }

    fn theme_of(theme: Theme, system: Option<egui::Theme>, registry: impl FnOnce() -> Result<bool, String>) -> ThemeId {
        let (look, error) = Look::wanted(Mode::Overlay, theme, system, registry);
        assert_eq!(error, None);
        look.theme
    }

    #[test]
    fn follow_windows_takes_the_reported_theme_first_then_the_registry() {
        assert_eq!(theme_of(Theme::System, Some(egui::Theme::Light), no_registry), ThemeId::Daylight);
        assert_eq!(theme_of(Theme::System, Some(egui::Theme::Dark), no_registry), ThemeId::Graphite);
        assert_eq!(theme_of(Theme::System, None, || Ok(true)), ThemeId::Daylight);
        assert_eq!(theme_of(Theme::System, None, || Ok(false)), ThemeId::Graphite);
        // Реестр не прочелся — тёмная тема и ошибка для журнала, а не тишина.
        let (look, error) = Look::wanted(Mode::Overlay, Theme::System, None, || Err("RegGetValueW: 2".into()));
        assert_eq!(look.theme, ThemeId::Graphite);
        assert!(error.is_some_and(|e| e.contains("RegGetValueW: 2")));
    }

    #[test]
    fn explicit_theme_ignores_windows() {
        for system in [Some(egui::Theme::Light), Some(egui::Theme::Dark), None] {
            assert_eq!(theme_of(Theme::Graphite, system, no_registry), ThemeId::Graphite);
            assert_eq!(theme_of(Theme::Slate, system, no_registry), ThemeId::Slate);
            assert_eq!(theme_of(Theme::Daylight, system, no_registry), ThemeId::Daylight);
        }
    }

    #[test]
    fn look_is_reapplied_only_when_its_key_changes() {
        let wanted = |mode, theme, system| Look::wanted(mode, theme, system, || Ok(false)).0;
        let dark = Some(egui::Theme::Dark);
        let applied = wanted(Mode::Overlay, Theme::Graphite, dark);
        assert!(applied.differs_from(None), "первый кадр");
        assert!(!wanted(Mode::Overlay, Theme::Graphite, dark).differs_from(Some(applied)));
        assert!(wanted(Mode::Engine, Theme::Graphite, dark).differs_from(Some(applied)), "режим");
        assert!(wanted(Mode::Overlay, Theme::Slate, dark).differs_from(Some(applied)), "тема");
        assert!(wanted(Mode::Overlay, Theme::System, Some(egui::Theme::Light)).differs_from(Some(applied)), "Windows посветлела");
        // При явной теме смена темы Windows тоже повод: winit перекрасил заголовок под систему, его надо вернуть.
        assert!(wanted(Mode::Overlay, Theme::Graphite, Some(egui::Theme::Light)).differs_from(Some(applied)));
        // «Как в Windows» при тёмной Windows — та же палитра, но настройка другая: вид тот же, перевыставлять нечего.
        assert!(!wanted(Mode::Overlay, Theme::System, dark).differs_from(Some(applied)));
    }

    /// Явная тёмная тема при светлой Windows: winit мог перекрасить заголовок без события (любое
    /// `WM_SETTINGCHANGE`), поэтому при смене фокуса отметка заголовка ставится снова — без смены вида целиком.
    #[test]
    fn title_bar_is_marked_again_when_focus_changes() {
        let (applied, _) = Look::wanted(Mode::Overlay, Theme::Graphite, Some(egui::Theme::Light), no_registry);
        assert_eq!(applied.step(Some(applied), false), LookStep::Keep);
        assert_eq!(applied.step(Some(applied), true), LookStep::MarkTitleBar);
        // Первый кадр и смена вида выставляют всё, заголовок тоже: отдельная отметка не нужна.
        assert_eq!(applied.step(None, false), LookStep::Apply);
        assert_eq!(applied.step(None, true), LookStep::Apply);
        let (engine, _) = Look::wanted(Mode::Engine, Theme::Graphite, Some(egui::Theme::Light), no_registry);
        assert_eq!(engine.step(Some(applied), true), LookStep::Apply);
    }
}
