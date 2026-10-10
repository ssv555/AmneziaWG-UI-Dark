//! Управление туннелями: штатный `amneziawg.exe` (службы) и UAPI-каналы (состояние) — режим 1;
//! свой движок и хранилище — режим 2. Оба работают в ядре; окно говорит с ядром через `daemon::CoreApi`.

use std::collections::BTreeSet;
use std::io;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::crash::lock;
use crate::uapi::{self, Peer, Status};

const CONFIG_EXT: &str = ".conf.dpapi";
pub const MANAGER_SERVICE: &str = "AmneziaWGManager";

/// Где живут туннели и как их включать: режим 1 (`Real`) и режим 2 (`EngineHost`) — в ядре, `Demo` — выдуманные
/// туннели демо-режима. У окна с ядром хоста нет вовсе: туннели ведёт ядро (`monitor::Shared::host`).
pub trait TunnelHost: Send + Sync {
    fn configs(&self) -> io::Result<Vec<String>>;
    fn running(&self) -> io::Result<Vec<String>>;
    fn query(&self, tunnel: &str) -> io::Result<Status>;
    fn connect(&self, tunnel: &str) -> Result<(), String>;
    fn disconnect(&self, tunnel: &str) -> Result<(), String>;

    /// Туннели — службы установленного AmneziaWG: они пропадают при его удалении (переустановка из обновлений).
    fn native_services(&self) -> bool {
        false
    }

    /// Служба туннеля есть (работает она или нет): после перезапуска ядра её стоит подождать — Windows поднимает
    /// службы туннелей сама. Не знаем — «есть»: подождать дольше лучше, чем пересоздать поднимающуюся службу.
    fn service_exists(&self, _tunnel: &str) -> bool {
        true
    }

    /// Почему служба туннеля остановилась (коды завершения), для журнала надзора. `None` — не знаем.
    fn stop_reason(&self, _tunnel: &str) -> Option<String> {
        None
    }

    /// Идёт установщик Windows: чужой MSI AmneziaWG убирает службы туннелей режима 1, и надзор не должен ни выводить
    /// их из набора, ни ставить заново посреди установки. Только у режима 1 (`Real`); остальным не нужно.
    fn installer_running(&self) -> bool {
        false
    }

    /// Задержка до узла, мс (пинг «трафик проходит»).
    fn ping_ms(&self, host: &str) -> Result<u32, String> {
        crate::ping::measure(host)
    }
}

/// Режим 1 (в ядре): туннели установленного AmneziaWG.
impl TunnelHost for Real {
    fn configs(&self) -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(&self.config_dir)? {
            let file = entry?.file_name().to_string_lossy().into_owned();
            if let Some(name) = file.strip_suffix(CONFIG_EXT) {
                names.push(name.to_string());
            }
        }
        Ok(names)
    }

    fn running(&self) -> io::Result<Vec<String>> {
        running_pipes()
    }

    fn query(&self, tunnel: &str) -> io::Result<Status> {
        uapi::query(tunnel)
    }

    fn connect(&self, tunnel: &str) -> Result<(), String> {
        self.run(&["/installtunnelservice", &self.config_path(tunnel)])
    }

    fn disconnect(&self, tunnel: &str) -> Result<(), String> {
        self.run(&["/uninstalltunnelservice", tunnel])
    }

    fn native_services(&self) -> bool {
        true
    }

    fn service_exists(&self, tunnel: &str) -> bool {
        crate::win::service_command(&format!("{NATIVE_TUNNEL_SERVICE}{tunnel}")).is_some()
    }

    fn installer_running(&self) -> bool {
        crate::win::installer_running()
    }
}

/// Имя службы туннеля AmneziaWG — префикс и имя туннеля (`services/names.go` amneziawg-windows).
const NATIVE_TUNNEL_SERVICE: &str = "AmneziaWGTunnel$";

/// Режим 2 (в ядре): свой движок (`tunnel.dll` в службе) и своё хранилище конфигов.
pub struct EngineHost;

impl EngineHost {
    /// Удалить туннель из хранилища (после подтверждения пользователя). Имя занято чужой службой Windows —
    /// останавливать нечего, просто удаляем файл.
    pub fn delete_tunnel(&self, tunnel: &str) -> Result<(), String> {
        if !crate::engine::is_foreign(tunnel) {
            crate::engine::disconnect(tunnel)?;
        }
        crate::store::remove(tunnel)
    }
}

impl TunnelHost for EngineHost {
    fn configs(&self) -> io::Result<Vec<String>> {
        crate::store::list()
    }

