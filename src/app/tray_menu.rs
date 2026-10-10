//! Меню трея с туннелями: строки — из состояния туннелей и раскладки окна (группы), выбор пункта — то же
//! переключение, что у окна (`Switcher`): один запрос ядру, «несколько сразу» и конфликты решает ядро.
//! Меню строит поток окна и тогда, когда окно скрыто и кадров не рисует, — поэтому всё нужное лежит здесь, а не в `App`.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use eframe::egui;

use crate::crash::lock;
use crate::groups::{self, Row, TunnelBook};
use crate::i18n::tr;
use crate::monitor::Snapshot;
use crate::settings::{DialogId, Settings};
use crate::tray::{self, Entry, Hooks};

use super::list::{asks_first, Primary};
use super::Switcher;

/// Что из настроек окна нужно меню. Меняет их только окно (`publish`), а читает меню — и при скрытом окне.
#[derive(Clone, Default, PartialEq)]
pub(super) struct Layout {
    groups: bool,
    book: TunnelBook,
    multiple: bool,
    /// Скрытые диалоги: спрашивать ли подтверждение отключения (`asks_first`), как окно.
    hidden: BTreeSet<DialogId>,
}

impl Layout {
    pub(super) fn of(s: &Settings) -> Layout {
        Layout { groups: s.view.groups, book: s.book.clone(), multiple: s.multiple, hidden: s.hidden_dialogs.clone() }
    }
}

/// Раскладка, общая для окна и трея.
pub(super) type SharedLayout = Arc<Mutex<Layout>>;

/// Туннель, отключение которого из трея ждёт подтверждения в окне. Пишет трей, забирает окно в кадре.
pub(super) type DisconnectAsk = Arc<Mutex<Option<String>>>;

/// Положить в общую раскладку настройки окна, если они изменились (зовётся каждый кадр — сравнение дешёвое).
pub(super) fn publish(layout: &SharedLayout, s: &Settings) {
    let mut l = lock(layout);
    if l.groups != s.view.groups || l.multiple != s.multiple || l.book != s.book || l.hidden != s.hidden_dialogs {
        *l = Layout::of(s);
    }
}

/// Мост трея к окну: строки меню, переключение туннеля, «Выход».
pub(super) struct TrayHooks {
    pub(super) layout: SharedLayout,
    pub(super) switcher: Switcher,
    /// «Выход» в трее — как в меню: при подключённых туннелях окно спросит, отключать ли их.
    pub(super) exit_request: Arc<AtomicBool>,
    /// Отключение из трея — с тем же подтверждением, что в окне: трей кладёт сюда туннель и поднимает окно.
    pub(super) disconnect_ask: DisconnectAsk,
}

impl Hooks for TrayHooks {
    fn entries(&self) -> Vec<Entry> {
        let layout = lock(&self.layout).clone();
        let shared = &self.switcher.shared;
        entries(&layout, &shared.snapshot_clone(), &|name| shared.pending_label(name))
    }

    fn toggle(&self, tunnel: &str) {
        // Состояние — на момент выбора, не на момент открытия меню: за это время туннель мог переключиться.
        let shared = &self.switcher.shared;
        let primary = primary(&shared.snapshot_clone(), tunnel, shared.pending_label(tunnel));
        // Туннель уже переключается — как у окна: второе нажатие ничего не делает (пункт был серым).
        let Some(plan) = primary.plan() else { return };
        let (multiple, ask) = {
            let layout = lock(&self.layout);
            (layout.multiple, asks_first(plan, &layout.hidden))
        };
        if ask {
            // Диалог рисует окно; скрытое окно кадров не рисует — его сначала показываем, как при выходе.
            *lock(&self.disconnect_ask) = Some(tunnel.to_string());
            tray::show_window();
            self.switcher.ctx.request_repaint();
            return;
        }
        self.switcher.switch(tunnel.to_string(), plan, multiple);
    }

    fn exit(&self) {
        // Скрытое окно кадров не рисует — его сначала показываем; без туннелей выходим сразу.
        if !self.switcher.shared.any_running() {
            tray::remove();
            std::process::exit(0);
        }
        self.exit_request.store(true, Ordering::SeqCst);
        tray::show_window();
        self.switcher.ctx.request_repaint();
    }
}

