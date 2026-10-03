//! Управление туннелями: штатный `amneziawg.exe` (службы) и UAPI-каналы (состояние).
//! Оригинальный клиент не меняется: используем те же команды, что и его менеджер.

use std::collections::BTreeSet;
use std::io;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::uapi::{self, Peer, Status};

const CONFIG_EXT: &str = ".conf.dpapi";
pub const MANAGER_SERVICE: &str = "AmneziaWGManager";

#[derive(Clone)]
pub enum Backend {
    Real(Real),
    /// Выдуманные туннели — проверка интерфейса на машине без AmneziaWG.
    Demo(Arc<Demo>),
}

impl Backend {
    pub fn configs(&self) -> io::Result<Vec<String>> {
        match self {
            Backend::Real(r) => r.configs(),
            Backend::Demo(d) => Ok(d.names.clone()),
        }
    }

    pub fn running(&self) -> io::Result<Vec<String>> {
        match self {
            Backend::Real(_) => running_pipes(),
            Backend::Demo(d) => Ok(d.running.lock().unwrap().iter().cloned().collect()),
        }
    }

    pub fn query(&self, tunnel: &str) -> io::Result<Status> {
        match self {
            Backend::Real(_) => uapi::query(tunnel),
            Backend::Demo(d) => Ok(d.status(tunnel)),
        }
    }

    pub fn connect(&self, tunnel: &str) -> Result<(), String> {
        match self {
            Backend::Real(r) => r.run(&["/installtunnelservice", &r.config_path(tunnel)]),
            Backend::Demo(d) => {
                d.running.lock().unwrap().insert(tunnel.to_string());
                Ok(())
            }
        }
    }

    pub fn disconnect(&self, tunnel: &str) -> Result<(), String> {
        match self {
            Backend::Real(r) => r.run(&["/uninstalltunnelservice", tunnel]),
            Backend::Demo(d) => {
                d.running.lock().unwrap().remove(tunnel);
                Ok(())
            }
        }
    }

    /// Открыть родное окно AmneziaWG (импорт и правка конфигов остаются там).
    pub fn open_original(&self) -> Result<(), String> {
        match self {
            Backend::Real(r) => Command::new(&r.exe).spawn().map(drop).map_err(|e| format!("{}: {e}", r.exe.display())),
            Backend::Demo(_) => Ok(()),
        }
    }

    /// Родное окно: выделить туннель и нажать «Edit».
    pub fn edit_in_native(&self, tunnel: &str) -> Result<(), String> {
        match self {
            Backend::Real(r) => crate::native::edit(&r.exe, tunnel),
            Backend::Demo(_) => Ok(()),
        }
    }

    /// Удалить туннель в родном клиенте (после подтверждения пользователя).
    pub fn delete_tunnel(&self, tunnel: &str) -> Result<(), String> {
        match self {
            Backend::Real(r) => crate::native::delete(&r.exe, tunnel),
            Backend::Demo(d) => {
                d.running.lock().unwrap().remove(tunnel);
                Ok(())
            }
        }
    }

    /// Сведения о неподключённом туннеле из панели родного окна.
    pub fn native_details(&self, tunnel: &str) -> Result<crate::conf::TunnelInfo, String> {
        match self {
            Backend::Real(r) => crate::native::read_details(&r.exe, tunnel),
            Backend::Demo(_) => Ok(crate::conf::parse(&format!(
                "[Interface]\nAddress = 10.8.0.{}/32\nDNS = 1.1.1.1\nMTU = 1280\nJc = 4\n[Peer]\nPublicKey = TrMvSoP4jYQlY6RIzBgbssQqY3vxI2Pi+y71lOWWXX0=\nEndpoint = 203.0.113.{}:51820\nAllowedIPs = 0.0.0.0/0\nPersistentKeepalive = 25\n",
                tunnel.len(),
                tunnel.len()
            ))),
        }
    }

    /// Текст конфига туннеля из родного редактора (редактор закрывается без изменений).
    pub fn read_native_config(&self, tunnel: &str) -> Result<String, String> {
        match self {
            Backend::Real(r) => crate::native::read_config(&r.exe, tunnel),
            Backend::Demo(_) => Ok(format!("[Interface]\r\n# {tunnel}\r\n")),
        }
    }

    /// Записать конфиг туннеля через родной редактор (Save нажимается автоматически).
    pub fn write_native_config(&self, tunnel: &str, text: &str) -> Result<(), String> {
        match self {
            Backend::Real(r) => crate::native::write_config(&r.exe, tunnel, text),
            Backend::Demo(_) => Ok(()),
        }
    }

    /// Родное окно: импорт туннеля из файла (с `file` — диалог сразу на нём).
    pub fn import_in_native(&self, file: Option<&std::path::Path>) -> Result<(), String> {
        match self {
            Backend::Real(r) => crate::native::import(&r.exe, file),
            Backend::Demo(_) => Ok(()),
        }
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

    fn config_path(&self, tunnel: &str) -> String {
        self.config_dir.join(format!("{tunnel}{CONFIG_EXT}")).to_string_lossy().into_owned()
    }

    fn run(&self, args: &[&str]) -> Result<(), String> {
        let out = Command::new(&self.exe).args(args).output().map_err(|e| format!("{}: {e}", self.exe.display()))?;
        if out.status.success() {
            return Ok(());
        }
        let text = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(format!("amneziawg.exe {}: exit code {} {text}", args[0], out.status.code().unwrap_or(-1)))
    }
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

#[cfg(test)]
mod tests {
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

    /// Выдуманная задержка пинга, мс.
    pub fn ping_ms(&self) -> u32 {
        let t = self.started.elapsed().as_secs_f64();
        (42.0 + 6.0 * (t / 23.0).sin() + 3.0 * (t / 7.0).sin()) as u32
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
    crate::win::service_binary(MANAGER_SERVICE).unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("ProgramFiles").unwrap_or_else(|| r"C:\Program Files".into()))
            .join("AmneziaWG")
            .join("amneziawg.exe")
    })
}
