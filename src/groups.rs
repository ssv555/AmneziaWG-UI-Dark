//! Дерево групп туннелей. Группа — путь через «/» («Европа/Амстердам»), порядок в `Settings::groups` —
//! порядок среди соседей. Плоские имена прежних версий — группы верхнего уровня.
//! Здесь только логика без интерфейса: родитель и дети, добавление, переименование, удаление, перенос,
//! проверка перетаскивания, строки таблицы и итоги по поддереву.

use crate::settings::Settings;

pub const SEP: char = '/';
/// Узел «Без группы»: всегда последний на верхнем уровне. Управляющий символ — пользователь так группу не назовёт.
pub const UNGROUPED: &str = "\u{1}ungrouped";

pub fn parent(path: &str) -> Option<&str> {
    path.rsplit_once(SEP).map(|(p, _)| p)
}

/// Последняя часть пути — то, что видно в строке группы.
pub fn leaf(path: &str) -> &str {
    path.rsplit_once(SEP).map_or(path, |(_, l)| l)
}

pub fn join(parent: Option<&str>, name: &str) -> String {
    match parent {
        Some(p) => format!("{p}{SEP}{name}"),
        None => name.to_string(),
    }
}

/// `path` — сама группа `root` или её потомок.
pub fn within(path: &str, root: &str) -> bool {
    path == root || path.strip_prefix(root).is_some_and(|rest| rest.starts_with(SEP))
}

/// Дети группы (`None` — верхний уровень) в порядке списка.
pub fn children<'a>(groups: &'a [String], of: Option<&str>) -> Vec<&'a String> {
    groups.iter().filter(|g| parent(g) == of).collect()
}

/// Группа туннеля, если она есть в списке; иначе туннель «Без группы».
pub fn group_of<'a>(s: &'a Settings, tunnel: &str) -> Option<&'a str> {
    s.assignment.get(tunnel).filter(|g| s.groups.contains(g)).map(String::as_str)
}

/// Список из файла: пустые части пути отброшены, недостающие предки добавлены перед потомком, повторы убраны.
pub fn normalize(groups: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for g in groups {
        let parts: Vec<&str> = g.split(SEP).map(str::trim).filter(|p| !p.is_empty()).collect();
        for i in 1..=parts.len() {
            let path = parts[..i].join("/");
            if !out.contains(&path) {
                out.push(path);
            }
        }
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NameError {
    Empty,
    /// «/» разделяет уровни, управляющие символы не видны.
    BadChar,
    Exists,
}

/// Проверка имени новой или переименованной группы; `except` — сама переименовываемая группа.
/// Возвращает полный путь.
pub fn check_name(groups: &[String], parent: Option<&str>, name: &str, except: Option<&str>) -> Result<String, NameError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(NameError::Empty);
    }
    if name.contains(SEP) || name.chars().any(char::is_control) {
        return Err(NameError::BadChar);
    }
    let path = join(parent, name);
    if groups.iter().any(|g| *g == path && Some(g.as_str()) != except) {
        return Err(NameError::Exists);
    }
    Ok(path)
}

/// Новая группа последней среди соседей. Возвращает путь.
pub fn add(s: &mut Settings, parent: Option<&str>, name: &str) -> Result<String, NameError> {
    let path = check_name(&s.groups, parent, name, None)?;
    s.groups.push(path.clone());
    Ok(path)
}

/// Переименование: путь меняется у группы, потомков, назначений туннелей и свёрнутых.
pub fn rename(s: &mut Settings, path: &str, name: &str) -> Result<String, NameError> {
    let to = check_name(&s.groups, parent(path), name, Some(path))?;
    rebase_all(s, path, &to);
    Ok(to)
}

/// Удаление: подгруппы и туннели переходят к родителю (с верхнего уровня — на верхний уровень и «Без группы»).
/// Подгруппа с тем же именем, что уже есть у родителя, сливается с ней.
pub fn delete(s: &mut Settings, path: &str) {
    let Some(i) = s.groups.iter().position(|g| g == path) else { return };
    s.groups.remove(i);
    let up = parent(path).map(str::to_string);
    match &up {
        Some(p) => s.assignment.values_mut().filter(|g| *g == path).for_each(|g| *g = p.clone()),
        None => s.assignment.retain(|_, g| g != path),
    }
    s.collapsed.remove(path);
    rebase_all(s, path, up.as_deref().unwrap_or(""));
}