    // Каналы общие с оригиналом: туннель с тем же именем мог поднять он. Наш — только если работает наша служба.
    fn running(&self) -> io::Result<Vec<String>> {
        let own = crate::store::list()?;
        Ok(running_pipes()?.into_iter().filter(|n| own.contains(n) && crate::engine::is_running(n)).collect())
    }

    fn query(&self, tunnel: &str) -> io::Result<Status> {
        uapi::query(tunnel)
    }

    fn connect(&self, tunnel: &str) -> Result<(), String> {
        crate::engine::connect(&crate::store::path(tunnel))
    }

    fn disconnect(&self, tunnel: &str) -> Result<(), String> {
        crate::engine::disconnect(tunnel)
    }

    fn service_exists(&self, tunnel: &str) -> bool {
        crate::engine::has_service(tunnel)
    }

    fn stop_reason(&self, tunnel: &str) -> Option<String> {
        crate::engine::stop_reason(tunnel)
    }
}

/// Выдуманные туннели — проверка интерфейса на машине без AmneziaWG.
impl TunnelHost for Demo {
    fn configs(&self) -> io::Result<Vec<String>> {
        Ok(self.names.clone())
    }

    fn running(&self) -> io::Result<Vec<String>> {
        Ok(lock(&self.running).iter().cloned().collect())
    }

    fn query(&self, tunnel: &str) -> io::Result<Status> {
        Ok(self.status(tunnel))
    }

    fn connect(&self, tunnel: &str) -> Result<(), String> {
        lock(&self.running).insert(tunnel.to_string());
        Ok(())
    }

    fn disconnect(&self, tunnel: &str) -> Result<(), String> {
        lock(&self.running).remove(tunnel);
        Ok(())
    }

    /// Выдуманная задержка.
    fn ping_ms(&self, _host: &str) -> Result<u32, String> {
        let t = self.started.elapsed().as_secs_f64();
        Ok((42.0 + 6.0 * (t / 23.0).sin() + 3.0 * (t / 7.0).sin()) as u32)
    }
}


#[derive(Clone)]
pub struct Real {
    exe: PathBuf,
    config_dir: PathBuf,
}

impl Real {
    /// Папка AmneziaWG — из настроек службы менеджера; служба не найдена — стандартная в Program Files.
    pub fn new() -> Self {
        let exe = native_exe();
        let root = exe.parent().map(PathBuf::from).unwrap_or_default();
        Real { config_dir: root.join("Data").join("Configurations"), exe }
    }

    /// Родное окно: выделить туннель и нажать «Edit».
    pub fn edit_in_native(&self, tunnel: &str) -> Result<(), String> {
        crate::native::edit(&self.exe, tunnel)
    }

    /// Родное окно: импорт туннеля из файла (с `file` — диалог сразу на нём).
    pub fn import_in_native(&self, file: Option<&std::path::Path>) -> Result<(), String> {
        crate::native::import(&self.exe, file)
    }

    fn config_path(&self, tunnel: &str) -> String {
        self.config_dir.join(format!("{tunnel}{CONFIG_EXT}")).to_string_lossy().into_owned()
    }

    fn run(&self, args: &[&str]) -> Result<(), String> {
        let mut cmd = Command::new(&self.exe);
        cmd.args(args);
        let out = run_with_deadline(&mut cmd, RUN_TIMEOUT).map_err(|e| crate::fsutil::io_ctx(&self.exe, e))?;
        if out.status.success() {
            return Ok(());
        }
        let text = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(format!("amneziawg.exe {}: exit code {} {text}", args[0], out.status.code().unwrap_or(-1)))
    }
}

/// Сколько ждём `amneziawg.exe /installtunnelservice|/uninstalltunnelservice`: он ждёт SCM, а тот может застрять,
/// пока соседняя служба в START_PENDING; без предела `switching` держался бы вечно и ядро отвечало бы `busy` всем.
const RUN_TIMEOUT: Duration = Duration::from_secs(60);

/// Сколько после выхода процесса ждём конец его stderr: канал мог унаследовать потомок, тогда EOF не придёт.
const STDERR_GRACE: Duration = Duration::from_secs(2);

/// `Command::output()` с пределом ожидания: по истечении процесс убивается, это ошибка.
fn run_with_deadline(cmd: &mut Command, timeout: Duration) -> io::Result<std::process::Output> {
    run_with_deadline_grace(cmd, timeout, STDERR_GRACE)
}

