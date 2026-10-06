//! Что обновлениям нужно от ядра над туннелями. Туннели переключает только ядро (под своей блокировкой, с желаемым
//! набором и надзором); обновления лишь просят его — те же внутренние запросы `HoldNative`/`Release`/`ReconnectEngine`,
//! что ходят по каналу ядра (`pipe_core`). Менеджер живёт в агенте и зовёт ядро только так.

use std::time::Duration;

/// Связь по каналу ядра: для менеджера во втором процессе (агенте).
pub mod pipe_core;

/// Ошибка — готовый текст для журнала.
pub trait CoreLink: Send + Sync {
    /// Снять с надзора на `lease` туннели режима 1, которые уберёт установщик AmneziaWG, и пометить занятыми; аренда
    /// истекает сама. Какие — решает ядро: у менеджера в агенте своих сведений о туннелях нет. Ответ — взятые
    /// (их и вернуть `release`), в режиме 2 — пусто.
    fn hold_native(&self, lease: Duration) -> Result<Vec<String>, String>;
    /// Вернуть аренду; неработающие желаемые из них ядро подключает сразу, обычным переключением.
    fn release(&self, tunnels: &[String]) -> Result<(), String>;
    /// Движок заменён: ядро переподключит туннели режима 2 само, в своём потоке; возврат — сразу.
    fn reconnect_engine(&self) -> Result<(), String>;
}

#[cfg(test)]
pub(crate) mod fake {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    use super::*;

    /// Записывает просьбы по порядку: «hold 900», «release a,b», «reconnect engine»; `native` — туннели, которые ядро
    /// берёт в аренду (работающие и желаемые режима 1); `refuse_hold` — ядро аренду не дало (недоступно);
    /// `refuse_reconnect` — ядро недоступно для `ReconnectEngine` (меняется по ходу проверки).
    #[derive(Default)]
    pub(crate) struct RecordingCore {
        pub(crate) calls: Mutex<Vec<String>>,
        pub(crate) native: Vec<String>,
        pub(crate) refuse_hold: bool,
        pub(crate) refuse_reconnect: AtomicBool,
    }

    impl RecordingCore {
        pub(crate) fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl CoreLink for RecordingCore {
        fn hold_native(&self, lease: Duration) -> Result<Vec<String>, String> {
            self.calls.lock().unwrap().push(format!("hold {}", lease.as_secs()));
            if self.refuse_hold {
                return Err("core: stopped".into());
            }
            Ok(self.native.clone())
        }
        fn release(&self, tunnels: &[String]) -> Result<(), String> {
            self.calls.lock().unwrap().push(format!("release {}", tunnels.join(",")));
            Ok(())
        }
        fn reconnect_engine(&self) -> Result<(), String> {
            self.calls.lock().unwrap().push("reconnect engine".into());
            if self.refuse_reconnect.load(Ordering::SeqCst) {
                return Err("core: pipe unavailable".into());
            }
            Ok(())
        }
    }
}
