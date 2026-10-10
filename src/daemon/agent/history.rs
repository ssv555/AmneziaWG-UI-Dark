//! История скорости туннелей в агенте — для графика окна за сутки, месяц и год, в том числе за время, когда окно было
//! закрыто. Источник — те же ответы ядра, что у статистики (`core_poll`); на туннель три кольцевых ряда
//! (`HistoryRange`): сутки — минута × 1440, месяц — час × 744, год — сутки × 366. Файл `history.bin` в папке данных
//! ведёт только агент: целиком через временный файл, не чаще раза в минуту (и сразу после переименования или
//! удаления туннеля окном). Агента снаружи завершают без предупреждения (`TerminateProcess` сторожа, закрытие задания
//! ядра) — тогда теряется не больше минуты; при выходе из-за сбоя файл дописывается.
//!
//! Часы. Длительность между замерами — по монотонному `Instant`, место интервала в ряду — по unix-времени (часы
//! передаются функцией). Часы ушли вперёд — пропущенные интервалы просто пусты (разрыв), а всё старше срока ряда
//! вытесняется. Часы ушли назад — интервалы новее нового «сейчас» удаляются (их записали часы, спешившие вперёд;
//! верным считается последнее время системы), об этом запись в журнал: ряд остаётся упорядоченным и без будущего.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::core_poll::{CoreFeed, Log};
use super::proto::{History, HistoryBucket, HistoryRange};
use crate::crash::lock;
use crate::daemon::proto::CoreState;
use crate::events::Severity;
use crate::i18n::trf;

/// Чаще файл не пишется: при внезапном завершении агента теряется не больше этого.
pub(super) const SAVE_EVERY: Duration = Duration::from_secs(60);

/// Формат `history.bin` (числа little-endian): `MAGIC`, `VERSION`, u32 число туннелей; на туннель — u32 длина имени,
/// имя UTF-8, затем три ряда в порядке `HistoryRange::ALL`: u32 число интервалов и интервалы по `BUCKET_BYTES`.
const MAGIC: &[u8; 4] = b"AWGH";
const VERSION: u8 = 1;
/// Интервал на диске: начало u64, байты приёма и передачи u64, секунды и пики f32.
const BUCKET_BYTES: usize = 8 + 8 + 8 + 4 + 4 + 4;
/// Имена туннелей короче (32 символа); предел только защищает разбор чужого файла от огромного выделения.
const MAX_NAME: usize = 1024;

type Clock = dyn Fn() -> u64 + Send + Sync;

/// Интервал ряда: суммы, а не средние, — так продолжение интервала после перезапуска агента считается верно.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Bucket {
    /// Начало, unix-секунды, кратно ширине интервала ряда.
    start: u64,
    rx: u64,
    tx: u64,
    /// Сколько секунд внутри интервала туннель был подключён и замерялся.
    secs: f32,
    peak_rx: f32,
    peak_tx: f32,
}

impl Bucket {
    fn new(start: u64, sample: &Sample) -> Bucket {
        let mut bucket = Bucket { start, rx: 0, tx: 0, secs: 0.0, peak_rx: 0.0, peak_tx: 0.0 };
        bucket.merge(sample);
        bucket
    }

    fn merge(&mut self, sample: &Sample) {
        self.rx = self.rx.saturating_add(sample.rx);
        self.tx = self.tx.saturating_add(sample.tx);
        self.secs += sample.secs as f32;
        if let Some((rx, tx)) = sample.peak {
            self.peak_rx = self.peak_rx.max(rx as f32);
            self.peak_tx = self.peak_tx.max(tx as f32);
        }
    }

    /// Для окна: средние за подключённое время. Пик не меньше среднего: прирост, накопленный за два шага ядра,
    /// пик делит на оба шага (`Sample::between`), а среднее интервала может получить его за один.
    fn dto(&self) -> HistoryBucket {
        let secs = f64::from(self.secs);
        let avg = |bytes: u64| if secs > 0.0 { (bytes as f64 / secs).round() } else { 0.0 };
        let (rx, tx) = (avg(self.rx), avg(self.tx));
        let peak_rx = f64::from(self.peak_rx).round().max(rx);
        let peak_tx = f64::from(self.peak_tx).round().max(tx);
        HistoryBucket { start: self.start, secs, rx, tx, peak_rx, peak_tx }
    }
}

/// Кольцевой ряд: интервалы с данными по возрастанию начала, не старше срока ряда; пустых интервалов не хранит.
#[derive(Clone, Debug, Default, PartialEq)]
struct Series(VecDeque<Bucket>);

/// Три ряда туннеля, по индексу `HistoryRange as usize`.
type Tunnel = [Series; 3];

/// Начало самого старого интервала, который ещё входит в ряд при «сейчас» = `now`.
fn window_start(range: HistoryRange, now: u64) -> u64 {
    let width = range.bucket_secs();
    (now - now % width).saturating_sub((range.capacity() as u64 - 1) * width)
}

