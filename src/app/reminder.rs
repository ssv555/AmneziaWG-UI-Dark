//! Напоминания о неустановленном обновлении: когда сообщать, а когда подождать. Только решения, без окна и без
//! Windows: состояние пользователя приходит снаружи (`Presence`), время — аргументом, поэтому всё проверяется тестами.
//!
//! Правило: новая версия сообщается сразу; уже сообщённая и всё ещё не установленная — раз в сутки. И то и другое
//! только если пользователь за компьютером и Windows сейчас принимает уведомления; иначе проверка повторяется
//! через несколько минут. «Позже» и крестик на уведомлении откладывают следующее напоминание на сутки.

use std::collections::BTreeMap;
use std::time::Duration;

use super::updates::component_key;
use crate::update::Component;

/// Напоминание о том же неустановленном обновлении — не чаще раза в сутки.
pub(super) const REMIND_EVERY: u64 = 24 * 60 * 60;
/// Дольше без ввода — пользователя нет за компьютером, уведомление пропадёт зря.
pub(super) const IDLE_LIMIT: u64 = 5 * 60;
/// Через сколько повторить проверку, если сообщить сейчас нельзя. Проверка дешёвая (два вызова Win32).
pub(super) const RECHECK: Duration = Duration::from_secs(3 * 60);

/// Что пора сделать с обновлением.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Due {
    /// Есть версия, о которой ещё не сообщали.
    Announce,
    /// Всё сообщено, но обновление не установлено, а с прошлого напоминания прошли сутки.
    Remind,
}

/// Пора ли сообщать, если пользователь на месте. `offered` — то, что ядро предлагает сейчас (после установки
/// список пустеет — напоминания прекращаются), `notified` — версии, о которых уже сообщали, `reminded` — когда
/// сообщали в последний раз (unix-секунды).
pub(super) fn due(offered: &[(Component, String)], notified: &BTreeMap<String, String>, reminded: Option<u64>, now: u64) -> Option<Due> {
    if offered.is_empty() {
        return None;
    }
    if offered.iter().any(|(c, v)| notified.get(component_key(*c)) != Some(v)) {
        return Some(Due::Announce);
    }
    // Метка из будущего (часы переводили назад) не должна глушить напоминания на месяцы: считаем, что срок пришёл.
    let elapsed = reminded.filter(|t| *t <= now).map(|t| now - t);
    elapsed.map_or(true, |s| s >= REMIND_EVERY).then_some(Due::Remind)
}

/// Что известно о пользователе в эту минуту.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Presence {
    /// Сколько секунд без клавиатуры и мыши.
    pub idle_secs: u64,
    /// Windows принимает уведомления: нет полноэкранной программы, презентации, «не беспокоить» и заблокированного сеанса.
    pub notifications_accepted: bool,
}

/// Можно ли показать уведомление сейчас.
pub(super) fn available(p: Presence) -> bool {
    p.idle_secs < IDLE_LIMIT && p.notifications_accepted
}

/// Состояние пользователя из Windows. Если Windows не ответила — считаем, что пользователь на месте и уведомления
/// принимаются: так сбой опроса не гасит сообщение об обновлении навсегда; сбой попадает в stderr.
pub(super) fn probe() -> Presence {
    let idle_secs = crate::win::idle_seconds().unwrap_or_else(|| {
        eprintln!("напоминания: не удалось узнать время простоя, считаем пользователя активным");
        0
    });
    let notifications_accepted = crate::win::notifications_accepted().unwrap_or_else(|e| {
        eprintln!("напоминания: {e}; считаем, что уведомления принимаются");
        true
    });
    Presence { idle_secs, notifications_accepted }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(v: &str) -> Vec<(Component, String)> {
        vec![(Component::App, v.to_string())]
    }

    fn seen(v: &str) -> BTreeMap<String, String> {
        BTreeMap::from([("app".to_string(), v.to_string())])
    }

    const DAY: u64 = REMIND_EVERY;
    const T0: u64 = 1_800_000_000;

    #[test]
    fn nothing_offered_means_nothing_to_say_even_when_a_day_has_passed() {
        assert_eq!(due(&[], &seen("0.5.0"), Some(T0), T0 + 10 * DAY), None);
        assert_eq!(due(&[], &BTreeMap::new(), None, T0), None);
    }

    #[test]
    fn a_new_version_is_announced_at_once() {
        assert_eq!(due(&app("0.5.0"), &BTreeMap::new(), None, T0), Some(Due::Announce));
        assert_eq!(due(&app("0.6.0"), &seen("0.5.0"), Some(T0), T0 + 5), Some(Due::Announce), "новее сообщённой — сразу, не через сутки");
    }

    #[test]
    fn announced_update_is_reminded_after_a_day_not_before() {
        let notified = seen("0.5.0");
        assert_eq!(due(&app("0.5.0"), &notified, Some(T0), T0), None);
        assert_eq!(due(&app("0.5.0"), &notified, Some(T0), T0 + DAY - 1), None);
        assert_eq!(due(&app("0.5.0"), &notified, Some(T0), T0 + DAY), Some(Due::Remind));
        assert_eq!(due(&app("0.5.0"), &notified, Some(T0), T0 + 3 * DAY), Some(Due::Remind));
    }

    #[test]
    fn missing_reminder_time_of_an_announced_update_counts_as_due() {
        // Настройки прежней версии: о версии сообщали, а метки времени ещё не было.
        assert_eq!(due(&app("0.5.0"), &seen("0.5.0"), None, T0), Some(Due::Remind));
    }

    #[test]
    fn reminder_time_in_the_future_does_not_silence_reminders() {
        assert_eq!(due(&app("0.5.0"), &seen("0.5.0"), Some(T0 + 30 * DAY), T0), Some(Due::Remind));
    }

    #[test]
    fn after_the_update_is_installed_no_reminder_comes() {
        // Ядро больше не предлагает обновление — список пуст, какими бы старыми ни были метки.
        assert_eq!(due(&[], &seen("0.5.0"), Some(T0 - 30 * DAY), T0), None);
    }

    #[test]
    fn each_component_is_tracked_on_its_own() {
        let offered = vec![(Component::App, "0.5.0".to_string()), (Component::Native, "3.1.1".to_string())];
        assert_eq!(due(&offered, &seen("0.5.0"), Some(T0), T0 + 5), Some(Due::Announce), "у ядра версия новая");
    }

    fn at(idle_secs: u64, notifications_accepted: bool) -> Presence {
        Presence { idle_secs, notifications_accepted }
    }

    #[test]
    fn active_user_who_accepts_notifications_is_available() {
        assert!(available(at(0, true)));
        assert!(available(at(IDLE_LIMIT - 1, true)));
    }

    #[test]
    fn idle_user_is_postponed() {
        assert!(!available(at(IDLE_LIMIT, true)));
        assert!(!available(at(3 * 60 * 60, true)));
    }

    #[test]
    fn busy_presentation_fullscreen_or_quiet_hours_are_postponed() {
        assert!(!available(at(1, false)));
    }
}