/// Можно ли сдвинуть группу среди соседей на `delta` (−1 — выше, +1 — ниже).
pub fn can_move(groups: &[String], path: &str, delta: isize) -> bool {
    let sibs = children(groups, parent(path));
    let Some(k) = sibs.iter().position(|g| *g == path) else { return false };
    let j = k as isize + delta;
    j >= 0 && (j as usize) < sibs.len()
}

pub fn move_sibling(s: &mut Settings, path: &str, delta: isize) {
    let of = parent(path);
    let sibs: Vec<usize> = (0..s.groups.len()).filter(|&i| parent(&s.groups[i]) == of).collect();
    let Some(k) = sibs.iter().position(|&i| s.groups[i] == path) else { return };
    let j = k as isize + delta;
    if j >= 0 && (j as usize) < sibs.len() {
        s.groups.swap(sibs[k], sibs[j as usize]);
    }
}

/// Итог проверки перетаскивания или переноса.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Verdict {
    Valid,
    /// Ничего не меняется: туннель уже в этой группе, группа уже у этого родителя или брошена на себя.
    Noop,
    /// Нельзя: группа в собственного потомка или у нового родителя уже есть группа с таким именем.
    Invalid,
}

/// Перенос группы `path` к новому родителю (`None` — верхний уровень).
pub fn check_reparent(groups: &[String], path: &str, to: Option<&str>) -> Verdict {
    if to == Some(path) || to == parent(path) {
        return Verdict::Noop;
    }
    if to.is_some_and(|t| within(t, path)) || groups.contains(&join(to, leaf(path))) {
        return Verdict::Invalid;
    }
    Verdict::Valid
}

/// Туннель в группу `to` (`None` — «Без группы»).
pub fn check_assign(s: &Settings, tunnel: &str, to: Option<&str>) -> Verdict {
    if group_of(s, tunnel) == to { Verdict::Noop } else { Verdict::Valid }
}

/// Перенос группы со всем поддеревом последней к новому родителю. `false` — перенос невозможен.
pub fn reparent(s: &mut Settings, path: &str, to: Option<&str>) -> bool {
    if check_reparent(&s.groups, path, to) != Verdict::Valid {
        return false;
    }
    let target = join(to, leaf(path));
    let (moved, mut rest): (Vec<String>, Vec<String>) = s.groups.drain(..).partition(|g| within(g, path));
    rest.extend(moved);
    s.groups = rest;
    rebase_all(s, path, &target);
    true
}

/// Префикс `from` → `to` (пустой `to` — верхний уровень): у самой группы и потомков.
fn rebase(path: &str, from: &str, to: &str) -> Option<String> {
    if path == from {
        return Some(to.to_string());
    }
    let rest = path.strip_prefix(from)?.strip_prefix(SEP)?;
    Some(if to.is_empty() { rest.to_string() } else { format!("{to}{SEP}{rest}") })
}

fn rebase_all(s: &mut Settings, from: &str, to: &str) {
    for g in s.groups.iter_mut().chain(s.assignment.values_mut()) {
        if let Some(new) = rebase(g, from, to) {
            *g = new;
        }
    }
    s.collapsed = s.collapsed.iter().map(|c| rebase(c, from, to).unwrap_or_else(|| c.clone())).collect();
    let mut seen = std::collections::HashSet::new();
    s.groups.retain(|g| !g.is_empty() && seen.insert(g.clone()));
}

/// Строка таблицы туннелей в режиме групп.
#[derive(Clone, Debug, PartialEq)]
pub enum Row {
    /// `members` — видимые туннели всего поддерева (для итогов), `collapsed` — с учётом поиска.
    Group { path: String, depth: usize, collapsed: bool, members: Vec<String> },
    Ungrouped { collapsed: bool, members: Vec<String> },
    /// `group` — где туннель лежит (`None` — «Без группы»).
    Tunnel { name: String, depth: usize, group: Option<String> },
}