impl Series {
    /// Учесть замер в интервале, куда попадает `now`. `true` — убраны интервалы новее него (часы ушли назад).
    fn add(&mut self, range: HistoryRange, now: u64, sample: &Sample) -> bool {
        let start = now - now % range.bucket_secs();
        let mut moved_back = false;
        while self.0.back().is_some_and(|b| b.start > start) {
            self.0.pop_back();
            moved_back = true;
        }
        match self.0.back_mut() {
            Some(last) if last.start == start => last.merge(sample),
            _ => self.0.push_back(Bucket::new(start, sample)),
        }
        self.expire(range, now);
        moved_back
    }

    /// Вытеснить интервалы старше срока ряда. `true` — что-то убрано.
    fn expire(&mut self, range: HistoryRange, now: u64) -> bool {
        let oldest = window_start(range, now);
        let before = self.0.len();
        while self.0.front().is_some_and(|b| b.start < oldest) {
            self.0.pop_front();
        }
        self.0.len() != before
    }

    /// Интервалы, которые окно видит при «сейчас» = `now`: в сроке ряда и не из будущего.
    fn visible(&self, range: HistoryRange, now: u64) -> impl Iterator<Item = &Bucket> {
        let oldest = window_start(range, now);
        self.0.iter().filter(move |b| b.start >= oldest && b.start <= now)
    }
}

/// Прирост между двумя соседними замерами одной сессии туннеля.
struct Sample {
    rx: u64,
    tx: u64,
    secs: f64,
    /// Скорость для пика; `None` — промежуток с прошлой смены счётчиков слишком велик, чтобы считать её.
    peak: Option<(f64, f64)>,
}

/// Последнее чтение счётчиков туннеля в этом запуске агента.
#[derive(Clone, Copy)]
struct Seen {
    at: Instant,
    /// Когда счётчики менялись в последний раз: от него считается пик (тот же замер ядра может прийти дважды).
    changed: Instant,
    rx: u64,
    tx: u64,
    port: u16,
}

/// Промежуток между замерами годится как время подключения — то же правило, что у статистики.
fn within_gap(secs: f64) -> bool {
    secs > 0.0 && secs <= crate::stats::MAX_GAP_SECS
}

impl Sample {
    /// Прирост с прошлого замера. `None` — разрыв (ядро не отвечало, агент спал) или новая сессия (служба туннеля
    /// перезапущена: счётчики с нуля, а когда набраны байты — неизвестно): это не данные, а пробел в ряду.
    fn between(prev: &Seen, rx: u64, tx: u64, port: u16, at: Instant) -> Option<Sample> {
        let secs = at.saturating_duration_since(prev.at).as_secs_f64();
        let same_session = port == prev.port && rx >= prev.rx && tx >= prev.tx;
        if !same_session || !within_gap(secs) {
            return None;
        }
        let (rx, tx) = (rx - prev.rx, tx - prev.tx);
        let rate_secs = at.saturating_duration_since(prev.changed).as_secs_f64();
        let peak = within_gap(rate_secs).then(|| (rx as f64 / rate_secs, tx as f64 / rate_secs));
        Some(Sample { rx, tx, secs, peak })
    }
}

#[derive(Default)]
struct Inner {
    tunnels: BTreeMap<String, Tunnel>,
    seen: BTreeMap<String, Seen>,
    /// Есть изменения, которых нет в файле.
    dirty: bool,
    /// Ошибка записи файла, о которой уже есть запись в журнале.
    save_failing: Option<String>,
}

/// История скорости всех туннелей: одна владелица рядов и файла `history.bin`.
pub(crate) struct AgentHistory {
    path: PathBuf,
    clock: Box<Clock>,
    log: Box<Log>,
    inner: Mutex<Inner>,
}

impl AgentHistory {
    pub(super) fn new(path: PathBuf, clock: Box<Clock>, log: Box<Log>) -> AgentHistory {
        let tunnels = load(&path, &*log);
        AgentHistory { path, clock, log, inner: Mutex::new(Inner { tunnels, ..Default::default() }) }
    }

    /// Настоящая: `history.bin` в папке данных, рядом со `Stats.ini` (те же права: SYSTEM и администраторы).
    pub(super) fn real(log: Box<Log>) -> AgentHistory {
        AgentHistory::new(crate::daemon::data_dir().join("history.bin"), Box::new(crate::monitor::unix_now), log)
    }

    /// Ответ на `History`: интервалы ряда в его сроке. Туннеля нет — пустой ряд той же ширины.
    pub(super) fn history(&self, tunnel: &str, range: HistoryRange) -> History {
        let now = (self.clock)();
        let inner = lock(&self.inner);
        let buckets = inner.tunnels.get(tunnel).map(|t| t[range as usize].visible(range, now).map(Bucket::dto).collect()).unwrap_or_default();
        History { bucket_s: range.bucket_secs(), buckets }
    }

    /// Окно: туннель переименован (ядро подтвердило). Пишется сразу, как и статистика.
    pub(super) fn rename(&self, old: &str, new: &str) -> Result<(), String> {
        let mut inner = lock(&self.inner);
        if let Some(seen) = inner.seen.remove(old) {
            inner.seen.insert(new.to_string(), seen);
        }
        if let Some(tunnel) = inner.tunnels.remove(old) {
            inner.tunnels.insert(new.to_string(), tunnel);
            inner.dirty = true;
        }
        self.flush(&mut inner)
    }

