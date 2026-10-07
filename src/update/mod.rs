//! Обновления и откаты трёх компонентов: оригинальный AmneziaWG, движок режима 2 (`tunnel.dll` + `wintun.dll`),
//! сборка программы. Всё делает ядро (SYSTEM); окно «Обновления и откаты» только показывает и командует.
//! Перед каждым обновлением и откатом — резервная копия текущего состояния; история — с кнопкой «Вернуть».

use serde::{Deserialize, Serialize};

/// HTTPS-загрузка (WinHTTP).
pub mod net;
/// Релизы GitHub: последняя версия, файлы, суммы.
pub mod feed;
/// Оригинальный AmneziaWG: установленная версия, подпись MSI, установка и удаление, копия туннелей.
pub mod native;
/// Подписанный манифест наших релизов (программа и движок).
pub mod sign;
/// Наши компоненты: движок режима 2 и сборка программы (из наших подписанных релизов).
pub mod ours;
/// История обновлений: номера строк, пределы, точка отката, `history.json`.
/// Резервные копии: папка копии, загрузка и проверка MSI, слияние туннелей.
mod backup;
/// Что делает менеджер сейчас (текст для окна собирается при чтении).
mod busy;
/// Что у каждого компонента своё: версии, копия, установка, возврат (стратегия для `Manager`).
mod component;
/// Что обновлениям нужно от ядра над туннелями (аренда на время MSI, переподключение после замены движка).
pub mod core_link;
/// Часы планировщика ежедневной проверки.
mod clock;
mod history;
/// Цель кнопки «Вернуть» строки истории: одно правило для окна (через состояние) и для ядра.
mod restore_target;
pub use restore_target::{RestoreBlock, RestoreOffer};
/// Файловые помощники хранилища: JSON, перенос файла, ротация журналов, имена копий.
mod jsonstore;
/// Источники релизов и файлов за интерфейсом (настоящие — GitHub; в тестах — подделка).
mod sources;
/// Модель окна: строки компонентов, версии без пояснений, о чём сообщать.
mod rows;
/// Менеджер обновлений ядра: проверка, копии, установка, возврат, история.
pub mod manager;

/// Порядок обновления: программа последней — она перезапускает ядро.
const ORDER: [Component; 3] = [Component::Native, Component::Engine, Component::App];

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Component {
    /// Оригинальный клиент AmneziaWG (MSI).
    Native,
    /// Движок режима 2: `tunnel.dll` + `wintun.dll`.
    Engine,
    /// Сборка программы (`awg-ui.exe`).
    App,
}

impl Component {
    /// Имя компонента в командной строке (`--core-updates apply <компонент> <версия>`).
    fn as_str(self) -> &'static str {
        match self {
            Component::Native => "native",
            Component::Engine => "engine",
            Component::App => "app",
        }
    }

    fn parse(s: &str) -> Option<Component> {
        ORDER.into_iter().find(|c| c.as_str() == s)
    }
}

/// Строка компонента в окне.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct ComponentState {
    pub component: Option<Component>,
    /// Установленная версия; `None` — не установлен (AmneziaWG) или неизвестна.
    pub installed: Option<String>,
    /// Когда поставлен на этом компьютере (unix, сек), если известно: подсказка к установленной версии.
    pub installed_at: Option<u64>,
    /// Дата выхода установленной версии (unix, сек) — та же величина, что `Available::published` для этой версии, чтобы
    /// одна версия не показывалась с двумя датами. `None` — неизвестна. В ответах ядра прежних версий поля нет.
    #[serde(default)]
    pub released: Option<u64>,
    /// Найденная при последней проверке версия (даже если не новее установленной).
    pub available: Option<Available>,
    /// Доступная версия новее установленной.
    pub update: bool,
    /// Ошибка последней проверки этого компонента.
    pub error: Option<String>,
    /// `error` — не сбой: в нашем релизе нет подписанного манифеста, его ставят только вручную (окно показывает
    /// нейтрально, полный текст — в подсказке).
    #[serde(default)]
    pub manual_only: bool,
    /// Только у движка: как наша версия соотносится с новейшим релизом amneziawg-windows; `None` — не проверялось.
    #[serde(default)]
    pub upstream: Option<EngineUpstream>,
}

