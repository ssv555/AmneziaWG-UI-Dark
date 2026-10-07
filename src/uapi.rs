//! Состояние туннеля по UAPI-протоколу WireGuard: именованный канал службы туннеля, запрос `get=1`.

use std::collections::BTreeSet;
use std::io::{self, BufRead, BufReader, Write};
use std::sync::{mpsc, Mutex};
use std::time::Duration;

pub const PIPE_ROOT: &str = r"\\.\pipe\";
pub const PIPE_PREFIX: &str = r"ProtectedPrefix\Administrators\AmneziaWG\";

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Peer {
    pub public_key: String,
    pub endpoint: String,
    pub last_handshake_sec: u64,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub keepalive: u32,
    pub allowed_ips: Vec<String>,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Status {
    pub public_key: String,
    pub listen_port: u16,
    /// Параметры обфускации AmneziaWG (jc, jmin, s1, h1 …) в порядке ответа.
    pub awg_params: Vec<(String, String)>,
    pub peers: Vec<Peer>,
}

impl Status {
    pub fn rx_bytes(&self) -> u64 {
        self.peers.iter().map(|p| p.rx_bytes).sum()
    }
    pub fn tx_bytes(&self) -> u64 {
        self.peers.iter().map(|p| p.tx_bytes).sum()
    }
    /// Самое свежее рукопожатие среди пиров, unix-секунды; 0 — рукопожатия не было.
    pub fn last_handshake_sec(&self) -> u64 {
        self.peers.iter().map(|p| p.last_handshake_sec).max().unwrap_or(0)
    }
}

/// Дольше этого служба туннеля на UAPI не отвечает — считаем зависшей (запрос идёт раз в секунду из монитора).
const QUERY_TIMEOUT: Duration = Duration::from_secs(3);

/// Туннели, чей запрос ещё висит в рабочем потоке: новые к ним не шлём, иначе зависшая служба копила бы по потоку в секунду.
static STUCK: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// Снимает имя из `STUCK`, когда рабочий поток закончил (в том числе паникой).
struct Release(String);

impl Drop for Release {
    fn drop(&mut self) {
        crate::crash::lock(&STUCK).remove(&self.0);
    }
}

/// Запуск рабочего потока; подменяется в тесте, чтобы воспроизвести отказ ОС создать поток.
type Spawner = fn(Box<dyn FnOnce() + Send>) -> io::Result<()>;

fn spawn_thread(job: Box<dyn FnOnce() + Send>) -> io::Result<()> {
    // `thread::spawn` при нехватке ресурсов паникует, а запрос идёт из монитора ядра: паника остановила бы службу.
    std::thread::Builder::new().name("uapi-query".into()).spawn(job).map(drop)
}

/// Блокирующее чтение канала UAPI нельзя прервать, поэтому оно идёт в потоке, а вызывающий ждёт не дольше `timeout`:
/// зависшая служба туннеля не должна останавливать опрос всех остальных.
/// Не удалось создать поток — это ошибка именно этого запроса (строка ошибки у туннеля), процесс живёт дальше.
fn with_deadline(tunnel: &str, timeout: Duration, work: impl FnOnce() -> io::Result<Status> + Send + 'static) -> io::Result<Status> {
    with_deadline_using(spawn_thread, tunnel, timeout, work)
}

fn with_deadline_using(
    spawn: Spawner,
    tunnel: &str,
    timeout: Duration,
    work: impl FnOnce() -> io::Result<Status> + Send + 'static,
) -> io::Result<Status> {
    if !crate::crash::lock(&STUCK).insert(tunnel.to_string()) {
        return Err(io::Error::new(io::ErrorKind::TimedOut, format!("UAPI {tunnel}: the previous query is still waiting for an answer")));
    }
    let release = Release(tunnel.to_string());
    let (tx, rx) = mpsc::channel();
    // При отказе spawn замыкание (а с ним `release`) уничтожается, имя снимается с `STUCK` и следующий опрос пройдёт.
    spawn(Box::new(move || {
        let _release = release;
        // Получатель мог уйти по таймауту — результат тогда никому не нужен.
        let _ = tx.send(work());
    }))
    .map_err(|e| io::Error::new(e.kind(), format!("UAPI {tunnel}: cannot start the query thread: {e}")))?;
    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, format!("UAPI {tunnel}: no answer in {} s", timeout.as_secs_f32()))),
    }
}

pub fn query(tunnel: &str) -> io::Result<Status> {
    let name = tunnel.to_string();
    with_deadline(tunnel, QUERY_TIMEOUT, move || query_blocking(&name))
}

fn query_blocking(tunnel: &str) -> io::Result<Status> {
    let path = format!("{PIPE_ROOT}{PIPE_PREFIX}{tunnel}");
    let mut pipe = std::fs::OpenOptions::new().read(true).write(true).open(path)?;
    pipe.write_all(b"get=1\n\n")?;
    // Сервер не закрывает канал после ответа: читаем до строки errno=.
    let mut reader = BufReader::new(pipe);
    let mut lines = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        let done = l.starts_with("errno=");
        lines.push(l.to_string());
        if done {
            break;
        }
    }
    parse(&lines)
}

