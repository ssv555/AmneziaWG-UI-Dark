//! Окно «Обновления и откаты» (Справка → «Проверить обновления…»): компоненты, их версии и что нового,
//! установка выбранного с подтверждением, история с кнопкой «Вернуть». Всё делает ядро: окно спрашивает
//! его состояние раз в секунду, пока открыто, и раз в 30 минут — для отметки «есть новое» в меню «Справка».

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Align2, Color32, Layout, RichText, Ui, Vec2};

use super::dialog::{dialog_window, window_escape, window_keys};
use super::markdown;
use super::modals::{Modal, Outcome as ModalOutcome, Turn};
use super::{dialog_buttons, mono, Core, ErrorSink, GRAY, GREEN, RED, YELLOW};
use crate::fmt;
use crate::i18n::{tr, trf};
use crate::monitor::Shared;
use crate::update::{Action, Available, Component, ComponentState, HistoryEntry, RestoreBlock, RestoreOffer, UpdateOp, UpdatesState};
use crate::elevated::Outcome;

const POLL_EVERY: Duration = Duration::from_secs(1);
const BADGE_EVERY: Duration = Duration::from_secs(30 * 60);
/// Окно-уведомление о новых версиях: `id` и для позиции, и для Esc верхнему окну.
const NEWS_ID: &str = "updates-news";
/// Окно обновлений: то же для него.
const WINDOW_ID: &str = "updates";
/// Окно «Что нового»: оно одно, поэтому `id` постоянный — новое открытие занимает место прежнего.
const NOTES_ID: &str = "updates-notes";
const ROW: f32 = 26.0;
const BUTTON: Vec2 = Vec2::new(120.0, 26.0);
// Ширины колонок; «Компонент» занимает остаток, но не меньше своего минимума.
const W_CHECK: f32 = 28.0;
const W_VERSION: f32 = 170.0;
const W_STATUS: f32 = 250.0;
const W_NEWS: f32 = 110.0;
/// Окно «Что нового»: начальный и наименьший размер.
const NOTES_SIZE: [f32; 2] = [560.0, 420.0];
const NOTES_MIN: [f32; 2] = [320.0, 200.0];
const MIN_NAME: f32 = 200.0;
const W_DATE: f32 = 140.0;
const W_ACTION: f32 = 120.0;
const W_FROM_TO: f32 = 150.0;
const W_SIZE: f32 = 80.0;
const W_RESULT: f32 = 70.0;
// Подпись «Вернуть <версия>»: версии вида 3.1.20260814 длинные.
const W_RESTORE: f32 = 170.0;
const MIN_HIST_NAME: f32 = 120.0;

/// Окно «Обновления и откаты»: состояние, показ, опрос ядра и подтверждение. Принадлежит `App`, остальное окно
/// видит только публичные методы; поля закрыты.
pub(super) struct UpdatesWindow {
    /// С чем окно говорит: ядро, журнал ошибок, строка уведомления, перерисовка, режим монитора.
    link: Link,
    open: bool,
    polled: Arc<Mutex<Polled>>,
    /// Идёт опрос состояния — следующий не начинается, пока не ответил этот.
    polling: InFlight,
    polled_at: Option<Instant>,
    /// Команда (проверка, установка, возврат) ждёт ответа ядра; их может быть несколько сразу.
    command: InFlight,
    /// Номер последнего запроса: ответ, пришедший позже более нового, не затирает его.
    seq: u64,
    selected: BTreeSet<Component>,
    /// Компоненты, чьё обновление уже показано: галочку ставим один раз, снятая так и остаётся.
    offered: BTreeSet<Component>,
    badge: Arc<AtomicBool>,
    badge_at: Option<Instant>,
    /// Обновления из последнего ответа ядра; кадр забирает их и решает, о чём сообщить (настройки — в потоке окна).
    proposed: Arc<Mutex<Option<Vec<(Component, String)>>>>,
    /// Показанное и ещё не закрытое уведомление: компонент и версия.
    news: Vec<(Component, String)>,
    /// Уведомление — активное окно: последний щелчок мыши был по нему. Только тогда ему достаётся Esc.
    news_focused: bool,
    /// Открытое окно «Что нового» (одно): текст релиза выбранного компонента.
    notes: Option<Notes>,
}

/// Текст релиза в окне «Что нового»: разобран один раз при открытии, дальше окно не зависит от опроса ядра.
struct Notes {
    title: String,
    blocks: Vec<markdown::Block>,
}

/// Внешнее, что окну нужно от программы; клонируется в потоки запросов.
#[derive(Clone)]
pub(super) struct Link {
    core: Core,
    error: ErrorSink,
    /// Строка уведомления в главном окне (отмена UAC).
    notice: Arc<Mutex<Option<String>>>,
    ctx: egui::Context,
    shared: Arc<Shared>,
}

impl Link {
    pub(super) fn new(core: Core, error: ErrorSink, notice: Arc<Mutex<Option<String>>>, ctx: egui::Context, shared: Arc<Shared>) -> Self {
        Self { core, error, notice, ctx, shared }
    }
}

/// Запросы в пути: у каждого свой `Pending`, счётчик ненулевой, пока жив хоть один. Прежний общий флаг сбрасывал
/// тот, кто закончил первым, даже если рядом ещё шёл другой запрос.
#[derive(Clone, Default)]
struct InFlight(Arc<AtomicUsize>);

/// Один запрос в пути; запись о нём снимается в `Drop`, в том числе при панике потока.
struct Pending(InFlight);

impl InFlight {
    fn begin(&self) -> Pending {
        self.0.fetch_add(1, Ordering::SeqCst);
        Pending(self.clone())
    }

    fn any(&self) -> bool {
        self.0.load(Ordering::SeqCst) > 0
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        (self.0).0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct Polled {
    state: UpdatesState,
    /// Ошибка последнего запроса; уходит в журнал один раз, пока не сменится.
    error: Option<String>,
    applied: u64,
}

/// Что ждёт подтверждения в окне обновлений.
#[derive(Clone)]
pub(super) enum Confirm {
    Apply(Vec<Component>),
    /// Возврат: номер строки истории (цель по ней решает ядро), компонент и версия цели — для текста вопроса.
    Restore { id: u64, component: Component, version: String },
}

pub(super) fn is_updates_confirm(m: &Modal) -> bool {
    matches!(m, Modal::UpdatesConfirm(_))
}

/// Нажатия в окне — разбираются после кадра окна.
enum Click {
    Check,
    Toggle(Component),
    Notes(Component),
    Apply(Vec<Component>),
    Restore { id: u64, component: Component, version: String },
}

struct View<'a> {
    state: &'a UpdatesState,
    error: Option<&'a str>,
    selected: &'a BTreeSet<Component>,
    /// Ядро занято или ждём ответа на команду.
    busy: bool,
    /// Кнопки недоступны: занято или открыто подтверждение.
    locked: bool,
}

/// Что окно просит у остального приложения после кадра.
pub(super) struct Frame {
    /// Нажато «Обновить» или «Вернуть»: подтверждение рисуется как диалог `Modals`.
    pub(super) confirm: Option<Confirm>,
    /// Окно закрыто в этом кадре — его подтверждение закрывается вместе с ним.
    pub(super) closed: bool,
}

impl UpdatesWindow {
    pub(super) fn new(link: Link) -> Self {
        Self {
            link,
            open: false,
            polled: Arc::default(),
            polling: InFlight::default(),
            polled_at: None,
            command: InFlight::default(),
            seq: 0,
            selected: BTreeSet::new(),
            offered: BTreeSet::new(),
            badge: Arc::default(),
            badge_at: None,
            proposed: Arc::default(),
            news: Vec::new(),
            news_focused: false,
            notes: None,
        }
    }

    pub(super) fn is_open(&self) -> bool {
        self.open
    }

    /// У какого-то компонента есть обновление — отметка в меню «Справка».
    pub(super) fn has_new(&self) -> bool {
        self.badge.load(Ordering::SeqCst)
    }

    /// Справка → «Проверить обновления…»: окно сразу с текущим состоянием и проверка источников.
    pub(super) fn open(&mut self) {
        self.open = true;
        self.polled_at = None;
        if !self.command.any() {
            self.request(UpdateOp::Check, true);
        }
    }

    /// Каждый кадр до окна: отметка в меню и новые обновления, о которых ещё не сообщали (`notified` — настройки).
    /// Всплывающее уведомление Windows или его отсутствие решает вызывающий: он знает, скрыто ли главное окно.
    pub(super) fn tick(&mut self, notified: &mut BTreeMap<String, String>) -> Vec<(Component, String)> {
        self.refresh_badge();
        self.collect_news(notified)
    }