/// Строки дерева: подгруппы, затем туннели группы; «Без группы» — последней на верхнем уровне.
/// `tunnels` — видимые туннели в порядке сортировки. При поиске (`filtering`) группы без совпадений скрыты,
/// а предки совпавших туннелей раскрыты. Пустая «Без группы» показывается, если `keep_ungrouped`
/// (во время перетаскивания — чтобы было куда бросить).
pub fn rows(s: &Settings, tunnels: &[&String], filtering: bool, keep_ungrouped: bool) -> Vec<Row> {
    let mut out = Vec::new();
    walk(s, tunnels, filtering, None, 0, &mut out);
    let loose: Vec<String> = tunnels.iter().filter(|t| group_of(s, t).is_none()).map(|t| t.to_string()).collect();
    if !loose.is_empty() || keep_ungrouped {
        let collapsed = !filtering && s.collapsed.contains(UNGROUPED);
        out.push(Row::Ungrouped { collapsed, members: loose.clone() });
        if !collapsed {
            out.extend(loose.into_iter().map(|name| Row::Tunnel { name, depth: 1, group: None }));
        }
    }
    out
}

fn walk(s: &Settings, tunnels: &[&String], filtering: bool, of: Option<&str>, depth: usize, out: &mut Vec<Row>) {
    for g in children(&s.groups, of) {
        let members: Vec<String> =
            tunnels.iter().filter(|t| group_of(s, t).is_some_and(|tg| within(tg, g))).map(|t| t.to_string()).collect();
        if filtering && members.is_empty() {
            continue;
        }
        let collapsed = !filtering && s.collapsed.contains(g);
        out.push(Row::Group { path: g.clone(), depth, collapsed, members });
        if collapsed {
            continue;
        }
        walk(s, tunnels, filtering, Some(g), depth + 1, out);
        for t in tunnels.iter().filter(|t| group_of(s, t) == Some(g.as_str())) {
            out.push(Row::Tunnel { name: t.to_string(), depth: depth + 1, group: Some(g.clone()) });
        }
    }
}

/// Итоги строки группы по всему поддереву.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Agg {
    pub total: usize,
    pub active: usize,
    pub rx: u64,
    pub tx: u64,
    /// Максимум пиков.
    pub peak: f64,
    /// Сумма долей времени.
    pub share: f64,
}

