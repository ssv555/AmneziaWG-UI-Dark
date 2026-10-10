//! Цель кнопки «Вернуть»: к какой копии и версии вернёт строка истории. Правило одно и для окна (ядро кладёт
//! результат в `UpdatesState::restores`, окно папок копий не видит), и для команды `Restore(id)` ядра.
//!
//! 1. Строка «Резервная копия» — её собственная копия.
//! 2. Строка обновления — копия, сделанная прямо перед обновлением (версия «было»).
//! 3. Строка возврата — копия версии, которую возврат заменил (её «было»).
//!
//! У строк 2 и 3 копия указана явно (`prior_backup`); в истории прежних версий ссылки нет, и берётся последняя
//! копия того же компонента той же версии, сделанная не позже строки. Копия, которой нет на диске (отброшена
//! пределом или удалена), в цель не годится. Сборка программы старше `MIN_APP_RESTORE` тоже: у неё нет агента, а окно
//! этой версии ходит за обновлениями и конфигами только к нему — вернувшись, владелец не смог бы из окна ни обновиться
//! обратно, ни править туннели.

use serde::{Deserialize, Serialize};

use super::{feed, Action, Component, HistoryEntry};

/// Первая сборка с агентом: более старые сборки программы из окна не возвращаются.
pub const MIN_APP_RESTORE: &str = "0.5.0";

/// Что кнопка «Вернуть» строки истории делает сейчас.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct RestoreOffer {
    /// Номер строки истории.
    pub id: u64,
    /// Версия, к которой вернёт кнопка; `None` — у строки версии «было» нет.
    pub version: Option<String>,
    /// Почему вернуть нельзя; `None` — можно.
    pub blocked: Option<RestoreBlock>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreBlock {
    /// Копии нет: отброшена пределом, удалена, не сделана (сбой копии) или версии у строки нет.
    NoCopy,
    /// Версия цели уже установлена.
    Installed,
    /// Сборка программы старше `MIN_APP_RESTORE`: без агента окно этой версии с ней не работает.
    TooOld,
}

/// Куда вернуть: папка копии в хранилище и версия.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Target {
    pub(super) backup: String,
    pub(super) version: String,
}

/// Цель возврата строки `id`. `installed` — установленная версия её компонента; `exists` — папка копии есть на
/// диске (и имя безопасно). Отказ — `Err((версия цели, причина))`; строки `id` нет — `None` вместо цели.
pub(super) fn resolve(
    history: &[HistoryEntry],
    id: u64,
    installed: Option<&str>,
    exists: &dyn Fn(&str) -> bool,
) -> Option<Result<Target, RestoreBlock>> {
    let row = history.iter().find(|e| e.id == id)?;
    let version = row.from.as_deref().map(str::trim).filter(|v| !v.is_empty());
    let Some(version) = version else { return Some(Err(RestoreBlock::NoCopy)) };
    if installed.map(str::trim) == Some(version) {
        return Some(Err(RestoreBlock::Installed));
    }
    if row.component == Component::App && feed::newer(MIN_APP_RESTORE, version) {
        return Some(Err(RestoreBlock::TooOld));
    }
    let backup = match row.action {
        Action::Backup => row.backup.as_deref().filter(|b| exists(b)),
        Action::Update | Action::Restore => linked(history, row, exists).or_else(|| by_version(history, row, version, exists)),
    };
    Some(backup.map(|b| Target { backup: b.to_string(), version: version.to_string() }).ok_or(RestoreBlock::NoCopy))
}

/// Предложения для всех строк истории; `installed` — установленные версии компонентов.
pub(super) fn offers(history: &[HistoryEntry], installed: &[(Component, Option<String>)], exists: &dyn Fn(&str) -> bool) -> Vec<RestoreOffer> {
    history
        .iter()
        .map(|e| {
            let now = installed.iter().find(|(c, _)| *c == e.component).and_then(|(_, v)| v.as_deref());
            let blocked = match resolve(history, e.id, now, exists) {
                Some(Ok(_)) | None => None,
                Some(Err(block)) => Some(block),
            };
            RestoreOffer { id: e.id, version: e.from.clone(), blocked }
        })
        .collect()
}

/// Копия, на которую строка ссылается явно: строка «Резервная копия» того же компонента, копия на месте.
fn linked<'a>(history: &'a [HistoryEntry], row: &HistoryEntry, exists: &dyn Fn(&str) -> bool) -> Option<&'a str> {
    let link = row.prior_backup?;
    history
        .iter()
        .find(|e| e.id == link && e.component == row.component && e.action == Action::Backup)
        .and_then(|e| e.backup.as_deref())
        .filter(|b| exists(b))
}

