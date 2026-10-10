//! Именованный канал ядра. Права канала: SYSTEM и администраторы — полные, учётная запись владельца (решение 11.1)
//! — только чтение и запись: окно владельца работает без прав администратора, но не может ни поменять права
//! канала, ни создать свой экземпляр с тем же именем. Другие пользователи компьютера к ядру не подключатся.
//! Владелец канала — SYSTEM; это проверяют обе стороны: клиент — что говорит со службой, сервер — что его
//! экземпляр не встроился в чужой канал с тем же именем.
//! Ввод-вывод — перекрывающийся, у каждого чтения и записи есть срок: молчащий клиент не держит поток ядра
//! вечно, а зависший запрос не держит вечно поток окна.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};

use windows_sys::core::BOOL;
use windows_sys::Win32::Foundation::{
    GetLastError, LocalFree, ERROR_ACCESS_DENIED, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED,
    GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1, SE_KERNEL_OBJECT};
use windows_sys::Win32::Security::{
    CheckTokenMembership, CreateWellKnownSid, GetTokenInformation, RevertToSelf, TokenElevation, TokenUser, WinBuiltinAdministratorsSid,
    OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, TOKEN_ELEVATION, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientSessionId, ImpersonateNamedPipeClient, WaitNamedPipeW, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{CreateEventW, GetCurrentThread, OpenThreadToken, ResetEvent, WaitForSingleObject, INFINITE};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

use super::phase_trace::mark;
use super::proto::{Request, Response};
use crate::win::wide;

/// Имя канала ядра. Второй процесс (агент) держит свой канал под другим именем: `Server::new` и `call_to` принимают имя.
pub const NAME: &str = r"\\.\pipe\AmneziaWG-UI-Dark-Core";
const BUFFER: u32 = 1 << 16;
/// Предел запроса к ядру (импорт сотни туннелей — сотни килобайт).
const MAX_REQUEST: u64 = 8 << 20;
/// Предел ответа ядра (состояние со списками адресов).
const MAX_RESPONSE: u64 = 64 << 20;
/// Чтение и запись данных, атрибутов и расширенных атрибутов, READ_CONTROL, SYNCHRONIZE — без WRITE_DAC,
/// WRITE_OWNER и FILE_CREATE_PIPE_INSTANCE.
const OWNER_RIGHTS: &str = "0x12019b";
/// Сколько ядро ждёт строку запроса от подключившегося клиента и сколько — пока клиент заберёт ответ. Окно
/// пишет запрос сразу после подключения; клиент, который подключился и молчит, иначе держал бы поток и место
/// из `MAX_CONNECTIONS` ядра вечно, и 32 таких клиента отрезали бы окно от ядра.
const SERVER_IO_TIMEOUT: Duration = Duration::from_secs(10);
/// Сколько окно ждёт, пока ядро примет запрос.
const CLIENT_SEND_TIMEOUT: Duration = Duration::from_secs(10);
/// Сколько окно ждёт ответа: дольше самого долгого законного запроса — два действия в родном окне подряд (по
/// `helper::TIMEOUT`, 120 с, они идут по одному) и запас. Ядро, которое так и не ответило, не вешает поток окна.
const CLIENT_REPLY_TIMEOUT: Duration = Duration::from_secs(300);

/// Конец канала, открытый для перекрывающегося ввода-вывода, и событие для ожидания его операций.
struct Pipe {
    handle: OwnedHandle,
    event: OwnedHandle,
}

impl Pipe {
    /// Взять во владение дескриптор канала, открытый с `FILE_FLAG_OVERLAPPED`; закрывается вместе с `Pipe`.
    unsafe fn new(h: HANDLE) -> Result<Pipe, String> {
        let handle = OwnedHandle::from_raw_handle(h);
        let event = CreateEventW(null(), 1, 0, null());
        if event.is_null() {
            return Err(format!("CreateEvent: {}", std::io::Error::last_os_error()));
        }
        Ok(Pipe { handle, event: OwnedHandle::from_raw_handle(event) })
    }

    fn raw(&self) -> HANDLE {
        self.handle.as_raw_handle()
    }

    /// Одна операция: `start` запускает её с данным OVERLAPPED; ждать до `deadline` (None — без срока). Срок
    /// вышел — операция отменяется, и функция ждёт конца отмены: буфер и OVERLAPPED не должны пережить её.
    fn io(&self, deadline: Option<Instant>, start: impl FnOnce(HANDLE, *mut OVERLAPPED) -> BOOL) -> std::io::Result<u32> {
        let h = self.raw();
        unsafe {
            let mut ov: OVERLAPPED = std::mem::zeroed();
            ov.hEvent = self.event.as_raw_handle();
            ResetEvent(ov.hEvent);
            if start(h, &mut ov) == 0 {
                let e = GetLastError();
                if e != ERROR_IO_PENDING {
                    return Err(std::io::Error::from_raw_os_error(e as i32));
                }
                let waited = WaitForSingleObject(ov.hEvent, wait_ms(deadline, Instant::now()));
                if waited != WAIT_OBJECT_0 {
                    let e = if waited == WAIT_TIMEOUT { std::io::ErrorKind::TimedOut.into() } else { std::io::Error::last_os_error() };
                    CancelIoEx(h, &ov);
                    let mut n = 0u32;
                    GetOverlappedResult(h, &ov, &mut n, 1);
                    return Err(e);
                }
            }
            let mut n = 0u32;
            if GetOverlappedResult(h, &ov, &mut n, 0) == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(n)
        }
    }

    /// Чтение и запись до `deadline` — общего срока на всё сообщение.
    fn until(&self, deadline: Instant) -> Timed<'_> {
        Timed { pipe: self, deadline }
    }
}