/// Наш движок против новейшего стабильного релиза amneziawg-windows (метка — как в репозитории, `v3.1.20260814`).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum EngineUpstream {
    /// Наша версия — новейшая.
    Current(String),
    /// Amnezia выпустила более новую; она придёт с обновлением программы.
    Newer(String),
    /// Проверить не удалось (подробности — в журнале событий).
    Unchecked,
}

/// Найденная версия компонента. Версия и пояснение хранятся порознь: потребителям (сравнение, подтверждение, история,
/// уведомления) нужна голая версия, а пояснение нужно только для показа (`shown`). Прежние `state.json` и ответы ядра
/// хранили их одной строкой «3.1.20260814 · wintun 0.14.1» — `StoredAvailable` разбирает её при чтении.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(from = "StoredAvailable")]
pub struct Available {
    pub version: String,
    /// Версия wintun, поставляемая вместе с движком; у остальных компонентов `None`.
    pub wintun: Option<String>,
    /// Дата выхода (unix, сек).
    pub published: u64,
    /// Что нового — текст релиза, коротко.
    pub notes: String,
}

impl Available {
    /// Версия для показа: движок — «3.1.20260814 · wintun 0.14.1».
    pub fn shown(&self) -> String {
        match &self.wintun {
            Some(w) => format!("{}{WINTUN_JOINER}{w}", self.version),
            None => self.version.clone(),
        }
    }
}

/// Разделитель версии и пояснения в прежней однострочной записи.
const WINTUN_JOINER: &str = " · wintun ";

/// Запись `Available` на диске и по каналу: новая (с полем `wintun`) и прежняя (пояснение внутри `version`).
#[derive(Deserialize)]
struct StoredAvailable {
    version: String,
    #[serde(default)]
    wintun: Option<String>,
    published: u64,
    notes: String,
}

impl From<StoredAvailable> for Available {
    fn from(s: StoredAvailable) -> Self {
        let (version, wintun) = match (s.wintun, s.version.split_once(WINTUN_JOINER)) {
            (None, Some((v, w))) => (v.trim().to_string(), Some(w.trim().to_string())),
            (w, _) => (s.version, w),
        };
        Available { version, wintun, published: s.published, notes: s.notes }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Backup,
    Update,
    /// Возврат к версии из резервной копии.
    Restore,
}

/// Строка истории: резервная копия, обновление или возврат.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct HistoryEntry {
    pub id: u64,
    pub at: u64,
    pub component: Component,
    pub action: Action,
    pub from: Option<String>,
    pub to: Option<String>,
    /// Резервная копия этой строки (имя папки в хранилище копий); есть — можно «Вернуть».
    pub backup: Option<String>,
    pub backup_size: u64,
    pub ok: bool,
    pub error: Option<String>,
    /// У строки обновления или возврата — номер строки «Резервная копия», сделанной прямо перед ней (копия прежней
    /// версии: точка отката). В истории прежних версий поля нет — цель ищет `restore_target` по версии.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prior_backup: Option<u64>,
}

/// Последняя законченная работа с MSI родного AmneziaWG — для окна в `State` агента (`app::native_reopen`). MSI
/// закрывает окно AmneziaWG пользователя, а открыть его снова может только окно программы в сеансе пользователя.
/// Было ли окно открыто, смотрит агент сам прямо перед MSI: окну с опросом раз в секунду не успеть — MSI идёт
/// 1–3 с и закрывает окно в самом начале. Хранится в `state.json`: агент, перезапущенный следом обновлением
/// программы, отдаёт отметку и тогда, когда окно не успело её увидеть.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NativeUiMark {
    /// Номер законченной работы с MSI; только растёт. Новый номер — окно решает один раз.
    pub seq: u64,
    /// Перед MSI в сеансе пользователя работало окно AmneziaWG.
    pub was_open: bool,
}

/// Всё, что показывает окно «Обновления и откаты».
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct UpdatesState {
    pub components: Vec<ComponentState>,
    /// Новые сверху.
    pub history: Vec<HistoryEntry>,
    /// Последняя проверка (unix, сек).
    pub checked_at: Option<u64>,
    /// Идёт проверка, загрузка, копия или установка — текст для окна («Загрузка AmneziaWG 3.1.1… 45 %»).
    pub busy: Option<String>,
    /// Куда вернёт кнопка «Вернуть» каждой строки истории (решает ядро: окно папок копий не видит). В ответах ядра
    /// прежних версий поля нет.
    #[serde(default)]
    pub restores: Vec<RestoreOffer>,
}