    /// Окно со списком компонентов и историей, опрос ядра. `confirm_open` — подтверждение этого окна уже открыто.
    pub(super) fn show(&mut self, ctx: &egui::Context, confirm_open: bool) -> Frame {
        let mut frame = Frame { confirm: None, closed: false };
        if !self.open {
            return frame;
        }
        self.poll();
        let (state, error) = {
            let p = self.polled.lock().unwrap();
            (p.state.clone(), p.error.clone())
        };
        sync_selection(&state.components, &mut self.selected, &mut self.offered);
        let busy = state.busy.is_some() || self.command.any();
        let mut clicks = Vec::new();
        let mut open = true;
        dialog_window(ctx, tr("upd.title"), WINDOW_ID, &mut open)
            .resizable(true)
            .default_size([1000.0, 560.0])
            .min_size([920.0, 360.0])
            .show(ctx, |ui| {
                let view = View { state: &state, error: error.as_deref(), selected: &self.selected, busy, locked: busy || confirm_open };
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| window_body(ui, &view, &mut clicks));
            });
        // Подтверждение (оно в `Modals`, его кадр уже прошёл) забрало Enter и Esc; без него Esc закрывает само окно,
        // если оно верхнее.
        if !confirm_open && window_escape(ctx, WINDOW_ID) {
            open = false;
        }
        for click in clicks {
            match click {
                Click::Check => self.request(UpdateOp::Check, true),
                Click::Toggle(id) => toggle(&mut self.selected, id),
                Click::Notes(id) => self.open_notes(ctx, &state, id),
                Click::Apply(list) => frame.confirm = Some(Confirm::Apply(list)),
                Click::Restore { id, component, version } => frame.confirm = Some(Confirm::Restore { id, component, version }),
            }
        }
        if open {
            self.show_notes(ctx);
            ctx.request_repaint_after(POLL_EVERY);
        } else {
            self.open = false;
            self.notes = None;
            frame.closed = true;
        }
        frame
    }

    /// «Что нового…» в строке компонента: окно с текстом его релиза. Открытое окно другого компонента заменяется.
    fn open_notes(&mut self, ctx: &egui::Context, state: &UpdatesState, id: Component) {
        let found = state.components.iter().find(|c| c.component == Some(id)).and_then(release_notes);
        let Some(a) = found else { return };
        let title = trf("upd.notes_title", &[&short(id), &a.version]);
        self.notes = Some(Notes { title, blocks: markdown::parse(&a.notes) });
        // Окно, открытое раньше, остаётся на своём месте, но выходит наверх: оно же и получает Esc.
        ctx.move_to_top(egui::LayerId::new(egui::Order::Middle, egui::Id::new(NOTES_ID)));
    }

    /// Окно «Что нового»: заголовок, перетаскивание, крестик, Esc и Enter закрывают; размер меняется, текст прокручивается,
    /// «Закрыть» всегда внизу справа.
    fn show_notes(&mut self, ctx: &egui::Context) {
        let Some(notes) = &self.notes else { return };
        let mut open = true;
        let mut close = false;
        dialog_window(ctx, notes.title.as_str(), NOTES_ID, &mut open)
            .resizable(true)
            .default_size(NOTES_SIZE)
            .min_size(NOTES_MIN)
            .show(ctx, |ui| {
                // Кнопка не уходит за край окна при любой длине текста: прокручивается только он.
                let body = (ui.available_height() - ROW - ui.spacing().item_spacing.y).max(NOTES_MIN[1] / 2.0);
                egui::ScrollArea::vertical().id_salt("upd-notes-scroll").auto_shrink([false, false]).max_height(body).show(ui, |ui| {
                    markdown::show(ui, &notes.blocks);
                });
                close = dialog_buttons(ui, &tr("btn.close"), true, None).0;
            });
        let (enter, escape) = window_keys(ctx, NOTES_ID);
        if close || enter || escape || !open {
            self.notes = None;
        }
    }

    /// Подтверждение установки или возврата — диалог из `Modals` поверх окна обновлений.
    pub(super) fn show_confirm(&mut self, ctx: &egui::Context, confirm: &Confirm, turn: Turn) -> ModalOutcome {
        let state = &self.polled.lock().unwrap().state.clone();
        let busy = state.busy.is_some() || self.command.any();
        let (title, primary, lines, notes) = match confirm {
            Confirm::Apply(list) => {
                let mut notes = warnings(list);
                notes.push(tr("upd.warn_backup"));
                (tr("upd.confirm_title"), tr("upd.update"), apply_lines(state, list), notes)
            }
            Confirm::Restore { component, version, .. } => {
                let mut notes = warnings(&[*component]);
                notes.push(tr("upd.warn_uac"));
                (tr("upd.restore_title"), tr("upd.restore"), vec![restore_text(*component, version)], notes)
            }
        };
        let (mut yes, mut no) = (false, false);
        let mut open = true;
        dialog_window(ctx, title, "updates-confirm", &mut open)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                ui.set_width(480.0);
                for line in &lines {
                    ui.add(egui::Label::new(line).wrap());
                }
                if !notes.is_empty() {
                    ui.add_space(8.0);
                    for note in &notes {
                        ui.add(egui::Label::new(RichText::new(note).color(YELLOW)).wrap());
                    }
                }
                ui.add_space(10.0);
                (yes, no) = dialog_buttons(ui, &primary, !busy, Some(&tr("btn.cancel")));
            });
        let (enter, escape) = turn.keys(ctx);
        match answer(yes || enter, no || escape || !open, busy) {
            Answer::Run => {
                match confirm {
                    Confirm::Apply(list) => self.request(UpdateOp::Apply(targets(state, list)), true),
                    Confirm::Restore { id, .. } => self.restore_elevated(*id),
                }
                ModalOutcome::Close
            }
            Answer::Close => ModalOutcome::Close,
            Answer::Keep => ModalOutcome::Keep,
        }
    }

    /// Небольшое окно без блокировки: остальная программа работает, фокус ввода не забирается.
    /// Появившись, уведомление не становится активным окном: оно в слое `Foreground` (поверх окон, но вне их
    /// очереди за Esc — `window_escape` смотрит слой `Middle`), а Esc получает, только когда по нему щёлкнули.
    pub(super) fn show_news(&mut self, ctx: &egui::Context) {
        if self.open {
            // Список обновлений уже перед глазами.
            self.news.clear();
        }
        if self.news.is_empty() {
            self.news_focused = false;
            return;
        }
        let text = news_text(&self.news);
        let (mut shown, mut open, mut later) = (true, false, false);
        // Активным уведомление делает щелчок по нему, щелчок в любом другом месте — снимает. Проверка — до показа:
        // попадание считается по раскладке прошлого кадра (её и видел пользователь); после `show` в этом кадре
        // `layer_id_at` уведомление ещё не находит (проверено тестом `news_takes_escape_only_after_click`).
        if let Some(at) = ctx.input(|i| i.pointer.press_origin().filter(|_| i.pointer.any_pressed())) {
            self.news_focused = ctx.layer_id_at(at) == Some(egui::LayerId::new(egui::Order::Foreground, egui::Id::new(NEWS_ID)));
        }
        let escape = self.news_focused && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        // Сначала в правом нижнем углу, но не прибито: перетаскивается за заголовок, крестик и Esc — «Позже».
        dialog_window(ctx, tr("upd.notice_title"), NEWS_ID, &mut shown)
            .order(egui::Order::Foreground)
            .pivot(Align2::RIGHT_BOTTOM)
            .default_pos(ctx.screen_rect().right_bottom() - Vec2::splat(12.0))
            .show(ctx, |ui| {
                ui.set_max_width(320.0);
                ui.add(egui::Label::new(text).wrap());
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    open = ui.button(tr("upd.notice_open")).clicked();
                    later = ui.button(tr("upd.notice_later")).clicked();
                });
            });
        later |= !shown || escape;
        if open || later {
            self.news.clear();
            self.news_focused = false;
        }
        if open {
            self.open();
        }
    }

    /// Опрос состояния раз в секунду, пока окно открыто; запросы не накладываются.
    fn poll(&mut self) {
        let now = Instant::now();
        if !poll_due(self.polling.any(), self.polled_at, now) {
            return;
        }
        self.polled_at = Some(now);
        self.request(UpdateOp::State, false);
    }

    /// Запрос ядру в фоне. Ошибка команды всегда уходит в журнал, ошибка опроса — один раз, пока не сменится.
    fn request(&mut self, op: UpdateOp, command: bool) {
        self.seq += 1;
        let pending = if command { self.command.begin() } else { self.polling.begin() };
        let (link, seq, polled, badge, proposed) = (self.link.clone(), self.seq, self.polled.clone(), self.badge.clone(), self.proposed.clone());
        std::thread::spawn(move || {
            take_updates_reply(link.core.updates(op), command, seq, &polled, &badge, &proposed, &link.error);
            drop(pending);
            link.ctx.request_repaint();
        });
    }

    /// «Вернуть»: ядро выполняет возврат только по запросу с правами администратора, поэтому его отправляет наш же
    /// exe, запущенный через UAC; окно тем временем опрашивает состояние как обычно. Отмена UAC — не ошибка.
    fn restore_elevated(&mut self, id: u64) {
        let pending = self.command.begin();
        let link = self.link.clone();
        std::thread::spawn(move || {
            match crate::elevated::run(&crate::update::restore_args(id), "upd.restore_failed") {
                Outcome::Done(_) => {}
                Outcome::Failed(e) => link.error.push(e),
                Outcome::Cancelled => *link.notice.lock().unwrap() = Some(tr("upd.restore_cancelled")),
            }
            drop(pending);
            link.ctx.request_repaint();
        });
    }

    /// Отметка «есть новое»: при запуске и раз в 30 минут, только с ядром. Пока окно открыто, её ведёт опрос окна.
    fn refresh_badge(&mut self) {
        let now = Instant::now();
        if !badge_due(self.open, self.link.shared.mirrors_core(), self.badge_at, now) {
            return;
        }
        self.badge_at = Some(now);
        let (link, badge, proposed) = (self.link.clone(), self.badge.clone(), self.proposed.clone());
        std::thread::spawn(move || {
            // Ошибку в журнал не пишем: фоновая проверка повторялась бы в нём каждые 30 минут; окно покажет её само.
            if let Ok(state) = link.core.updates(UpdateOp::State) {
                badge.store(has_updates(&state), Ordering::SeqCst);
                *proposed.lock().unwrap() = Some(offered_updates(&state));
                link.ctx.request_repaint();
            }
        });
    }

    /// Новый ответ ядра: что из предложенного ещё не сообщали.
    fn collect_news(&mut self, notified: &mut BTreeMap<String, String>) -> Vec<(Component, String)> {
        let Some(offered) = self.proposed.lock().unwrap().take() else { return Vec::new() };
        refresh_news(&mut self.news, &offered, notified)
    }
}