/// Главное действие с туннелем — то же решение, что у окна (`Primary`).
fn primary(snap: &Snapshot, name: &str, pending: Option<&str>) -> Primary {
    Primary::of(snap.health(name, pending, None).level, snap.core_lost)
}

/// Строки меню: туннели по имени; при включённых группах — подменю групп, как в таблице, туннели без группы —
/// после них, в самом меню. Отметка — туннель не отключён (подключён, переключается или его переподключает ядро),
/// серый — переключается. Нет связи с ядром — первой строкой пояснение, туннели без отметок и серые, как в окне.
fn entries(layout: &Layout, snap: &Snapshot, pending: &dyn Fn(&str) -> Option<&'static str>) -> Vec<Entry> {
    let mut out: Vec<Entry> = snap.core_lost.then(|| Entry::Note(tr("tray.core_lost"))).into_iter().collect();
    out.extend(tunnel_entries(layout, snap, pending));
    out
}

fn tunnel_entries(layout: &Layout, snap: &Snapshot, pending: &dyn Fn(&str) -> Option<&'static str>) -> Vec<Entry> {
    let entry = |name: &str| {
        let primary = primary(snap, name, pending(name));
        Entry::Tunnel { name: name.to_string(), connected: primary.is_on(), enabled: primary.plan().is_some() }
    };
    let names: Vec<&String> = snap.tunnels.iter().collect();
    if !layout.groups || !layout.book.has_groups() {
        return names.iter().map(|n| entry(n)).collect();
    }
    // Те же строки, что у таблицы в режиме поиска: пустые группы скрыты, свёрнутые раскрыты.
    tree(layout.book.rows(&names, true, false), &entry)
}

/// Плоские строки таблицы (порядок обхода, глубина) -> дерево подменю.
fn tree(rows: Vec<Row>, entry: &dyn Fn(&str) -> Entry) -> Vec<Entry> {
    // Открытые группы от корня: (глубина, заголовок, строки).
    let mut open: Vec<(usize, String, Vec<Entry>)> = Vec::new();
    let mut root = Vec::new();
    let close_to = |depth: usize, open: &mut Vec<(usize, String, Vec<Entry>)>, root: &mut Vec<Entry>| {
        while open.last().is_some_and(|(d, _, _)| *d >= depth) {
            let (_, title, entries) = open.pop().expect("checked above");
            let group = Entry::Group { title, entries };
            match open.last_mut() {
                Some((_, _, parent)) => parent.push(group),
                None => root.push(group),
            }
        }
    };
    for row in rows {
        match row {
            Row::Group { path, depth, .. } => {
                close_to(depth, &mut open, &mut root);
                open.push((depth, groups::leaf(&path).to_string(), Vec::new()));
            }
            Row::Ungrouped { .. } => close_to(0, &mut open, &mut root),
            Row::Tunnel { name, group: None, .. } => root.push(entry(&name)),
            Row::Tunnel { name, depth, .. } => {
                close_to(depth, &mut open, &mut root);
                match open.last_mut() {
                    Some((_, _, entries)) => entries.push(entry(&name)),
                    None => root.push(entry(&name)),
                }
            }
        }
    }
    close_to(0, &mut open, &mut root);
    root
}

/// Щелчок по уведомлению Windows о туннеле (окно уже поднято треем): выбрать туннель, раскрыть его группы и
/// прокрутить к нему список.
pub(super) fn select_from_toast(book: &mut TunnelBook, tunnel: &str, ctx: &egui::Context) {
    book.select_tunnel(tunnel);
    // Назначение в группу, которой уже нет, таблица показывает «Без группы».
    match book.group_of(tunnel).filter(|g| book.groups().iter().any(|x| x == g)).map(str::to_string) {
        Some(mut group) => loop {
            book.expand(&group);
            match groups::parent(&group) {
                Some(p) => group = p.to_string(),
                None => break,
            }
        },
        None => book.expand(groups::UNGROUPED),
    }
    super::list::reveal_selection(ctx);
}