impl Agg {
    pub fn add(&mut self, active: bool, rx: u64, tx: u64, peak: f64, share: f64) {
        self.total += 1;
        self.active += usize::from(active);
        self.rx += rx;
        self.tx += tx;
        self.peak = self.peak.max(peak);
        self.share += share;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(groups: &[&str], assign: &[(&str, &str)]) -> Settings {
        let mut s = Settings::default();
        s.groups = groups.iter().map(|g| g.to_string()).collect();
        s.assignment = assign.iter().map(|(t, g)| (t.to_string(), g.to_string())).collect();
        s
    }

    fn list(s: &Settings) -> Vec<&str> {
        s.groups.iter().map(String::as_str).collect()
    }

    #[test]
    fn paths() {
        assert_eq!(parent("Europe/Amsterdam"), Some("Europe"));
        assert_eq!(parent("Lab"), None);
        assert_eq!(leaf("Europe/NL/Amsterdam"), "Amsterdam");
        assert!(within("Europe/NL", "Europe"));
        assert!(within("Europe", "Europe"));
        assert!(!within("Europe2/NL", "Europe"));
        let g: Vec<String> = ["A", "A/B", "C", "A/D", "A/B/E"].iter().map(|s| s.to_string()).collect();
        assert_eq!(children(&g, None), ["A", "C"]);
        assert_eq!(children(&g, Some("A")), ["A/B", "A/D"]);
    }

    #[test]
    fn normalize_adds_ancestors_and_keeps_flat_names() {
        let raw: Vec<String> = ["Home", "Europe/NL/Ams", " /Lab/ ", "Home"].iter().map(|s| s.to_string()).collect();
        assert_eq!(normalize(&raw), ["Home", "Europe", "Europe/NL", "Europe/NL/Ams", "Lab"]);
    }

    #[test]
    fn names_are_validated() {
        let g = vec!["Europe".to_string(), "Europe/NL".to_string()];
        assert_eq!(check_name(&g, None, "  ", None), Err(NameError::Empty));
        assert_eq!(check_name(&g, None, "a/b", None), Err(NameError::BadChar));
        assert_eq!(check_name(&g, Some("Europe"), "NL", None), Err(NameError::Exists));
        assert_eq!(check_name(&g, Some("Europe"), "NL", Some("Europe/NL")), Ok("Europe/NL".into()));
        assert_eq!(check_name(&g, None, " NL ", None), Ok("NL".into()));
    }

    #[test]
    fn add_subgroup() {
        let mut s = settings(&["Europe"], &[]);
        assert_eq!(add(&mut s, Some("Europe"), "DE"), Ok("Europe/DE".into()));
        assert_eq!(add(&mut s, Some("Europe"), "DE"), Err(NameError::Exists));
        assert_eq!(list(&s), ["Europe", "Europe/DE"]);
    }

    #[test]
    fn rename_cascades() {
        let mut s = settings(&["Europe", "Europe/NL", "Europe/NL/Ams", "Europe2"], &[("t1", "Europe/NL/Ams"), ("t2", "Europe2")]);
        s.collapsed.insert("Europe/NL".into());
        assert_eq!(rename(&mut s, "Europe", "EU"), Ok("EU".into()));
        assert_eq!(list(&s), ["EU", "EU/NL", "EU/NL/Ams", "Europe2"]);
        assert_eq!(s.assignment["t1"], "EU/NL/Ams");
        assert_eq!(s.assignment["t2"], "Europe2");
        assert!(s.collapsed.contains("EU/NL"));
        assert_eq!(rename(&mut s, "EU/NL", "x/y"), Err(NameError::BadChar));
    }

    #[test]
    fn delete_moves_children_to_parent() {
        let mut s = settings(&["Europe", "Europe/NL", "Europe/NL/Ams", "Europe/DE"], &[("t1", "Europe/NL"), ("t2", "Europe/NL/Ams")]);
        delete(&mut s, "Europe/NL");
        assert_eq!(list(&s), ["Europe", "Europe/Ams", "Europe/DE"]);
        assert_eq!(s.assignment["t1"], "Europe");
        assert_eq!(s.assignment["t2"], "Europe/Ams");
        // С верхнего уровня: подгруппы — наверх, туннели — «Без группы».
        delete(&mut s, "Europe");
        assert_eq!(list(&s), ["Ams", "DE"]);
        assert!(!s.assignment.contains_key("t1"));
        assert_eq!(s.assignment["t2"], "Ams");
    }

    #[test]
    fn delete_merges_same_named_subgroup() {
        let mut s = settings(&["A", "A/B", "A/B/C", "A/C"], &[("t", "A/B/C")]);
        delete(&mut s, "A/B");
        assert_eq!(list(&s), ["A", "A/C"]);
        assert_eq!(s.assignment["t"], "A/C");
    }

    #[test]
    fn move_among_siblings() {
        let mut s = settings(&["A", "A/X", "B", "A/Y", "C"], &[]);
        assert!(!can_move(&s.groups, "A", -1));
        assert!(can_move(&s.groups, "A", 1));
        move_sibling(&mut s, "C", -1);
        assert_eq!(children(&s.groups, None), ["A", "C", "B"]);
        move_sibling(&mut s, "A/Y", -1);
        assert_eq!(children(&s.groups, Some("A")), ["A/Y", "A/X"]);
        move_sibling(&mut s, "A/X", 1); // уже последняя — без изменений
        assert_eq!(children(&s.groups, Some("A")), ["A/Y", "A/X"]);
    }

    #[test]
    fn reparent_moves_subtree_and_forbids_cycles() {
        let mut s = settings(&["A", "A/B", "A/B/C", "D"], &[("t", "A/B/C")]);
        s.collapsed.insert("A/B".into());
        assert_eq!(check_reparent(&s.groups, "A", Some("A/B/C")), Verdict::Invalid);
        assert_eq!(check_reparent(&s.groups, "A", Some("A")), Verdict::Noop);
        assert_eq!(check_reparent(&s.groups, "A/B", Some("A")), Verdict::Noop);
        assert!(!reparent(&mut s, "A", Some("A/B")));
        assert!(reparent(&mut s, "A/B", Some("D")));
        assert_eq!(list(&s), ["A", "D", "D/B", "D/B/C"]);
        assert_eq!(s.assignment["t"], "D/B/C");
        assert!(s.collapsed.contains("D/B"));
        assert!(reparent(&mut s, "D/B", None));
        assert_eq!(list(&s), ["A", "D", "B", "B/C"]);
        // Имя занято у нового родителя.
        let s2 = settings(&["A", "A/X", "X"], &[]);
        assert_eq!(check_reparent(&s2.groups, "A/X", None), Verdict::Invalid);
    }

    #[test]
    fn assign_verdict() {
        let s = settings(&["A"], &[("t", "A"), ("lost", "Gone")]);
        assert_eq!(check_assign(&s, "t", Some("A")), Verdict::Noop);
        assert_eq!(check_assign(&s, "t", None), Verdict::Valid);
        // Назначение в несуществующую группу — это «Без группы».
        assert_eq!(check_assign(&s, "lost", None), Verdict::Noop);
    }

    #[test]
    fn rows_tree_and_ungrouped_last() {
        let mut s = settings(&["Europe", "Europe/NL", "Lab"], &[("nl", "Europe/NL"), ("eu", "Europe"), ("lab", "Lab")]);
        let names: Vec<String> = ["eu", "free", "lab", "nl"].iter().map(|s| s.to_string()).collect();
        let refs: Vec<&String> = names.iter().collect();
        let r = rows(&s, &refs, false, false);
        let short: Vec<String> = r
            .iter()
            .map(|row| match row {
                Row::Group { path, depth, members, .. } => format!("G{depth}:{path}:{}", members.len()),
                Row::Ungrouped { members, .. } => format!("U:{}", members.len()),
                Row::Tunnel { name, depth, .. } => format!("T{depth}:{name}"),
            })
            .collect();
        assert_eq!(short, ["G0:Europe:2", "G1:Europe/NL:1", "T2:nl", "T1:eu", "G0:Lab:1", "T1:lab", "U:1", "T1:free"]);
        s.collapsed.insert("Europe".into());
        let r = rows(&s, &refs, false, false);
        assert!(matches!(&r[0], Row::Group { collapsed: true, members, .. } if members.len() == 2));
        assert!(matches!(&r[1], Row::Group { path, .. } if path == "Lab"));
    }

    #[test]
    fn search_keeps_ancestors_expanded() {
        let mut s = settings(&["Europe", "Europe/NL", "Lab"], &[("nl", "Europe/NL"), ("lab", "Lab")]);
        s.collapsed.insert("Europe".into());
        s.collapsed.insert("Europe/NL".into());
        let nl = "nl".to_string();
        let r = rows(&s, &[&nl], true, false);
        assert_eq!(r.len(), 3);
        assert!(matches!(&r[0], Row::Group { path, collapsed: false, .. } if path == "Europe"));
        assert!(matches!(&r[1], Row::Group { path, collapsed: false, .. } if path == "Europe/NL"));
        assert!(matches!(&r[2], Row::Tunnel { name, depth: 2, .. } if name == "nl"));
        // Без совпадений пустой «Без группы» не нужен, при перетаскивании — показан.
        assert!(!rows(&s, &[], false, false).iter().any(|r| matches!(r, Row::Ungrouped { .. })));
        assert!(rows(&s, &[], false, true).iter().any(|r| matches!(r, Row::Ungrouped { .. })));
    }

    #[test]
    fn aggregate_sums_and_max() {
        let mut a = Agg::default();
        a.add(true, 10, 1, 5.0, 0.25);
        a.add(false, 20, 2, 9.0, 0.5);
        a.add(true, 0, 0, 1.0, 0.0);
        assert_eq!((a.total, a.active, a.rx, a.tx), (3, 2, 30, 3));
        assert_eq!(a.peak, 9.0);
        assert_eq!(a.share, 0.75);
    }
}