/// Опрос состояния: прошлый ответ получен и с прошлого опроса прошла секунда.
fn poll_due(polling: bool, polled_at: Option<Instant>, now: Instant) -> bool {
    !polling && polled_at.map_or(true, |t| now.saturating_duration_since(t) >= POLL_EVERY)
}

/// Фоновая проверка для отметки меню: окно закрыто (иначе её ведёт его опрос), ядро есть, с прошлой — полчаса.
fn badge_due(window_open: bool, mirrors_core: bool, badge_at: Option<Instant>, now: Instant) -> bool {
    !window_open && mirrors_core && badge_at.map_or(true, |t| now.saturating_duration_since(t) >= BADGE_EVERY)
}

/// Что делает подтверждение после кадра.
#[derive(Debug, PartialEq)]
enum Answer {
    Run,
    Close,
    Keep,
}

/// `go` — «Да» или Enter, `cancel` — «Отмена», Esc или крестик. «Да» при занятом ядре не срабатывает и диалог остаётся;
/// «Отмена» работает всегда.
fn answer(go: bool, cancel: bool, busy: bool) -> Answer {
    if go && !busy {
        Answer::Run
    } else if cancel {
        Answer::Close
    } else {
        Answer::Keep
    }
}

fn toggle(selected: &mut BTreeSet<Component>, id: Component) {
    if !selected.remove(&id) {
        selected.insert(id);
    }
}

/// Заголовок меню «Справка»: с отметкой — маленькая жёлтая точка после текста, по центру строки.
pub(super) fn menu_title(ui: &Ui, text: String, badge: bool) -> egui::WidgetText {
    if !badge {
        return text.into();
    }
    let font = egui::TextStyle::Button.resolve(ui.style());
    let mut job = egui::text::LayoutJob::default();
    // PLACEHOLDER — цвет текста подставит кнопка меню, как у соседних пунктов.
    job.append(&text, 0.0, egui::TextFormat { font_id: font.clone(), color: Color32::PLACEHOLDER, ..Default::default() });
    let dot = egui::FontId::proportional(font.size * 0.55);
    job.append("●", 4.0, egui::TextFormat { font_id: dot, color: YELLOW, valign: Align::Center, ..Default::default() });
    job.into()
}

// ───────────────────────────── окно ─────────────────────────────

fn window_body(ui: &mut Ui, v: &View, clicks: &mut Vec<Click>) {
    components_table(ui, v, clicks);
    ui.add_space(8.0);
    let en = enabled(v.state, v.selected, v.locked);
    ui.horizontal(|ui| {
        if ui.add_enabled(en.check, egui::Button::new(tr("upd.check")).min_size(BUTTON)).clicked() {
            clicks.push(Click::Check);
        }
        if ui.add_enabled(en.selected, egui::Button::new(tr("upd.apply_selected")).min_size(BUTTON)).clicked() {
            clicks.push(Click::Apply(chosen(v.state, v.selected)));
        }
        if ui.add_enabled(en.all, egui::Button::new(tr("upd.apply_all")).min_size(BUTTON)).clicked() {
            clicks.push(Click::Apply(all_updates(v.state)));
        }
    });
    ui.add_space(4.0);
    status_line(ui, v);
    ui.add_space(4.0);
    ui.separator();
    egui::CollapsingHeader::new(RichText::new(tr("upd.history")).strong())
        .id_salt("upd-history")
        .default_open(true)
        .show(ui, |ui| history_table(ui, v, clicks));
}

/// Ячейка постоянной ширины: колонки строк и заголовка совпадают, длинное обрезается.
fn cell(ui: &mut Ui, width: f32, right: bool, add: impl FnOnce(&mut Ui)) {
    let layout = if right { Layout::right_to_left(Align::Center) } else { Layout::left_to_right(Align::Center) };
    ui.allocate_ui_with_layout(Vec2::new(width, ROW), layout, |ui| {
        ui.set_min_size(Vec2::new(width, ROW));
        add(ui);
    });
}

fn head(ui: &mut Ui, width: f32, right: bool, key: &str) {
    cell(ui, width, right, |ui| {
        ui.add(egui::Label::new(RichText::new(tr(key)).color(GRAY)).truncate());
    });
}

/// Текст, обрезанный по ячейке, полный — во всплывающей подсказке.
fn clipped(ui: &mut Ui, text: RichText) {
    let full = text.text().to_string();
    clipped_tip(ui, text, full);
}

/// То же, но подсказка — свой текст (полное объяснение состояния), а не только то, что не влезло.
fn clipped_tip(ui: &mut Ui, text: RichText, tip: String) {
    ui.add(egui::Label::new(text).truncate()).on_hover_text(tip);
}

/// Остаток ширины для гибкой колонки после постоянных и промежутков между ячейками.
fn flex(total: f32, fixed: &[f32], spacing: f32, min: f32) -> f32 {
    (total - fixed.iter().sum::<f32>() - spacing * fixed.len() as f32).max(min)
}

/// Версия моноширинным и дата выхода этой версии серым; `—`, если версии нет. `tip` — подсказка к ячейке.
fn version_cell(ui: &mut Ui, version: Option<&str>, released: Option<u64>, tip: Option<String>, missing: &str) {
    match version {
        Some(v) => {
            let mut shown = vec![ui.label(mono(v, Color32::WHITE))];
            if let Some(at) = released.filter(|t| *t > 0) {
                shown.push(ui.label(mono(fmt::date(at), GRAY)));
            }
            if let Some(tip) = tip {
                for r in shown {
                    r.on_hover_text(&tip);
                }
            }
        }
        None => {
            ui.label(RichText::new(missing).color(GRAY));
        }
    }
}

/// Подсказка к установленной версии: когда она поставлена на этом компьютере. В колонке — дата выхода версии, одна и та
/// же в «Установлено» и «Доступно»; дата установки — другая величина, поэтому только здесь.
fn installed_tip(c: &ComponentState) -> Option<String> {
    c.installed_at.filter(|t| *t > 0).map(|t| trf("upd.installed_on", &[&fmt::date(t)]))
}

fn components_table(ui: &mut Ui, v: &View, clicks: &mut Vec<Click>) {
    let sp = ui.spacing().item_spacing.x;
    let w_name = flex(ui.available_width(), &[W_CHECK, W_VERSION, W_VERSION, W_STATUS, W_NEWS], sp, MIN_NAME);
    ui.horizontal(|ui| {
        cell(ui, W_CHECK, false, |_| {});
        head(ui, w_name, false, "upd.col_component");
        head(ui, W_VERSION, false, "upd.col_installed");
        head(ui, W_VERSION, false, "upd.col_available");
        head(ui, W_STATUS, false, "upd.col_status");
        cell(ui, W_NEWS, false, |_| {});
    });
    ui.separator();
    for c in &v.state.components {
        let Some(id) = c.component else { continue };
        ui.horizontal(|ui| {
            cell(ui, W_CHECK, false, |ui| {
                let mut on = c.update && v.selected.contains(&id);
                if ui.add_enabled(c.update, egui::Checkbox::without_text(&mut on)).changed() {
                    clicks.push(Click::Toggle(id));
                }
            });
            cell(ui, w_name, false, |ui| clipped(ui, RichText::new(name(id))));
            let missing = if id == Component::Native { tr("upd.not_installed") } else { tr("upd.unknown") };
            cell(ui, W_VERSION, false, |ui| version_cell(ui, c.installed.as_deref(), c.released, installed_tip(c), &missing));
            let avail = offered(c);
            cell(ui, W_VERSION, false, |ui| version_cell(ui, avail.map(Available::shown).as_deref(), avail.map(|a| a.published), None, "—"));
            cell(ui, W_STATUS, false, |ui| {
                let s = status(c);
                clipped_tip(ui, RichText::new(status_text(&s)).color(status_color(&s)), status_tip(&s));
            });
            // Текст релиза — в отдельном окне: высота строки от его длины не зависит.
            cell(ui, W_NEWS, false, |ui| {
                if release_notes(c).is_some() && ui.small_button(tr("upd.notes")).clicked() {
                    clicks.push(Click::Notes(id));
                }
            });
        });
    }
}

