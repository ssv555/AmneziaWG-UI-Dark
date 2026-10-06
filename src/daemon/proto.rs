//! Протокол окна и ядра: одна строка JSON — запрос, одна строка JSON — ответ, затем соединение закрывается.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::archive::Entry;
use crate::conf::TunnelInfo;
use crate::events::Event;
use crate::settings::Mode;
use crate::stats::Stats;
use crate::store::ImportReport;
use crate::uapi::Status;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub enum Plan {
    Connect,
    Disconnect,
    Reconnect,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum Request {
    /// Версия ядра и режим.
    Hello,
    /// Всё, что показывает окно; события — только новее `events_after`.
    State { events_after: u64 },
    /// Переключить туннель; без `multiple` остальные подключённые отключаются.
    Switch { tunnel: String, plan: Plan, multiple: bool },
    /// Сменить режим работы: туннели прежнего режима отключаются, окно не перезапускается.
    SetMode(Mode),
    /// Только от окна прежней версии: пинг ведёт агент (`agent::proto::AgentRequest::SetPing`), ядро отвечает отказом.
    SetPing { enabled: bool, host: String },
    /// Язык окна (код ISO 639-2): на нём ядро пишет журнал событий.
    SetLanguage(String),
    // Конфиги туннелей и родное окно AmneziaWG (Read, Write, Details, Import, ExportAll, NewTunnel, TakeNative, Native,
    // Delete режима 1) обслуживает агент (`agent::proto::TunnelRequest`); окну прежней версии ядро отвечает отказом.
    /// Конфиг туннеля текстом (режим 2 — хранилище, режим 1 — родной редактор).
    Read(String),
    Write { tunnel: String, text: String },
    Delete(String),
    /// Сведения о неподключённом туннеле.
    Details(String),
    // Только режим 2.
    Import(Vec<Entry>),
    ExportAll,
    Rename { old: String, new: String },
    NewTunnel(String),
    TakeNative,
    // Только режим 1: действия в родном окне AmneziaWG (в сеансе пользователя, с его правами администратора).
    Native(NativeOp),
    /// Окно «Обновления и откаты».
    Updates(crate::update::UpdateOp),
    /// «Повторить»: переподключение желаемого туннеля по расписанию с начала (`retry`).
    Retry(String),
    // Внутренние, только от SYSTEM (второй процесс ядра; окно получает отказ): обновления, которым нужны туннели.
    /// Снять туннели режима 1 с надзора на `lease_s` секунд и пометить занятыми (установщик AmneziaWG убирает их
    /// службы). Какие — решает ядро (работающие и желаемые), ответ — `Held` с этим списком; режим 2 — пустой.
    /// Аренда истекает сама: держатель пропал — надзор идёт снова.
    HoldNative { lease_s: u64 },
    /// Вернуть аренду; неработающие желаемые из них ядро сразу подключает обычным переключением.
    Release { tunnels: Vec<String> },
    /// Движок заменён: ядро переподключает туннели режима 2 в своём потоке надзора, ответ — сразу.
    ReconnectEngine,
    /// Агент удалил туннель режима 1 в родном окне: ядро убирает его из желаемого набора и забывает его сведения.
    Forget(String),
    /// Агент прочитал сведения туннеля режима 1 из родного окна (`Some`) или изменил его конфиг (`None` — прежние
    /// сведения устарели): по ним ядро видит, с кем туннель конфликтует при подключении.
    Footprint { tunnel: String, info: Option<TunnelInfo> },
}

/// Действия в родном окне AmneziaWG; выполняет помощник с повышенными правами в сеансе пользователя.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum NativeOp {
    Open,
    Edit(String),
    /// Диалог импорта, файл выделен (если задан).
    Import(Option<String>),
    /// Закрыть родное окно и его значок в трее (туннели не трогаются).
    Close,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum Response {
    Ok,
    /// Запрос выполнялся и не удался; ошибку действия над туннелем (`Switch`) ядро уже записало в свой журнал.
    Err(String),
    /// Ядро не взяло запрос (занято, запрос не прочитан, недопустимое имя или текст) и в журнал его не писало:
    /// показать отказ должен клиент, иначе щелчок в окне не оставит следа.
    Refused(String),
    Hello { version: String, mode: Mode },
    State(Box<CoreState>),
    Text(String),
    Info(TunnelInfo),
    Entries(Vec<Entry>),
    Report(ImportReport),
    Updates(Box<crate::update::UpdatesState>),
    /// Туннели, взятые в аренду по `HoldNative`: их и возвращает `Release`.
    Held(Vec<String>),
}

/// Состояние ядра для окна.
#[derive(Serialize, Deserialize, Debug, Default)]
pub struct CoreState {
    pub mode: Option<Mode>,
    pub tunnels: Vec<String>,
    /// Подключённые туннели: состояние из канала службы или текст ошибки.
    pub running: BTreeMap<String, Result<Status, String>>,
    pub error: Option<String>,
    /// Статистику ведёт агент (`agent::proto::AgentState`); ядро шлёт пустую — окно прежней версии без поля ответа не
    /// разберёт. Ядро прежней версии шлёт настоящую, новое окно её не читает.
    #[serde(default)]
    pub stats: Stats,
    /// Пинг ведёт агент (`agent::proto::AgentState`); ядро шлёт пустой — окно прежней версии без поля ответа не
    /// разберёт. Ядро прежней версии шлёт настоящий, новое окно его не читает.
    #[serde(default)]
    pub ping: PingDto,
    /// Новые события с их номерами (по возрастанию).
    pub events: Vec<(u64, Event)>,
    /// Код отсчёта номеров событий: меняется при каждом запуске ядра — окно узнаёт перезапуск.
    #[serde(default)]
    pub events_instance: u64,
    /// Сколько событий ядро подгрузило из файла при запуске (номера 1..=это): окно их уже видело.
    #[serde(default)]
    pub events_loaded: u64,
    /// Служба менеджера AmneziaWG (режим 1).
    pub service: String,
    /// Туннели, которые ядро сейчас переключает.
    pub busy: Vec<String>,
    /// Желаемые туннели, которые ядро переподключает (`retry`). Ядро прежней версии поля не шлёт — пусто.
    #[serde(default)]
    pub retries: BTreeMap<String, RetryState>,
    /// Второй процесс (агент) глазами сторожа ядра. Ядро прежней версии агента не запускает и поля не шлёт — `None`.
    #[serde(default)]
    pub agent: Option<AgentStatus>,
}

/// Агент: запускается, работает или не работает (сторож ждёт паузу перед следующим запуском).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum AgentStatus {
    Starting,
    Up { pid: u32 },
    /// Не работает с `since` (unix-секунды); `last_exit` — чем кончился последний запуск.
    Down { since: u64, last_exit: AgentExit },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum AgentExit {
    /// Процесс вышел сам (упал, превысил предел памяти задания) с этим кодом.
    Code(u32),
    /// Не отвечал на `Hello` сторожа и был завершён.
    Hung,
    /// Процесс не запустился: текст ошибки Windows.
    NotStarted(String),
}

/// Переподключение туннеля: сколько попыток сделано, через сколько секунд следующая, последняя ошибка; `slow` — первые
/// 10 минут не помогли, дальше попытка раз в 10 минут.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct RetryState {
    pub attempt: u32,
    pub next_in_s: u64,
    pub last_error: String,
    pub slow: bool,
}

