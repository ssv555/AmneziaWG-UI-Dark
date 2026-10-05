//! Модель окна обновлений: строки компонентов (результат проверки + установленные версии сейчас),
//! о каких обновлениях сообщать в журнал, порядок обновления и сверка подтверждённой версии.

use serde::{Deserialize, Deserializer, Serialize};

use super::manager::Fetched;
use super::sources::OursError;
use super::{feed, sign};
use super::{Action, Available, Component, ComponentState, EngineUpstream, HistoryEntry, ORDER};
use crate::i18n::trf;

/// Результат последней проверки одного компонента — то, что хранится в `state.json`. Установленная версия, дата
/// установки и признак «есть обновление» здесь не хранятся: они зависят от того, что установлено сейчас, и
/// считаются при чтении (`rows`), иначе сохранённые поля устаревали бы.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub(super) struct CheckResult {
    pub(super) component: Component,
    /// Найденная версия (даже если не новее установленной).
    pub(super) available: Option<Available>,
    /// Ошибка проверки этого компонента.
    pub(super) error: Option<String>,
    /// `error` — «в релизе нет манифеста», не сбой (см. `ComponentState::manual_only`).
    #[serde(default)]
    pub(super) manual_only: bool,
}

/// Результат последней проверки новейшей метки amneziawg-windows (`state.json`). Нашлась метка — `latest`; нет —
/// `error`. Что это значит для нашего движка, считается при чтении (`engine_upstream`): установленный движок мог
/// смениться после проверки.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub(super) struct UpstreamCheck {
    pub(super) latest: Option<String>,
    pub(super) error: Option<String>,
}

impl From<Result<String, String>> for UpstreamCheck {
    fn from(r: Result<String, String>) -> Self {
        match r {
            Ok(tag) => UpstreamCheck { latest: Some(tag), error: None },
            Err(e) => UpstreamCheck { latest: None, error: Some(e) },
        }
    }
}

/// Наш движок против новейшей метки Amnezia. Проверки не было — `None` (в `state.json` прежних версий её нет).
/// Версия движка неизвестна (`0` — DLL не опознаны) — сказать «актуально» или «новее» нечего.
fn engine_upstream(installed: Option<&str>, check: Option<&UpstreamCheck>) -> Option<EngineUpstream> {
    let check = check?;
    let Some(latest) = &check.latest else { return Some(EngineUpstream::Unchecked) };
    let ours = installed.filter(|v| *v != "0")?;
    Some(if feed::newer(latest, ours) { EngineUpstream::Newer(latest.clone()) } else { EngineUpstream::Current(latest.clone()) })
}

/// Чтение `state.json` прежних версий: там лежала целая `ComponentState` (с установленной версией, датой и
/// `update`) и `component: null` для строки без компонента. Лишние поля пропускаются, строки без компонента —
/// тоже: их никогда никто не находил (`rows` ищет по компоненту), и сообщать о них было нечего.
pub(super) fn deserialize_results<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<CheckResult>, D::Error> {
    #[derive(Deserialize)]
    struct Stored {
        component: Option<Component>,
        available: Option<Available>,
        error: Option<String>,
        #[serde(default)]
        manual_only: bool,
    }
    let stored = Vec::<Stored>::deserialize(d)?;
    Ok(stored
        .into_iter()
        .filter_map(|s| Some(CheckResult { component: s.component?, available: s.available, error: s.error, manual_only: s.manual_only }))
        .collect())
}

/// Выбранные компоненты с подтверждёнными версиями в порядке обновления, без повторов (первый выбор компонента).
pub(super) fn apply_order(list: &[(Component, String)]) -> Vec<(Component, String)> {
    ORDER.into_iter().filter_map(|c| list.iter().find(|(k, _)| *k == c).cloned()).collect()
}

/// Свежая проверка нашла `found`, пользователь подтвердил `confirmed`. Другая версия — ошибка с найденной.
pub(super) fn check_confirmed(found: &str, confirmed: &str) -> Result<(), String> {
    if found == confirmed {
        Ok(())
    } else {
        Err(trf("updm.version_changed", &[found]))
    }
}