/// Занято — крутилка и текст ядра; иначе время последней проверки. Ошибка — красным строкой ниже.
fn status_line(ui: &mut Ui, v: &View) {
    cell(ui, ui.available_width(), false, |ui| {
        if v.busy {
            ui.spinner();
            let text = v.state.busy.clone().unwrap_or_else(|| tr("upd.waiting"));
            clipped(ui, RichText::new(text).color(YELLOW));
        } else {
            let text = match v.state.checked_at {
                Some(t) => trf("upd.checked_at", &[&fmt::date_time(t)]),
                None => tr("upd.never_checked"),
            };
            ui.label(RichText::new(text).color(GRAY));
        }
    });
    if let Some(e) = v.error {
        cell(ui, ui.available_width(), false, |ui| clipped(ui, RichText::new(e).color(RED)));
    }
}

fn history_table(ui: &mut Ui, v: &View, clicks: &mut Vec<Click>) {
    if v.state.history.is_empty() {
        ui.weak(tr("upd.history_empty"));
        return;
    }
    let sp = ui.spacing().item_spacing.x;
    let fixed = [W_DATE, W_ACTION, W_FROM_TO, W_SIZE, W_RESULT, W_RESTORE];
    let w_name = flex(ui.available_width(), &fixed, sp, MIN_HIST_NAME);
    ui.horizontal(|ui| {
        head(ui, W_DATE, false, "upd.h_date");
        head(ui, w_name, false, "upd.col_component");
        head(ui, W_ACTION, false, "upd.h_action");
        head(ui, W_FROM_TO, false, "upd.h_version");
        head(ui, W_SIZE, true, "upd.h_backup");
        head(ui, W_RESULT, false, "upd.h_result");
        cell(ui, W_RESTORE, false, |_| {});
    });
    ui.separator();
    for e in &v.state.history {
        ui.horizontal(|ui| {
            cell(ui, W_DATE, false, |ui| {
                ui.label(mono(fmt::date_time(e.at), GRAY));
            });
            cell(ui, w_name, false, |ui| clipped(ui, RichText::new(short(e.component))));
            cell(ui, W_ACTION, false, |ui| clipped(ui, RichText::new(action_name(e.action))));
            cell(ui, W_FROM_TO, false, |ui| clipped(ui, mono(versions(e), Color32::WHITE)));
            cell(ui, W_SIZE, true, |ui| {
                let size = if e.backup.is_some() { fmt::bytes(e.backup_size as f64) } else { "—".to_string() };
                ui.label(mono(size, GRAY));
            });
            cell(ui, W_RESULT, false, |ui| {
                if e.ok {
                    ui.label(RichText::new(tr("upd.result_ok")).color(GREEN));
                } else {
                    let r = ui.label(RichText::new(tr("upd.result_err")).color(RED));
                    if let Some(err) = &e.error {
                        r.on_hover_text(err);
                    }
                }
            });
            cell(ui, W_RESTORE, false, |ui| {
                let button = restore_button(v.state.restores.iter().find(|o| o.id == e.id));
                let r = ui.add_enabled(button.version.is_some() && !v.locked, egui::Button::new(&button.label));
                if r.clicked() {
                    if let Some(version) = button.version {
                        clicks.push(Click::Restore { id: e.id, component: e.component, version });
                    }
                } else if let Some(why) = button.why_not {
                    r.on_disabled_hover_text(why);
                }
            });
        });
    }
}

// ───────────────────────────── чистые правила окна ─────────────────────────────

/// Что показывает колонка «Доступно»: найденную версию, только если она новее установленной. Равная, более старая
/// и результат неудавшейся проверки — «—»: колонка говорит «есть что ставить», а не повторяет установленное.
/// Установленная версия неизвестна — сравнивать не с чем, найденная показывается как есть.
fn offered(c: &ComponentState) -> Option<&Available> {
    if c.error.is_some() && !c.manual_only {
        return None;
    }
    let found = c.available.as_ref()?;
    match c.installed.as_deref() {
        Some(installed) if !crate::update::feed::newer(&found.version, installed) => None,
        _ => Some(found),
    }
}

/// Предложенная версия, если у неё есть текст релиза: по ней появляется кнопка «Что нового».
fn release_notes(c: &ComponentState) -> Option<&Available> {
    offered(c).filter(|a| !a.notes.trim().is_empty())
}

fn name(c: Component) -> String {
    tr(match c {
        Component::Native => "upd.c_native",
        Component::Engine => "upd.c_engine",
        Component::App => "upd.c_app",
    })
}

/// Короткое имя — для истории и подтверждений.
fn short(c: Component) -> String {
    tr(match c {
        Component::Native => "upd.s_native",
        Component::Engine => "upd.s_engine",
        Component::App => "upd.s_app",
    })
}

fn action_name(a: Action) -> String {
    tr(match a {
        Action::Backup => "upd.a_backup",
        Action::Update => "upd.a_update",
        Action::Restore => "upd.a_restore",
    })
}

#[derive(Debug, PartialEq)]
enum Status {
    Ok,
    /// Движок: наша версия — новейшая у Amnezia (метка релиза amneziawg-windows).
    OkEngine(String),
    Update,
    Missing,
    /// Источник ещё не проверялся.
    Unknown,
    /// В нашем релизе нет манифеста — только вручную; полный текст для подсказки. Не сбой.
    Manual(String),
    /// Движок: Amnezia выпустила более новый (метка); придёт с обновлением программы.
    UpstreamNewer(String),
    /// Движок: новейший релиз Amnezia проверить не удалось (причина — в журнале событий).
    UpstreamUnchecked,
    /// Настоящий сбой проверки: единственный красный статус.
    Error(String),
}

/// Состояние строки. Порядок: сбой, нет компонента, есть обновление, сверка движка с Amnezia, «только вручную».
/// Движок, равный новейшему у Amnezia, «актуален» и при релизе без манифеста: обновлять нечего.
fn status(c: &ComponentState) -> Status {
    use crate::update::EngineUpstream as Up;
    match (&c.error, &c.upstream) {
        (Some(e), _) if !c.manual_only => Status::Error(e.clone()),
        _ if c.installed.is_none() && c.component == Some(Component::Native) => Status::Missing,
        _ if c.update => Status::Update,
        (_, Some(Up::Current(tag))) => Status::OkEngine(tag.clone()),
        (_, Some(Up::Newer(tag))) => Status::UpstreamNewer(tag.clone()),
        (Some(e), _) => Status::Manual(e.clone()),
        (_, Some(Up::Unchecked)) => Status::UpstreamUnchecked,
        _ if c.available.is_some() => Status::Ok,
        _ => Status::Unknown,
    }
}

fn status_text(s: &Status) -> String {
    match s {
        Status::Ok | Status::OkEngine(_) => tr("upd.st_ok"),
        Status::Update => tr("upd.st_update"),
        Status::Missing => tr("upd.st_missing"),
        Status::Unknown => tr("upd.st_unknown"),
        Status::Manual(_) => tr("upd.st_manual"),
        Status::UpstreamNewer(tag) => trf("upd.st_upstream_newer", &[tag]),
        Status::UpstreamUnchecked => tr("upd.st_upstream_unchecked"),
        Status::Error(e) => e.clone(),
    }
}

/// Полный текст для подсказки; у состояний, где всё сказано самим текстом, подсказка — он же.
fn status_tip(s: &Status) -> String {
    match s {
        Status::OkEngine(tag) => trf("upd.st_ok_engine", &[tag]),
        Status::Manual(full) => full.clone(),
        Status::UpstreamUnchecked => tr("upd.st_upstream_unchecked_tip"),
        other => status_text(other),
    }
}

fn status_color(s: &Status) -> Color32 {
    match s {
        Status::Ok | Status::OkEngine(_) => GREEN,
        Status::Update | Status::UpstreamNewer(_) => YELLOW,
        Status::Missing | Status::Unknown | Status::Manual(_) | Status::UpstreamUnchecked => GRAY,
        Status::Error(_) => RED,
    }
}

/// Ответ ядра на запрос окна обновлений `seq`: состояние — в окно (если новее показанного) и в отметку меню;
/// ошибка — в журнал событий: у команды всегда, у опроса — один раз, пока текст не сменится.
fn take_updates_reply(
    result: Result<UpdatesState, String>,
    command: bool,
    seq: u64,
    polled: &Mutex<Polled>,
    badge: &AtomicBool,
    proposed: &Mutex<Option<Vec<(Component, String)>>>,
    error: &ErrorSink,
) {
    let mut p = polled.lock().unwrap();
    match result {
        Ok(state) => {
            badge.store(has_updates(&state), Ordering::SeqCst);
            *proposed.lock().unwrap() = Some(offered_updates(&state));
            if seq > p.applied {
                p.state = state;
                p.applied = seq;
            }
            p.error = None;
        }
        Err(e) => {
            if command || p.error.as_ref() != Some(&e) {
                error.push(e.clone());
            }
            p.error = Some(e);
        }
    }
}

fn has_updates(s: &UpdatesState) -> bool {
    s.components.iter().any(|c| c.update)
}

/// Компоненты, у которых есть обновление, и его версия.
fn offered_updates(s: &UpdatesState) -> Vec<(Component, String)> {
    s.components
        .iter()
        .filter(|c| c.update)
        .filter_map(|c| Some((c.component?, c.available.as_ref()?.version.clone())))
        .collect()
}

