//! Окно AmneziaWG после обновления или возврата AmneziaWG. Их MSI закрывает процесс `amneziawg.exe` в сеансе
//! пользователя, и сам он не возвращается. Ядро и агент работают от SYSTEM в сеансе 0 и окон пользователю не
//! запускают, поэтому открывает его снова окно программы.
//!
//! Было ли окно AmneziaWG открыто, смотрит агент — сторона, которая запускает MSI, — прямо перед ним
//! (`update::NativeUiMark`): MSI идёт 1–3 с и закрывает окно в самом начале, опрос окна раз в секунду не застал бы
//! ни окно, ни саму работу. В конце работы агент даёт отметке новый номер; окно видит его в `State` агента (опрос и в
//! трее) и по каждому новому номеру решает один раз: окно AmneziaWG было открыто — открыть тем же запуском, что пункт
//! меню «Окно AmneziaWG», в любом режиме. Номер, увиденный первым после запуска окна программы, — прошлая работа:
//! по нему не открывается. Окна программы нет вовсе — открыть некому.

use super::errors::ErrorSink;
use super::Core;
use crate::daemon::proto::NativeOp;
use crate::i18n::trf;
use crate::update::NativeUiMark;

/// Окно AmneziaWG снаружи; в тестах — подделка.
pub(super) trait NativeWindow {
    /// Процесс AmneziaWG работает в сеансе окна программы (уже вернулся — второй раз не открываем).
    fn running(&self) -> bool;
    /// AmneziaWG установлен (после неудачного возврата его может не быть). Режим не важен: в обоих возвращается то,
    /// что у пользователя было открыто.
    fn installed(&self) -> bool;
    fn open(&self);
}

/// Какие отметки агента окно уже разобрало.
#[derive(Default)]
pub(super) struct NativeReopen {
    /// Номер последней разобранной отметки; `None` — агента с отметкой окно ещё не видело.
    seen: Option<u64>,
}

impl NativeReopen {
    /// Очередной опрос агента: `None` — агент не ответил или прежней версии (ничего не меняется). `true` — окно
    /// AmneziaWG открывается снова.
    pub(super) fn observe(&mut self, mark: Option<NativeUiMark>, window: &dyn NativeWindow) -> bool {
        let Some(mark) = mark else { return false };
        let first = self.seen.is_none();
        if self.seen == Some(mark.seq) {
            return false;
        }
        self.seen = Some(mark.seq);
        let reopen = !first && mark.was_open && !window.running() && window.installed();
        if reopen {
            window.open();
        }
        reopen
    }
}

/// Настоящее окно AmneziaWG: процесс ищется в сеансе окна программы, открывает его агент через помощника в сеансе
/// пользователя тем же запуском, что пункт меню «Окно AmneziaWG» (`NativeOp::Reopen`: в отличие от `Open` — и в
/// режиме 2, ведь возвращается то, что у пользователя было открыто).
pub(super) struct LiveNativeWindow {
    pub(super) core: Core,
    pub(super) error: ErrorSink,
}

impl NativeWindow for LiveNativeWindow {
    fn running(&self) -> bool {
        let exe = crate::backend::native_exe();
        exe.file_name().is_some_and(|name| crate::win::session_process_running(&name.to_string_lossy()))
    }

    fn installed(&self) -> bool {
        crate::backend::native_exe().is_file()
    }

    /// В своём потоке: помощник агента ждёт окно AmneziaWG секунды, а опрос агента (пинг, статистика) ждать не должен.
    fn open(&self) {
        let (core, error) = (self.core.clone(), self.error.clone());
        std::thread::spawn(move || reopen_via(&core, &error));
    }
}

/// Запрос агенту открыть окно AmneziaWG снова; отказ или сбой — в журнал событий окна.
fn reopen_via(core: &Core, error: &ErrorSink) {
    if let Err(e) = core.native(NativeOp::Reopen) {
        error.push(trf("upd.native_reopen_failed", &[&e]));
    }
}