    /// Окно: туннель удалён (ядро подтвердило) — история уходит вместе с его статистикой. Пишется сразу: иначе
    /// внезапное завершение агента вернуло бы её из файла, и её уже никто бы не убрал.
    pub(super) fn forget(&self, tunnel: &str) -> Result<(), String> {
        let mut inner = lock(&self.inner);
        inner.seen.remove(tunnel);
        if inner.tunnels.remove(tunnel).is_some() {
            inner.dirty = true;
        }
        self.flush(&mut inner)
    }

    /// Статистика этих туннелей убрана по сроку (`stats::prune`, туннеля нет 90 дней): история уходит с ней.
    pub(super) fn drop_pruned(&self, names: &[String]) {
        let mut inner = lock(&self.inner);
        for name in names {
            if inner.tunnels.remove(name).is_some() {
                inner.dirty = true;
            }
        }
    }

    /// Шаг записи (раз в `SAVE_EVERY` и при выходе из-за сбоя): вытеснить устаревшее — туннель, у которого за год
    /// не осталось ни одного интервала, убирается целиком, — и записать файл, если есть что. Ошибка записи — в журнал
    /// один раз, пока запись снова не пройдёт.
    pub(super) fn tick(&self) {
        let now = (self.clock)();
        let mut inner = lock(&self.inner);
        let mut expired = false;
        inner.tunnels.retain(|_, tunnel| {
            for (series, range) in tunnel.iter_mut().zip(HistoryRange::ALL) {
                expired |= series.expire(range, now);
            }
            tunnel.iter().any(|s| !s.0.is_empty())
        });
        inner.dirty |= expired;
        let result = self.flush(&mut inner);
        match (result, inner.save_failing.is_some()) {
            (Ok(()), _) => inner.save_failing = None,
            (Err(e), false) => {
                (self.log)(Severity::Bad, &trf("hist.save_failed", &[&e]));
                inner.save_failing = Some(e);
            }
            (Err(e), true) => inner.save_failing = Some(e),
        }
    }

    /// Записать файл, если в памяти есть то, чего в нём нет. Без журнала: вызвавший решает, кому сообщить.
    fn flush(&self, inner: &mut Inner) -> Result<(), String> {
        if !inner.dirty {
            return Ok(());
        }
        save(&self.path, &encode(&inner.tunnels)).map_err(|e| crate::fsutil::io_ctx(&self.path, e))?;
        inner.dirty = false;
        Ok(())
    }

    /// Учесть ответ ядра, снятый в `at` (монотонно) при unix-времени `now`.
    fn observe(&self, state: &CoreState, at: Instant, now: u64) {
        let mut guard = lock(&self.inner);
        let inner = &mut *guard;
        let mut next = BTreeMap::new();
        let mut moved_back = false;
        for (name, result) in &state.running {
            let prev = inner.seen.remove(name);
            let Ok(st) = result else {
                // Ядро не прочитало счётчики: замера нет, следующий считается от прошлого.
                if let Some(prev) = prev {
                    next.insert(name.clone(), prev);
                }
                continue;
            };
            let (rx, tx, port) = (st.rx_bytes(), st.tx_bytes(), st.listen_port);
            let changed = match prev {
                Some(p) if (p.rx, p.tx, p.port) == (rx, tx, port) => p.changed,
                _ => at,
            };
            if let Some(sample) = prev.and_then(|p| Sample::between(&p, rx, tx, port, at)) {
                let tunnel = inner.tunnels.entry(name.clone()).or_default();
                for range in HistoryRange::ALL {
                    moved_back |= tunnel[range as usize].add(range, now, &sample);
                }
                inner.dirty = true;
            }
            next.insert(name.clone(), Seen { at, changed, rx, tx, port });
        }
        inner.seen = next;
        if moved_back {
            (self.log)(Severity::Info, &trf("hist.clock_back", &[&crate::fmt::date_time(now)]));
        }
    }
}

impl CoreFeed for AgentHistory {
    fn accept(&self, state: &CoreState, at: Instant) {
        self.observe(state, at, (self.clock)());
    }
}

fn save(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    crate::fsutil::write_atomic(path, bytes)
}