fn run_with_deadline_grace(cmd: &mut Command, timeout: Duration, grace: Duration) -> io::Result<std::process::Output> {
    use std::io::Read;
    use std::process::Stdio;
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn()?;
    // stderr читаем в потоке, иначе полный буфер канала остановил бы процесс до таймаута.
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let (sent, received) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new().name("run-stderr".into()).spawn(move || {
        let mut buf = Vec::new();
        // Текст stderr — только пояснение к коду выхода; не прочитался — уйдёт то, что успели получить.
        let _ = stderr.read_to_end(&mut buf);
        let _ = sent.send(buf); // получатель мог уйти по `grace`
    });
    if let Err(e) = spawned {
        let _ = child.kill();
        let _ = child.wait();
        return Err(io::Error::new(e.kind(), format!("cannot start the stderr reader: {e}")));
    }
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() >= timeout {
            let killed = child.kill();
            let _ = child.wait(); // читатель stderr не ждём: внук мог унаследовать канал
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("no answer in {} s, the process was killed{}", timeout.as_secs_f32(), killed.err().map(|e| format!(" ({e})")).unwrap_or_default()),
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    // Процесс вышел, но канал может держать оставшийся потомок: без предела ожидание вернуло бы A9 (`switching` занят навсегда).
    // Вместо текста — пометка: вызывающий показывает stderr при ненулевом коде, так потеря пояснения видна в журнале.
    let stderr = received.recv_timeout(grace).unwrap_or_else(|_| {
        format!("(stderr was not closed within {} s after the process exited; its text is lost)", grace.as_secs_f32()).into_bytes()
    });
    Ok(std::process::Output { status, stdout: Vec::new(), stderr })
}

/// Запущенный туннель = существующий UAPI-канал его службы.
fn running_pipes() -> io::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(uapi::PIPE_ROOT)? {
        let file = entry?.file_name().to_string_lossy().into_owned();
        if let Some(name) = file.strip_prefix(uapi::PIPE_PREFIX) {
            names.push(name.to_string());
        }
    }
    Ok(names)
}

pub struct Demo {
    names: Vec<String>,
    running: Mutex<BTreeSet<String>>,
    started: Instant,
}

impl Demo {
    pub fn new() -> Self {
        // Выдуманные имена и адреса (TEST-NET-3, RFC 5737), ключи — из примера в man wg(8).
        let names = [
            "home.nl-ams.full", "home.nl-ams.split", "home.de-fra.full", "home.de-fra.split",
            "office.gw-primary", "office.gw-reserve", "travel.fi-hel.v4", "travel.fi-hel.v6",
            "travel.se-sto.v4", "travel.se-sto.v6", "lab.us-nyc.dual", "lab.us-nyc.v4", "lab.sg.v4", "lab.sg.v6",
        ];
        Demo {
            names: names.iter().map(|s| s.to_string()).collect(),
            running: Mutex::new(BTreeSet::from(["home.nl-ams.full".to_string()])),
            started: Instant::now(),
        }
    }

    pub fn names(&self) -> &[String] {
        &self.names
    }

    fn status(&self, tunnel: &str) -> Status {
        let t = self.started.elapsed().as_secs_f64();
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        Status {
            public_key: "xTIBA5rboUvnH4htodjb6e697QjLERt1NAB4mZqp8Dg=".into(),
            listen_port: 51820,
            awg_params: [("jc", "4"), ("jmin", "50"), ("jmax", "1000"), ("s1", "30"), ("s2", "90")]
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            peers: vec![Peer {
                public_key: "TrMvSoP4jYQlY6RIzBgbssQqY3vxI2Pi+y71lOWWXX0=".into(),
                endpoint: format!("203.0.113.{}:51820", 10 + tunnel.len()),
                last_handshake_sec: now - (t as u64 % 120),
                rx_bytes: 2_065_694 + demo_bytes(t, 60_000.0, 0.0),
                tx_bytes: 1_153_433 + demo_bytes(t, 14_000.0, 2.1),
                keepalive: 25,
                allowed_ips: (0..180).map(|i| format!("{}.{}.0.0/{}", 3 + i % 200, i * 7 % 255, 12 + i % 12)).collect(),
            }],
        }
    }

    /// Выдуманные сведения о туннеле (панель неподключённого).
    pub fn details(&self, tunnel: &str) -> crate::conf::TunnelInfo {
        crate::conf::parse(&format!(
            "[Interface]\nAddress = 10.8.0.{}/32\nDNS = 1.1.1.1\nMTU = 1280\nJc = 4\n[Peer]\nPublicKey = TrMvSoP4jYQlY6RIzBgbssQqY3vxI2Pi+y71lOWWXX0=\nEndpoint = 203.0.113.{}:51820\nAllowedIPs = 0.0.0.0/0\nPersistentKeepalive = 25\n",
            tunnel.len(),
            tunnel.len()
        ))
    }