/// Миллисекунды до срока для WaitForSingleObject; без срока — INFINITE.
fn wait_ms(deadline: Option<Instant>, now: Instant) -> u32 {
    deadline.map_or(INFINITE, |d| d.saturating_duration_since(now).as_millis().min(u128::from(INFINITE - 1)) as u32)
}

/// Канал со сроком: `Read` и `Write` для `read_json`/`write_json`.
struct Timed<'a> {
    pipe: &'a Pipe,
    deadline: Instant,
}

impl Read for Timed<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let len = buf.len().min(u32::MAX as usize) as u32;
        let ptr = buf.as_mut_ptr();
        match self.pipe.io(Some(self.deadline), |h, ov| unsafe { ReadFile(h, ptr, len, null_mut(), ov) }) {
            // Другой конец закрыл канал — конец данных, как у обычного файла.
            Err(e) if e.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) => Ok(0),
            r => r.map(|n| n as usize),
        }
    }
}

impl Write for Timed<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let len = buf.len().min(u32::MAX as usize) as u32;
        let ptr = buf.as_ptr();
        self.pipe.io(Some(self.deadline), |h, ov| unsafe { WriteFile(h, ptr, len, null_mut(), ov) }).map(|n| n as usize)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Соединение со стороны ядра: запрос, кто прислал (SID и сеанс Windows) и ответ.
pub struct ServerConn {
    pipe: Pipe,
    /// Срок на строку запроса — от подключения клиента.
    read_by: Instant,
    pub session: u32,
    /// SID учётной записи клиента; не удалось узнать — None (действия в сеансе пользователя тогда запрещены).
    pub client_sid: Option<String>,
    /// Клиент работает с правами администратора (подтверждён UAC); не удалось узнать — false.
    pub elevated: bool,
}

impl ServerConn {
    pub fn read(&mut self) -> Result<Request, String> {
        self.read_as()
    }

    pub fn reply(&mut self, r: &Response) -> Result<(), String> {
        self.reply_with(r)
    }

    /// То же с чужим протоколом: у агента свои `AgentRequest`/`AgentResponse`, а срок, предел и разбор строки общие.
    pub fn read_as<T: serde::de::DeserializeOwned>(&mut self) -> Result<T, String> {
        read_json(&mut self.pipe.until(self.read_by), MAX_REQUEST)
    }

    pub fn reply_with<T: serde::Serialize>(&mut self, r: &T) -> Result<(), String> {
        write_json(&mut self.pipe.until(Instant::now() + SERVER_IO_TIMEOUT), r)
    }

    /// Клиент — сама система: так ядро проверяет при старте, что его канал отвечает.
    pub fn client_is_system(&self) -> bool {
        self.client_sid.as_deref() == Some(crate::win::LOCAL_SYSTEM_SID)
    }
}