/// Пинг без `Instant`: возраст замеров в секундах.
#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq)]
pub struct PingDto {
    pub host: String,
    pub last: Option<Result<u32, String>>,
    pub fails: u32,
    pub history: Vec<(f64, Option<u32>)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_and_state_survive_the_pipe() {
        let reqs = vec![
            Request::Switch { tunnel: "office".into(), plan: Plan::Reconnect, multiple: false },
            Request::SetMode(Mode::Engine),
            Request::Import(vec![Entry { name: "a".into(), text: "[Interface]\n".into() }]),
            Request::Native(NativeOp::Import(Some(r"C:\a b\x.conf".into()))),
        ];
        for r in reqs {
            let line = serde_json::to_string(&r).unwrap();
            assert!(!line.contains('\n'), "одна строка на сообщение");
            let back: Request = serde_json::from_str(&line).unwrap();
            assert_eq!(format!("{back:?}"), format!("{r:?}"));
        }
        let mut state = CoreState { mode: Some(Mode::Overlay), tunnels: vec!["office".into()], ..Default::default() };
        state.running.insert("office".into(), Err("pipe".into()));
        state.events.push((7, Event::new(1, "office", crate::events::Severity::Warn, "текст\nс переводом", true)));
        let line = serde_json::to_string(&Response::State(Box::new(state))).unwrap();
        assert!(!line.contains('\n'));
        match serde_json::from_str::<Response>(&line).unwrap() {
            Response::State(s) => {
                assert_eq!(s.running["office"].as_ref().err().map(String::as_str), Some("pipe"));
                assert_eq!(s.events[0].0, 7);
                assert_eq!(s.events[0].1.text, "текст\nс переводом");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn ping_ages_roundtrip() {
        let dto = PingDto { host: "1.1.1.1".into(), last: Some(Ok(33)), fails: 0, history: vec![(5.0, Some(30)), (0.5, None)] };
        let state = crate::ping::PingState::from_dto(dto);
        let back = state.to_dto();
        assert_eq!(back.history.len(), 2);
        assert!((back.history[0].0 - 5.0).abs() < 0.5 && back.history[1].1.is_none());
    }

    #[test]
    fn state_from_core_without_event_instance_still_parses() {
        let line = serde_json::to_string(&Response::State(Box::default())).unwrap();
        let old = line.replace(",\"events_instance\":0", "").replace(",\"events_loaded\":0", "");
        assert!(!old.contains("events_instance"), "{old}");
        match serde_json::from_str::<Response>(&old).unwrap() {
            Response::State(s) => assert_eq!((s.events_instance, s.events_loaded), (0, 0)),
            other => panic!("{other:?}"),
        }
    }

    /// Ядро без статистики (будущее, где поле убрано) разбирается; ядро прежней версии со статистикой — тоже.
    #[test]
    fn state_without_or_with_stats_parses() {
        let line = serde_json::to_string(&Response::State(Box::default())).unwrap();
        let without = line.replace("\"stats\":{},", "");
        assert!(!without.contains("\"stats\""), "{without}");
        match serde_json::from_str::<Response>(&without).unwrap() {
            Response::State(s) => assert!(s.stats.is_empty()),
            other => panic!("{other:?}"),
        }
        let mut state = CoreState::default();
        state.stats.insert("office".into(), Default::default());
        let old_core = serde_json::to_string(&Response::State(Box::new(state))).unwrap();
        match serde_json::from_str::<Response>(&old_core).unwrap() {
            Response::State(s) => assert!(s.stats.contains_key("office")),
            other => panic!("{other:?}"),
        }
    }

    /// Новое окно со старым ядром (поля нет) и старое окно с новым ядром (лишнее поле) понимают друг друга.
    #[test]
    fn retry_state_is_backward_compatible() {
        let mut state = CoreState::default();
        let retry = RetryState { attempt: 5, next_in_s: 7, last_error: "Element not found".into(), slow: false };
        state.retries.insert("office".into(), retry.clone());
        let line = serde_json::to_string(&Response::State(Box::new(state))).unwrap();
        match serde_json::from_str::<Response>(&line).unwrap() {
            Response::State(s) => assert_eq!(s.retries["office"], retry),
            other => panic!("{other:?}"),
        }
        let old_core = serde_json::to_string(&Response::State(Box::default())).unwrap().replace(",\"retries\":{}", "");
        assert!(!old_core.contains("retries"), "{old_core}");
        match serde_json::from_str::<Response>(&old_core).unwrap() {
            Response::State(s) => assert!(s.retries.is_empty()),
            other => panic!("{other:?}"),
        }
        // Старое окно: та же структура без поля `retries` — serde пропускает незнакомые поля.
        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct OldState {
            tunnels: Vec<String>,
            busy: Vec<String>,
        }
        let json = serde_json::to_value(CoreState { retries: [("a".to_string(), RetryState { attempt: 1, next_in_s: 1, last_error: String::new(), slow: true })].into(), ..Default::default() }).unwrap();
        assert!(serde_json::from_value::<OldState>(json).is_ok());
    }

    /// Состояние агента: доходит до окна; ядро без агента (поля нет) — `None`; старое окно лишнее поле пропускает.
    #[test]
    fn agent_status_is_backward_compatible() {
        let statuses = [
            AgentStatus::Starting,
            AgentStatus::Up { pid: 4242 },
            AgentStatus::Down { since: 1_700_000_000, last_exit: AgentExit::Code(2) },
            AgentStatus::Down { since: 1, last_exit: AgentExit::Hung },
            AgentStatus::Down { since: 1, last_exit: AgentExit::NotStarted("CreateProcess: denied".into()) },
        ];
        for status in statuses {
            let state = CoreState { agent: Some(status.clone()), ..Default::default() };
            let line = serde_json::to_string(&Response::State(Box::new(state))).unwrap();
            assert!(!line.contains('\n'));
            match serde_json::from_str::<Response>(&line).unwrap() {
                Response::State(s) => assert_eq!(s.agent, Some(status)),
                other => panic!("{other:?}"),
            }
        }
        let old_core = serde_json::to_string(&Response::State(Box::default())).unwrap().replace(",\"agent\":null", "");
        assert!(!old_core.contains("agent"), "{old_core}");
        match serde_json::from_str::<Response>(&old_core).unwrap() {
            Response::State(s) => assert_eq!(s.agent, None),
            other => panic!("{other:?}"),
        }
        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct OldState {
            tunnels: Vec<String>,
            retries: BTreeMap<String, RetryState>,
        }
        let json = serde_json::to_value(CoreState { agent: Some(AgentStatus::Up { pid: 1 }), ..Default::default() }).unwrap();
        assert!(serde_json::from_value::<OldState>(json).is_ok());
    }

    /// Пинг ушёл к агенту. Старое окно с новым ядром: поле `ping` в ответе есть (пустое), иначе ответ не разобрать.
    /// Будущее ядро без поля: новое окно разбирает ответ, пинг пуст.
    #[test]
    fn core_state_keeps_the_ping_field_for_older_windows() {
        let json = serde_json::to_value(CoreState::default()).unwrap();
        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct OldState {
            tunnels: Vec<String>,
            ping: PingDto,
        }
        assert!(serde_json::from_value::<OldState>(json).is_ok());
        let without = serde_json::to_string(&Response::State(Box::default())).unwrap();
        let without = without.replace(&format!(",\"ping\":{}", serde_json::to_string(&PingDto::default()).unwrap()), "");
        assert!(!without.contains("\"ping\""), "{without}");
        match serde_json::from_str::<Response>(&without).unwrap() {
            Response::State(s) => assert_eq!(s.ping, PingDto::default()),
            other => panic!("{other:?}"),
        }
    }

    /// Строки запросов окна 0.4.0 как они идут по каналу (serde той версии; список — её `Request`). Новое ядро обязано
    /// их разобрать: иначе окно прежней версии получает «ошибку разбора» вместо понятного отказа (`core.moved_to_agent`,
    /// `core.updates_moved`, `core.ping_moved` — их проверяют тесты `server`). Ловит переименование варианта или поля,
    /// которое круговой тест на текущем enum не видит.
    #[test]
    fn requests_of_a_0_4_0_window_still_decode() {
        let lines = [
            (r#""Hello""#, "Hello"),
            (r#"{"State":{"events_after":5}}"#, "State"),
            (r#"{"Switch":{"tunnel":"office","plan":"Connect","multiple":false}}"#, "Switch"),
            (r#"{"SetMode":"Engine"}"#, "SetMode"),
            (r#"{"SetPing":{"enabled":false,"host":"1.1.1.1"}}"#, "SetPing"),
            (r#"{"SetLanguage":"rus"}"#, "SetLanguage"),
            (r#"{"Read":"office"}"#, "Read"),
            (r#"{"Write":{"tunnel":"office","text":"[Interface]\n"}}"#, "Write"),
            (r#"{"Delete":"office"}"#, "Delete"),
            (r#"{"Details":"office"}"#, "Details"),
            (r#"{"Import":[{"name":"a","text":"[Interface]\n"}]}"#, "Import"),
            (r#""ExportAll""#, "ExportAll"),
            (r#"{"Rename":{"old":"a","new":"b"}}"#, "Rename"),
            (r#"{"NewTunnel":"a"}"#, "NewTunnel"),
            (r#""TakeNative""#, "TakeNative"),
            (r#"{"Native":"Open"}"#, "Native(Open)"),
            (r#"{"Native":{"Edit":"a"}}"#, "Native(Edit"),
            (r#"{"Native":{"Import":null}}"#, "Native(Import(None))"),
            (r#"{"Native":"Close"}"#, "Native(Close)"),
            (r#"{"Updates":"State"}"#, "Updates(State)"),
            (r#"{"Updates":"Check"}"#, "Updates(Check)"),
            (r#"{"Updates":{"Apply":[["App","0.5.0"]]}}"#, "Updates(Apply"),
            (r#"{"Updates":{"Restore":3}}"#, "Updates(Restore(3))"),
            (r#"{"Retry":"office"}"#, "Retry"),
        ];
        for (line, variant) in lines {
            let req: Request = serde_json::from_str(line).unwrap_or_else(|e| panic!("{line}: {e}"));
            assert!(format!("{req:?}").starts_with(variant), "{line} -> {req:?}");
        }
    }

    /// Ответ `State` ядра 0.4.0 целиком (без поля `agent`, с пингом и статистикой ядра) — новое окно над старым ядром
    /// (обновлено только окно или ядро откатилось) должно его разобрать, иначе пропало бы управление VPN. Ловит смену
    /// типа или обязательность любого поля, а не только нового: круговые тесты выше собирают JSON из текущих типов.
    #[test]
    fn state_of_a_0_4_0_core_still_decodes() {
        let status = r#"{"public_key":"pk","listen_port":51820,"awg_params":[["Jc","4"]],"peers":[{"public_key":"pp","endpoint":"192.0.2.1:51820","last_handshake_sec":10,"rx_bytes":100,"tx_bytes":50,"keepalive":25,"allowed_ips":["0.0.0.0/0"]}]}"#;
        let stats = r#"{"office":{"rx":1,"tx":2,"peak_rx":3.5,"peak_tx":4.0,"seconds":5.0,"since":6,"last_rx":1,"last_tx":2,"last_port":51820,"last_seen":7}}"#;
        let line = format!(
            r#"{{"State":{{"mode":"Engine","tunnels":["office","home"],"running":{{"office":{{"Ok":{status}}},"home":{{"Err":"pipe"}}}},"error":null,"stats":{stats},"ping":{{"host":"1.1.1.1","last":{{"Ok":33}},"fails":0,"history":[[5.0,30],[0.5,null]]}},"events":[[7,{{"at":1790000000,"tunnel":"office","severity":"Warn","text":"handshake late","notify":true}}]],"events_instance":11,"events_loaded":3,"service":"running","busy":[],"retries":{{"home":{{"attempt":2,"next_in_s":5,"last_error":"x","slow":false}}}}}}}}"#
        );
        let state = match serde_json::from_str::<Response>(&line).unwrap_or_else(|e| panic!("{e}: {line}")) {
            Response::State(s) => s,
            other => panic!("{other:?}"),
        };
        assert_eq!(state.mode, Some(Mode::Engine));
        assert_eq!(state.running["office"].as_ref().map(|s| (s.listen_port, s.rx_bytes())), Ok((51820, 100)));
        assert_eq!(state.stats["office"].rx, 1);
        assert_eq!((state.events[0].0, state.events[0].1.origin), (7, None));
        assert_eq!((state.events_instance, state.retries["home"].attempt), (11, 2));
        assert_eq!(state.agent, None, "ядро 0.4.0 об агенте не знает");
        assert!(matches!(serde_json::from_str::<Response>(r#"{"Hello":{"version":"0.4.0","mode":"Overlay"}}"#), Ok(Response::Hello { mode: Mode::Overlay, .. })));
    }
}
