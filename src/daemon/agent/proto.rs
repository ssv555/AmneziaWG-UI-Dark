//! Протокол агента: та же рамка, что у ядра (одна строка JSON — запрос, одна — ответ), но свои типы. Окно и ядро
//! говорят с агентом только этими сообщениями; запросы ядра (`daemon::proto`) сюда не попадают и наоборот.

use serde::{Deserialize, Serialize};

use crate::archive::Entry;
use crate::conf::TunnelInfo;
use crate::daemon::proto::{NativeOp, PingDto};
use crate::events::{Event, Origin};
use crate::stats::Stats;
use crate::store::ImportReport;

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub enum AgentRequest {
    /// Версия агента: так ядро (сторож) и окно проверяют, что агент отвечает.
    Hello,
    /// Окно «Обновления и откаты»: те же команды, что у запроса ядра `Updates`.
    Updates(crate::update::UpdateOp),
    /// Всё, что окно берёт у агента (`AgentState`); окно спрашивает раз в секунду, рядом с `State` ядра.
    State,
    /// Пинг через VPN: включён ли и до какого узла. Агент хранит это в `agent.ini`.
    SetPing { enabled: bool, host: String },
    /// Окно: ядро переименовало туннель — статистика переходит к новому имени.
    StatsRename { old: String, new: String },
    /// Окно: ядро удалило туннель — его статистика больше не нужна.
    StatsForget(String),
    /// Журнал агента (файл: события ядра и свои) новее этого номера: окно берёт отсюда историю при открытии и
    /// события самого агента (обновления) на ходу.
    Events { after: u64 },
    /// Конфиги туннелей и родное окно AmneziaWG (`tunnels`): долгие действия помощника и хранилище режима 2.
    Tunnel(TunnelRequest),
    /// История скорости туннеля за сутки, месяц или год (`history`, файл `history.bin`); только чтение. Агент прежней
    /// версии отвечает на него отказом «unknown variant» — окно показывает это как «истории нет».
    History { tunnel: String, range: HistoryRange },
}

/// Ряд истории скорости: ширина интервала и сколько их хранится. Часть протокола: окно рисует по `bucket_s`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryRange {
    /// 1440 интервалов по минуте.
    Day,
    /// 744 интервала по часу (31 сутки).
    Month,
    /// 366 интервалов по суткам (UTC).
    Year,
}

impl HistoryRange {
    pub const ALL: [HistoryRange; 3] = [HistoryRange::Day, HistoryRange::Month, HistoryRange::Year];

    /// Ширина интервала, секунды; интервал начинается на кратном ей unix-времени.
    pub fn bucket_secs(self) -> u64 {
        match self {
            HistoryRange::Day => 60,
            HistoryRange::Month => 3600,
            HistoryRange::Year => 86_400,
        }
    }

    /// Сколько интервалов хранится: столько же — наибольший ответ на `History`.
    pub fn capacity(self) -> usize {
        match self {
            HistoryRange::Day => 1440,
            HistoryRange::Month => 744,
            HistoryRange::Year => 366,
        }
    }
}

/// Ответ на `History`: интервалы с данными по возрастанию `start`. Интервала нет — туннель тогда не был подключён
/// (или агент не работал): это разрыв, а не ноль.
#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq)]
#[serde(default)]
pub struct History {
    /// Ширина интервала, секунды (`HistoryRange::bucket_secs`).
    pub bucket_s: u64,
    pub buckets: Vec<HistoryBucket>,
}

/// Один интервал истории. Средние — по времени, когда туннель был подключён внутри интервала (`secs`), а не по всей
/// его ширине; пик — наибольшая скорость между двумя замерами, не меньше среднего.
#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq)]
#[serde(default)]
pub struct HistoryBucket {
    /// Начало интервала, unix-секунды.
    pub start: u64,
    /// Сколько секунд интервала туннель был подключён и замерялся.
    pub secs: f64,
    /// Средняя скорость приёма и передачи, байт/с.
    pub rx: f64,
    pub tx: f64,
    /// Пиковая скорость приёма и передачи, байт/с.
    pub peak_rx: f64,
    pub peak_tx: f64,
}