/// Команды окна ядру.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum UpdateOp {
    /// Текущее состояние, без сети.
    State,
    /// Проверить источники сейчас.
    Check,
    /// Обновить выбранные компоненты до подтверждённых пользователем версий (каждый — с резервной копией перед
    /// установкой). Источник успел выпустить другую версию — компонент не трогается, ошибка.
    Apply(Vec<(Component, String)>),
    /// Вернуть компонент к версии, которую для строки истории `id` определяет `restore_target` (копия самой строки
    /// или копия прежней версии, сделанная перед обновлением).
    Restore(u64),
}

/// Ключ проверки и помощника обновлений:
/// `--core-updates state|check|apply <компонент> <версия>|restore <id> [--result <pid>:<описатель>]`.
/// `restore` окно запускает с правами администратора (UAC) через `elevated::run`: ядро выполняет возврат только по
/// такому запросу. `apply` — тот же запрос, что кнопка «Обновить» окна: сценарий проверки живого обновления.
pub const CLI_FLAG: &str = "--core-updates";

/// Команда ядру из аргументов после `--core-updates`.
pub fn parse_cli(rest: &[String]) -> Result<UpdateOp, String> {
    match rest.first().map(String::as_str) {
        Some("state") => Ok(UpdateOp::State),
        Some("check") => Ok(UpdateOp::Check),
        Some("apply") => match (rest.get(1), rest.get(2)) {
            (Some(name), Some(version)) => match Component::parse(name) {
                Some(component) => Ok(UpdateOp::Apply(vec![(component, version.clone())])),
                None => Err(format!("{CLI_FLAG} apply: unknown component {name:?}, expected native|engine|app")),
            },
            _ => Err(format!("{CLI_FLAG} apply: expected <native|engine|app> <version>, got {:?}", &rest[1..])),
        },
        Some("restore") => match rest.get(1).map(|id| id.parse::<u64>()) {
            Some(Ok(id)) => Ok(UpdateOp::Restore(id)),
            _ => Err(format!("{CLI_FLAG} restore: expected a history entry number, got {:?}", rest.get(1))),
        },
        other => Err(format!("{CLI_FLAG}: expected state|check|apply <component> <version>|restore <id>, got {other:?}")),
    }
}

/// Сколько командная строка ждёт, пока ядро занято запросом: установка MSI идёт минутами, проверка — нет.
pub fn cli_wait(op: &UpdateOp) -> std::time::Duration {
    std::time::Duration::from_secs(match op {
        UpdateOp::Apply(_) => 600,
        _ => 120,
    })
}

/// Аргументы помощника, который отправляет ядру возврат `id` (канал итога добавляет `elevated::run`).
pub fn restore_args(id: u64) -> String {
    format!("{CLI_FLAG} restore {id}")
}