pub fn parse<S: AsRef<str>>(lines: &[S]) -> io::Result<Status> {
    let mut st = Status::default();
    for line in lines {
        let Some((k, v)) = line.as_ref().split_once('=') else { continue };
        if k == "errno" {
            if v != "0" {
                return Err(io::Error::other(format!("UAPI errno={v}")));
            }
            continue;
        }
        if k == "public_key" {
            st.peers.push(Peer { public_key: base64(&hex(v)), ..Default::default() });
            continue;
        }
        match st.peers.last_mut() {
            Some(p) => match k {
                "endpoint" => p.endpoint = v.to_string(),
                "last_handshake_time_sec" => p.last_handshake_sec = v.parse().unwrap_or(0),
                "rx_bytes" => p.rx_bytes = v.parse().unwrap_or(0),
                "tx_bytes" => p.tx_bytes = v.parse().unwrap_or(0),
                "persistent_keepalive_interval" => p.keepalive = v.parse().unwrap_or(0),
                "allowed_ip" => p.allowed_ips.push(v.to_string()),
                _ => {}
            },
            None => match k {
                "private_key" => st.public_key = public_key(&hex(v)),
                "listen_port" => st.listen_port = v.parse().unwrap_or(0),
                "fwmark" => {}
                _ => st.awg_params.push((k.to_string(), v.to_string())),
            },
        }
    }
    Ok(st)
}

pub fn public_key(private: &[u8]) -> String {
    let Ok(bytes) = <[u8; 32]>::try_from(private) else { return String::new() };
    let secret = x25519_dalek::StaticSecret::from(bytes);
    base64(x25519_dalek::PublicKey::from(&secret).as_bytes())
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len() / 2).filter_map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()).collect()
}

pub fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in bytes.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stuck_tunnel_times_out_and_is_not_queried_again_until_it_answers() {
        let (release, hold) = mpsc::channel::<()>();
        let started = std::time::Instant::now();
        let err = with_deadline("t-stuck", Duration::from_millis(100), move || {
            let _ = hold.recv();
            Ok(Status::default())
        })
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(2));
        // Пока первый запрос висит, второй не стартует (иначе по потоку в секунду).
        let again = with_deadline("t-stuck", Duration::from_millis(100), || panic!("не должен запускаться")).unwrap_err();
        assert!(again.to_string().contains("previous query"), "{again}");
        // Другой туннель опрашивается как обычно.
        assert!(with_deadline("t-other", Duration::from_secs(5), || Ok(Status::default())).is_ok());
        // Служба ожила — имя освобождается.
        drop(release);
        let mut freed = false;
        for _ in 0..100 {
            if with_deadline("t-stuck", Duration::from_secs(5), || Ok(Status::default())).is_ok() {
                freed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(freed, "после ответа туннель должен опрашиваться снова");
    }

    #[test]
    fn thread_spawn_failure_is_an_error_of_that_query_and_does_not_block_the_next() {
        fn refuse(_: Box<dyn FnOnce() + Send>) -> io::Result<()> {
            Err(io::Error::new(io::ErrorKind::OutOfMemory, "no threads"))
        }
        let err = with_deadline_using(refuse, "t-nospawn", Duration::from_secs(1), || panic!("не должен запускаться")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::OutOfMemory);
        assert!(err.to_string().contains("t-nospawn") && err.to_string().contains("no threads"), "{err}");
        // Имя не осталось в STUCK: когда потоки снова создаются, туннель опрашивается как обычно.
        assert!(with_deadline("t-nospawn", Duration::from_secs(5), || Ok(Status::default())).is_ok());
    }

    const SAMPLE: &[&str] = &[
        "private_key=0000000000000000000000000000000000000000000000000000000000000000",
        "listen_port=51820",
        "jc=4",
        "jmin=50",
        "h1=1234567890",
        "public_key=de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f",
        "preshared_key=0000000000000000000000000000000000000000000000000000000000000000",
        "endpoint=203.0.113.5:51820",
        "last_handshake_time_sec=1790000000",
        "last_handshake_time_nsec=5",
        "tx_bytes=1153433",
        "rx_bytes=2065694",
        "persistent_keepalive_interval=25",
        "allowed_ip=1.1.1.1/32",
        "allowed_ip=3.173.0.0/17",
        "errno=0",
    ];

    #[test]
    fn parses_interface_and_peer() {
        let st = parse(SAMPLE).unwrap();
        assert_eq!(st.listen_port, 51820);
        assert_eq!(st.awg_params, vec![
            ("jc".into(), "4".into()),
            ("jmin".into(), "50".into()),
            ("h1".into(), "1234567890".into()),
        ]);
        let p = &st.peers[0];
        // Открытый ключ Боба из RFC 7748: hex из UAPI переводится в base64.
        assert_eq!(p.public_key, "3p7bfXt9wbTTW2HC7OQ1Nz+DQ8hbeGdNrfx+FG+IK08=");
        assert_eq!(p.endpoint, "203.0.113.5:51820");
        assert_eq!((p.rx_bytes, p.tx_bytes, p.keepalive), (2065694, 1153433, 25));
        assert_eq!(p.allowed_ips, vec!["1.1.1.1/32", "3.173.0.0/17"]);
        assert_eq!(st.last_handshake_sec(), 1790000000);
    }

    #[test]
    fn derives_public_key_from_private() {
        // RFC 7748, раздел 6.1: пара ключей Алисы.
        let st = parse(&[
            "private_key=77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a",
            "errno=0",
        ])
        .unwrap();
        let expected = base64(&hex("8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a"));
        assert_eq!(st.public_key, expected);
    }

    #[test]
    fn errno_is_error() {
        assert!(parse(&["errno=2"]).is_err());
    }

    #[test]
    fn base64_padding() {
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
    }
}
