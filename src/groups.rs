//! Книга туннелей: дерево групп, назначения туннелей, источники и выбор. Группа — путь через «/»
//! («Европа/Амстердам»), порядок в списке — порядок среди соседей. Плоские имена прежних версий — группы
//! верхнего уровня.
//! Состояние закрыто в `TunnelBook`: инварианты (пути уникальны и без пустых частей, свёрнутые и выбранная
//! группа переезжают вместе с переименованной, назначение — только в существующую группу, выбор туннеля
//! снимает подсветку группы) держат её методы, а не вызывающий код. Здесь же логика без интерфейса:
//! строки таблицы, проверка перетаскивания и итоги по поддереву.

use std::collections::{BTreeMap, BTreeSet};

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

/// Итог проверки перетаскивания или переноса.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Verdict {
    Valid,
    /// Ничего не меняется: туннель уже в этой группе, группа уже у этого родителя или брошена на себя.
    Noop,
    /// Нельзя: группа в собственного потомка или у нового родителя уже есть группа с таким именем.
    Invalid,
}

/// Назначение в группу, которой нет в списке (устаревшее действие: группу удалили в том же кадре).
#[derive(Clone, Debug, PartialEq)]
pub struct NoSuchGroup(pub String);

/// Префикс `from` → `to` (пустой `to` — верхний уровень): у самой группы и потомков.
fn rebase(path: &str, from: &str, to: &str) -> Option<String> {
    if path == from {
        return Some(to.to_string());
    }
    let rest = path.strip_prefix(from)?.strip_prefix(SEP)?;
    Some(if to.is_empty() { rest.to_string() } else { format!("{to}{SEP}{rest}") })
}

/// Всё, что окно помнит о туннелях и их раскладке: группы, свёрнутые, назначения, источники, выбор.
/// Поля закрыты; `Settings` хранит книгу и пишет её в `Settings.ini` (формат прежний).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TunnelBook {
    /// Полные пути групп; порядок — порядок среди соседей.
    groups: Vec<String>,
    /// Свёрнутые группы (пути, возможно `UNGROUPED`).
    collapsed: BTreeSet<String>,
    /// туннель → путь группы
    assignment: BTreeMap<String, String>,
    /// туннель → незашифрованный .conf-источник пользователя
    sources: BTreeMap<String, String>,
    /// Выбранный туннель текущего режима; остаётся выбранным (для панели сведений и в файле),
    /// пока подсвечена группа.
    tunnel: Option<String>,
    /// Выбор другого режима — у режимов разные наборы туннелей.
    other_tunnel: Option<String>,
    /// Подсвеченная группа (путь или `UNGROUPED`); в файл не пишется.
    group: Option<String>,
}

impl TunnelBook {
    /// Из файла настроек: список групп нормализуется, остальное берётся как есть (назначение в группу,
    /// которой нет, остаётся в файле, но туннель показывается «Без группы»).
    pub fn from_stored(
        groups: &[String],
        collapsed: BTreeSet<String>,
        assignment: BTreeMap<String, String>,
        sources: BTreeMap<String, String>,
        tunnel: Option<String>,
        other_tunnel: Option<String>,
    ) -> Self {
        TunnelBook { groups: normalize(groups), collapsed, assignment, sources, tunnel, other_tunnel, group: None }
    }

    // ---- чтение ----

    pub fn groups(&self) -> &[String] {
        &self.groups
    }

    pub fn has_groups(&self) -> bool {
        !self.groups.is_empty()
    }

    pub fn collapsed(&self) -> impl Iterator<Item = &String> {
        self.collapsed.iter()
    }

    /// (туннель, группа) — как записаны, включая назначения в уже несуществующие группы.
    pub fn assignments(&self) -> impl Iterator<Item = (&String, &String)> {
        self.assignment.iter()
    }

    pub fn sources(&self) -> impl Iterator<Item = (&String, &String)> {
        self.sources.iter()
    }

    pub fn source(&self, tunnel: &str) -> Option<&str> {
        self.sources.get(tunnel).map(String::as_str)
    }

    /// Дети группы (`None` — верхний уровень) в порядке списка.
    pub fn children(&self, of: Option<&str>) -> Vec<&String> {
        self.groups.iter().filter(|g| parent(g) == of).collect()
    }