#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::sync::Arc;

    use super::*;
    use crate::app::testkit::{errors, sink};
    use crate::daemon::fake::FakeCore;
    use crate::daemon::proto::Response;

    /// Окно просит именно `Reopen` (агент выполняет его и в режиме 2), отказ агента виден в журнале.
    #[test]
    fn reopen_asks_the_agent_for_reopen_and_logs_a_refusal() {
        let (shared, error) = sink();
        let fake = Arc::new(FakeCore::new(|_| Ok(Response::Ok)));
        reopen_via(&(fake.clone() as Core), &error);
        assert_eq!(fake.requests(), ["Native(Reopen)"]);
        assert!(errors(&shared).is_empty());
        let refusing: Core = Arc::new(FakeCore::new(|_| Ok(Response::Err("no".into()))));
        reopen_via(&refusing, &error);
        assert_eq!(errors(&shared).len(), 1);
        assert!(errors(&shared)[0].1.contains("no"));
    }

    /// Окно AmneziaWG из теста: работает ли процесс, установлен ли AmneziaWG и сколько раз открыли.
    struct Fake {
        running: Cell<bool>,
        installed: bool,
        opened: Cell<u32>,
    }

    impl Fake {
        /// MSI уже закрыл окно AmneziaWG: процесса нет.
        fn closed() -> Fake {
            Fake { running: Cell::new(false), installed: true, opened: Cell::new(0) }
        }
    }

    impl NativeWindow for Fake {
        fn running(&self) -> bool {
            self.running.get()
        }
        fn installed(&self) -> bool {
            self.installed
        }
        fn open(&self) {
            self.opened.set(self.opened.get() + 1);
            self.running.set(true);
        }
    }

    fn mark(seq: u64, was_open: bool) -> Option<NativeUiMark> {
        Some(NativeUiMark { seq, was_open })
    }

    fn run(r: &mut NativeReopen, w: &Fake, polls: &[Option<NativeUiMark>]) -> Vec<bool> {
        polls.iter().map(|&m| r.observe(m, w)).collect()
    }

    /// Работа с MSI короче опроса: занятости окно не видело ни разу, только новый номер с «было открыто».
    #[test]
    fn job_shorter_than_the_poll_is_caught_by_its_new_number() {
        let (mut r, w) = (NativeReopen::default(), Fake::closed());
        assert_eq!(run(&mut r, &w, &[mark(4, false), mark(5, true)]), [false, true]);
        assert_eq!(w.opened.get(), 1);
    }

    /// Окно AmneziaWG закрыто MSI в самом начале работы: к опросу его уже нет, решает снимок агента до MSI.
    #[test]
    fn window_killed_at_job_start_is_reopened_by_the_agent_snapshot() {
        let (mut r, w) = (NativeReopen::default(), Fake::closed());
        assert_eq!(run(&mut r, &w, &[mark(0, false), mark(0, false), mark(1, true), mark(1, true)]), [false, false, true, false]);
    }

    #[test]
    fn window_closed_before_the_msi_is_not_opened() {
        let (mut r, w) = (NativeReopen::default(), Fake::closed());
        assert_eq!(run(&mut r, &w, &[mark(1, true), mark(2, false)]), [false, false]);
        assert_eq!(w.opened.get(), 0);
    }

    #[test]
    fn same_number_seen_again_opens_once() {
        let (mut r, w) = (NativeReopen::default(), Fake::closed());
        assert_eq!(run(&mut r, &w, &[mark(1, false), mark(2, true), mark(2, true)]), [false, true, false]);
        w.running.set(false);
        // Пользователь закрыл окно AmneziaWG сам; агент перезапущен и отдаёт ту же отметку из `state.json`.
        assert_eq!(run(&mut r, &w, &[None, None, mark(2, true)]), [false, false, false]);
        assert_eq!(w.opened.get(), 1);
    }

    /// Окно программы запущено после работы: первый номер — прошлое, по нему не открывается.
    #[test]
    fn first_number_after_start_is_the_past() {
        let (mut r, w) = (NativeReopen::default(), Fake::closed());
        assert_eq!(run(&mut r, &w, &[None, mark(7, true), mark(7, true)]), [false, false, false]);
        assert_eq!(w.opened.get(), 0);
    }

    /// Обновление программы следом перезапустило агента раньше, чем окно увидело отметку: новый агент отдаёт её
    /// из `state.json`, и окно открывается.
    #[test]
    fn mark_from_a_restarted_agent_still_opens() {
        let (mut r, w) = (NativeReopen::default(), Fake::closed());
        assert_eq!(run(&mut r, &w, &[mark(3, false), None, None, mark(4, true)]), [false, false, false, true]);
    }

    #[test]
    fn window_already_back_or_amneziawg_gone_is_not_opened() {
        let (mut r, w) = (NativeReopen::default(), Fake::closed());
        w.running.set(true);
        assert_eq!(run(&mut r, &w, &[mark(1, false), mark(2, true)]), [false, false], "вернулось само — второй раз не открываем");
        let (mut r, mut w) = (NativeReopen::default(), Fake::closed());
        w.installed = false;
        assert_eq!(run(&mut r, &w, &[mark(1, false), mark(2, true)]), [false, false], "возврат не удался, AmneziaWG нет");
        assert_eq!(w.opened.get(), 0);
    }
}