/// Не удалось дождаться клиента. `taken` — имя канала занято чужим каналом: его создала другая программа, пока
/// канала ядра не было. Тогда дело не в сборке ядра, а в той программе (решение о возврате прежней сборки — по этому признаку).
#[derive(Debug)]
pub struct AcceptError {
    pub text: String,
    pub taken: bool,
}

impl AcceptError {
    fn other(text: String) -> AcceptError {
        AcceptError { text, taken: false }
    }
}

/// SDDL канала: SYSTEM и администраторы полностью, владелец окна — с `OWNER_RIGHTS`. Если `owner_sid` не SID, он не
/// вставляется, а возвращается вторым элементом.
fn pipe_sddl(owner_sid: &str) -> (String, Option<String>) {
    const BASE: &str = "O:SYD:P(A;;GA;;;SY)(A;;GA;;;BA)";
    if crate::win::is_sid(owner_sid) {
        (format!("{BASE}(A;;{OWNER_RIGHTS};;;{owner_sid})"), None)
    } else {
        (BASE.to_string(), Some(owner_sid.to_string()))
    }
}

/// Владелец канала, которому верят обе стороны: служба (SYSTEM).
#[cfg(not(test))]
fn trusted_owner() -> &'static str {
    crate::win::LOCAL_SYSTEM_SID
}

/// Тестовая сборка работает от учётной записи разработчика и создать канал с владельцем SYSTEM не может: у неё
/// владелец канала — она сама. Так ядро и поддельный агент проверяются настоящими каналами под тестовыми именами
/// (`server::tests`, тест изоляции); в сборку программы это не попадает.
#[cfg(test)]
fn trusted_owner() -> &'static str {
    static OWNER: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    OWNER.get_or_init(|| crate::win::current_user_sid().unwrap_or_else(|e| panic!("test pipe owner SID: {e}")))
}

/// Сервер: каждый вызов ждёт следующего клиента.
pub struct Server {
    name: String,
    sddl: String,
    first: bool,
}

impl Server {
    /// Канал владельца окна. SID берётся из ini и попадает прямо в SDDL, поэтому негодный (не `is_sid`) не вставляется:
    /// канал остаётся только у администраторов и SYSTEM, а отвергнутая строка возвращается для журнала ядра.
    pub fn new(name: &str, owner_sid: &str) -> (Server, Option<String>) {
        let (sddl, rejected) = pipe_sddl(owner_sid);
        #[cfg(test)]
        let sddl = sddl.replacen("O:SY", &format!("O:{}", trusted_owner()), 1);
        (Server { name: name.to_string(), sddl, first: true }, rejected)
    }

    pub fn accept(&mut self) -> Result<ServerConn, AcceptError> {
        mark("accept: create pipe instance");
        unsafe {
            let mut sd: PSECURITY_DESCRIPTOR = null_mut();
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(wide(&self.sddl).as_ptr(), SDDL_REVISION_1, &mut sd, null_mut()) == 0 {
                return Err(AcceptError::other(format!("pipe SDDL: {}", std::io::Error::last_os_error())));
            }
            let sa = SECURITY_ATTRIBUTES { nLength: size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: sd, bInheritHandle: 0 };
            let open = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | if self.first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 };
            let mode = PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;
            let h = CreateNamedPipeW(wide(&self.name).as_ptr(), open, mode, PIPE_UNLIMITED_INSTANCES, BUFFER, BUFFER, 0, &sa);
            let created = std::io::Error::last_os_error();
            LocalFree(sd);
            if h == INVALID_HANDLE_VALUE {
                // С FILE_FLAG_FIRST_PIPE_INSTANCE отказ в доступе значит: канал с этим именем уже есть, и он не наш.
                let taken = self.first && created.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32);
                self.first = true;
                return Err(AcceptError { text: if taken { crate::i18n::tr("core.pipe_taken") } else { format!("CreateNamedPipe: {created}") }, taken });
            }
            let pipe = Pipe::new(h).map_err(AcceptError::other)?;
            // Все экземпляры канала закрылись, и имя успел занять кто-то другой — наш экземпляр встал бы в его
            // канал с его правами. Не служим такому каналу и начинаем заново с первого экземпляра.
            if owner_of(h).as_deref() != Some(trusted_owner()) {
                self.first = true;
                return Err(AcceptError { text: crate::i18n::tr("core.pipe_taken"), taken: true });
            }
            self.first = false;
            mark("accept: instance ready, waiting for a client");
            match pipe.io(None, |h, ov| ConnectNamedPipe(h, ov)) {
                Ok(_) => {}
                // Клиент подключился между созданием экземпляра и ожиданием — это тоже подключение.
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_CONNECTED as i32) => {}
                Err(e) => return Err(AcceptError::other(format!("ConnectNamedPipe: {e}"))),
            }
            mark("accept: client connected");
            let read_by = Instant::now() + SERVER_IO_TIMEOUT;
            let mut session = 0u32;
            GetNamedPipeClientSessionId(h, &mut session);
            let (client_sid, elevated) = client_identity(h);
            mark("accept: client identity read");
            Ok(ServerConn { pipe, read_by, session, client_sid, elevated })
        }
    }
}