/// Последняя копия компонента строки с версией `version`, сделанная раньше строки (по номеру).
fn by_version<'a>(history: &'a [HistoryEntry], row: &HistoryEntry, version: &str, exists: &dyn Fn(&str) -> bool) -> Option<&'a str> {
    history
        .iter()
        .filter(|e| e.component == row.component && e.action == Action::Backup && e.id < row.id)
        .filter(|e| e.from.as_deref().map(str::trim) == Some(version))
        .filter(|e| e.backup.as_deref().is_some_and(exists))
        .max_by_key(|e| e.id)
        .and_then(|e| e.backup.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::history::entry;

    const C: Component = Component::Engine;

    /// Строка «Резервная копия» версии `version` с копией `name` (версия — в «было», как пишет `Manager::backup`).
    fn backup(id: u64, version: &str, name: Option<&str>) -> HistoryEntry {
        let mut e = entry(id, C, Action::Backup, Some(version.into()), None);
        e.backup = name.map(str::to_string);
        e
    }

    fn update(id: u64, from: &str, to: &str, link: Option<u64>) -> HistoryEntry {
        let mut e = entry(id, C, Action::Update, Some(from.into()), Some(to.into()));
        e.prior_backup = link;
        e
    }

    fn on_disk<'a>(names: &'a [&'a str]) -> impl Fn(&str) -> bool + 'a {
        move |n| names.contains(&n)
    }

    fn target(backup: &str, version: &str) -> Option<Result<Target, RestoreBlock>> {
        Some(Ok(Target { backup: backup.into(), version: version.into() }))
    }

    #[test]
    fn backup_row_resolves_to_its_own_copy() {
        let h = vec![backup(2, "2.0", Some("2-engine-2.0")), update(3, "2.0", "3.0", None)];
        assert_eq!(resolve(&h, 2, Some("3.0"), &on_disk(&["2-engine-2.0"])), target("2-engine-2.0", "2.0"));
    }

    #[test]
    fn update_row_with_link_resolves_to_the_copy_made_before_it() {
        // Две копии 2.0: ссылка выбирает ту, что сделана перед этим обновлением, а не более позднюю.
        let h = vec![update(5, "2.0", "3.0", Some(4)), backup(4, "2.0", Some("4-engine-2.0")), backup(1, "2.0", Some("1-engine-2.0"))];
        assert_eq!(resolve(&h, 5, Some("3.0"), &on_disk(&["4-engine-2.0", "1-engine-2.0"])), target("4-engine-2.0", "2.0"));
    }

    #[test]
    fn restore_row_resolves_to_the_copy_of_the_replaced_version() {
        let mut r = entry(6, C, Action::Restore, Some("3.0".into()), Some("2.0".into()));
        r.prior_backup = Some(5);
        let h = vec![r, backup(5, "3.0", Some("5-engine-3.0")), backup(4, "2.0", Some("4-engine-2.0"))];
        assert_eq!(resolve(&h, 6, Some("2.0"), &on_disk(&["5-engine-3.0", "4-engine-2.0"])), target("5-engine-3.0", "3.0"));
    }

    #[test]
    fn update_row_of_old_format_takes_the_latest_earlier_copy_of_its_version() {
        let h = vec![
            backup(9, "2.0", Some("9-engine-2.0")), // позже обновления — не годится
            update(7, "2.0", "3.0", None),
            backup(6, "2.0", Some("6-engine-2.0")),
            backup(3, "2.0", Some("3-engine-2.0")),
            backup(5, "1.0", Some("5-engine-1.0")), // другая версия
        ];
        let all = ["9-engine-2.0", "6-engine-2.0", "3-engine-2.0", "5-engine-1.0"];
        assert_eq!(resolve(&h, 7, Some("3.0"), &on_disk(&all)), target("6-engine-2.0", "2.0"));
        // Новейшей копии на диске нет — берётся предыдущая той же версии.
        assert_eq!(resolve(&h, 7, Some("3.0"), &on_disk(&["3-engine-2.0"])), target("3-engine-2.0", "2.0"));
    }

    #[test]
    fn update_row_of_old_format_ignores_other_components() {
        let mut other = backup(2, "2.0", Some("2-app-2.0"));
        other.component = Component::App;
        let h = vec![update(3, "2.0", "3.0", None), other];
        assert_eq!(resolve(&h, 3, Some("3.0"), &on_disk(&["2-app-2.0"])), Some(Err(RestoreBlock::NoCopy)));
    }

    #[test]
    fn pruned_copy_blocks_the_row() {
        // Копию сняли (`backup` пуст) или папки нет на диске, а явная ссылка ведёт в никуда.
        let pruned = vec![backup(2, "2.0", None), update(3, "2.0", "3.0", Some(2))];
        assert_eq!(resolve(&pruned, 2, Some("3.0"), &on_disk(&[])), Some(Err(RestoreBlock::NoCopy)));
        assert_eq!(resolve(&pruned, 3, Some("3.0"), &on_disk(&[])), Some(Err(RestoreBlock::NoCopy)));
        let gone = vec![backup(2, "2.0", Some("2-engine-2.0")), update(3, "2.0", "3.0", Some(2))];
        assert_eq!(resolve(&gone, 2, Some("3.0"), &on_disk(&[])), Some(Err(RestoreBlock::NoCopy)));
        assert_eq!(resolve(&gone, 3, Some("3.0"), &on_disk(&[])), Some(Err(RestoreBlock::NoCopy)));
    }

    #[test]
    fn target_equal_to_installed_is_blocked_even_with_a_copy() {
        let h = vec![backup(2, "2.0", Some("2-engine-2.0")), update(3, "2.0", "3.0", Some(2))];
        let disk = on_disk(&["2-engine-2.0"]);
        assert_eq!(resolve(&h, 2, Some("2.0"), &disk), Some(Err(RestoreBlock::Installed)));
        assert_eq!(resolve(&h, 3, Some(" 2.0 "), &disk), Some(Err(RestoreBlock::Installed)));
        assert!(resolve(&h, 3, None, &disk).unwrap().is_ok(), "версия не определена — сравнивать не с чем");
    }

    /// Сборка программы до агента (0.4.x) из окна не возвращается, даже с копией на диске; движок той же версии — да.
    #[test]
    fn app_build_older_than_the_first_with_an_agent_is_refused() {
        let app = |id: u64, version: &str, name: &str| {
            let mut e = backup(id, version, Some(name));
            e.component = Component::App;
            e
        };
        let h = vec![app(2, "0.4.0", "2-app-0.4.0"), app(3, "0.5.0", "3-app-0.5.0"), app(4, "0.4.9", "4-app-0.4.9"), backup(5, "0.4.0", Some("5-engine-0.4.0"))];
        let disk = on_disk(&["2-app-0.4.0", "3-app-0.5.0", "4-app-0.4.9", "5-engine-0.4.0"]);
        assert_eq!(resolve(&h, 2, Some("0.5.5"), &disk), Some(Err(RestoreBlock::TooOld)));
        assert_eq!(resolve(&h, 4, Some("0.5.5"), &disk), Some(Err(RestoreBlock::TooOld)));
        assert_eq!(resolve(&h, 3, Some("0.5.5"), &disk), target("3-app-0.5.0", "0.5.0"), "первая сборка с агентом годится");
        assert_eq!(resolve(&h, 5, Some("3.0"), &disk), target("5-engine-0.4.0", "0.4.0"), "предел только для сборки программы");
        // Уже установленная старая версия — «установлена», не «старая»: сравнение с установленной идёт первым.
        assert_eq!(resolve(&h, 2, Some("0.4.0"), &disk), Some(Err(RestoreBlock::Installed)));
        let o = offers(&h, &[(Component::App, Some("0.5.5".into()))], &disk);
        assert_eq!(o.iter().find(|x| x.id == 2).unwrap().blocked, Some(RestoreBlock::TooOld));
        assert!(feed::newer("0.5.0", "0.4.99") && !feed::newer(MIN_APP_RESTORE, "0.5.0"));
    }

    #[test]
    fn unknown_row_and_row_without_version_have_no_target() {
        let mut no_from = update(3, "x", "3.0", None);
        no_from.from = None;
        let h = vec![no_from, backup(2, "2.0", Some("2-engine-2.0"))];
        assert_eq!(resolve(&h, 99, None, &on_disk(&["2-engine-2.0"])), None);
        assert_eq!(resolve(&h, 3, None, &on_disk(&["2-engine-2.0"])), Some(Err(RestoreBlock::NoCopy)));
    }

    #[test]
    fn link_to_a_copy_of_another_component_is_ignored() {
        let mut foreign = backup(2, "2.0", Some("2-app-2.0"));
        foreign.component = Component::App;
        let h = vec![update(3, "2.0", "3.0", Some(2)), foreign];
        assert_eq!(resolve(&h, 3, Some("3.0"), &on_disk(&["2-app-2.0"])), Some(Err(RestoreBlock::NoCopy)));
    }

    #[test]
    fn offers_cover_every_row_with_its_components_installed_version() {
        let h = vec![update(3, "2.0", "3.0", Some(2)), backup(2, "2.0", Some("2-engine-2.0")), backup(1, "3.0", Some("1-engine-3.0"))];
        let installed = [(Component::Native, Some("9".to_string())), (C, Some("3.0".to_string()))];
        let o = offers(&h, &installed, &on_disk(&["2-engine-2.0", "1-engine-3.0"]));
        let got: Vec<_> = o.iter().map(|x| (x.id, x.version.as_deref(), x.blocked)).collect();
        assert_eq!(got, [(3, Some("2.0"), None), (2, Some("2.0"), None), (1, Some("3.0"), Some(RestoreBlock::Installed))]);
    }

    #[test]
    fn old_history_json_without_the_link_still_loads() {
        let old = r#"{"id":1,"at":0,"component":"Engine","action":"Update","from":"2.0","to":"3.0","backup":null,"backup_size":0,"ok":true,"error":null}"#;
        let e: HistoryEntry = serde_json::from_str(old).unwrap();
        assert_eq!(e.prior_backup, None);
        assert!(!serde_json::to_string(&e).unwrap().contains("prior_backup"), "пустая ссылка не пишется: файл прежнего вида");
    }
}