/// Запросы окна о конфигах туннелей — те, что раньше обслуживало ядро (`daemon::proto::Request` с теми же именами).
/// `Delete` здесь — только режима 1: удаление туннеля режима 2 убирает его службу, это делает ядро.
#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub enum TunnelRequest {
    /// Конфиг текстом (режим 2 — хранилище, режим 1 — родной редактор).
    Read(String),
    Write { tunnel: String, text: String },
    Delete(String),
    /// Сведения о неподключённом туннеле.
    Details(String),
    // Только режим 2.
    Import(Vec<Entry>),
    ExportAll,
    NewTunnel(String),
    TakeNative,
    /// Только режим 1: действие в родном окне AmneziaWG.
    Native(NativeOp),
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub enum AgentResponse {
    Hello { version: String },
    /// Агент не взял запрос (не прочитан, неизвестный вариант, занят): показать отказ должен клиент.
    Refused(String),
    Updates(Box<crate::update::UpdatesState>),
    Ok,
    /// Запрос выполнялся и не удался.
    Err(String),
    State(Box<AgentState>),
    Events(Box<AgentEvents>),
    Text(String),
    Info(TunnelInfo),
    Entries(Vec<Entry>),
    Report(ImportReport),
    History(Box<History>),
}

/// Состояние агента для окна. Каждое поле — `serde(default)` (на всей структуре): агент другой версии, который поля
/// не знает или ещё не шлёт, даёт пустое значение, а не нечитаемый ответ.
#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq)]
#[serde(default)]
pub struct AgentState {
    pub ping: PingDto,
    /// Накопительная статистика трафика (`Stats.ini` агента).
    pub stats: Stats,
    /// Последняя законченная работа с MSI родного AmneziaWG и было ли перед ним открыто окно AmneziaWG. По ней окно
    /// открывает снова окно, закрытое MSI (`app::native_reopen`); `None` — агент прежней версии.
    pub native_ui: Option<crate::update::NativeUiMark>,
}

/// Ответ на `Events` — как события в `State` ядра: номера, код отсчёта и сколько подгружено из файла (`events::Cursor`).
#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq)]
#[serde(default)]
pub struct AgentEvents {
    pub instance: u64,
    pub loaded: u64,
    pub events: Vec<(u64, Event)>,
    /// До какого места журнала ядра события в журнале агента есть подряд: окно не повторяет их, когда они приходят
    /// и от ядра. `None` — агент ещё не получил от ядра ни одного ответа.
    pub core: Option<Origin>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_survive_the_pipe_as_single_lines() {
        let line = serde_json::to_string(&AgentRequest::Hello).unwrap();
        assert!(!line.contains('\n'), "одна строка на сообщение");
        assert_eq!(serde_json::from_str::<AgentRequest>(&line).unwrap(), AgentRequest::Hello);
        let ping = PingDto { host: "1.1.1.1".into(), last: Some(Ok(30)), fails: 0, history: vec![(1.5, Some(30))] };
        for reply in [AgentResponse::Hello { version: "1.2.3".into() }, AgentResponse::State(Box::new(AgentState { ping, stats: stats_of("t", 42), native_ui: Some(crate::update::NativeUiMark { seq: 3, was_open: true }) }))] {
            let line = serde_json::to_string(&reply).unwrap();
            assert!(!line.contains('\n'));
            assert_eq!(serde_json::from_str::<AgentResponse>(&line).unwrap(), reply);
        }
        for set in [
            AgentRequest::SetPing { enabled: false, host: "h".into() },
            AgentRequest::StatsRename { old: "a".into(), new: "b".into() },
            AgentRequest::StatsForget("a".into()),
        ] {
            assert_eq!(serde_json::from_str::<AgentRequest>(&serde_json::to_string(&set).unwrap()).unwrap(), set);
        }
    }