/// Дата выхода версии компонента, замеченная проверкой (`state.json`). Нужна, чтобы у установленной версии, которая
/// уже не «доступная», тоже была дата выхода — и чтобы окно, которое опрашивает ядро, не ходило за ней в сеть.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub(super) struct ReleaseDate {
    pub(super) component: Component,
    pub(super) version: String,
    /// Unix, сек; больше нуля (ноль — «не указана» — не запоминается).
    pub(super) published: u64,
}

/// Дата выхода `version` компонента `c`: у найденной проверкой версии — её дата (та же, что в колонке «Доступно»),
/// иначе запомненная. Неизвестна — `None`.
pub(super) fn released_at(c: Component, version: &str, found: Option<&Available>, cache: &[ReleaseDate]) -> Option<u64> {
    found
        .filter(|a| a.version == version && a.published > 0)
        .map(|a| a.published)
        .or_else(|| cache.iter().find(|r| r.component == c && r.version == version).map(|r| r.published))
}

/// Запомнить даты выхода найденных версий (запись версии заменяется: источник — последний релиз, он главнее).
pub(super) fn remember_found(cache: &mut Vec<ReleaseDate>, results: &[CheckResult]) {
    for r in results {
        let Some(a) = r.available.as_ref().filter(|a| a.published > 0) else { continue };
        remember(cache, r.component, &a.version, a.published);
    }
}

pub(super) fn remember(cache: &mut Vec<ReleaseDate>, component: Component, version: &str, published: u64) {
    cache.retain(|r| !(r.component == component && r.version == version));
    cache.push(ReleaseDate { component, version: version.to_string(), published });
}

/// Установленные версии, у которых даты выхода пока нет ни в проверке, ни в запомненных. Версия `0` (движок не
/// опознан) — не версия.
pub(super) fn unknown_release_dates(
    cache: &[ReleaseDate],
    results: &[CheckResult],
    installed: &[(Component, Option<String>)],
) -> Vec<(Component, String)> {
    installed
        .iter()
        .filter_map(|(c, v)| Some((*c, v.as_ref().filter(|v| *v != "0")?.clone())))
        .filter(|(c, v)| {
            let found = results.iter().find(|r| r.component == *c).and_then(|r| r.available.as_ref());
            released_at(*c, v, found, cache).is_none()
        })
        .collect()
}

/// В `state.json` остаются только даты, нужные окну: найденных проверкой и установленных сейчас версий.
pub(super) fn prune_released(cache: &mut Vec<ReleaseDate>, results: &[CheckResult], installed: &[(Component, Option<String>)]) {
    cache.retain(|r| {
        let is_found = results.iter().any(|f| f.component == r.component && f.available.as_ref().is_some_and(|a| a.version == r.version));
        let is_installed = installed.iter().any(|(c, v)| *c == r.component && v.as_deref() == Some(r.version.as_str()));
        is_found || is_installed
    });
}

/// Есть обновление: компонент установлен, и найденная версия новее.
pub(super) fn has_update(installed: Option<&str>, available: Option<&str>) -> bool {
    matches!((installed, available), (Some(i), Some(a)) if feed::newer(a, i))
}

/// Строки окна: результат проверки + установленные версии сейчас.
pub(super) fn rows(
    saved: &[CheckResult],
    upstream: Option<&UpstreamCheck>,
    installed: &[(Component, Option<String>)],
    history: &[HistoryEntry],
    released: &[ReleaseDate],
) -> Vec<ComponentState> {
    installed
        .iter()
        .map(|(c, inst)| {
            let found = saved.iter().find(|s| s.component == *c);
            let mut row = ComponentState {
                component: Some(*c),
                installed: inst.clone(),
                available: found.and_then(|f| f.available.clone()),
                error: found.and_then(|f| f.error.clone()),
                manual_only: found.is_some_and(|f| f.manual_only),
                upstream: if *c == Component::Engine { engine_upstream(inst.as_deref(), upstream) } else { None },
                ..Default::default()
            };
            row.installed_at = inst.as_deref().and_then(|v| {
                history
                    .iter()
                    .find(|e| e.component == *c && e.ok && e.action != Action::Backup && e.to.as_deref() == Some(v))
                    .map(|e| e.at)
            });
            row.released = inst.as_deref().and_then(|v| released_at(*c, v, row.available.as_ref(), released));
            row.update = has_update(inst.as_deref(), row.available.as_ref().map(|a| a.version.as_str()));
            row
        })
        .collect()
}