/// Кто клиент канала: SID и есть ли у него права администратора. Олицетворить его (уровня «идентификация»
/// достаточно), взять токен потока и сразу вернуться к своим правам; токен читается уже после возврата.
unsafe fn client_identity(pipe: HANDLE) -> (Option<String>, bool) {
    if ImpersonateNamedPipeClient(pipe) == 0 {
        return (None, false);
    }
    let mut token: HANDLE = null_mut();
    let opened = OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut token) != 0;
    RevertToSelf();
    if !opened {
        return (None, false);
    }
    let token = OwnedHandle::from_raw_handle(token);
    (token_user_sid(token.as_raw_handle()), token_elevated_admin(token.as_raw_handle()))
}

/// Токен повышен (UAC) и группа «Администраторы» в нём действует, а не оставлена только для запретов, как в
/// обычном токене администратора без UAC. Ошибка любого вызова — «не администратор».
unsafe fn token_elevated_admin(token: HANDLE) -> bool {
    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut len = 0u32;
    let size = size_of::<TOKEN_ELEVATION>() as u32;
    if GetTokenInformation(token, TokenElevation, (&mut elevation as *mut TOKEN_ELEVATION).cast(), size, &mut len) == 0
        || elevation.TokenIsElevated == 0
    {
        return false;
    }
    token_in_admins(token) == Some(true)
}

/// Группа «Администраторы» в токене действует (не только для запретов). `token` — токен олицетворения: с первичным
/// `CheckTokenMembership` отказывает. `None` — узнать не удалось.
pub(super) unsafe fn token_in_admins(token: HANDLE) -> Option<bool> {
    // SECURITY_MAX_SID_SIZE — 68 байт; u64 — ради выравнивания SID.
    let mut admins = [0u64; 9];
    let mut admins_len = size_of_val(&admins) as u32;
    if CreateWellKnownSid(WinBuiltinAdministratorsSid, null_mut(), admins.as_mut_ptr().cast(), &mut admins_len) == 0 {
        return None;
    }
    let mut member = 0;
    (CheckTokenMembership(token, admins.as_mut_ptr().cast(), &mut member) != 0).then_some(member != 0)
}

/// SID пользователя токена.
pub unsafe fn token_user_sid(token: HANDLE) -> Option<String> {
    let mut buf = vec![0u64; 64];
    let mut len = 0u32;
    if GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), (buf.len() * 8) as u32, &mut len) == 0 {
        return None;
    }
    let user = &*(buf.as_ptr() as *const TOKEN_USER);
    crate::win::sid_string(user.User.Sid)
}

/// Владелец объекта ядра Windows по дескриптору.
unsafe fn owner_of(h: HANDLE) -> Option<String> {
    let (mut owner, mut sd): (PSID, PSECURITY_DESCRIPTOR) = (null_mut(), null_mut());
    if GetSecurityInfo(h, SE_KERNEL_OBJECT, OWNER_SECURITY_INFORMATION, &mut owner, null_mut(), null_mut(), null_mut(), &mut sd) != 0 {
        return None;
    }
    let sid = crate::win::sid_string(owner);
    LocalFree(sd);
    sid
}

/// Сроки одного запроса: сколько ждать, пока сервер примет запрос, и сколько — его ответа.
#[derive(Clone, Copy, Debug)]
pub struct Timeouts {
    pub send: Duration,
    pub reply: Duration,
}

