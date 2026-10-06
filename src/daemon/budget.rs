//! Места одновременных соединений канала: общее для ядра (`server`) и агента (`agent`).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Сколько мест у каждого из счётчиков `Budgets`.
#[derive(Clone, Copy)]
pub(crate) struct Limits {
    /// Окно и остальные программы владельца.
    pub user: usize,
    /// Клиенты-SYSTEM (ядро и агент ходят друг к другу от SYSTEM).
    pub system: usize,
    /// Запас SYSTEM для проверки живости (`Hello`) при занятых `system`; 0 — запаса нет.
    pub self_check: usize,
}

/// Счётчики занятых мест: свои для окна (и остальных) и для SYSTEM, плюс запас на проверку канала. Проверка канала
/// (Hello от самой системы: ядро при старте, сторож ядра у агента) не должна упираться в места программ владельца:
/// иначе 16–32 молчащих подключения любой программы владельца сорвали бы запуск ядра, а сторож принимал бы живого
/// агента за зависшего и убивал его.
pub(crate) struct Budgets {
    limits: Limits,
    user: Arc<AtomicUsize>,
    system: Arc<AtomicUsize>,
    self_check: Arc<AtomicUsize>,
}

impl Budgets {
    pub fn new(limits: Limits) -> Budgets {
        Budgets { limits, user: Arc::default(), system: Arc::default(), self_check: Arc::default() }
    }

    /// Место для нового клиента; `true` во втором элементе — место из запаса проверки канала, на нём отвечают
    /// только на Hello. `None` — мест нет.
    pub fn admit(&self, from_system: bool) -> Option<(Slot, bool)> {
        if !from_system {
            return Slot::take(&self.user, self.limits.user).map(|s| (s, false));
        }
        Slot::take(&self.system, self.limits.system)
            .map(|s| (s, false))
            .or_else(|| Slot::take(&self.self_check, self.limits.self_check).map(|s| (s, true)))
    }
}

/// Место в счётчике соединений: возвращается в `Drop` — и при панике обработчика запроса. Иначе каждая паника
/// навсегда занимала бы место, и через `MAX_CONNECTIONS` паник ядро отвечало бы «занято» на всё.
pub(crate) struct Slot(Arc<AtomicUsize>);

impl Slot {
    fn take(counter: &Arc<AtomicUsize>, max: usize) -> Option<Slot> {
        if counter.fetch_add(1, Ordering::SeqCst) >= max {
            counter.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(Slot(counter.clone()))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panicking_request_returns_its_connection_slot() {
        let counter: Arc<AtomicUsize> = Arc::default();
        let slot = Slot::take(&counter, 1).expect("свободное место");
        assert!(Slot::take(&counter, 1).is_none(), "мест больше нет");
        assert_eq!(counter.load(Ordering::SeqCst), 1, "отказ не занимает место");
        let r = crate::crash::isolate(move || {
            let _slot = slot;
            panic!("handler bug");
        });
        assert!(r.is_err());
        assert_eq!(counter.load(Ordering::SeqCst), 0, "место вернулось после паники");
        assert!(Slot::take(&counter, 1).is_some());
    }

    #[test]
    fn no_reserve_means_system_over_budget_is_refused() {
        let b = Budgets::new(Limits { user: 1, system: 1, self_check: 0 });
        let _held = b.admit(true).unwrap();
        assert!(b.admit(true).is_none());
    }
}