    /// Выдуманный текст конфига.
    pub fn config_text(&self, tunnel: &str) -> String {
        format!("[Interface]\r\n# {tunnel}\r\n")
    }
}

/// Счётчик байт демо-туннеля: интеграл скорости `rate·(1 + 0.55·sin(t/9) + 0.35·sin(t/2.7))`.
/// Скорость всегда положительна, поэтому счётчик только растёт, а график похож на живой трафик.
fn demo_bytes(t: f64, rate: f64, phase: f64) -> u64 {
    let integral = |t: f64| t - 0.55 * 9.0 * (t / 9.0 + phase).cos() - 0.35 * 2.7 * (t / 2.7 + phase).cos();
    (rate * (integral(t) - integral(0.0))) as u64
}

/// amneziawg.exe — из настроек службы менеджера; служба не найдена — стандартный путь в Program Files.
pub fn native_exe() -> PathBuf {
    // Не из переменной окружения: её пользователь может подменить, а помощник ядра запускает этот exe
    // с правами администратора.
    crate::win::service_binary(MANAGER_SERVICE).unwrap_or_else(|| crate::win::program_files().join("AmneziaWG").join("amneziawg.exe"))
}

#[cfg(test)]
mod tests {
    /// Паника потока с замком демо-туннелей не роняет следующих: список восстанавливается, команды идут дальше.
    #[test]
    fn demo_survives_a_poisoned_lock() {
        use super::TunnelHost;
        let demo = std::sync::Arc::new(super::Demo::new());
        let d = demo.clone();
        let _ = std::thread::spawn(move || {
            let _g = d.running.lock().unwrap();
            panic!("poison it");
        })
        .join();
        assert!(demo.running.is_poisoned());
        assert_eq!(demo.running().unwrap(), ["home.nl-ams.full"]);
        demo.connect("lab.sg.v4").unwrap();
        assert_eq!(demo.running().unwrap(), ["home.nl-ams.full", "lab.sg.v4"]);
        assert!(!demo.running.is_poisoned());
    }

    #[test]
    fn hung_process_is_killed_at_the_deadline() {
        let started = std::time::Instant::now();
        let mut cmd = std::process::Command::new("ping");
        cmd.args(["-n", "30", "127.0.0.1"]);
        let err = super::run_with_deadline(&mut cmd, std::time::Duration::from_millis(300)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut, "{err}");
        assert!(started.elapsed() < std::time::Duration::from_secs(10), "ждали процесс, а не предел");
    }

    #[test]
    fn leftover_child_holding_stderr_does_not_block_after_the_process_exits() {
        // `start /b` запускает ping с унаследованным stderr и сразу выходит: канал остаётся открытым у потомка.
        let started = std::time::Instant::now();
        let mut cmd = std::process::Command::new("cmd");
        cmd.args(["/c", "start /b ping -n 8 127.0.0.1 >nul & exit 0"]);
        let out = super::run_with_deadline_grace(&mut cmd, std::time::Duration::from_secs(30), std::time::Duration::from_millis(300)).unwrap();
        assert!(out.status.success());
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "ждали потомка, а не предел: {:?}", started.elapsed());
    }

    #[test]
    fn finished_process_returns_exit_code_and_stderr() {
        let mut cmd = std::process::Command::new("cmd");
        cmd.args(["/c", "echo boom 1>&2 & exit 3"]);
        let out = super::run_with_deadline(&mut cmd, std::time::Duration::from_secs(30)).unwrap();
        assert_eq!(out.status.code(), Some(3));
        assert!(String::from_utf8_lossy(&out.stderr).contains("boom"));
    }

    #[test]
    fn demo_counter_only_grows() {
        let mut prev = 0;
        for i in 1..3000 {
            let now = super::demo_bytes(i as f64 * 0.25, 60_000.0, 2.1);
            assert!(now > prev, "t={}", i as f64 * 0.25);
            prev = now;
        }
    }

    #[test]
    fn lists_pipes_on_windows() {
        // Каналы есть в любой работающей Windows; ошибка здесь = неверный путь к пространству каналов.
        let all = std::fs::read_dir(crate::uapi::PIPE_ROOT).unwrap().count();
        assert!(all > 0);
        super::running_pipes().unwrap();
    }
}