/// Найденные версии по ответам источников; ошибка одного источника не мешает другим.
pub(super) fn available_rows(f: &Fetched) -> Vec<CheckResult> {
    let row = |c: Component, r: Result<Available, OursError>| match r {
        Ok(a) => CheckResult { component: c, available: Some(a), error: None, manual_only: false },
        Err(e) => CheckResult { component: c, available: None, error: Some(e.message), manual_only: e.no_manifest },
    };
    let ours = |pick: fn(&sign::Manifest) -> (String, Option<String>)| {
        f.ours.as_ref().map(|(rel, m)| {
            let (version, wintun) = pick(m);
            Available { version, wintun, published: rel.published, notes: rel.notes.clone() }
        }).map_err(OursError::clone)
    };
    vec![
        row(
            Component::Native,
            f.native
                .as_ref()
                .map(|r| Available { version: r.version.clone(), wintun: None, published: r.published, notes: r.notes.clone() })
                .map_err(|e| OursError::from(e.clone())),
        ),
        row(Component::Engine, ours(|m| (m.engine.version.clone(), Some(m.engine.wintun.clone())))),
        row(Component::App, ours(|m| (m.version.clone(), None))),
    ]
}

/// Обновления, о которых ещё не сообщали; запоминает их в `announced`.
pub(super) fn to_announce(rows: &[ComponentState], announced: &mut Vec<(Component, String)>) -> Vec<(Component, String)> {
    let mut news = Vec::new();
    for r in rows.iter().filter(|r| r.update) {
        let (Some(c), Some(a)) = (r.component, &r.available) else { continue };
        let v = a.version.clone();
        if announced.iter().any(|(k, old)| *k == c && *old == v) {
            continue;
        }
        announced.retain(|(k, _)| *k != c);
        announced.push((c, v.clone()));
        news.push((c, v));
    }
    news
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::history::hist;

    #[test]
    fn mismatched_version_is_not_applied() {
        assert_eq!(check_confirmed("1.0.5", "1.0.5"), Ok(()));
        assert_eq!(check_confirmed("1.0.6", "1.0.5"), Err(trf("updm.version_changed", &["1.0.6"])));
        assert_eq!(check_confirmed("3.1.20260901", "3.1.20260814"), Err(trf("updm.version_changed", &["3.1.20260901"])));
    }

    #[test]
    fn update_flag_needs_installed_and_newer() {
        assert!(has_update(Some("1.0.4"), Some("1.0.5")));
        assert!(!has_update(Some("1.0.5"), Some("1.0.5")));
        assert!(!has_update(None, Some("9.9")), "не установлен — не обновление");
        assert!(!has_update(Some("1.0"), None));
        let saved = vec![CheckResult {
            component: Component::Native,
            available: Some(Available { version: "1.0.5".into(), published: 1, ..Default::default() }),
            error: None,
            manual_only: false,
        }];
        let history = vec![hist(3, Component::Native, Action::Update, Some("1.0.4"), None)];
        let r = rows(&saved, None, &[(Component::Native, Some("1.0.4".into())), (Component::App, Some("0.3.0".into()))], &history, &[]);
        assert!(r[0].update && r[0].installed_at == Some(history[0].at));
        assert_eq!((r[1].component, r[1].update, r[1].installed_at), (Some(Component::App), false, None));
    }

    fn found(c: Component, version: &str, published: u64) -> CheckResult {
        CheckResult {
            component: c,
            available: Some(Available { version: version.into(), published, ..Default::default() }),
            error: None,
            manual_only: false,
        }
    }

    fn date_row(saved: &[CheckResult], cache: &[ReleaseDate], c: Component, installed: &str) -> ComponentState {
        let all = [(Component::Native, None), (Component::Engine, None), (Component::App, None)];
        let installed: Vec<_> = all.into_iter().map(|(k, v)| (k, if k == c { Some(installed.to_string()) } else { v })).collect();
        rows(saved, None, &installed, &[], cache).into_iter().find(|r| r.component == Some(c)).unwrap()
    }

    /// Установленная и доступная версия — одна: одна и та же дата выхода, а не «дата установки» против «даты выхода».
    #[test]
    fn same_version_installed_and_available_shows_the_release_date_in_both_columns() {
        let saved = [found(Component::Native, "3.1.0", 1_700_000_000)];
        let hist = [hist(9, Component::Native, Action::Update, Some("3.1.0"), None)];
        let all = [(Component::Native, Some("3.1.0".to_string()))];
        let r = rows(&saved, None, &all, &hist, &[]).remove(0);
        assert_eq!((r.released, r.available.as_ref().map(|a| a.published)), (Some(1_700_000_000), Some(1_700_000_000)));
        assert_eq!(r.installed_at, Some(hist[0].at), "дата установки остаётся — для подсказки");
    }

    #[test]
    fn older_installed_version_takes_its_date_from_the_remembered_ones() {
        let saved = [found(Component::Native, "3.2.0", 2_000)];
        let cache = [ReleaseDate { component: Component::Native, version: "3.1.0".into(), published: 1_000 }];
        let r = date_row(&saved, &cache, Component::Native, "3.1.0");
        assert_eq!((r.released, r.update), (Some(1_000), true));
        assert_eq!(date_row(&saved, &cache, Component::Native, "3.0.0").released, None, "версии нет ни в проверке, ни в запомненных");
        let other = [ReleaseDate { component: Component::App, ..cache[0].clone() }];
        assert_eq!(date_row(&saved, &other, Component::Native, "3.1.0").released, None, "дата другого компонента не подходит");
        assert_eq!(date_row(&[found(Component::App, "0.5.0", 0)], &[], Component::App, "0.5.0").released, None, "дата 0 — не указана");
    }

    #[test]
    fn release_dates_are_remembered_replaced_and_pruned() {
        let mut cache = Vec::new();
        remember_found(&mut cache, &[found(Component::App, "0.5.0", 5), found(Component::Engine, "3.1", 0)]);
        assert_eq!(cache.len(), 1, "нулевая дата не запоминается");
        remember_found(&mut cache, &[found(Component::App, "0.5.0", 7)]);
        assert_eq!(cache, [ReleaseDate { component: Component::App, version: "0.5.0".into(), published: 7 }], "последний релиз главнее");
        remember(&mut cache, Component::Native, "3.0.0", 3);
        remember(&mut cache, Component::Native, "2.0.0", 2);
        let installed = [(Component::Native, Some("3.0.0".to_string())), (Component::App, Some("0.6.0".to_string()))];
        prune_released(&mut cache, &[found(Component::App, "0.5.0", 7)], &installed);
        let kept: Vec<_> = cache.iter().map(|r| (r.component, r.version.as_str())).collect();
        assert_eq!(kept, [(Component::App, "0.5.0"), (Component::Native, "3.0.0")], "остались найденная и установленная");
    }

    #[test]
    fn only_installed_versions_without_a_date_need_a_lookup() {
        let results = [found(Component::App, "0.5.0", 5), found(Component::Native, "3.2.0", 9)];
        let cache = [ReleaseDate { component: Component::Native, version: "3.0.0".into(), published: 3 }];
        let installed = [
            (Component::Native, Some("3.1.0".to_string())),
            (Component::Engine, Some("0".to_string())),
            (Component::App, Some("0.5.0".to_string())),
        ];
        assert_eq!(unknown_release_dates(&cache, &results, &installed), [(Component::Native, "3.1.0".to_string())]);
        assert!(unknown_release_dates(&cache, &results, &[(Component::Native, None)]).is_empty(), "не установлен — искать нечего");
        let known = [(Component::Native, Some("3.0.0".to_string()))];
        assert!(unknown_release_dates(&cache, &results, &known).is_empty(), "запомненная дата — запроса нет");
    }
    #[test]
    fn apply_order_is_native_engine_app() {
        let t = |c, v: &str| (c, v.to_string());
        let all = [t(Component::App, "1"), t(Component::Native, "2"), t(Component::Engine, "3"), t(Component::App, "4")];
        assert_eq!(apply_order(&all), vec![t(Component::Native, "2"), t(Component::Engine, "3"), t(Component::App, "1")]);
        assert_eq!(apply_order(&[t(Component::App, "1"), t(Component::Engine, "3")]), vec![t(Component::Engine, "3"), t(Component::App, "1")]);
    }

    #[test]
    fn new_versions_are_announced_once() {
        let row = |c, v: &str, update| ComponentState {
            component: Some(c),
            available: Some(Available { version: v.into(), ..Default::default() }),
            update,
            ..Default::default()
        };
        let mut announced = Vec::new();
        let rows1 = vec![row(Component::Native, "1.0.5", true), row(Component::App, "0.3.0", false)];
        assert_eq!(to_announce(&rows1, &mut announced), vec![(Component::Native, "1.0.5".to_string())]);
        assert!(to_announce(&rows1, &mut announced).is_empty(), "второй раз не сообщается");
        let rows2 = vec![row(Component::Native, "1.0.6", true)];
        assert_eq!(to_announce(&rows2, &mut announced), vec![(Component::Native, "1.0.6".to_string())]);
        assert_eq!(announced.len(), 1);
    }

    fn up(latest: &str) -> UpstreamCheck {
        UpstreamCheck::from(Ok(latest.to_string()))
    }

    fn engine_state(installed: &str, check: Option<&UpstreamCheck>) -> ComponentState {
        let all = [(Component::Native, None), (Component::Engine, Some(installed.to_string())), (Component::App, Some("0.3.0".into()))];
        rows(&[], check, &all, &[], &[]).remove(1)
    }

    #[test]
    fn engine_equal_to_newest_amnezia_tag_is_current() {
        let r = engine_state("3.1.20260814", Some(&up("v3.1.20260814")));
        assert_eq!(r.upstream, Some(EngineUpstream::Current("v3.1.20260814".into())));
    }

    #[test]
    fn engine_behind_amnezia_gets_the_newer_tag() {
        let r = engine_state("3.1.20260814", Some(&up("v3.1.20260901")));
        assert_eq!(r.upstream, Some(EngineUpstream::Newer("v3.1.20260901".into())));
    }

    #[test]
    fn upstream_failure_is_unchecked_and_only_the_engine_row_carries_it() {
        let failed = UpstreamCheck::from(Err("HTTP 403".to_string()));
        let all = [(Component::Native, None), (Component::Engine, Some("3.1".to_string())), (Component::App, Some("0.3.0".into()))];
        let r = rows(&[], Some(&failed), &all, &[], &[]);
        assert_eq!(r.iter().map(|r| r.upstream.clone()).collect::<Vec<_>>(), [None, Some(EngineUpstream::Unchecked), None]);
    }

    #[test]
    fn no_upstream_statement_without_a_check_or_a_known_engine_version() {
        assert_eq!(engine_state("3.1", None).upstream, None, "проверки не было (state.json прежней версии)");
        assert_eq!(engine_state("0", Some(&up("v3.1"))).upstream, None, "версия 0 — DLL не опознаны");
    }

    #[test]
    fn missing_manifest_marks_the_row_but_a_failure_does_not() {
        let msg = || OursError::no_manifest("no manifest".into());
        let mut f = Fetched { native: Err("offline".into()), ours: Err(msg()) };
        let r = available_rows(&f);
        assert_eq!((r[1].manual_only, r[2].manual_only, r[0].manual_only), (true, true, false));
        assert_eq!(r[2].error.as_deref(), Some("no manifest"));
        f.ours = Err("HTTP 500".into());
        let r = available_rows(&f);
        assert!(r.iter().all(|r| !r.manual_only), "настоящий сбой не «только вручную»");
        let shown = rows(&available_rows(&Fetched { native: Err("x".into()), ours: Err(msg()) }), None, &[(Component::App, Some("0.3.0".into()))], &[], &[]);
        assert!(shown[0].manual_only && shown[0].error.is_some());
    }

    #[test]
    fn available_rows_keep_per_source_errors() {
        let f = Fetched {
            native: Ok(feed::Release { tag: "1.0.5".into(), version: "1.0.5".into(), published: 5, notes: "n".into(), assets: vec![] }),
            ours: Err("offline".into()),
        };
        let r = available_rows(&f);
        assert_eq!(r[0].available.as_ref().map(|a| a.version.as_str()), Some("1.0.5"));
        assert_eq!((r[1].error.as_deref(), r[2].error.as_deref()), (Some("offline"), Some("offline")));
        assert_eq!(r.iter().map(|r| r.component).collect::<Vec<_>>(), ORDER.to_vec());
    }
}
