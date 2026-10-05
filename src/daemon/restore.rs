//! Желаемый набор туннелей: какие пользователь оставил подключёнными. Ядро помнит его (`Config::tunnels`), чтобы
//! после перезапуска (перезагрузка, пропало питание) и после падения туннеля поднять их снова: в начале загрузки
//! движок может не поднять туннель (адаптер ещё не готов — «Element not found»), а службы может не оказаться вовсе.
//! Поднимает надзор `retry` — восстановление после запуска ядра его первый такт.

use std::time::Duration;

use super::proto::Plan;

/// Обновление с версии без набора: сколько ждать, пока службы туннелей поднимутся сами, прежде чем взять работающие
/// за желаемый набор (`Core::adopt_running`).
pub(crate) const ADOPT_AFTER: Duration = Duration::from_secs(30);

/// Желаемый набор после команды пользователя — по намерению, а не по исходу: подключаемый в нём, отключаемый и
/// заменённые им — нет. Не поднялся туннель сейчас — ядро будет пробовать снова (`retry`) и скажет в журнале.
pub(super) fn after_switch(desired: &[String], name: &str, plan: Plan, replaced: &[String]) -> Vec<String> {
    let mut next: Vec<String> = desired.iter().filter(|t| *t != name && !replaced.contains(t)).cloned().collect();
    if plan != Plan::Disconnect {
        next.push(name.to_string());
    }
    next
}