#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use std::time::{Duration, Instant};

    use super::*;
    use crate::monitor::{Live, Shared};
    use crate::daemon::fake::FakeCore;
    use crate::daemon::proto::Response;
    use super::super::testkit::sink;

    fn snap(tunnels: &[&str], running: &[&str]) -> Snapshot {
        Snapshot {
            tunnels: tunnels.iter().map(|t| t.to_string()).collect(),
            running: running.iter().map(|t| (t.to_string(), Live::default())).collect(),
            polls: 1,
            ..Snapshot::default()
        }
    }

    fn tunnel(name: &str, connected: bool, enabled: bool) -> Entry {
        Entry::Tunnel { name: name.into(), connected, enabled }
    }

    fn none(_: &str) -> Option<&'static str> {
        None
    }

    /// Группы Work и Work/Lab; office в Work, lab в Work/Lab, home без группы.
    fn book() -> TunnelBook {
        let lab = groups::join(Some("Work"), "Lab");
        let assignment = BTreeMap::from([("office".to_string(), "Work".to_string()), ("lab".to_string(), lab.clone())]);
        // Свёрнутая группа в меню всё равно раскрыта: подменю и так закрыто, пока на него не навели.
        TunnelBook::from_stored(&["Work".into(), lab], BTreeSet::from(["Work".to_string()]), assignment, BTreeMap::new(), None, None)
    }

    #[test]
    fn flat_menu_marks_connected_and_busy_tunnels() {
        let layout = Layout { groups: false, book: book(), multiple: false, ..Layout::default() };
        let busy = |name: &str| (name == "lab").then_some("busy.connect");
        let got = entries(&layout, &snap(&["home", "lab", "office"], &["office"]), &busy);
        // Переключаемый туннель отмечен, как в таблице («занят» — не «отключён»), но выбрать его нельзя.
        assert_eq!(got, [tunnel("home", false, true), tunnel("lab", true, false), tunnel("office", true, true)]);
    }

    #[test]
    fn grouped_menu_follows_the_table_tree() {
        let layout = Layout { groups: true, book: book(), multiple: false, ..Layout::default() };
        let got = entries(&layout, &snap(&["home", "lab", "office"], &[]), &none);
        let lab = Entry::Group { title: "Lab".into(), entries: vec![tunnel("lab", false, true)] };
        let work = Entry::Group { title: "Work".into(), entries: vec![lab, tunnel("office", false, true)] };
        assert_eq!(got, [work, tunnel("home", false, true)], "без группы — прямо в меню, после групп");
    }

    #[test]
    fn groups_view_without_groups_is_flat() {
        let layout = Layout { groups: true, ..Layout::default() };
        assert_eq!(entries(&layout, &snap(&["a"], &[]), &none), [tunnel("a", false, true)]);
    }

    #[test]
    fn lost_core_shows_unknown_not_connected() {
        // Ядро не отвечает: состояние неизвестно. Раньше все туннели были отмечены (как подключённые) и предлагали
        // «Отключить»; теперь, как в окне («Неизвестно: нет связи с ядром»), — пояснение, без отметок, серые.
        let mut s = snap(&["a", "b"], &["b"]);
        s.core_lost = true;
        let layout = Layout { groups: true, book: book(), multiple: false, ..Layout::default() };
        let got = entries(&layout, &s, &none);
        assert_eq!(got, [Entry::Note(tr("tray.core_lost")), tunnel("a", false, false), tunnel("b", false, false)]);
        s.core_lost = false;
        assert!(!entries(&layout, &s, &none).iter().any(|e| matches!(e, Entry::Note(_))), "связь есть — пояснения нет");
    }

    #[test]
    fn layout_is_republished_only_on_change() {
        let shared: SharedLayout = Arc::default();
        let mut s = Settings::default();
        s.multiple = true;
        publish(&shared, &s);
        assert!(shared.lock().unwrap().multiple);
        s.book = book();
        publish(&shared, &s);
        assert_eq!(shared.lock().unwrap().book, book());
    }

    #[test]
    fn toast_click_selects_the_tunnel_and_opens_its_groups() {
        let mut b = book();
        b.toggle(&groups::join(Some("Work"), "Lab"));
        select_from_toast(&mut b, "lab", &egui::Context::default());
        assert_eq!(b.tunnel(), Some("lab"));
        assert_eq!(b.collapsed().count(), 0, "группа туннеля и её предки раскрыты");
    }

    fn hooks(core: Arc<FakeCore>, shared: Arc<Shared>, error: super::super::errors::ErrorSink, multiple: bool) -> TrayHooks {
        let layout = Arc::new(Mutex::new(Layout { multiple, ..Layout::default() }));
        let switcher = Switcher { shared, core, error, ctx: egui::Context::default() };
        TrayHooks { layout, switcher, exit_request: Arc::default(), disconnect_ask: Arc::default() }
    }

    #[test]
    fn tray_click_sends_the_same_switch_request_as_the_window() {
        let core = Arc::new(FakeCore::new(|_| Ok(Response::Ok)));
        let (shared, error) = sink();
        hooks(core.clone(), shared.clone(), error, true).toggle("office");
        // Запрос уходит из потока переключения; пометка «занят» снимается после ответа ядра.
        let deadline = Instant::now() + Duration::from_secs(5);
        while shared.is_pending("office") && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(core.requests(), [r#"Switch { tunnel: "office", plan: Connect, multiple: true }"#], "«несколько сразу» — из настроек окна");
    }

    #[test]
    fn tray_click_on_a_busy_tunnel_sends_nothing() {
        let core = Arc::new(FakeCore::new(|_| Ok(Response::Ok)));
        let (shared, error) = sink();
        let _busy = shared.try_pending_guard("office", "busy.connect").expect("free");
        hooks(core.clone(), shared, error, false).toggle("office");
        assert!(core.requests().is_empty());
    }

    /// Туннель, который ядро держит (переподключает): главное действие — «Отключить».
    fn held(shared: &Shared, name: &str) {
        let retry = crate::daemon::proto::RetryState { attempt: 1, next_in_s: 5, last_error: String::new(), slow: false };
        shared.set_retries(BTreeMap::from([(name.to_string(), retry)]));
    }

    #[test]
    fn tray_disconnect_asks_in_the_window_first() {
        let core = Arc::new(FakeCore::new(|_| Ok(Response::Ok)));
        let (shared, error) = sink();
        held(&shared, "office");
        let hooks = hooks(core.clone(), shared, error, false);
        hooks.toggle("office");
        assert!(core.requests().is_empty(), "без подтверждения ядру ничего не уходит");
        assert_eq!(hooks.disconnect_ask.lock().unwrap().as_deref(), Some("office"), "вопрос ждёт окно");
    }

    #[test]
    fn tray_disconnect_after_dont_ask_again_goes_straight_to_the_core() {
        let core = Arc::new(FakeCore::new(|_| Ok(Response::Ok)));
        let (shared, error) = sink();
        held(&shared, "office");
        let hooks = hooks(core.clone(), shared.clone(), error, false);
        hooks.layout.lock().unwrap().hidden.insert(DialogId::Disconnect);
        hooks.toggle("office");
        let deadline = Instant::now() + Duration::from_secs(5);
        while core.requests().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(core.requests(), [r#"Switch { tunnel: "office", plan: Disconnect, multiple: false }"#]);
        assert!(hooks.disconnect_ask.lock().unwrap().is_none());
    }

    #[test]
    fn layout_carries_the_remembered_answer() {
        let shared: SharedLayout = Arc::default();
        let mut s = Settings::default();
        publish(&shared, &s);
        assert!(asks_first(crate::daemon::proto::Plan::Disconnect, &shared.lock().unwrap().hidden));
        s.hidden_dialogs.insert(DialogId::Disconnect);
        publish(&shared, &s);
        assert!(!asks_first(crate::daemon::proto::Plan::Disconnect, &shared.lock().unwrap().hidden), "трей видит «больше не спрашивать»");
    }
}