impl Timeouts {
    /// Для канала ядра: ответ ждём дольше самого долгого законного запроса.
    pub const CORE: Timeouts = Timeouts { send: CLIENT_SEND_TIMEOUT, reply: CLIENT_REPLY_TIMEOUT };

    /// Обычный срок отправки и свой срок ответа (внутренние запросы ядру ждут ответа коротко).
    pub const fn with_reply(reply: Duration) -> Timeouts {
        Timeouts { send: CLIENT_SEND_TIMEOUT, reply }
    }
}

/// Один запрос к ядру. Ядро не запущено, на том конце не наше ядро или оно не ответило в срок — ошибка.
pub fn call(req: &Request) -> Result<Response, String> {
    call_to(NAME, req, Timeouts::CORE)
}

/// Один запрос к каналу с заданным именем и сроками (канал ядра — `NAME`, `Timeouts::CORE`).
pub fn call_to(name: &str, req: &Request, timeouts: Timeouts) -> Result<Response, String> {
    call_with(name, req, timeouts)
}

/// То же для канала со своими типами запроса и ответа (канал агента — `agent::proto`).
pub fn call_with<Q: serde::Serialize, R: serde::de::DeserializeOwned>(name: &str, req: &Q, timeouts: Timeouts) -> Result<R, String> {
    mark("client: connect");
    let pipe = connect(name)?;
    mark("client: connected, server owner checked");
    write_json(&mut pipe.until(Instant::now() + timeouts.send), req)?;
    mark("client: request sent");
    let reply = read_json(&mut pipe.until(Instant::now() + timeouts.reply), MAX_RESPONSE);
    mark("client: reply received");
    reply
}

fn connect(name: &str) -> Result<Pipe, String> {
    let name = wide(name);
    for _ in 0..20 {
        unsafe {
            // Уровень «идентификация»: ядро узнаёт, кто мы, но действовать от нашего имени не может.
            let flags = SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION | FILE_FLAG_OVERLAPPED;
            let h = CreateFileW(name.as_ptr(), GENERIC_READ | GENERIC_WRITE, 0, null(), OPEN_EXISTING, flags, null_mut());
            if h != INVALID_HANDLE_VALUE {
                let pipe = Pipe::new(h)?;
                // Владелец канала — SYSTEM: его создала служба, а не подставная программа пользователя.
                let owner = owner_of(h);
                if owner.as_deref() != Some(trusted_owner()) {
                    return Err(crate::i18n::trf("core.foreign", &[owner.as_deref().unwrap_or("?")]));
                }
                return Ok(pipe);
            }
            if GetLastError() != ERROR_PIPE_BUSY {
                return Err(crate::i18n::trf("core.unavailable", &[&std::io::Error::last_os_error().to_string()]));
            }
            mark("client: all pipe instances busy, waiting");
            WaitNamedPipeW(name.as_ptr(), 500);
        }
    }
    Err(crate::i18n::trf("core.unavailable", &["busy"]))
}

fn write_json<T: serde::Serialize>(out: &mut impl Write, value: &T) -> Result<(), String> {
    let mut line = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    line.push(b'\n');
    out.write_all(&line).map_err(|e| format!("pipe write: {e}"))
}