/// Прочитать `history.bin`. Нет файла — пустая история (первый запуск). Файл не читается или повреждён — он
/// отодвигается в `<имя>.unreadable-<дата>` (`ini::quarantine`, как нечитаемые настройки), история начинается
/// заново, в журнал — путь, причина и куда отодвинут: молча принять файл за пустой нельзя, первая же запись затёрла бы его.
fn load(path: &Path, log: &Log) -> BTreeMap<String, Tunnel> {
    let error = match std::fs::read(path) {
        Ok(bytes) => match decode(&bytes) {
            Ok(tunnels) => return tunnels,
            Err(e) => e,
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return BTreeMap::new(),
        Err(e) => e.to_string(),
    };
    let shown = path.display().to_string();
    match crate::ini::quarantine(path) {
        Ok(to) => log(Severity::Bad, &trf("hist.unreadable", &[&shown, &error, &to.display().to_string()])),
        Err(why) => log(Severity::Bad, &trf("hist.unreadable_kept", &[&shown, &error, &why])),
    }
    BTreeMap::new()
}

fn encode(tunnels: &BTreeMap<String, Tunnel>) -> Vec<u8> {
    let buckets: usize = tunnels.values().flat_map(|t| t.iter()).map(|s| s.0.len()).sum();
    let mut out = Vec::with_capacity(16 + tunnels.len() * 64 + buckets * BUCKET_BYTES);
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.extend_from_slice(&(tunnels.len() as u32).to_le_bytes());
    for (name, tunnel) in tunnels {
        out.extend_from_slice(&(name.len() as u32).to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        for series in tunnel {
            out.extend_from_slice(&(series.0.len() as u32).to_le_bytes());
            for b in &series.0 {
                out.extend_from_slice(&b.start.to_le_bytes());
                out.extend_from_slice(&b.rx.to_le_bytes());
                out.extend_from_slice(&b.tx.to_le_bytes());
                out.extend_from_slice(&b.secs.to_le_bytes());
                out.extend_from_slice(&b.peak_rx.to_le_bytes());
                out.extend_from_slice(&b.peak_tx.to_le_bytes());
            }
        }
    }
    out
}

/// Чтение по порядку с проверкой длины: обрезанный файл — ошибка, а не паника.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N], String> {
        let bytes = self.bytes(N)?;
        Ok(bytes.try_into().expect("bytes(N) returns N bytes"))
    }

    fn bytes(&mut self, n: usize) -> Result<&'a [u8], String> {
        if self.0.len() < n {
            return Err("the file is cut short".into());
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Ok(head)
    }

    fn u32(&mut self) -> Result<u32, String> {
        self.take().map(u32::from_le_bytes)
    }

    fn u64(&mut self) -> Result<u64, String> {
        self.take().map(u64::from_le_bytes)
    }

    fn f32(&mut self) -> Result<f32, String> {
        self.take().map(f32::from_le_bytes)
    }
}

fn decode(bytes: &[u8]) -> Result<BTreeMap<String, Tunnel>, String> {
    let mut r = Reader(bytes);
    if &r.take::<4>()? != MAGIC {
        return Err("not a speed history file".into());
    }
    let [version] = r.take::<1>()?;
    if version != VERSION {
        return Err(format!("unknown format version {version}"));
    }
    let mut tunnels = BTreeMap::new();
    for _ in 0..r.u32()? {
        let len = r.u32()? as usize;
        if len > MAX_NAME {
            return Err(format!("tunnel name of {len} bytes"));
        }
        let name = String::from_utf8(r.bytes(len)?.to_vec()).map_err(|_| "tunnel name is not UTF-8".to_string())?;
        let mut tunnel = Tunnel::default();
        for (series, range) in tunnel.iter_mut().zip(HistoryRange::ALL) {
            *series = read_series(&mut r, range).map_err(|e| format!("{name}, {range:?}: {e}"))?;
        }
        if tunnels.insert(name.clone(), tunnel).is_some() {
            return Err(format!("tunnel {name} is stored twice"));
        }
    }
    if !r.0.is_empty() {
        return Err(format!("{} extra bytes at the end", r.0.len()));
    }
    Ok(tunnels)
}