fn component_key(c: Component) -> &'static str {
    match c {
        Component::Native => "native",
        Component::Engine => "engine",
        Component::App => "app",
    }
}

/// Предложенные обновления, о которых ещё не сообщали; запоминает их в `notified`. По компоненту помнится одна
/// версия: выпущенная позже сообщается снова, одна и та же — один раз.
fn to_notify(offered: &[(Component, String)], notified: &mut BTreeMap<String, String>) -> Vec<(Component, String)> {
    let mut fresh = Vec::new();
    for (c, v) in offered {
        if notified.get(component_key(*c)) == Some(v) {
            continue;
        }
        notified.insert(component_key(*c).to_string(), v.clone());
        fresh.push((*c, v.clone()));
    }
    fresh
}

/// Открытое уведомление следует за ответом ядра: обновлённое выпадает, новая версия заменяет старую. Возвращает
/// то, о чём сообщается впервые.
fn refresh_news(
    news: &mut Vec<(Component, String)>,
    offered: &[(Component, String)],
    notified: &mut BTreeMap<String, String>,
) -> Vec<(Component, String)> {
    news.retain(|n| offered.contains(n));
    let fresh = to_notify(offered, notified);
    for (c, v) in &fresh {
        news.retain(|(k, _)| k != c);
        news.push((*c, v.clone()));
    }
    fresh
}

pub(super) fn news_text(list: &[(Component, String)]) -> String {
    let items: Vec<String> = list.iter().map(|(c, v)| format!("{} {v}", short(*c))).collect();
    trf("upd.notice_text", &[&items.join(", ")])
}

/// Компоненты с версиями, которые пользователь видел в подтверждении: ядро ставит только их.
fn targets(s: &UpdatesState, list: &[Component]) -> Vec<(Component, String)> {
    list.iter()
        .filter_map(|c| {
            let row = s.components.iter().find(|r| r.component == Some(*c))?;
            Some((*c, row.available.as_ref()?.version.clone()))
        })
        .collect()
}

fn all_updates(s: &UpdatesState) -> Vec<Component> {
    s.components.iter().filter(|c| c.update).filter_map(|c| c.component).collect()
}

/// Отмеченные строки, у которых есть обновление.
fn chosen(s: &UpdatesState, selected: &BTreeSet<Component>) -> Vec<Component> {
    all_updates(s).into_iter().filter(|c| selected.contains(c)).collect()
}

#[derive(Debug, PartialEq)]
struct Enabled {
    check: bool,
    selected: bool,
    all: bool,
}

fn enabled(s: &UpdatesState, selected: &BTreeSet<Component>, locked: bool) -> Enabled {
    Enabled { check: !locked, selected: !locked && !chosen(s, selected).is_empty(), all: !locked && has_updates(s) }
}

/// Новое обновление отмечается само один раз; без обновления отметка снимается.
fn sync_selection(components: &[ComponentState], selected: &mut BTreeSet<Component>, offered: &mut BTreeSet<Component>) {
    for c in components {
        let Some(id) = c.component else { continue };
        if !c.update {
            offered.remove(&id);
            selected.remove(&id);
        } else if offered.insert(id) {
            selected.insert(id);
        }
    }
}

/// «AmneziaWG: 3.1.0 → 3.1.1» для каждого выбранного компонента.
fn apply_lines(s: &UpdatesState, list: &[Component]) -> Vec<String> {
    list.iter()
        .map(|id| {
            let c = s.components.iter().find(|c| c.component == Some(*id));
            let from = c.and_then(|c| c.installed.clone()).unwrap_or_else(|| tr("upd.not_installed"));
            let to = c.and_then(|c| c.available.as_ref()).map(Available::shown).unwrap_or_else(|| tr("upd.unknown"));
            format!("{}: {from} → {to}", short(*id))
        })
        .collect()
}

/// Что заденет установка или возврат этих компонентов.
fn warnings(list: &[Component]) -> Vec<String> {
    let mut w = Vec::new();
    if list.contains(&Component::Native) {
        w.push(tr("upd.warn_native"));
    }
    if list.contains(&Component::App) {
        w.push(tr("upd.warn_app"));
    }
    w
}

fn restore_text(component: Component, version: &str) -> String {
    trf("upd.restore_text", &[&short(component), version])
}

/// Кнопка «Вернуть» строки истории: подпись с версией цели, цель (`version`, есть — кнопка может работать) и причина
/// отказа для подсказки. Цель решило ядро (`UpdatesState::restores`); ответа о строке нет — вернуть нельзя.
struct RestoreButton {
    label: String,
    version: Option<String>,
    why_not: Option<String>,
}

fn restore_button(offer: Option<&RestoreOffer>) -> RestoreButton {
    let Some(offer) = offer else {
        return RestoreButton { label: tr("upd.restore"), version: None, why_not: Some(tr("upd.no_backup")) };
    };
    let label = offer.version.as_deref().map_or_else(|| tr("upd.restore"), |v| trf("upd.restore_to", &[v]));
    let why_not = offer.blocked.map(|block| match (block, offer.version.as_deref()) {
        (RestoreBlock::Installed, Some(v)) => trf("upd.restore_installed", &[v]),
        _ => tr("upd.no_backup"),
    });
    let version = if offer.blocked.is_none() { offer.version.clone() } else { None };
    RestoreButton { label, version, why_not }
}