fn read_json<T: serde::de::DeserializeOwned>(input: &mut impl Read, limit: u64) -> Result<T, String> {
    let mut line = Vec::new();
    let mut reader = BufReader::new(input).take(limit);
    reader.read_until(b'\n', &mut line).map_err(|e| format!("pipe read: {e}"))?;
    if line.is_empty() {
        return Err("pipe: closed".into());
    }
    if line.last() != Some(&b'\n') {
        return Err(format!("pipe: message over {limit} bytes"));
    }
    serde_json::from_slice(&line).map_err(|e| format!("pipe: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_sddl_takes_only_a_valid_owner_sid() {
        let (sddl, rejected) = pipe_sddl("S-1-5-21-1-2-3-1001");
        assert!(sddl.ends_with(";;;S-1-5-21-1-2-3-1001)") && rejected.is_none(), "{sddl}");
        // Через «владельца» в SDDL не должно попасть ничего, кроме SYSTEM и администраторов.
        for bad in ["", "S-1-", "WD", "S-1-5-21)(A;;GA;;;WD", "S-1-5-21-1 (A;;GA;;;WD)"] {
            let (sddl, rejected) = pipe_sddl(bad);
            assert_eq!(sddl, "O:SYD:P(A;;GA;;;SY)(A;;GA;;;BA)", "{bad:?}");
            assert_eq!(rejected.as_deref(), Some(bad));
        }
    }

    /// Пара концов канала с уникальным именем (не имя ядра): сервер и клиент, оба перекрывающиеся.
    fn pair(tag: &str) -> (Pipe, Pipe) {
        let name = wide(&format!(r"\\.\pipe\awg-ui-test-{tag}-{}", std::process::id()));
        unsafe {
            let open = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE;
            let mode = PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;
            let s = CreateNamedPipeW(name.as_ptr(), open, mode, 1, BUFFER, BUFFER, 0, null());
            assert_ne!(s, INVALID_HANDLE_VALUE, "{}", std::io::Error::last_os_error());
            let server = Pipe::new(s).unwrap();
            let c = CreateFileW(name.as_ptr(), GENERIC_READ | GENERIC_WRITE, 0, null(), OPEN_EXISTING, FILE_FLAG_OVERLAPPED, null_mut());
            assert_ne!(c, INVALID_HANDLE_VALUE, "{}", std::io::Error::last_os_error());
            let connected = server.io(None, |h, ov| ConnectNamedPipe(h, ov));
            assert!(connected.is_ok() || connected.as_ref().err().and_then(|e| e.raw_os_error()) == Some(ERROR_PIPE_CONNECTED as i32));
            (server, Pipe::new(c).unwrap())
        }
    }

    #[test]
    fn call_to_a_missing_pipe_fails_fast() {
        let name = format!(r"\\.\pipe\awg-ui-test-missing-{}", std::process::id());
        let started = Instant::now();
        let e = call_to(&name, &Request::Hello, Timeouts { send: Duration::from_secs(1), reply: Duration::from_secs(1) }).unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
        assert_eq!(e, crate::i18n::trf("core.unavailable", &[&std::io::Error::from_raw_os_error(2).to_string()]), "нет канала — «ядро недоступно»");
    }

    #[test]
    fn silent_client_times_out_instead_of_holding_the_thread() {
        let (server, _client) = pair("silent");
        let started = Instant::now();
        let r: Result<Request, String> = read_json(&mut server.until(started + Duration::from_millis(200)), MAX_REQUEST);
        let e = r.unwrap_err();
        assert!(e.starts_with("pipe read:"), "{e}");
        let waited = started.elapsed();
        assert!(waited >= Duration::from_millis(150) && waited < Duration::from_secs(5), "{waited:?}");
    }

    #[test]
    fn request_and_reply_go_through_with_deadlines() {
        let (server, client) = pair("roundtrip");
        let soon = || Instant::now() + Duration::from_secs(5);
        write_json(&mut client.until(soon()), &Request::Hello).unwrap();
        assert!(matches!(read_json(&mut server.until(soon()), MAX_REQUEST), Ok(Request::Hello)));
        write_json(&mut server.until(soon()), &Response::Ok).unwrap();
        assert!(matches!(read_json(&mut client.until(soon()), MAX_RESPONSE), Ok(Response::Ok)));
    }

    #[test]
    fn closed_peer_is_end_of_data_not_a_hang() {
        let (server, client) = pair("closed");
        drop(client);
        let r: Result<Request, String> = read_json(&mut server.until(Instant::now() + Duration::from_secs(5)), MAX_REQUEST);
        assert_eq!(r.unwrap_err(), "pipe: closed");
    }

    #[test]
    fn deadline_maps_to_wait_milliseconds() {
        let now = Instant::now();
        assert_eq!(wait_ms(None, now), INFINITE);
        assert_eq!(wait_ms(Some(now), now + Duration::from_secs(1)), 0, "срок прошёл — не ждать");
        assert_eq!(wait_ms(Some(now + Duration::from_millis(1500)), now), 1500);
        assert_eq!(wait_ms(Some(now + Duration::from_secs(u64::from(u32::MAX))), now), INFINITE - 1, "долгий срок — не «вечно»");
    }
}
