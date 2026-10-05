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
    SetPing { enabled: bool, host: String },
    /// Язык окна (код ISO 639-2): на нём ядро пишет журнал событий.
    SetLanguage(String),
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
}

/// Действия в родном окне AmneziaWG; выполняет помощник с повышенными правами в сеансе пользователя.
#[derive(Serialize, Deserialize, Debug, Clone)]
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
}

/// Состояние ядра для окна.
#[derive(Serialize, Deserialize, Debug, Default)]
pub struct CoreState {
    pub mode: Option<Mode>,
    pub tunnels: Vec<String>,
    /// Подключённые туннели: состояние из канала службы или текст ошибки.
    pub running: BTreeMap<String, Result<Status, String>>,
    pub error: Option<String>,
    pub stats: Stats,
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
}

/// Пинг без `Instant`: возраст замеров в секундах.
#[derive(Serialize, Deserialize, Debug, Default, Clone)]
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
}