/// «было → стало»; одна версия, если они совпадают или известна только одна.
fn versions(e: &HistoryEntry) -> String {
    match (&e.from, &e.to) {
        (Some(f), Some(t)) if f != t => format!("{f} → {t}"),
        (Some(v), _) | (None, Some(v)) => v.clone(),
        (None, None) => "—".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn row(id: Component, installed: Option<&str>, available: Option<&str>, update: bool) -> ComponentState {
        ComponentState {
            component: Some(id),
            installed: installed.map(String::from),
            installed_at: None,
            available: available.map(|v| Available { version: v.to_string(), ..Default::default() }),
            update,
            error: None,
            ..Default::default()
        }
    }

    pub(super) fn state() -> UpdatesState {
        UpdatesState {
            components: vec![
                row(Component::Native, Some("3.1.0"), Some("3.1.1"), true),
                row(Component::Engine, Some("1.0"), Some("1.0"), false),
                row(Component::App, Some("0.4.0"), Some("0.5.0"), true),
            ],
            ..Default::default()
        }
    }

    /// Колонка «Установлено» показывает дату выхода, дата установки на этом компьютере — в подсказке.
    #[test]
    fn install_date_goes_to_the_tooltip_of_the_installed_cell() {
        let mut r = row(Component::Native, Some("3.1.0"), Some("3.1.0"), false);
        assert_eq!(installed_tip(&r), None, "дата установки неизвестна — подсказки нет");
        r.installed_at = Some(0);
        assert_eq!(installed_tip(&r), None, "ноль — не дата");
        r.installed_at = Some(1_791_209_253);
        assert_eq!(installed_tip(&r), Some(trf("upd.installed_on", &[&fmt::date(1_791_209_253)])));
        assert!(installed_tip(&r).unwrap().starts_with("Installed on this computer: 2026."));
    }

    /// Колонка «Доступно» и кнопка «Что нового» — только для версии новее установленной.
    #[test]
    fn available_cell_shows_only_a_newer_version() {
        let shown = |c: &ComponentState| offered(c).map(|a| a.version.clone());
        for id in [Component::Native, Component::Engine, Component::App] {
            assert_eq!(shown(&row(id, Some("3.1.0"), Some("3.1.0"), false)), None, "равная — «—»");
            assert_eq!(shown(&row(id, Some("3.1.0"), Some("3.1.1"), true)), Some("3.1.1".into()), "новее — версия");
            assert_eq!(shown(&row(id, Some("3.1.1"), Some("3.1.0"), false)), None, "старее установленной — «—»");
            assert_eq!(shown(&row(id, None, Some("3.1.0"), false)), Some("3.1.0".into()), "установленная неизвестна — как есть");
            assert_eq!(shown(&row(id, Some("3.1.0"), None, false)), None, "ничего не найдено");
        }
        let mut failed = row(Component::App, Some("0.4.0"), Some("0.5.0"), true);
        failed.error = Some("timeout".into());
        assert_eq!(shown(&failed), None, "проверка не удалась — «—», даже если остался прежний ответ");
        let with_notes = |mut c: ComponentState| {
            c.available.as_mut().unwrap().notes = "fix".into();
            c
        };
        assert!(release_notes(&with_notes(row(Component::App, Some("0.5.0"), Some("0.5.0"), false))).is_none(), "равная версия — кнопки нет");
        assert!(release_notes(&with_notes(row(Component::App, Some("0.4.0"), Some("0.5.0"), true))).is_some());
    }

    fn pair(c: Component, v: &str) -> (Component, String) {
        (c, v.to_string())
    }

    #[test]
    fn nothing_offered_nothing_announced() {
        let mut notified = BTreeMap::new();
        assert!(to_notify(&[], &mut notified).is_empty());
        assert!(notified.is_empty());
        let none = UpdatesState { components: vec![row(Component::App, Some("0.5.0"), Some("0.5.0"), false)], ..Default::default() };
        assert!(offered_updates(&none).is_empty(), "версия не новее установленной — не предложение");
    }

    #[test]
    fn same_version_is_announced_once() {
        let mut notified = BTreeMap::new();
        let offered = [pair(Component::Native, "3.1.1")];
        assert_eq!(to_notify(&offered, &mut notified), offered.to_vec());
        assert!(to_notify(&offered, &mut notified).is_empty(), "повторная проверка той же версии молчит");
        // Версия запомнена и после перезапуска: настройки уходят в INI и возвращаются.
        let mut s = crate::settings::Settings::default();
        s.update_notified = notified;
        s = crate::settings::Settings::from_ini(&crate::ini::Ini::parse(&s.to_ini().to_text()));
        assert!(to_notify(&offered, &mut s.update_notified).is_empty());
    }

    #[test]
    fn newer_version_is_announced_again() {
        let mut notified = BTreeMap::new();
        to_notify(&[pair(Component::App, "0.5.0")], &mut notified);
        assert_eq!(to_notify(&[pair(Component::App, "0.6.0")], &mut notified), vec![pair(Component::App, "0.6.0")]);
        assert_eq!(notified["app"], "0.6.0");
    }

    #[test]
    fn several_components_are_tracked_separately() {
        let mut notified = BTreeMap::new();
        to_notify(&[pair(Component::Native, "3.1.1")], &mut notified);
        let offered = [pair(Component::Native, "3.1.1"), pair(Component::Engine, "3.1.20260901"), pair(Component::App, "0.5.0")];
        assert_eq!(
            to_notify(&offered, &mut notified),
            vec![pair(Component::Engine, "3.1.20260901"), pair(Component::App, "0.5.0")],
            "AmneziaWG уже сообщали, остальные — нет"
        );
    }

    #[test]
    fn offers_and_targets_carry_the_bare_engine_version() {
        let mut engine = row(Component::Engine, Some("1"), Some("3.1.20260901"), true);
        engine.available.as_mut().unwrap().wintun = Some("0.14.1".into());
        let s = UpdatesState { components: vec![engine], ..Default::default() };
        assert_eq!(offered_updates(&s), vec![pair(Component::Engine, "3.1.20260901")]);
        assert_eq!(targets(&s, &[Component::Engine]), vec![pair(Component::Engine, "3.1.20260901")], "ядру уходит версия без пояснения");
        assert_eq!(apply_lines(&s, &[Component::Engine]), vec!["Mode 2 engine: 1 → 3.1.20260901 · wintun 0.14.1"], "в подтверждении — с пояснением");
    }

    #[test]
    fn open_notice_follows_the_core_answer() {
        let (mut notified, mut news) = (BTreeMap::new(), Vec::new());
        let fresh = refresh_news(&mut news, &[pair(Component::Native, "3.1.1")], &mut notified);
        assert_eq!((fresh.len(), news.len()), (1, 1));
        // Та же версия: ничего нового, уведомление остаётся.
        assert!(refresh_news(&mut news, &[pair(Component::Native, "3.1.1")], &mut notified).is_empty());
        assert_eq!(news, vec![pair(Component::Native, "3.1.1")]);
        // Вышла более новая: заменяет прежнюю строку.
        refresh_news(&mut news, &[pair(Component::Native, "3.2.0")], &mut notified);
        assert_eq!(news, vec![pair(Component::Native, "3.2.0")]);
        // Обновили (предложений нет): уведомление исчезает, запомненная версия остаётся.
        assert!(refresh_news(&mut news, &[], &mut notified).is_empty());
        assert!(news.is_empty());
        assert_eq!(notified["native"], "3.2.0");
    }

    #[test]
    fn notice_text_lists_components() {
        assert_eq!(
            news_text(&[pair(Component::Native, "3.1.0"), pair(Component::App, "0.5.0")]),
            "Updates available: AmneziaWG 3.1.0, AmneziaWG UI Dark 0.5.0"
        );
    }

    fn entry(from: Option<&str>, to: Option<&str>) -> HistoryEntry {
        HistoryEntry {
            id: 7,
            at: 0,
            component: Component::Native,
            action: Action::Update,
            from: from.map(String::from),
            to: to.map(String::from),
            backup: Some("b1".into()),
            backup_size: 1024,
            ok: true,
            error: None,
            prior_backup: None,
        }
    }

    // Тесты идут на языке по умолчанию (английский).
    #[test]
    fn row_status() {
        assert_eq!(status(&row(Component::Engine, Some("1"), Some("1"), false)), Status::Ok);
        assert_eq!(status(&row(Component::Engine, Some("1"), Some("2"), true)), Status::Update);
        assert_eq!(status(&row(Component::Native, None, Some("3.1.1"), true)), Status::Missing);
        assert_eq!(status(&row(Component::App, None, None, false)), Status::Unknown);
        let mut broken = row(Component::App, Some("1"), Some("2"), true);
        broken.error = Some("timeout".into());
        assert_eq!(status(&broken), Status::Error("timeout".into()));
        assert_eq!(status_text(&Status::Error("timeout".into())), "timeout");
        assert_eq!(status_color(&Status::Update), YELLOW);
    }

    fn engine(up: Option<crate::update::EngineUpstream>) -> ComponentState {
        ComponentState { upstream: up, ..row(Component::Engine, Some("3.1.20260814"), None, false) }
    }

    /// Релиз без манифеста — не сбой: серый короткий текст, полный — в подсказке, красного нет.
    #[test]
    fn missing_manifest_is_neutral_manual_only() {
        let full = "Release 0.3.0 has no signed update manifest — it can only be installed manually";
        let mut app = row(Component::App, Some("0.3.0"), None, false);
        app.error = Some(full.into());
        app.manual_only = true;
        let s = status(&app);
        assert_eq!(s, Status::Manual(full.into()));
        assert_eq!(status_text(&s), tr("upd.st_manual"));
        assert_eq!(status_tip(&s), full);
        assert_eq!(status_color(&s), GRAY);
        assert!(status_text(&s).chars().count() < 40, "короткий текст помещается в колонку");
        // Тот же текст без признака — настоящий сбой, красный.
        app.manual_only = false;
        assert_eq!(status(&app), Status::Error(full.into()));
        assert_eq!(status_color(&status(&app)), RED);
    }

    #[test]
    fn engine_status_follows_the_upstream_check() {
        use crate::update::EngineUpstream as Up;
        let tag = "v3.1.20260814".to_string();
        let ok = status(&engine(Some(Up::Current(tag.clone()))));
        assert_eq!((&ok, status_color(&ok)), (&Status::OkEngine(tag.clone()), GREEN));
        assert_eq!(status_text(&ok), "Up to date");
        assert_eq!(status_tip(&ok), "Built from amneziawg-windows v3.1.20260814 — the newest Amnezia release");

        let newer = status(&engine(Some(Up::Newer("v3.2.0".into()))));
        assert_eq!((status_text(&newer), status_color(&newer)), ("Amnezia released v3.2.0 — comes with an app update".to_string(), YELLOW));

        let failed = status(&engine(Some(Up::Unchecked)));
        assert_eq!((status_text(&failed), status_color(&failed)), ("Could not check".to_string(), GRAY));
        assert!(status_tip(&failed).contains("event log"));

        // Проверки не было (state.json прежней версии): прежнее поведение.
        assert_eq!(status(&engine(None)), Status::Unknown);
    }

    #[test]
    fn engine_equal_to_upstream_is_current_even_without_manifest_but_a_real_error_stays_red() {
        use crate::update::EngineUpstream as Up;
        let mut c = engine(Some(Up::Current("v3.1.20260814".into())));
        c.error = Some("no manifest".into());
        c.manual_only = true;
        assert_eq!(status(&c), Status::OkEngine("v3.1.20260814".into()));
        c.upstream = Some(Up::Unchecked);
        assert_eq!(status(&c), Status::Manual("no manifest".into()), "без ответа Amnezia остаётся «только вручную»");
        c.manual_only = false;
        c.error = Some("HTTP 503".into());
        assert_eq!(status(&c), Status::Error("HTTP 503".into()));
        // Обновление, найденное в нашем релизе, важнее сверки с Amnezia.
        let mut upd = engine(Some(Up::Newer("v9".into())));
        upd.update = true;
        assert_eq!(status(&upd), Status::Update);
    }

    #[test]
    fn every_status_has_a_full_tooltip() {
        for s in [Status::Ok, Status::Update, Status::Missing, Status::Unknown, Status::UpstreamUnchecked, Status::Error("e".into())] {
            assert!(!status_tip(&s).is_empty(), "{s:?}");
        }
    }

    #[test]
    fn overlapping_requests_keep_the_flag_until_the_last_one_finishes() {
        let in_flight = InFlight::default();
        assert!(!in_flight.any());
        let (check, restore) = (in_flight.begin(), in_flight.begin());
        drop(check);
        assert!(in_flight.any(), "восстановление ещё идёт, хотя проверка закончилась");
        drop(restore);
        assert!(!in_flight.any());
    }

    #[test]
    fn panicking_request_still_clears_its_mark() {
        let in_flight = InFlight::default();
        let pending = in_flight.begin();
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _pending = pending;
            panic!("request failed");
        }));
        assert!(r.is_err());
        assert!(!in_flight.any());
    }

    #[test]
    fn buttons_follow_selection_and_busy() {
        let s = state();
        let none = BTreeSet::new();
        assert_eq!(enabled(&s, &none, false), Enabled { check: true, selected: false, all: true });
        // Отметка без обновления не считается.
        let engine = BTreeSet::from([Component::Engine]);
        assert!(!enabled(&s, &engine, false).selected);
        let native = BTreeSet::from([Component::Native]);
        assert!(enabled(&s, &native, false).selected);
        assert_eq!(enabled(&s, &native, true), Enabled { check: false, selected: false, all: false });
        assert_eq!(chosen(&s, &BTreeSet::from([Component::App, Component::Engine])), vec![Component::App]);
        assert_eq!(all_updates(&s), vec![Component::Native, Component::App]);
        assert!(!has_updates(&UpdatesState::default()));
    }

    #[test]
    fn selection_checked_once_and_cleared_without_update() {
        let mut s = state();
        let (mut sel, mut offered) = (BTreeSet::new(), BTreeSet::new());
        sync_selection(&s.components, &mut sel, &mut offered);
        assert_eq!(sel, BTreeSet::from([Component::Native, Component::App]));
        // Снятая пользователем отметка не возвращается.
        sel.remove(&Component::App);
        sync_selection(&s.components, &mut sel, &mut offered);
        assert_eq!(sel, BTreeSet::from([Component::Native]));
        // Обновление поставлено — отметка уходит; новое обновление отмечается снова.
        s.components[0].update = false;
        sync_selection(&s.components, &mut sel, &mut offered);
        assert!(sel.is_empty());
        s.components[0].update = true;
        sync_selection(&s.components, &mut sel, &mut offered);
        assert_eq!(sel, BTreeSet::from([Component::Native]));
    }

    #[test]
    fn confirmation_lines_and_warnings() {
        let s = state();
        assert_eq!(apply_lines(&s, &[Component::Native, Component::App]), vec!["AmneziaWG: 3.1.0 → 3.1.1", "AmneziaWG UI Dark: 0.4.0 → 0.5.0"]);
        assert!(warnings(&[Component::Engine]).is_empty());
        assert_eq!(warnings(&[Component::Native, Component::App]).len(), 2);
        assert!(restore_text(Component::Native, "3.1.0").contains("3.1.0"));
    }

    fn offer(version: Option<&str>, blocked: Option<RestoreBlock>) -> RestoreOffer {
        RestoreOffer { id: 7, version: version.map(String::from), blocked }
    }

    #[test]
    fn restore_button_names_the_target_and_says_why_it_is_off() {
        let ready = restore_button(Some(&offer(Some("2.0.1"), None)));
        assert_eq!((ready.label.as_str(), ready.version.as_deref(), ready.why_not), ("Restore 2.0.1", Some("2.0.1"), None));
        let same = restore_button(Some(&offer(Some("3.1.0"), Some(RestoreBlock::Installed))));
        assert_eq!((same.label.as_str(), same.version, same.why_not), ("Restore 3.1.0", None, Some(trf("upd.restore_installed", &["3.1.0"]))));
        let gone = restore_button(Some(&offer(Some("2.0.1"), Some(RestoreBlock::NoCopy))));
        assert_eq!((gone.version, gone.why_not), (None, Some(tr("upd.no_backup"))));
        let unknown = restore_button(Some(&offer(None, Some(RestoreBlock::NoCopy))));
        assert_eq!((unknown.label.as_str(), unknown.version), ("Restore", None));
        let missing = restore_button(None);
        assert_eq!((missing.version, missing.why_not), (None, Some(tr("upd.no_backup"))));
    }

    #[test]
    fn history_versions_and_flex_width() {
        assert_eq!(versions(&entry(Some("3.1.0"), Some("3.1.1"))), "3.1.0 → 3.1.1");
        assert_eq!(versions(&entry(Some("3.1.0"), Some("3.1.0"))), "3.1.0");
        assert_eq!(versions(&entry(None, None)), "—");
        assert_eq!(flex(900.0, &[100.0, 200.0], 10.0, 50.0), 580.0);
        assert_eq!(flex(300.0, &[100.0, 200.0], 10.0, 50.0), 50.0);
    }
}