    /// Группа туннеля, если она есть в списке; иначе туннель «Без группы».
    pub fn group_of(&self, tunnel: &str) -> Option<&str> {
        self.assignment.get(tunnel).filter(|g| self.groups.contains(g)).map(String::as_str)
    }

    /// Сколько подгрупп и туннелей лежит под группой `path` (для подтверждения удаления).
    pub fn count_below(&self, path: &str) -> (usize, usize) {
        let subgroups = self.groups.iter().filter(|g| *g != path && within(g, path)).count();
        let tunnels = self.assignment.values().filter(|g| self.groups.contains(g) && within(g, path)).count();
        (subgroups, tunnels)
    }

    /// Проверка имени новой или переименованной группы; `except` — сама переименовываемая группа.
    /// Возвращает полный путь.
    pub fn check_name(&self, parent: Option<&str>, name: &str, except: Option<&str>) -> Result<String, NameError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(NameError::Empty);
        }
        if name.contains(SEP) || name.chars().any(char::is_control) {
            return Err(NameError::BadChar);
        }
        let path = join(parent, name);
        if self.groups.iter().any(|g| *g == path && Some(g.as_str()) != except) {
            return Err(NameError::Exists);
        }
        Ok(path)
    }

    /// Можно ли сдвинуть группу среди соседей на `delta` (−1 — выше, +1 — ниже).
    pub fn can_move(&self, path: &str, delta: isize) -> bool {
        let sibs = self.children(parent(path));
        let Some(k) = sibs.iter().position(|g| *g == path) else { return false };
        let j = k as isize + delta;
        j >= 0 && (j as usize) < sibs.len()
    }

    /// Перенос группы `path` к новому родителю (`None` — верхний уровень).
    pub fn check_reparent(&self, path: &str, to: Option<&str>) -> Verdict {
        if to == Some(path) || to == parent(path) {
            return Verdict::Noop;
        }
        if to.is_some_and(|t| within(t, path)) || self.groups.contains(&join(to, leaf(path))) {
            return Verdict::Invalid;
        }
        Verdict::Valid
    }

    /// Туннель в группу `to` (`None` — «Без группы»).
    pub fn check_assign(&self, tunnel: &str, to: Option<&str>) -> Verdict {
        if self.group_of(tunnel) == to { Verdict::Noop } else { Verdict::Valid }
    }

    // ---- изменение групп ----

    /// Заменить список групп (нормализуется); назначения и свёрнутые не трогаются. Для демо-набора.
    pub fn replace_groups(&mut self, groups: &[String]) {
        self.groups = normalize(groups);
    }

    /// Новая группа последней среди соседей. Возвращает путь.
    pub fn add(&mut self, parent: Option<&str>, name: &str) -> Result<String, NameError> {
        let path = self.check_name(parent, name, None)?;
        self.groups.push(path.clone());
        Ok(path)
    }

    /// Переименование: путь меняется у группы, потомков, назначений туннелей, свёрнутых и выбранной группы.
    pub fn rename(&mut self, path: &str, name: &str) -> Result<String, NameError> {
        let to = self.check_name(parent(path), name, Some(path))?;
        self.rebase_all(path, &to);
        Ok(to)
    }

    /// Удаление: подгруппы и туннели переходят к родителю (с верхнего уровня — на верхний уровень и «Без группы»).
    /// Подгруппа с тем же именем, что уже есть у родителя, сливается с ней. Выбранная удалённая группа
    /// заменяется родителем.
    pub fn delete(&mut self, path: &str) {
        let Some(i) = self.groups.iter().position(|g| g == path) else { return };
        self.groups.remove(i);
        let up = parent(path).map(str::to_string);
        match &up {
            Some(p) => self.assignment.values_mut().filter(|g| *g == path).for_each(|g| *g = p.clone()),
            None => self.assignment.retain(|_, g| g != path),
        }
        self.collapsed.remove(path);
        let was_selected = self.group.as_deref() == Some(path);
        self.rebase_all(path, up.as_deref().unwrap_or(""));
        if was_selected {
            self.group = up;
        }
    }

    pub fn move_sibling(&mut self, path: &str, delta: isize) {
        let of = parent(path);
        let sibs: Vec<usize> = (0..self.groups.len()).filter(|&i| parent(&self.groups[i]) == of).collect();
        let Some(k) = sibs.iter().position(|&i| self.groups[i] == path) else { return };
        let j = k as isize + delta;
        if j >= 0 && (j as usize) < sibs.len() {
            self.groups.swap(sibs[k], sibs[j as usize]);
        }
    }

    /// Перенос группы со всем поддеревом последней к новому родителю. `false` — перенос невозможен.
    pub fn reparent(&mut self, path: &str, to: Option<&str>) -> bool {
        if self.check_reparent(path, to) != Verdict::Valid {
            return false;
        }
        let target = join(to, leaf(path));
        let (moved, mut rest): (Vec<String>, Vec<String>) = self.groups.drain(..).partition(|g| within(g, path));
        rest.extend(moved);
        self.groups = rest;
        self.rebase_all(path, &target);
        true
    }

    /// Туннель в группу (`None` — «Без группы»). В группу, которой нет, назначить нельзя.
    pub fn assign(&mut self, tunnel: &str, to: Option<&str>) -> Result<(), NoSuchGroup> {
        match to {
            Some(g) if !self.groups.iter().any(|x| x == g) => Err(NoSuchGroup(g.to_string())),
            Some(g) => {
                self.assignment.insert(tunnel.to_string(), g.to_string());
                Ok(())
            }
            None => {
                self.assignment.remove(tunnel);
                Ok(())
            }
        }
    }

    /// Свернуть группу или развернуть.
    pub fn toggle(&mut self, path: &str) {
        if !self.collapsed.remove(path) {
            self.collapsed.insert(path.to_string());
        }
    }

    pub fn expand(&mut self, path: &str) {
        self.collapsed.remove(path);
    }

    fn rebase_all(&mut self, from: &str, to: &str) {
        for g in self.groups.iter_mut().chain(self.assignment.values_mut()).chain(self.group.iter_mut()) {
            if let Some(new) = rebase(g, from, to) {
                *g = new;
            }
        }
        if self.group.as_deref() == Some("") {
            self.group = None;
        }
        self.collapsed = self.collapsed.iter().map(|c| rebase(c, from, to).unwrap_or_else(|| c.clone())).collect();
        let mut seen = std::collections::HashSet::new();
        self.groups.retain(|g| !g.is_empty() && seen.insert(g.clone()));
    }

    // ---- источники и туннели ----

    pub fn set_source(&mut self, tunnel: &str, path: String) {
        self.sources.insert(tunnel.to_string(), path);
    }

    /// Туннель удалён: убрать всё, что на него ссылается (группа, источник, выбор). Выбор другого режима не трогаем —
    /// это туннель другого набора.
    pub fn forget_tunnel(&mut self, name: &str) {
        self.assignment.remove(name);
        self.sources.remove(name);
        if self.tunnel.as_deref() == Some(name) {
            self.tunnel = None;
        }
    }

    /// Туннель переименован: группа, источник и выбор переезжают на новое имя.
    pub fn rename_tunnel(&mut self, old: &str, new: &str) {
        if let Some(g) = self.assignment.remove(old) {
            self.assignment.insert(new.to_string(), g);
        }
        if let Some(src) = self.sources.remove(old) {
            self.sources.insert(new.to_string(), src);
        }
        if self.tunnel.as_deref() == Some(old) {
            self.tunnel = Some(new.to_string());
        }
    }

    // ---- выбор ----

    /// Выбранный туннель текущего режима.
    pub fn tunnel(&self) -> Option<&str> {
        self.tunnel.as_deref()
    }

    /// Выбранный туннель другого режима (только для файла).
    pub fn other_tunnel(&self) -> Option<&str> {
        self.other_tunnel.as_deref()
    }

    /// Подсвеченная группа.
    pub fn selected_group(&self) -> Option<&str> {
        self.group.as_deref()
    }

    /// Строка туннеля подсвечена: выбран он, а не группа.
    pub fn is_tunnel_highlighted(&self, name: &str) -> bool {
        self.group.is_none() && self.tunnel.as_deref() == Some(name)
    }

    /// Пользователь выбрал туннель: подсветка группы снимается.
    pub fn select_tunnel(&mut self, name: &str) {
        self.tunnel = Some(name.to_string());
        self.group = None;
    }

    /// Автовыбор (прежний пропал, первый запуск): подсветка группы остаётся.
    pub fn adopt_tunnel(&mut self, name: Option<String>) {
        self.tunnel = name;
    }

    /// Пользователь выбрал группу; туннель остаётся выбранным для панели сведений.
    pub fn select_group(&mut self, path: &str) {
        self.group = Some(path.to_string());
    }

    /// Подсветка группы, которой уже нет (или группы выключены в меню «Вид»), снимается.
    pub fn drop_stale_group(&mut self, groups_shown: bool) {
        let stale = self.group.as_ref().is_some_and(|g| !groups_shown || (g != UNGROUPED && !self.groups.contains(g)));
        if stale {
            self.group = None;
        }
    }

    /// Смена режима: выбранный туннель меняется местами с запомненным для того режима.
    pub fn swap_mode_selection(&mut self) {
        std::mem::swap(&mut self.tunnel, &mut self.other_tunnel);
    }

    // ---- строки таблицы ----

    /// Строки дерева: подгруппы, затем туннели группы; «Без группы» — последней на верхнем уровне.
    /// `tunnels` — видимые туннели в порядке сортировки. При поиске (`filtering`) группы без совпадений скрыты,
    /// а предки совпавших туннелей раскрыты. Пустая «Без группы» показывается, если `keep_ungrouped`
    /// (во время перетаскивания — чтобы было куда бросить).
    pub fn rows(&self, tunnels: &[&String], filtering: bool, keep_ungrouped: bool) -> Vec<Row> {
        let mut out = Vec::new();
        self.walk(tunnels, filtering, None, 0, &mut out);
        let loose: Vec<String> = tunnels.iter().filter(|t| self.group_of(t).is_none()).map(|t| t.to_string()).collect();
        if !loose.is_empty() || keep_ungrouped {
            let collapsed = !filtering && self.collapsed.contains(UNGROUPED);
            out.push(Row::Ungrouped { collapsed, members: loose.clone() });
            if !collapsed {
                out.extend(loose.into_iter().map(|name| Row::Tunnel { name, depth: 1, group: None }));
            }
        }
        out
    }

    fn walk(&self, tunnels: &[&String], filtering: bool, of: Option<&str>, depth: usize, out: &mut Vec<Row>) {
        for g in self.children(of) {
            let members: Vec<String> =
                tunnels.iter().filter(|t| self.group_of(t).is_some_and(|tg| within(tg, g))).map(|t| t.to_string()).collect();
            if filtering && members.is_empty() {
                continue;
            }
            let collapsed = !filtering && self.collapsed.contains(g);
            out.push(Row::Group { path: g.clone(), depth, collapsed, members });
            if collapsed {
                continue;
            }
            self.walk(tunnels, filtering, Some(g), depth + 1, out);
            for t in tunnels.iter().filter(|t| self.group_of(t) == Some(g.as_str())) {
                out.push(Row::Tunnel { name: t.to_string(), depth: depth + 1, group: Some(g.clone()) });
            }
        }
    }
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

    fn book(groups: &[&str], assign: &[(&str, &str)]) -> TunnelBook {
        TunnelBook { groups: groups.iter().map(|g| g.to_string()).collect(), assignment: assign.iter().map(|(t, g)| (t.to_string(), g.to_string())).collect(), ..Default::default() }
    }

    fn list(b: &TunnelBook) -> Vec<&str> {
        b.groups.iter().map(String::as_str).collect()
    }

    #[test]
    fn paths() {
        assert_eq!(parent("Europe/Amsterdam"), Some("Europe"));
        assert_eq!(parent("Lab"), None);
        assert_eq!(leaf("Europe/NL/Amsterdam"), "Amsterdam");
        assert!(within("Europe/NL", "Europe"));
        assert!(within("Europe", "Europe"));
        assert!(!within("Europe2/NL", "Europe"));
        let b = book(&["A", "A/B", "C", "A/D", "A/B/E"], &[]);
        assert_eq!(b.children(None), ["A", "C"]);
        assert_eq!(b.children(Some("A")), ["A/B", "A/D"]);
    }

    #[test]
    fn normalize_adds_ancestors_and_keeps_flat_names() {
        let raw: Vec<String> = ["Home", "Europe/NL/Ams", " /Lab/ ", "Home"].iter().map(|s| s.to_string()).collect();
        assert_eq!(normalize(&raw), ["Home", "Europe", "Europe/NL", "Europe/NL/Ams", "Lab"]);
    }

    #[test]
    fn names_are_validated() {
        let b = book(&["Europe", "Europe/NL"], &[]);
        assert_eq!(b.check_name(None, "  ", None), Err(NameError::Empty));
        assert_eq!(b.check_name(None, "a/b", None), Err(NameError::BadChar));
        assert_eq!(b.check_name(Some("Europe"), "NL", None), Err(NameError::Exists));
        assert_eq!(b.check_name(Some("Europe"), "NL", Some("Europe/NL")), Ok("Europe/NL".into()));
        assert_eq!(b.check_name(None, " NL ", None), Ok("NL".into()));
    }

    #[test]
    fn add_subgroup() {
        let mut b = book(&["Europe"], &[]);
        assert_eq!(b.add(Some("Europe"), "DE"), Ok("Europe/DE".into()));
        assert_eq!(b.add(Some("Europe"), "DE"), Err(NameError::Exists));
        assert_eq!(list(&b), ["Europe", "Europe/DE"]);
    }

    #[test]
    fn rename_cascades() {
        let mut b = book(&["Europe", "Europe/NL", "Europe/NL/Ams", "Europe2"], &[("t1", "Europe/NL/Ams"), ("t2", "Europe2")]);
        b.toggle("Europe/NL");
        assert_eq!(b.rename("Europe", "EU"), Ok("EU".into()));
        assert_eq!(list(&b), ["EU", "EU/NL", "EU/NL/Ams", "Europe2"]);
        assert_eq!(b.assignment["t1"], "EU/NL/Ams");
        assert_eq!(b.assignment["t2"], "Europe2");
        assert!(b.collapsed.contains("EU/NL"));
        assert_eq!(b.rename("EU/NL", "x/y"), Err(NameError::BadChar));
    }

    #[test]
    fn delete_moves_children_to_parent() {
        let mut b = book(&["Europe", "Europe/NL", "Europe/NL/Ams", "Europe/DE"], &[("t1", "Europe/NL"), ("t2", "Europe/NL/Ams")]);
        b.delete("Europe/NL");
        assert_eq!(list(&b), ["Europe", "Europe/Ams", "Europe/DE"]);
        assert_eq!(b.assignment["t1"], "Europe");
        assert_eq!(b.assignment["t2"], "Europe/Ams");
        // С верхнего уровня: подгруппы — наверх, туннели — «Без группы».
        b.delete("Europe");
        assert_eq!(list(&b), ["Ams", "DE"]);
        assert!(!b.assignment.contains_key("t1"));
        assert_eq!(b.assignment["t2"], "Ams");
    }

    #[test]
    fn delete_merges_same_named_subgroup() {
        let mut b = book(&["A", "A/B", "A/B/C", "A/C"], &[("t", "A/B/C")]);
        b.delete("A/B");
        assert_eq!(list(&b), ["A", "A/C"]);
        assert_eq!(b.assignment["t"], "A/C");
    }

    #[test]
    fn move_among_siblings() {
        let mut b = book(&["A", "A/X", "B", "A/Y", "C"], &[]);
        assert!(!b.can_move("A", -1));
        assert!(b.can_move("A", 1));
        b.move_sibling("C", -1);
        assert_eq!(b.children(None), ["A", "C", "B"]);
        b.move_sibling("A/Y", -1);
        assert_eq!(b.children(Some("A")), ["A/Y", "A/X"]);
        b.move_sibling("A/X", 1); // уже последняя — без изменений
        assert_eq!(b.children(Some("A")), ["A/Y", "A/X"]);
    }

    #[test]
    fn reparent_moves_subtree_and_forbids_cycles() {
        let mut b = book(&["A", "A/B", "A/B/C", "D"], &[("t", "A/B/C")]);
        b.toggle("A/B");
        assert_eq!(b.check_reparent("A", Some("A/B/C")), Verdict::Invalid);
        assert_eq!(b.check_reparent("A", Some("A")), Verdict::Noop);
        assert_eq!(b.check_reparent("A/B", Some("A")), Verdict::Noop);
        assert!(!b.reparent("A", Some("A/B")));
        assert!(b.reparent("A/B", Some("D")));
        assert_eq!(list(&b), ["A", "D", "D/B", "D/B/C"]);
        assert_eq!(b.assignment["t"], "D/B/C");
        assert!(b.collapsed.contains("D/B"));
        assert!(b.reparent("D/B", None));
        assert_eq!(list(&b), ["A", "D", "B", "B/C"]);
        // Имя занято у нового родителя.
        let b2 = book(&["A", "A/X", "X"], &[]);
        assert_eq!(b2.check_reparent("A/X", None), Verdict::Invalid);
    }

    #[test]
    fn assign_verdict() {
        let b = book(&["A"], &[("t", "A"), ("lost", "Gone")]);
        assert_eq!(b.check_assign("t", Some("A")), Verdict::Noop);
        assert_eq!(b.check_assign("t", None), Verdict::Valid);
        // Назначение в несуществующую группу — это «Без группы».
        assert_eq!(b.check_assign("lost", None), Verdict::Noop);
    }

    #[test]
    fn assign_only_into_existing_group() {
        let mut b = book(&["A"], &[]);
        assert_eq!(b.assign("t", Some("A")), Ok(()));
        assert_eq!(b.group_of("t"), Some("A"));
        // Группа удалена в том же кадре, а действие от неё осталось: назначение не появляется.
        assert_eq!(b.assign("u", Some("Gone")), Err(NoSuchGroup("Gone".into())));
        assert_eq!(b.group_of("u"), None);
        assert!(!b.assignment.contains_key("u"));
        assert_eq!(b.assign("t", None), Ok(()));
        assert_eq!(b.group_of("t"), None);
    }

    #[test]
    fn rows_tree_and_ungrouped_last() {
        let mut b = book(&["Europe", "Europe/NL", "Lab"], &[("nl", "Europe/NL"), ("eu", "Europe"), ("lab", "Lab")]);
        let names: Vec<String> = ["eu", "free", "lab", "nl"].iter().map(|s| s.to_string()).collect();
        let refs: Vec<&String> = names.iter().collect();
        let r = b.rows(&refs, false, false);
        let short: Vec<String> = r
            .iter()
            .map(|row| match row {
                Row::Group { path, depth, members, .. } => format!("G{depth}:{path}:{}", members.len()),
                Row::Ungrouped { members, .. } => format!("U:{}", members.len()),
                Row::Tunnel { name, depth, .. } => format!("T{depth}:{name}"),
            })
            .collect();
        assert_eq!(short, ["G0:Europe:2", "G1:Europe/NL:1", "T2:nl", "T1:eu", "G0:Lab:1", "T1:lab", "U:1", "T1:free"]);
        b.toggle("Europe");
        let r = b.rows(&refs, false, false);
        assert!(matches!(&r[0], Row::Group { collapsed: true, members, .. } if members.len() == 2));
        assert!(matches!(&r[1], Row::Group { path, .. } if path == "Lab"));
    }

    #[test]
    fn search_keeps_ancestors_expanded() {
        let mut b = book(&["Europe", "Europe/NL", "Lab"], &[("nl", "Europe/NL"), ("lab", "Lab")]);
        b.toggle("Europe");
        b.toggle("Europe/NL");
        let nl = "nl".to_string();
        let r = b.rows(&[&nl], true, false);
        assert_eq!(r.len(), 3);
        assert!(matches!(&r[0], Row::Group { path, collapsed: false, .. } if path == "Europe"));
        assert!(matches!(&r[1], Row::Group { path, collapsed: false, .. } if path == "Europe/NL"));
        assert!(matches!(&r[2], Row::Tunnel { name, depth: 2, .. } if name == "nl"));
        // Без совпадений пустой «Без группы» не нужен, при перетаскивании — показан.
        assert!(!b.rows(&[], false, false).iter().any(|r| matches!(r, Row::Ungrouped { .. })));
        assert!(b.rows(&[], false, true).iter().any(|r| matches!(r, Row::Ungrouped { .. })));
    }

    #[test]
    fn count_below_counts_subtree() {
        let b = book(&["A", "A/B", "A/B/C", "D"], &[("t1", "A"), ("t2", "A/B/C"), ("t3", "D"), ("lost", "A/Gone")]);
        assert_eq!(b.count_below("A"), (2, 2));
        assert_eq!(b.count_below("D"), (0, 1));
    }

    #[test]
    fn selecting_a_tunnel_clears_the_group_and_not_vice_versa() {
        let mut b = book(&["A"], &[]);
        b.select_tunnel("t");
        b.select_group("A");
        assert_eq!((b.tunnel(), b.selected_group()), (Some("t"), Some("A")), "туннель остаётся для панели сведений");
        assert!(!b.is_tunnel_highlighted("t"));
        b.select_tunnel("u");
        assert_eq!((b.tunnel(), b.selected_group()), (Some("u"), None));
        assert!(b.is_tunnel_highlighted("u"));
        // Автовыбор подсветку группы не трогает.
        b.select_group("A");
        b.adopt_tunnel(Some("v".into()));
        assert_eq!((b.tunnel(), b.selected_group()), (Some("v"), Some("A")));
    }

    #[test]
    fn selected_group_follows_rename_reparent_and_delete() {
        let mut b = book(&["A", "A/B", "D"], &[]);
        b.select_group("A/B");
        b.rename("A", "X").unwrap();
        assert_eq!(b.selected_group(), Some("X/B"), "потомок переименованной группы переезжает с ней");
        assert!(b.reparent("X/B", Some("D")));
        assert_eq!(b.selected_group(), Some("D/B"));
        b.delete("D/B");
        assert_eq!(b.selected_group(), Some("D"), "удалённая выбранная группа заменяется родителем");
        b.delete("D");
        assert_eq!(b.selected_group(), None, "у верхнего уровня родителя нет");
        b.select_group(UNGROUPED);
        b.rename("X", "Y").unwrap();
        assert_eq!(b.selected_group(), Some(UNGROUPED));
    }

    #[test]
    fn stale_group_selection_is_dropped() {
        let mut b = book(&["A"], &[]);
        b.select_group("A");
        b.drop_stale_group(true);
        assert_eq!(b.selected_group(), Some("A"));
        b.drop_stale_group(false);
        assert_eq!(b.selected_group(), None, "группы выключены в меню «Вид»");
        b.select_group(UNGROUPED);
        b.drop_stale_group(true);
        assert_eq!(b.selected_group(), Some(UNGROUPED));
        b.select_group("Gone");
        b.drop_stale_group(true);
        assert_eq!(b.selected_group(), None);
    }

    #[test]
    fn rename_and_forget_tunnel_move_all_keyed_state() {
        let mut b = book(&["Home"], &[("old", "Home"), ("keep", "Home")]);
        b.sources.insert("old".into(), "old.conf".into());
        b.tunnel = Some("old".into());
        b.other_tunnel = Some("old".into());
        b.rename_tunnel("old", "new");
        assert_eq!((b.group_of("new"), b.source("new"), b.tunnel()), (Some("Home"), Some("old.conf"), Some("new")));
        assert!(b.assignment.len() == 2 && b.sources.len() == 1, "старые ключи не остались");
        assert_eq!(b.other_tunnel(), Some("old"), "туннель другого режима — не этот");
        b.forget_tunnel("new");
        assert_eq!((b.assignment.len(), b.sources.len(), b.tunnel()), (1, 0, None));
        assert_eq!(b.other_tunnel(), Some("old"));
    }

    #[test]
    fn mode_swap_exchanges_selections() {
        let mut b = TunnelBook::default();
        b.select_tunnel("a");
        b.swap_mode_selection();
        assert_eq!((b.tunnel(), b.other_tunnel()), (None, Some("a")));
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