/// Набор неизвестен (`core.ini` прежней версии без ключа `tunnels`) — им становятся работающие туннели; возвращает
/// принятый набор. Известный, в том числе явно пустой, не трогается: `None`.
pub(super) fn adopt(config: &mut super::Config, running: Vec<String>) -> Option<Vec<String>> {
    if config.tunnels.is_some() {
        return None;
    }
    config.multiple = running.len() > 1;
    config.tunnels = Some(running.clone());
    Some(running)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::TunnelHost;
    use crate::crash::lock;
    use crate::daemon::retry::{Retries, Seen, FAST_EVERY};
    use crate::daemon::server::{run_switch, to_replace};
    use std::sync::Mutex;
    use std::time::Instant;

    /// Машина после перезагрузки: какие службы туннелей есть, какие из них поднимутся сами и через сколько опросов.
    #[derive(Default)]
    struct Boot {
        running: Mutex<Vec<String>>,
        services: Mutex<Vec<String>>,
        /// (туннель, через сколько опросов `running` его служба поднимется сама).
        coming: Mutex<Vec<(String, u32)>>,
        calls: Mutex<Vec<String>>,
    }

    impl Boot {
        fn with_services(services: &[&str]) -> Boot {
            let boot = Boot::default();
            *lock(&boot.services) = services.iter().map(|s| s.to_string()).collect();
            boot
        }
    }

    impl TunnelHost for Boot {
        fn configs(&self) -> std::io::Result<Vec<String>> {
            Ok(vec![])
        }
        fn running(&self) -> std::io::Result<Vec<String>> {
            let mut coming = lock(&self.coming);
            for (t, polls) in coming.iter_mut() {
                if *polls == 0 {
                    lock(&self.running).push(t.clone());
                }
                *polls = polls.saturating_sub(1);
            }
            coming.retain(|(t, _)| !lock(&self.running).contains(t));
            Ok(lock(&self.running).clone())
        }
        fn query(&self, tunnel: &str) -> std::io::Result<crate::uapi::Status> {
            Err(std::io::Error::other(tunnel.to_string()))
        }
        fn connect(&self, tunnel: &str) -> Result<(), String> {
            lock(&self.calls).push(format!("up {tunnel}"));
            lock(&self.running).push(tunnel.into());
            lock(&self.services).push(tunnel.into());
            Ok(())
        }
        fn disconnect(&self, tunnel: &str) -> Result<(), String> {
            lock(&self.calls).push(format!("down {tunnel}"));
            lock(&self.running).retain(|n| n != tunnel);
            lock(&self.services).retain(|n| n != tunnel);
            Ok(())
        }
        fn service_exists(&self, tunnel: &str) -> bool {
            lock(&self.services).iter().any(|s| s == tunnel)
        }
    }

    /// Первые секунды после запуска ядра: такты надзора, подключение — обычным переключением (`to_replace` +
    /// `run_switch`), как в `Core::supervise_tick`. Исход каждого подключения и секунда, на которой оно было.
    fn restore_timed(host: &Boot, desired: &[&str], multiple: bool, fp: impl Fn(&str) -> Option<crate::conf::Footprint>) -> Vec<(String, u64, Result<(), String>)> {
        let desired: Vec<String> = desired.iter().map(|s| s.to_string()).collect();
        let connect = |t: &str| {
            let running = host.running().map_err(|e| e.to_string())?;
            if running.iter().any(|r| r == t) {
                return Ok(false);
            }
            let others = to_replace(t, Plan::Connect, multiple, &running, &fp);
            run_switch(host, t, Plan::Connect, &others).map(|()| true)
        };
        let t0 = Instant::now();
        let mut retries = Retries::default();
        let mut log = Vec::new();
        for s in 0..=FAST_EVERY.as_secs() {
            let now = t0 + Duration::from_secs(s);
            let running = host.running().unwrap();
            let seen = Seen {
                desired: &desired,
                running: Some(&running),
                pending: &|_| false,
                service_exists: &|t| host.service_exists(t),
                stop_reason: &|_| None,
                native_services: false,
                network_changed: false,
            };
            for t in retries.tick(now, &seen).due {
                let result = connect(&t);
                match &result {
                    Ok(true) => log.push((t.clone(), s, Ok(()))),
                    Ok(false) => {}
                    Err(e) => log.push((t.clone(), s, Err(e.clone()))),
                }
                retries.outcome(&t, now, result);
            }
        }
        log
    }

    fn restore_on(host: &Boot, desired: &[&str], multiple: bool, fp: impl Fn(&str) -> Option<crate::conf::Footprint>) -> Vec<(String, Result<(), String>)> {
        restore_timed(host, desired, multiple, fp).into_iter().map(|(t, _, r)| (t, r)).collect()
    }

    #[test]
    fn service_that_comes_up_by_itself_is_not_touched() {
        let host = Boot::with_services(&["office"]);
        lock(&host.coming).push(("office".into(), 3));
        assert!(restore_on(&host, &["office"], false, |_| None).is_empty());
        assert!(lock(&host.calls).is_empty(), "служба поднялась сама — ядро её не пересоздаёт");
    }

    #[test]
    fn service_that_failed_at_boot_is_brought_up_after_the_wait() {
        // Служба есть, но движок упал при загрузке («Element not found») и сам не поднялся.
        let host = Boot::with_services(&["office"]);
        let log = restore_timed(&host, &["office"], false, |_| None);
        assert_eq!(log, [("office".to_string(), FAST_EVERY.as_secs(), Ok(()))], "сначала ждём: Windows как раз может поднимать службу");
        assert_eq!(*lock(&host.calls), ["up office"]);
    }

    #[test]
    fn missing_service_is_recreated_without_waiting() {
        let host = Boot::default();
        let log = restore_timed(&host, &["office"], false, |_| None);
        assert_eq!(log, [("office".to_string(), 0, Ok(()))], "службы нет — ждать нечего");
        assert_eq!(*lock(&host.running), ["office"]);
    }

    #[test]
    fn running_tunnel_and_not_desired_ones_are_left_alone() {
        let host = Boot::with_services(&["office", "home"]);
        *lock(&host.running) = vec!["office".into()];
        assert!(restore_on(&host, &["office"], false, |_| None).is_empty());
        assert!(lock(&host.calls).is_empty(), "работающий не переподключается; «home» пользователь не оставлял подключённым");
        assert!(restore_on(&host, &[], false, |_| None).is_empty());
    }

    #[test]
    fn user_disconnect_removes_the_tunnel_from_the_desired_set() {
        let desired = after_switch(&[], "office", Plan::Connect, &[]);
        assert_eq!(desired, ["office"]);
        let desired = after_switch(&desired, "home", Plan::Connect, &["office".into()]);
        assert_eq!(desired, ["home"], "заменённый при подключении выходит из набора");
        let desired = after_switch(&desired, "lab", Plan::Connect, &[]);
        assert_eq!(after_switch(&desired, "lab", Plan::Reconnect, &[]), ["home", "lab"], "переподключение не дублирует");
        let desired = after_switch(&desired, "home", Plan::Disconnect, &[]);
        assert_eq!(desired, ["lab"]);

        // После перезагрузки отключённый пользователем не поднимается, даже если его служба осталась.
        let host = Boot::with_services(&["home"]);
        let desired: Vec<&str> = desired.iter().map(String::as_str).collect();
        let log = restore_on(&host, &desired, true, |_| None);
        assert_eq!(log, [("lab".to_string(), Ok(()))]);
        assert_eq!(*lock(&host.calls), ["up lab"]);
    }

    #[test]
    fn conflicting_desired_tunnels_follow_the_switch_rules() {
        let full = |a: &str| Some(crate::conf::Footprint::of(&crate::conf::parse(&format!("[Interface]\nAddress = {a}\n[Peer]\nAllowedIPs = 0.0.0.0/0\n"))));
        let lan = |a: &str| Some(crate::conf::Footprint::of(&crate::conf::parse(&format!("[Interface]\nAddress = {a}\n[Peer]\nAllowedIPs = 10.9.0.0/24\n"))));
        let fp = |n: &str| match n {
            "a" => full("10.255.254.2/32"),
            "b" => full("10.255.253.2/32"),
            "lan" => lan("10.9.0.2/32"),
            _ => None,
        };
        // Оба на весь трафик вместе не работают: второй заменяет первого, как при нажатии кнопки. Надзор идёт по
        // именам в порядке сортировки: «a», «b», «lan».
        let host = Boot::default();
        let log = restore_on(&host, &["a", "lan", "b"], true, fp);
        assert!(log.iter().all(|(_, r)| r.is_ok()), "{log:?}");
        assert_eq!(*lock(&host.running), ["b", "lan"]);
        assert_eq!(*lock(&host.calls), ["up a", "down a", "up b", "up lan"]);

        // Один туннель за раз: остаётся последний.
        let host = Boot::default();
        restore_on(&host, &["a", "lan"], false, fp);
        assert_eq!(*lock(&host.running), ["lan"]);
    }

    /// Пропало питание: набор записан в момент подключения, ядро не останавливалось и ничего не дописывало. Новый
    /// запуск читает `core.ini` с диска и поднимает туннель, чья служба при загрузке упала. Испорченный файл — ничего
    /// не поднимается (имена из мусора не берутся).
    #[test]
    fn power_loss_after_connect_brings_the_tunnel_back_on_next_start() {
        use crate::daemon::Config;
        let dir = std::env::temp_dir().join(format!("awg-restore-crash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("core.ini");

        // Работа до сбоя: пользователь подключил туннель, ядро записало набор; остановки не было.
        let (mut before, _) = Config::load_guarded_from(&path);
        before.mode = crate::settings::Mode::Engine;
        before.tunnels = Some(after_switch(&[], "home.full.v4", Plan::Connect, &[]));
        before.save_to(&path).unwrap();

        // Новый запуск ядра после загрузки: служба есть, но движок упал («Element not found»).
        let (after, problem) = Config::load_guarded_from(&path);
        assert!(problem.is_none());
        let host = Boot::with_services(&["home.full.v4"]);
        let desired: Vec<&str> = after.tunnels.iter().flatten().map(String::as_str).collect();
        let log = restore_on(&host, &desired, after.multiple, |_| None);
        assert_eq!(log, [("home.full.v4".to_string(), Ok(()))]);
        assert_eq!(*lock(&host.running), ["home.full.v4"]);

        // Файл испорчен (не UTF-8): отодвигается, набор неизвестен — подключать нечего.
        std::fs::write(&path, [b't', b'u', b'n', 0xff, 0xfe, b'\n']).unwrap();
        let (broken, problem) = Config::load_guarded_from(&path);
        assert!(problem.is_some(), "испорченный core.ini — запись в журнал (`log_unreadable`)");
        assert_eq!(broken.tunnels, None);
        // Обрезанный до нуля — тоже ничего.
        std::fs::write(&path, b"").unwrap();
        assert_eq!(Config::load_guarded_from(&path).0.tunnels, None);
        // Мусор в значении — только допустимые имена (дальше подключение несуществующего даст ошибку в журнале).
        std::fs::write(&path, "[core]\r\ntunnels=..\\..\\x,a b,\r\n").unwrap();
        assert_eq!(Config::load_guarded_from(&path).0.tunnels, Some(vec![]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Обновление с ядра без набора: ключа нет — набором становятся работающие и переживают запись на диск; явно
    /// пустой набор (пользователь всё отключил) остаётся пустым.
    #[test]
    fn upgrade_without_a_desired_set_adopts_running_tunnels_but_empty_stays_empty() {
        use crate::daemon::Config;
        let dir = std::env::temp_dir().join(format!("awg-restore-adopt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("core.ini");

        std::fs::write(&path, "[core]\r\nmode=engine\r\nowner_sid=S-1-5-21-1\r\n").unwrap();
        let (mut old, _) = Config::load_guarded_from(&path);
        assert_eq!(old.tunnels, None, "ключа нет — набор неизвестен");
        assert_eq!(adopt(&mut old, vec!["opt".into()]), Some(vec!["opt".to_string()]));
        assert!(!old.multiple);
        old.save_to(&path).unwrap();
        let (reloaded, _) = Config::load_guarded_from(&path);
        assert_eq!(reloaded.tunnels, Some(vec!["opt".to_string()]), "принятый набор записан");
        let mut again = reloaded.clone();
        assert_eq!(adopt(&mut again, vec!["other".into()]), None, "принимается один раз");
        assert_eq!(again, reloaded);

        let mut two = Config::load_guarded_from(&dir.join("absent.ini")).0;
        adopt(&mut two, vec!["a".into(), "b".into()]);
        assert!(two.multiple, "работают два — значит, было «несколько туннелей»");

        std::fs::write(&path, "[core]\r\nmode=engine\r\ntunnels=\r\n").unwrap();
        let (mut empty, _) = Config::load_guarded_from(&path);
        assert_eq!(empty.tunnels, Some(vec![]));
        assert_eq!(adopt(&mut empty, vec!["opt".into()]), None, "явно пустой набор не заполняется работающими");
        assert_eq!(empty.tunnels, Some(vec![]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_restore_is_reported_per_tunnel() {
        // Ошибка одного не мешает другим: «a» без конфига, «b» поднимается.
        struct Half(Boot);
        impl TunnelHost for Half {
            fn configs(&self) -> std::io::Result<Vec<String>> {
                self.0.configs()
            }
            fn running(&self) -> std::io::Result<Vec<String>> {
                self.0.running()
            }
            fn query(&self, t: &str) -> std::io::Result<crate::uapi::Status> {
                self.0.query(t)
            }
            fn connect(&self, t: &str) -> Result<(), String> {
                if t == "a" { Err("no config".into()) } else { self.0.connect(t) }
            }
            fn disconnect(&self, t: &str) -> Result<(), String> {
                self.0.disconnect(t)
            }
            fn service_exists(&self, t: &str) -> bool {
                self.0.service_exists(t)
            }
        }
        let host = Half(Boot::default());
        let desired = vec!["a".to_string(), "b".to_string()];
        let mut retries = Retries::default();
        let now = Instant::now();
        let running = host.running().unwrap();
        let seen = Seen { desired: &desired, running: Some(&running), pending: &|_| false, service_exists: &|t| host.service_exists(t), stop_reason: &|_| None, native_services: false, network_changed: false };
        let mut log = Vec::new();
        for t in retries.tick(now, &seen).due {
            let result = run_switch(&host, &t, Plan::Connect, &[]).map(|()| true);
            log.push((t.clone(), result.clone().map(drop)));
            retries.outcome(&t, now, result);
        }
        assert_eq!(log, [("a".to_string(), Err("no config".to_string())), ("b".to_string(), Ok(()))]);
        assert_eq!(retries.view(now)["a"].attempt, 1, "«a» остаётся под надзором");
        // «b» поднят и снимается с надзора, проработав `CONFIRM_FOR`.
        let running = host.running().unwrap();
        let later = now + crate::daemon::retry::CONFIRM_FOR;
        for at in [now, later] {
            let seen = Seen { running: Some(&running), ..seen };
            let notes = retries.tick(at, &seen).notes;
            assert_eq!(notes.is_empty(), at == now, "{notes:?}");
        }
        assert!(!retries.view(later).contains_key("b"));
    }
}