/// Окно обновлений против подделки ядра: что доходит до журнала событий.
#[cfg(test)]
mod core_tests {
    use super::super::testkit::{errors, sink};
    use super::tests::{row, state};
    use super::*;
    use crate::daemon::fake::FakeCore;
    use crate::daemon::CoreApi;

    fn poll(core: &FakeCore, seq: u64, polled: &Mutex<Polled>, error: &ErrorSink) {
        let (badge, proposed) = (AtomicBool::new(false), Mutex::new(None));
        take_updates_reply(core.updates(UpdateOp::State), false, seq, polled, &badge, &proposed, error);
    }

    #[test]
    fn poll_error_reaches_the_event_log_once_until_it_changes() {
        let core = FakeCore::unreachable("core unavailable");
        let (shared, error) = sink();
        let polled = Mutex::new(Polled::default());
        // Опрос раз в секунду: одна и та же ошибка не заполняет журнал.
        poll(&core, 1, &polled, &error);
        poll(&core, 2, &polled, &error);
        assert_eq!(errors(&shared), [(String::new(), "core unavailable".to_string())]);
        assert_eq!(polled.lock().unwrap().error.as_deref(), Some("core unavailable"));
        assert_eq!(core.requests(), ["Updates(State)", "Updates(State)"]);
    }

    #[test]
    fn command_error_is_logged_every_time_and_success_clears_the_error() {
        let core = FakeCore::new(|_| Err("refused".to_string()));
        let (shared, error) = sink();
        let polled = Mutex::new(Polled::default());
        let (badge, proposed) = (AtomicBool::new(false), Mutex::new(None));
        for seq in 1..=2 {
            take_updates_reply(core.updates(UpdateOp::State), true, seq, &polled, &badge, &proposed, &error);
        }
        assert_eq!(errors(&shared).len(), 2, "ошибка команды — каждый раз");
        let ok = FakeCore::new(|_| Ok(crate::daemon::proto::Response::Updates(Box::default())));
        poll(&ok, 3, &polled, &error);
        let p = polled.lock().unwrap();
        assert_eq!((p.error.as_deref(), p.applied), (None, 3));
    }

    fn window(core: &Arc<FakeCore>) -> UpdatesWindow {
        let (shared, error) = sink();
        UpdatesWindow::new(Link::new(core.clone(), error, Arc::default(), egui::Context::default(), shared))
    }