/// Возврат компонента к версии из копии — только по запросу с правами администратора (подтверждение UAC): иначе
/// любая программа учётной записи владельца молча откатила бы компонент к старой версии с известными дырами.
/// Правило — для любого возврата, а не только к более старой версии: версии сравнимы не всегда (AmneziaWG может
/// быть не установлен, номер сборки — неизвестен), а простое правило нечем обойти. Одно правило для каждого, кто
/// обслуживает `Updates` (ядро, агент).
pub fn updates_allowed(op: &UpdateOp, elevated: bool) -> Result<(), String> {
    match op {
        UpdateOp::Restore(_) if !elevated => Err(crate::i18n::tr("core.restore_needs_admin")),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn restore_needs_elevated_caller() {
        use crate::i18n::tr;
        assert_eq!(updates_allowed(&UpdateOp::Restore(3), false), Err(tr("core.restore_needs_admin")), "без UAC — отказ");
        assert_eq!(updates_allowed(&UpdateOp::Restore(3), true), Ok(()));
        for op in [UpdateOp::State, UpdateOp::Check, UpdateOp::Apply(vec![])] {
            assert_eq!(updates_allowed(&op, false), Ok(()), "{op:?} — без прав администратора, как раньше");
        }
    }

    #[test]
    fn cli_commands() {
        assert!(matches!(parse_cli(&args("state")), Ok(UpdateOp::State)));
        assert!(matches!(parse_cli(&args("check")), Ok(UpdateOp::Check)));
        assert!(matches!(parse_cli(&args("restore 42 --result 1234:420")), Ok(UpdateOp::Restore(42))));
        assert!(parse_cli(&args("restore")).is_err(), "без номера");
        assert!(parse_cli(&args("restore -1")).is_err(), "номер — только неотрицательный");
        assert!(parse_cli(&args("restore 4x")).is_err());
        assert!(parse_cli(&args("rollback 1")).is_err(), "неизвестная команда — ошибка, а не тихое «state»");
        assert!(parse_cli(&[]).is_err());
    }

    #[test]
    fn cli_apply() {
        for (name, component) in [("native", Component::Native), ("engine", Component::Engine), ("app", Component::App)] {
            assert_eq!(parse_cli(&args(&format!("apply {name} 1.2.3"))), Ok(UpdateOp::Apply(vec![(component, "1.2.3".into())])));
        }
        assert!(parse_cli(&args("apply")).is_err(), "без компонента");
        assert!(parse_cli(&args("apply native")).is_err(), "без версии — не «последняя найденная»");
        let unknown = parse_cli(&args("apply amnezia 1.0")).unwrap_err();
        assert!(unknown.contains("native|engine|app"), "{unknown}");
        assert!(parse_cli(&args("apply Native 1.0")).is_err(), "имена — как в справке, без угадывания");
    }

    /// Командная строка шлёт ядру тот же `Apply`, что кнопка окна, и проходит ту же проверку прав.
    #[test]
    fn cli_apply_goes_through_the_same_check() {
        let op = parse_cli(&args("apply native 1.2.3")).unwrap();
        assert_eq!(updates_allowed(&op, false), Ok(()), "как кнопка «Обновить» окна — без UAC");
        assert_eq!(cli_wait(&op), std::time::Duration::from_secs(600), "MSI ставится минутами");
        assert_eq!(cli_wait(&UpdateOp::Check), std::time::Duration::from_secs(120));
    }

    #[test]
    fn restore_args_parse_back() {
        let line = format!("{} {} 1234:420", restore_args(7), crate::elevated::RESULT_FLAG);
        let rest = args(&line);
        assert_eq!(rest[0], CLI_FLAG);
        assert!(matches!(parse_cli(&rest[1..]), Ok(UpdateOp::Restore(7))));
    }

    fn parse(json: &str) -> Available {
        serde_json::from_str(json).unwrap()
    }

    /// Прежние `state.json` и ответы ядра: пояснение внутри строки версии.
    #[test]
    fn available_reads_the_old_one_string_form() {
        let old = parse(r#"{"version":"3.1.20260814 · wintun 0.14.1","published":5,"notes":"n"}"#);
        assert_eq!((old.version.as_str(), old.wintun.as_deref(), old.published, old.notes.as_str()), ("3.1.20260814", Some("0.14.1"), 5, "n"));
        assert_eq!(old.shown(), "3.1.20260814 · wintun 0.14.1");
        let plain = parse(r#"{"version":"1.0.5","published":1,"notes":""}"#);
        assert_eq!((plain.version.as_str(), plain.wintun.as_deref(), plain.shown().as_str()), ("1.0.5", None, "1.0.5"));
    }

    #[test]
    fn available_roundtrips_in_the_new_form() {
        let a = Available { version: "3.2".into(), wintun: Some("0.14".into()), published: 9, notes: "x".into() };
        let json = serde_json::to_string(&a).unwrap();
        assert!(json.contains(r#""version":"3.2""#) && json.contains(r#""wintun":"0.14""#), "{json}");
        assert_eq!(parse(&json), a);
        let no_note = Available { version: "1".into(), ..Default::default() };
        assert_eq!(parse(&serde_json::to_string(&no_note).unwrap()), no_note);
    }

    #[test]
    fn explicit_wintun_field_wins_over_the_joiner_in_version() {
        let a = parse(r#"{"version":"3.1 · wintun 9","wintun":"0.14","published":0,"notes":""}"#);
        assert_eq!((a.version.as_str(), a.wintun.as_deref()), ("3.1 · wintun 9", Some("0.14")), "новая запись не переразбирается");
    }
}