    /// Агент другой версии: поля, которых он не шлёт, пусты; лишние (из будущих шагов) не ломают разбор.
    #[test]
    fn agent_state_reads_older_and_newer_agents() {
        let older: AgentState = serde_json::from_str("{}").unwrap();
        assert_eq!(older.ping.host, "");
        let newer: AgentState = serde_json::from_str(r#"{"ping":{"host":"h","last":null,"fails":2,"history":[]},"events":{"x":1}}"#).unwrap();
        assert_eq!((newer.ping.host.as_str(), newer.ping.fails), ("h", 2));
        assert!(older.stats.is_empty() && newer.stats.is_empty(), "агент без статистики — пустая, а не ошибка");
        assert_eq!(older.native_ui, None, "агент прежней версии — отметки нет, окно AmneziaWG не открывается");
    }

    fn stats_of(name: &str, rx: u64) -> Stats {
        let mut st = crate::stats::TunnelStats::default();
        st.observe(rx, 1, 7, None, 100);
        [(name.to_string(), st)].into()
    }

    /// Статистика доходит до окна целиком: и видимые числа, и счётчики сессии.
    #[test]
    fn agent_state_carries_the_stats() {
        let state = AgentState { stats: stats_of("office", 5_000), ..Default::default() };
        let back: AgentState = serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        assert_eq!(back, state);
        assert_eq!(back.stats["office"].rx, 5_000);
    }

    #[test]
    fn tunnel_messages_survive_the_pipe() {
        let requests = [
            AgentRequest::Tunnel(TunnelRequest::Write { tunnel: "a".into(), text: "[Interface]\nAddress = 10.0.0.2/32\n".into() }),
            AgentRequest::Tunnel(TunnelRequest::Import(vec![Entry { name: "b".into(), text: "x".into() }])),
            AgentRequest::Tunnel(TunnelRequest::Native(NativeOp::Import(Some(r"C:\a b\x.conf".into())))),
            AgentRequest::Tunnel(TunnelRequest::TakeNative),
        ];
        for request in requests {
            let line = serde_json::to_string(&request).unwrap();
            assert!(!line.contains('\n'), "одна строка на сообщение");
            assert_eq!(serde_json::from_str::<AgentRequest>(&line).unwrap(), request);
        }
        let report = ImportReport { added: vec!["a".into()], existing: vec![], bad_name: vec!["b c".into()], scripts: vec!["d".into()] };
        for reply in [AgentResponse::Text("t\nu".into()), AgentResponse::Info(TunnelInfo::default()), AgentResponse::Report(report)] {
            let line = serde_json::to_string(&reply).unwrap();
            assert!(!line.contains('\n'));
            assert_eq!(serde_json::from_str::<AgentResponse>(&line).unwrap(), reply);
        }
    }

    /// Запрос и ответ истории — одной строкой и без потерь; ответ агента без полей (другой версии) — пустая история.
    #[test]
    fn history_messages_survive_the_pipe() {
        for range in HistoryRange::ALL {
            let request = AgentRequest::History { tunnel: "office".into(), range };
            let line = serde_json::to_string(&request).unwrap();
            assert!(!line.contains('\n'));
            assert_eq!(serde_json::from_str::<AgentRequest>(&line).unwrap(), request);
        }
        assert_eq!(serde_json::to_string(&AgentRequest::History { tunnel: "a".into(), range: HistoryRange::Day }).unwrap(), r#"{"History":{"tunnel":"a","range":"Day"}}"#);
        let bucket = HistoryBucket { start: 120, secs: 59.5, rx: 1000.0, tx: 10.0, peak_rx: 4000.0, peak_tx: 50.0 };
        let reply = AgentResponse::History(Box::new(History { bucket_s: 60, buckets: vec![bucket] }));
        let line = serde_json::to_string(&reply).unwrap();
        assert!(!line.contains('\n'));
        assert_eq!(serde_json::from_str::<AgentResponse>(&line).unwrap(), reply);
        assert_eq!(serde_json::from_str::<History>("{}").unwrap(), History::default());
    }

    /// Ряды по заданию: сутки по минуте, месяц по часу, год по суткам — и каждый покрывает свой срок.
    #[test]
    fn history_ranges_have_the_agreed_shape() {
        let shape: Vec<_> = HistoryRange::ALL.iter().map(|r| (r.bucket_secs(), r.capacity())).collect();
        assert_eq!(shape, [(60, 1440), (3600, 744), (86_400, 366)]);
        assert_eq!(HistoryRange::Day.bucket_secs() * HistoryRange::Day.capacity() as u64, 86_400);
    }

    #[test]
    fn updates_messages_survive_the_pipe() {
        use crate::update::{UpdateOp, UpdatesState};
        let request = AgentRequest::Updates(UpdateOp::Restore(7));
        let line = serde_json::to_string(&request).unwrap();
        assert_eq!(serde_json::from_str::<AgentRequest>(&line).unwrap(), request);
        let reply = AgentResponse::Updates(Box::new(UpdatesState { checked_at: Some(42), busy: Some("x".into()), ..Default::default() }));
        let line = serde_json::to_string(&reply).unwrap();
        assert!(!line.contains('\n'));
        assert_eq!(serde_json::from_str::<AgentResponse>(&line).unwrap(), reply);
    }
}