    fn settle(w: &UpdatesWindow) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while (w.command.any() || w.polling.any()) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!w.command.any() && !w.polling.any(), "запросы не завершились");
    }

    fn offering_core() -> FakeCore {
        FakeCore::new(|_| {
            let row = ComponentState {
                component: Some(Component::App),
                installed: Some("0.4.0".into()),
                available: Some(Available { version: "0.5.0".into(), ..Default::default() }),
                update: true,
                ..Default::default()
            };
            Ok(crate::daemon::proto::Response::Updates(Box::new(UpdatesState { components: vec![row], ..Default::default() })))
        })
    }

    #[test]
    fn poll_waits_for_the_answer_and_for_a_second() {
        // Отсчёт от будущего: `Instant` не уходит раньше загрузки Windows, а машина может работать меньше получаса.
        let now = Instant::now() + Duration::from_secs(3600);
        let ago = |s| now.checked_sub(Duration::from_secs(s)).unwrap();
        assert!(poll_due(false, None, now), "первый опрос — сразу");
        assert!(!poll_due(true, None, now), "прошлый ответ ещё не пришёл");
        assert!(!poll_due(false, Some(ago(0)), now));
        assert!(poll_due(false, Some(ago(1)), now));
        assert!(!poll_due(true, Some(ago(60)), now), "запросы не накладываются, сколько бы ни прошло");
    }

    #[test]
    fn badge_check_needs_closed_window_a_core_and_half_an_hour() {
        // Отсчёт от будущего: `Instant` не уходит раньше загрузки Windows, а машина может работать меньше получаса.
        let now = Instant::now() + Duration::from_secs(3600);
        let ago = |s| now.checked_sub(Duration::from_secs(s)).unwrap();
        assert!(badge_due(false, true, None, now), "при запуске — сразу");
        assert!(!badge_due(true, true, None, now), "пока окно открыто, отметку ведёт его опрос");
        assert!(!badge_due(false, false, None, now), "без ядра обновлять нечего");
        assert!(!badge_due(false, true, Some(ago(29 * 60)), now));
        assert!(badge_due(false, true, Some(ago(30 * 60)), now));
    }

    #[test]
    fn confirmation_runs_only_when_the_core_is_free() {
        assert_eq!(answer(true, false, false), Answer::Run);
        assert_eq!(answer(true, false, true), Answer::Keep, "ядро занято — «Да» не срабатывает");
        assert_eq!(answer(true, true, true), Answer::Close, "«Отмена» работает и при занятом ядре");
        assert_eq!(answer(false, true, false), Answer::Close);
        assert_eq!(answer(false, false, false), Answer::Keep);
    }

    #[test]
    fn toggle_flips_a_selection() {
        let mut s = BTreeSet::new();
        toggle(&mut s, Component::App);
        assert!(s.contains(&Component::App));
        toggle(&mut s, Component::App);
        assert!(s.is_empty());
    }

    #[test]
    fn opening_asks_for_a_check_unless_one_is_already_running() {
        let gate = Arc::new(Mutex::new(()));
        let held = gate.lock().unwrap();
        let g = gate.clone();
        let core = Arc::new(FakeCore::new(move |_| {
            let _wait = g.lock().unwrap();
            Ok(crate::daemon::proto::Response::Updates(Box::default()))
        }));
        let mut w = window(&core);
        assert!(!w.is_open());
        w.open();
        w.open();
        assert!(w.is_open() && w.command.any());
        drop(held);
        settle(&w);
        assert_eq!(core.requests(), ["Updates(Check)"], "второе открытие при идущей проверке не повторяет её");
        w.open();
        settle(&w);
        assert_eq!(core.requests().len(), 2, "после ответа открытие проверяет снова");
    }

    #[test]
    fn answer_with_an_update_sets_the_badge_and_is_announced_once() {
        let mut w = window(&Arc::new(offering_core()));
        assert!(!w.has_new());
        w.request(UpdateOp::State, false);
        settle(&w);
        assert!(w.has_new(), "обновление в ответе — отметка");
        let mut notified = BTreeMap::new();
        assert_eq!(w.collect_news(&mut notified), vec![(Component::App, "0.5.0".to_string())]);
        w.request(UpdateOp::State, false);
        settle(&w);
        assert!(w.collect_news(&mut notified).is_empty(), "та же версия второй раз не сообщается");
        assert_eq!(w.news, vec![(Component::App, "0.5.0".to_string())], "уведомление ждёт, пока его откроют");
    }

    /// Кадр: уведомление, под ним окно-«редактор» (слой `Middle`, как у всех окон); `true` — Esc достался «редактору».
    fn news_frame(ctx: &egui::Context, w: &mut UpdatesWindow, events: Vec<egui::Event>) -> bool {
        let mut editor_escape = false;
        let _ = ctx.run(egui::RawInput { events, ..Default::default() }, |ctx| {
            w.show_news(ctx);
            let mut open = true;
            dialog_window(ctx, "editor", "test-editor", &mut open).default_pos([100.0, 100.0]).show(ctx, |ui| ui.label("text"));
            editor_escape = window_escape(ctx, "test-editor");
        });
        editor_escape
    }

    fn mouse(pos: egui::Pos2, pressed: bool) -> Vec<egui::Event> {
        let button = egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
        vec![egui::Event::PointerMoved(pos), button]
    }

    fn escape() -> Vec<egui::Event> {
        vec![egui::Event::Key { key: egui::Key::Escape, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE }]
    }

    /// Уведомление — не активное окно: только что появившись, Esc у окна, где работают, не отнимает; получает его
    /// после щелчка по себе и теряет после щелчка в другое место.
    #[test]
    fn news_takes_escape_only_after_click() {
        let mut w = window(&Arc::new(offering_core()));
        let news = vec![(Component::App, "0.5.0".to_string())];
        w.news = news.clone();
        let ctx = egui::Context::default();
        // Редактор активен (верхнее окно), уведомление появляется позже — поверх.
        news_frame(&ctx, &mut w, Vec::new());
        assert!(news_frame(&ctx, &mut w, escape()), "свежее уведомление забрало Esc у редактора");
        assert_eq!(w.news, news);

        let title = ctx.memory(|m| m.area_rect(egui::Id::new(NEWS_ID))).expect("уведомление не нарисовано").left_top() + Vec2::new(20.0, 8.0);
        let elsewhere = egui::pos2(120.0, 120.0);
        news_frame(&ctx, &mut w, mouse(title, true));
        news_frame(&ctx, &mut w, mouse(title, false));
        news_frame(&ctx, &mut w, mouse(elsewhere, true));
        news_frame(&ctx, &mut w, mouse(elsewhere, false));
        assert!(news_frame(&ctx, &mut w, escape()), "после щелчка мимо уведомления Esc снова у редактора");
        assert_eq!(w.news, news);

        news_frame(&ctx, &mut w, mouse(title, true));
        news_frame(&ctx, &mut w, mouse(title, false));
        assert!(!news_frame(&ctx, &mut w, escape()), "Esc активного уведомления ушёл и редактору");
        assert!(w.news.is_empty(), "Esc у активного уведомления — «Позже»");

        // Следующее уведомление снова появляется неактивным.
        w.news = news.clone();
        news_frame(&ctx, &mut w, Vec::new());
        assert!(news_frame(&ctx, &mut w, escape()), "новое уведомление унаследовало активность прежнего");
        assert_eq!(w.news, news);
    }
    fn with_notes(mut c: ComponentState, notes: &str) -> ComponentState {
        c.available.as_mut().expect("есть доступная версия").notes = notes.to_string();
        c
    }

    fn notes_state(notes: &str) -> UpdatesState {
        let mut s = state();
        s.components = s.components.into_iter().map(|c| with_notes(c, notes)).collect();
        s
    }

    /// Высота таблицы компонентов в кадре нужной ширины.
    fn table_height(state: &UpdatesState) -> f32 {
        let selected = BTreeSet::new();
        let view = View { state, error: None, selected: &selected, busy: false, locked: false };
        let ctx = egui::Context::default();
        let mut height = 0.0;
        let _ = ctx.run(egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(1000.0, 700.0))), ..Default::default() }, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                components_table(ui, &view, &mut Vec::new());
                height = ui.min_rect().height();
            });
        });
        height
    }

    /// Жалоба владельца: длинный текст релиза раздвигал таблицу, и строки с кнопками уходили из виду.
    /// Текст теперь в отдельном окне, высота таблицы от него не зависит.
    #[test]
    fn table_height_does_not_depend_on_notes_length() {
        let none = table_height(&notes_state(""));
        let long = "## Что изменилось\n".to_string() + &"* очень длинный пункт про исправление, которое не помещается в одну строку окна\n".repeat(300);
        assert!(none > 0.0);
        assert_eq!(table_height(&notes_state("fix")), none, "короткий текст релиза");
        assert_eq!(table_height(&notes_state(&long)), none, "длинный текст релиза");
    }

    #[test]
    fn notes_button_only_for_components_with_notes() {
        assert!(release_notes(&row(Component::App, Some("1"), Some("2"), true)).is_none());
        assert!(release_notes(&with_notes(row(Component::App, Some("1"), Some("2"), true), " \n ")).is_none(), "пустой текст — кнопки нет");
        assert!(release_notes(&with_notes(row(Component::App, Some("1"), Some("2"), true), "fix")).is_some());
        assert!(release_notes(&row(Component::Native, None, None, false)).is_none(), "версия не найдена");
    }

    #[test]
    fn opening_notes_replaces_the_open_window() {
        let mut w = window(&Arc::new(offering_core()));
        let ctx = egui::Context::default();
        let mut s = notes_state("## One\n- a");
        s.components[2] = with_notes(s.components[2].clone(), "## Two\n- b\n- c");
        s.components[1] = row(Component::Engine, Some("1.0"), Some("1.0"), false);
        w.open_notes(&ctx, &s, Component::Native);
        let first = w.notes.as_ref().expect("окно открыто");
        assert_eq!(first.title, trf("upd.notes_title", &[&short(Component::Native), "3.1.1"]));
        assert_eq!(first.blocks.len(), 2);
        w.open_notes(&ctx, &s, Component::App);
        assert_eq!(w.notes.as_ref().map(|n| n.blocks.len()), Some(3), "второе окно заменило первое");
        w.open_notes(&ctx, &s, Component::Engine);
        assert_eq!(w.notes.as_ref().map(|n| n.blocks.len()), Some(3), "у компонента без текста окно не открывается и прежнее не трогается");
    }

    /// Кадр с окном «Что нового» под окном-«редактором»; `true` — Esc достался «редактору».
    fn notes_frame(ctx: &egui::Context, w: &mut UpdatesWindow, events: Vec<egui::Event>) -> bool {
        let mut editor_escape = false;
        let _ = ctx.run(egui::RawInput { events, ..Default::default() }, |ctx| {
            let mut open = true;
            dialog_window(ctx, "editor", "test-editor", &mut open).default_pos([100.0, 100.0]).show(ctx, |ui| ui.label("text"));
            w.show_notes(ctx);
            editor_escape = window_escape(ctx, "test-editor");
        });
        editor_escape
    }

    #[test]
    fn notes_window_closes_on_escape_and_only_when_on_top() {
        let mut w = window(&Arc::new(offering_core()));
        let ctx = egui::Context::default();
        let s = notes_state("## T\n- a");
        notes_frame(&ctx, &mut w, Vec::new());
        w.open_notes(&ctx, &s, Component::App);
        notes_frame(&ctx, &mut w, Vec::new());
        assert!(w.notes.is_some());
        assert!(ctx.memory(|m| m.area_rect(egui::Id::new(NOTES_ID))).is_some(), "окно нарисовано");
        // Окно появилось позже редактора и верхнее: Esc — ему, редактор остаётся.
        assert!(!notes_frame(&ctx, &mut w, escape()), "Esc ушёл и редактору");
        assert!(w.notes.is_none(), "Esc закрывает окно «Что нового»");
        // Окна нет — Esc остаётся редактору.
        for _ in 0..3 {
            notes_frame(&ctx, &mut w, Vec::new());
        }
        assert!(notes_frame(&ctx, &mut w, escape()));
    }

}
