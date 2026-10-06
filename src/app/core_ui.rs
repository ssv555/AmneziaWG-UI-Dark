//! Окно и ядро: полоса «установить / обновить ядро», установка через один запрос UAC, режим от ядра.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{self, RichText};

use super::{Action, App, Core, ErrorSink, NEON, YELLOW};
use crate::settings::Mode;
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
                ui.label(RichText::new(text).color(YELLOW));
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

    /// Режим виден сразу: в режиме 2 иконки (трей, панель задач, заголовок) жёлтые, разделители и рамки —
    /// неоново-жёлтые, рамка окна Windows 11 — тоже; в режиме 1 всё обычное.
    pub(super) fn apply_mode_look(&mut self) {
        let engine = self.s.mode() == Mode::Engine;
        // Трей ставится после первого кадра — звать каждый кадр, повтор ничего не делает.
        crate::tray::set_engine(engine);
        if self.look == Some(self.s.mode()) {
            return;
        }
        self.look = Some(self.s.mode());
        self.about_icon = None;
        let stroke = if engine { NEON } else { egui::Visuals::dark().widgets.noninteractive.bg_stroke.color };
        self.ctx.all_styles_mut(|s| {
            s.visuals.widgets.noninteractive.bg_stroke.color = stroke;
            s.visuals.window_stroke.color = stroke;
        });
        if self.hwnd != 0 {
            crate::win::border_color(self.hwnd, engine.then_some([NEON.r(), NEON.g(), NEON.b()]));
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
}
