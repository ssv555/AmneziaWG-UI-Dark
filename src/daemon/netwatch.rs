//! Смена сети для надзора за туннелями (`retry`): появился адрес — сеть, возможно, готова, и переподключение стоит
//! попробовать сразу, а не ждать своего шага расписания.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::events::Severity;

/// Сколько раз менялись адреса с запуска ядра. Надзор сравнивает с прошлым значением.
static CHANGES: AtomicU64 = AtomicU64::new(0);

pub(super) fn changes() -> u64 {
    CHANGES.load(Ordering::SeqCst)
}

/// Следить за сменой адресов в своём потоке. `NotifyAddrChange` без дескриптора ждёт синхронно: поток спит в нём до
/// смены и живёт, пока живёт ядро (выйти из ожидания нечем — и незачем: поток ничего не держит). Сломалось ожидание —
/// одна запись в журнал, и надзор дальше идёт только по расписанию.
pub(super) fn spawn(log: impl Fn(Severity, &str) + Send + 'static) {
    crate::crash::spawn_named("netwatch", move || loop {
        // SAFETY: оба указателя нулевые — синхронный вызов без OVERLAPPED.
        let code = unsafe { windows_sys::Win32::NetworkManagement::IpHelper::NotifyAddrChange(std::ptr::null_mut(), std::ptr::null()) };
        if code != 0 {
            let error = std::io::Error::from_raw_os_error(code as i32);
            return log(Severity::Warn, &crate::i18n::trf("core.netwatch_failed", &[&error.to_string()]));
        }
        CHANGES.fetch_add(1, Ordering::SeqCst);
    });
}