/// Ряд из файла с теми же инвариантами, что в памяти: не длиннее ряда, интервалы выровнены и строго по возрастанию,
/// числа конечны и не отрицательны.
fn read_series(r: &mut Reader, range: HistoryRange) -> Result<Series, String> {
    let count = r.u32()? as usize;
    if count > range.capacity() {
        return Err(format!("{count} intervals, at most {}", range.capacity()));
    }
    let mut series = VecDeque::with_capacity(count);
    for _ in 0..count {
        let b = Bucket { start: r.u64()?, rx: r.u64()?, tx: r.u64()?, secs: r.f32()?, peak_rx: r.f32()?, peak_tx: r.f32()? };
        let aligned = b.start.is_multiple_of(range.bucket_secs());
        let ordered = series.back().is_none_or(|prev: &Bucket| prev.start < b.start);
        let sane = [b.secs, b.peak_rx, b.peak_tx].iter().all(|v| v.is_finite() && *v >= 0.0);
        if !(aligned && ordered && sane) {
            return Err(format!("bad interval at {}", b.start));
        }
        series.push_back(b);
    }
    Ok(Series(series))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::uapi::{Peer, Status};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    /// Полночь UTC: от неё удобно считать интервалы всех трёх рядов.
    const BASE: u64 = 1_789_948_800;
    const PORT: u16 = 51820;

    type Logged = Arc<Mutex<Vec<(Severity, String)>>>;

    /// История над временной папкой, часами теста и записью журнала; монотонное время — от `t0`.
    struct Rig {
        dir: PathBuf,
        history: AgentHistory,
        clock: Arc<AtomicU64>,
        logged: Logged,
        t0: Instant,
        /// Папку убирает только создавший её `Rig::new`, не повторное открытие.
        owns_dir: bool,
    }

    impl Rig {
        fn new(tag: &str) -> Rig {
            let dir = std::env::temp_dir().join(format!("awg-agent-history-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Rig::open(dir, true)
        }

        /// История над папкой `dir`; повторное открытие той же папки — как агент после перезапуска.
        fn open(dir: PathBuf, owns_dir: bool) -> Rig {
            let clock = Arc::new(AtomicU64::new(BASE));
            let logged = Logged::default();
            let (c, l) = (clock.clone(), logged.clone());
            let history = AgentHistory::new(
                dir.join("history.bin"),
                Box::new(move || c.load(Ordering::SeqCst)),
                Box::new(move |s, t| l.lock().unwrap().push((s, t.to_string()))),
            );
            Rig { dir, history, clock, logged, t0: Instant::now(), owns_dir }
        }

        /// Замер туннеля `t`: unix-время `unix`, монотонное — `mono` секунд от начала теста.
        fn sample(&self, unix: u64, mono: f64, rx: u64, tx: u64) {
            self.sample_port(unix, mono, rx, tx, PORT);
        }

        fn sample_port(&self, unix: u64, mono: f64, rx: u64, tx: u64, port: u16) {
            self.clock.store(unix, Ordering::SeqCst);
            let status = Status { listen_port: port, peers: vec![Peer { rx_bytes: rx, tx_bytes: tx, ..Default::default() }], ..Default::default() };
            let state = CoreState { tunnels: vec!["t".into()], running: [("t".to_string(), Ok(status))].into(), ..Default::default() };
            self.history.observe(&state, self.t0 + Duration::from_secs_f64(mono), unix);
        }

        /// Ответ ядра без туннеля: туннель отключён.
        fn down(&self, unix: u64, mono: f64) {
            self.clock.store(unix, Ordering::SeqCst);
            self.history.observe(&CoreState::default(), self.t0 + Duration::from_secs_f64(mono), unix);
        }

        fn series(&self, range: HistoryRange) -> Vec<HistoryBucket> {
            let h = self.history.history("t", range);
            assert_eq!(h.bucket_s, range.bucket_secs());
            h.buckets
        }

        fn starts(&self, range: HistoryRange) -> Vec<u64> {
            self.series(range).iter().map(|b| b.start).collect()
        }

        fn file(&self) -> PathBuf {
            self.dir.join("history.bin")
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            if self.owns_dir {
                let _ = std::fs::remove_dir_all(&self.dir);
            }
        }
    }

    /// Минута замеров раз в секунду: средние — байты за подключённое время, пик — наибольший прирост за секунду;
    /// месяц и год собирают те же замеры в свои интервалы.
    #[test]
    fn samples_are_averaged_and_peaked_per_bucket() {
        let rig = Rig::new("avg");
        let (mut rx, mut tx) = (1_000, 100);
        rig.sample(BASE, 0.0, rx, tx);
        for s in 1..=10u64 {
            // Секунда 5 — всплеск.
            let (drx, dtx) = if s == 5 { (50_000, 5_000) } else { (1_000, 100) };
            rx += drx;
            tx += dtx;
            rig.sample(BASE + s, s as f64, rx, tx);
        }
        let expected = HistoryBucket { start: BASE, secs: 10.0, rx: 5_900.0, tx: 590.0, peak_rx: 50_000.0, peak_tx: 5_000.0 };
        for range in HistoryRange::ALL {
            assert_eq!(rig.series(range), std::slice::from_ref(&expected), "{range:?}");
        }
    }

    /// Замеры через границу минуты: сутки делят их на два интервала, час и сутки года — один.
    #[test]
    fn a_minute_boundary_splits_only_the_day_series() {
        let rig = Rig::new("boundary");
        for s in 0..=4u64 {
            rig.sample(BASE + 58 + s, s as f64, s * 600, 0);
        }
        let day = rig.series(HistoryRange::Day);
        assert_eq!(day.iter().map(|b| (b.start, b.secs, b.rx)).collect::<Vec<_>>(), [(BASE, 1.0, 600.0), (BASE + 60, 3.0, 600.0)]);
        assert_eq!(rig.starts(HistoryRange::Month), [BASE]);
        assert_eq!(rig.series(HistoryRange::Month)[0].secs, 4.0);
    }

    /// Туннель отключён две минуты, ядро не отвечало 30 секунд, служба перезапущена (новый порт): этих интервалов нет
    /// вовсе — разрыв, а не ноль, — и байты, набранные за разрыв или в новой сессии до первого замера, не попадают
    /// ни в средние, ни в пик.
    #[test]
    fn down_time_core_gaps_and_new_sessions_are_gaps_not_zeros() {
        let rig = Rig::new("gaps");
        rig.sample(BASE, 0.0, 0, 0);
        rig.sample(BASE + 1, 1.0, 1_000, 0);
        // Отключён на минутах 1 и 2.
        rig.down(BASE + 60, 60.0);
        rig.down(BASE + 150, 150.0);
        // Подключился снова на минуте 3: первый замер — только начало отсчёта.
        rig.sample(BASE + 180, 180.0, 0, 0);
        rig.sample(BASE + 181, 181.0, 2_000, 0);
        // Ядро молчало 30 секунд, туннель жил: прирост за разрыв не данные.
        rig.sample(BASE + 211, 211.0, 900_000, 0);
        rig.sample(BASE + 212, 212.0, 902_000, 0);
        // Новая сессия на минуте 4 (другой порт): счётчики с нуля, байты до первого замера не учитываются.
        rig.sample_port(BASE + 240, 240.0, 70_000, 0, PORT + 1);
        rig.sample_port(BASE + 241, 241.0, 71_000, 0, PORT + 1);
        let day: Vec<_> = rig.series(HistoryRange::Day).iter().map(|b| (b.start - BASE, b.secs, b.rx, b.peak_rx)).collect();
        assert_eq!(day, [(0, 1.0, 1_000.0, 1_000.0), (180, 2.0, 2_000.0, 2_000.0), (240, 1.0, 1_000.0, 1_000.0)]);
    }

    /// Тот же замер ядра прочитан дважды: прирост следующего чтения делится на два шага для пика, а среднее
    /// интервала остаётся байтами за время; пик не меньше среднего.
    #[test]
    fn a_repeated_core_sample_does_not_double_the_peak() {
        let rig = Rig::new("repeat");
        rig.sample(BASE, 0.0, 0, 0);
        rig.sample(BASE + 1, 1.0, 1_000, 0);
        rig.sample(BASE + 2, 2.0, 1_000, 0);
        rig.sample(BASE + 3, 3.0, 3_000, 0);
        let b = &rig.series(HistoryRange::Day)[0];
        assert_eq!((b.rx, b.peak_rx), (1_000.0, 1_000.0));
    }

    /// Кольцо: суточный ряд держит 1440 последних минут, старше — вытесняется; минута без замеров — пробел.
    #[test]
    fn the_day_ring_wraps_after_1440_minutes() {
        let mut series = Series::default();
        let sample = Sample { rx: 60, tx: 0, secs: 1.0, peak: Some((60.0, 0.0)) };
        let minutes = 1500u64;
        for m in 0..minutes {
            if m != 1000 {
                series.add(HistoryRange::Day, BASE + m * 60 + 30, &sample);
            }
        }
        let now = BASE + (minutes - 1) * 60;
        let starts: Vec<u64> = series.visible(HistoryRange::Day, now).map(|b| b.start).collect();
        assert_eq!(series.0.len(), 1439, "1440 минут без одной пропущенной");
        assert_eq!(starts.first(), Some(&(BASE + 60 * 60)), "минуты 0..60 вытеснены");
        assert_eq!(starts.last(), Some(&now));
        assert!(!starts.contains(&(BASE + 1000 * 60)), "пропущенная минута — пробел");
        // Годовой ряд — 366 суток.
        let mut year = Series::default();
        for d in 0..400u64 {
            year.add(HistoryRange::Year, BASE + d * 86_400, &sample);
        }
        assert_eq!(year.0.len(), 366);
        assert_eq!(year.0.front().unwrap().start, BASE + 34 * 86_400);
    }

    /// Запрос окна через обработчик агента: интервалы ряда с данными; туннеля нет — пустой ряд той же ширины.
    #[test]
    fn history_request_returns_the_series() {
        let rig = Rig::new("request");
        rig.sample(BASE, 0.0, 0, 0);
        rig.sample(BASE + 1, 1.0, 4_000, 400);
        let h = rig.history.history("t", HistoryRange::Year);
        assert_eq!(h, History { bucket_s: 86_400, buckets: vec![HistoryBucket { start: BASE, secs: 1.0, rx: 4_000.0, tx: 400.0, peak_rx: 4_000.0, peak_tx: 400.0 }] });
        assert_eq!(rig.history.history("other", HistoryRange::Day), History { bucket_s: 60, buckets: vec![] });
        // Ряд за сутки, туннель не подключался больше суток: в ответе ничего старше срока.
        rig.clock.store(BASE + 86_400 + 60, Ordering::SeqCst);
        assert!(rig.series(HistoryRange::Day).is_empty());
        assert_eq!(rig.starts(HistoryRange::Month), [BASE]);
    }

    /// Запись и чтение: перезапущенный агент видит те же ряды и продолжает текущий интервал, а не начинает новый.
    #[test]
    fn history_survives_an_agent_restart() {
        let rig = Rig::new("restart");
        rig.history.tick();
        assert!(!rig.file().exists(), "нечего писать — файл не создаётся");
        for s in 0..=3u64 {
            rig.sample(BASE + 3_600 + s, s as f64, s * 1_000, s * 10);
        }
        rig.sample(BASE + 7_200, 100.0, 0, 0);
        rig.sample(BASE + 7_201, 101.0, 500, 5);
        rig.history.tick();
        assert!(rig.logged.lock().unwrap().is_empty(), "{:?}", rig.logged.lock().unwrap());
        let before: Vec<_> = HistoryRange::ALL.iter().map(|r| rig.series(*r)).collect();
        let dir = rig.dir.clone();
        let again = Rig::open(dir, false);
        again.clock.store(BASE + 7_201, Ordering::SeqCst);
        let after: Vec<_> = HistoryRange::ALL.iter().map(|r| again.series(*r)).collect();
        assert_eq!(after, before);
        assert!(again.logged.lock().unwrap().is_empty());
        again.sample(BASE + 7_210, 0.0, 0, 0);
        again.sample(BASE + 7_211, 1.0, 1_500, 15);
        let hour = &again.series(HistoryRange::Month)[1];
        assert_eq!((hour.start, hour.secs, hour.rx), (BASE + 7_200, 2.0, 1_000.0), "интервал продолжен");
        drop(rig);
    }

    /// Повреждённый или обрезанный файл не пропадает молча: он отодвинут рядом, в журнале путь и причина, история
    /// начинается заново, и следующая запись создаёт годный файл.
    #[test]
    fn a_corrupt_file_is_moved_aside_and_logged() {
        let rig = Rig::new("corrupt");
        rig.sample(BASE, 0.0, 0, 0);
        rig.sample(BASE + 1, 1.0, 1_000, 0);
        rig.history.tick();
        let good = std::fs::read(rig.file()).unwrap();
        let bad_files: [(&str, Vec<u8>); 4] = [
            ("garbage", b"not a history file".to_vec()),
            ("cut short", good[..good.len() - 1].to_vec()),
            ("extra bytes", [good.clone(), vec![0]].concat()),
            ("future version", [&good[..4], &[VERSION + 1][..], &good[5..]].concat()),
        ];
        for (what, bytes) in bad_files {
            let _ = std::fs::remove_dir_all(&rig.dir);
            std::fs::create_dir_all(&rig.dir).unwrap();
            std::fs::write(rig.file(), &bytes).unwrap();
            let again = Rig::open(rig.dir.clone(), false);
            again.clock.store(BASE + 1, Ordering::SeqCst);
            assert!(again.series(HistoryRange::Day).is_empty(), "{what}");
            let logged = again.logged.lock().unwrap().clone();
            assert_eq!(logged.len(), 1, "{what}: {logged:?}");
            assert!(logged[0].0 == Severity::Bad && logged[0].1.contains("history.bin"), "{what}: {logged:?}");
            let aside: Vec<_> = std::fs::read_dir(&rig.dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
            assert!(aside.len() == 1 && aside[0].starts_with("history.bin.unreadable-"), "{what}: {aside:?}");
            assert_eq!(std::fs::read(rig.dir.join(&aside[0])).unwrap(), bytes, "{what}: содержимое сохранено");
            again.sample(BASE + 2, 0.0, 0, 0);
            again.sample(BASE + 3, 1.0, 10, 0);
            again.history.tick();
            assert!(decode(&std::fs::read(again.file()).unwrap()).is_ok(), "{what}");

        }
    }

    /// Разбор отвергает нарушенные инварианты ряда, а не только обрезанный файл.
    #[test]
    fn decode_rejects_broken_series() {
        let one = |start: u64, secs: f32| Bucket { start, rx: 1, tx: 1, secs, peak_rx: 0.0, peak_tx: 0.0 };
        let file = |day: Vec<Bucket>| {
            let mut tunnel = Tunnel::default();
            tunnel[0] = Series(day.into());
            encode(&[("t".to_string(), tunnel)].into())
        };
        assert!(decode(&file(vec![one(BASE, 1.0), one(BASE + 60, 1.0)])).is_ok());
        assert!(decode(&file(vec![one(BASE + 60, 1.0), one(BASE, 1.0)])).unwrap_err().contains("bad interval"), "порядок");
        assert!(decode(&file(vec![one(BASE + 1, 1.0)])).is_err(), "не выровнен");
        assert!(decode(&file(vec![one(BASE, f32::NAN)])).is_err(), "не число");
        let long: Vec<Bucket> = (0..1441).map(|m| one(BASE + m * 60, 1.0)).collect();
        assert!(decode(&file(long)).unwrap_err().contains("1441"), "длиннее ряда");
    }

    /// Часы вперёд на 3 часа: пропущенное — пробел, старое в сроке остаётся. Вперёд на двое суток: суточный ряд
    /// начинается заново, месячный хранит прежнее.
    #[test]
    fn a_clock_jump_forward_leaves_a_gap() {
        let rig = Rig::new("forward");
        rig.sample(BASE, 0.0, 0, 0);
        rig.sample(BASE + 1, 1.0, 100, 0);
        rig.sample(BASE + 3 * 3_600 + 1, 2.0, 200, 0);
        assert_eq!(rig.starts(HistoryRange::Day), [BASE, BASE + 3 * 3_600]);
        rig.sample(BASE + 2 * 86_400 + 1, 3.0, 300, 0);
        assert_eq!(rig.starts(HistoryRange::Day), [BASE + 2 * 86_400]);
        assert_eq!(rig.starts(HistoryRange::Month), [BASE, BASE + 3 * 3_600, BASE + 2 * 86_400]);
        assert!(rig.logged.lock().unwrap().is_empty());
    }

    /// Часы назад на час (спешили): интервалы новее нового «сейчас» убраны, ряды упорядочены, замер ушёл в интервал
    /// нового времени, в журнале одна запись; дальше ряд идёт как обычно.
    #[test]
    fn a_clock_jump_back_drops_the_future_and_keeps_order() {
        let rig = Rig::new("back");
        rig.sample(BASE + 7_200, 0.0, 0, 0);
        rig.sample(BASE + 7_201, 1.0, 100, 0);
        rig.sample(BASE + 10_800, 2.0, 200, 0);
        assert_eq!(rig.starts(HistoryRange::Month), [BASE + 7_200, BASE + 10_800]);
        rig.sample(BASE + 9_000, 3.0, 300, 0);
        rig.sample(BASE + 9_001, 4.0, 400, 0);
        assert_eq!(rig.starts(HistoryRange::Day), [BASE + 7_200, BASE + 9_000]);
        assert_eq!(rig.starts(HistoryRange::Month), [BASE + 7_200], "час 2 продолжен, час 3 убран");
        assert_eq!(rig.series(HistoryRange::Month)[0].secs, 3.0);
        let logged = rig.logged.lock().unwrap().clone();
        assert_eq!(logged.len(), 1, "{logged:?}");
        assert_eq!(logged[0].0, Severity::Info);
        // Записанный файл тоже без будущего.
        rig.history.tick();
        let saved = decode(&std::fs::read(rig.file()).unwrap()).unwrap();
        assert!(saved["t"].iter().flat_map(|s| s.0.iter()).all(|b| b.start <= BASE + 9_001));
    }

    /// Окно переименовало и удалило туннель: история идёт за именем и уходит с удалением — в файле сразу.
    #[test]
    fn rename_and_forget_change_the_file_at_once() {
        let rig = Rig::new("rename");
        rig.sample(BASE, 0.0, 0, 0);
        rig.sample(BASE + 1, 1.0, 100, 0);
        rig.history.rename("t", "u").unwrap();
        assert!(rig.series(HistoryRange::Day).is_empty());
        assert_eq!(rig.history.history("u", HistoryRange::Day).buckets.len(), 1);
        assert_eq!(decode(&std::fs::read(rig.file()).unwrap()).unwrap().keys().collect::<Vec<_>>(), ["u"]);
        rig.history.forget("u").unwrap();
        assert!(decode(&std::fs::read(rig.file()).unwrap()).unwrap().is_empty());
        rig.history.forget("never").unwrap();
    }

    /// Статистика убрана по сроку — история тоже; туннель, у которого за год не осталось интервалов, уходит сам.
    #[test]
    fn pruned_and_expired_tunnels_leave_the_file() {
        let rig = Rig::new("prune");
        rig.sample(BASE, 0.0, 0, 0);
        rig.sample(BASE + 1, 1.0, 100, 0);
        rig.history.tick();
        rig.history.drop_pruned(&["t".to_string()]);
        rig.history.tick();
        assert!(decode(&std::fs::read(rig.file()).unwrap()).unwrap().is_empty());

        let rig = Rig::new("expire");
        rig.sample(BASE, 0.0, 0, 0);
        rig.sample(BASE + 1, 1.0, 100, 0);
        rig.history.tick();
        rig.clock.store(BASE + 400 * 86_400, Ordering::SeqCst);
        rig.history.tick();
        assert!(decode(&std::fs::read(rig.file()).unwrap()).unwrap().is_empty());
    }

    /// Файл не записался: одна запись в журнале, пока запись не пройдёт; окну на удаление — ошибка с путём.
    #[test]
    fn save_failure_is_logged_once_and_returned_to_the_window() {
        let rig = Rig::new("savefail");
        std::fs::create_dir_all(rig.file().join("inner")).unwrap();
        rig.sample(BASE, 0.0, 0, 0);
        rig.sample(BASE + 1, 1.0, 100, 0);
        rig.history.tick();
        rig.history.tick();
        let logged = rig.logged.lock().unwrap().clone();
        assert_eq!(logged.len(), 1, "{logged:?}");
        assert!(logged[0].1.contains("history.bin"), "{logged:?}");
        assert!(rig.history.forget("t").unwrap_err().contains("history.bin"));
    }

    /// Предел размера: полный файл одного туннеля — заголовок, имя и 2550 интервалов по 36 байт.
    #[test]
    fn a_full_tunnel_fits_the_stated_size_bound() {
        let sample = Sample { rx: u64::MAX / 4, tx: 1, secs: 1.0, peak: Some((1.0, 1.0)) };
        let mut tunnel = Tunnel::default();
        for (series, range) in tunnel.iter_mut().zip(HistoryRange::ALL) {
            for i in 0..range.capacity() as u64 * 2 {
                series.add(range, BASE + i * range.bucket_secs(), &sample);
            }
        }
        let name = "a".repeat(32);
        let bytes = encode(&[(name, tunnel)].into());
        assert_eq!(bytes.len(), 5 + 4 + 4 + 32 + 3 * 4 + 2550 * BUCKET_BYTES);
        assert_eq!(bytes.len(), 91_857);
    }
}
