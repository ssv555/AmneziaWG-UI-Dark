//! DNS of a mode 2 tunnel. The engine's `tunnel.dll` (amneziawg-windows, root package built as the DLL) sets the
//! interface's addresses, routes, MTU and metric but never its DNS servers: its `configureInterface` has no `SetDNS`
//! call (the original AmneziaWG client sets DNS itself). Without this module a profile's `DNS =` was ignored and
//! Windows resolved through the other adapters, outside the tunnel.
//!
//! The tunnel service process (`engine::run_service`) runs [`keep`] next to the engine for the life of the tunnel:
//! it waits for the adapter (its alias is the tunnel name), sets the profile's servers and search domains for IPv4 and
//! IPv6 like the official WireGuard client does (`SetInterfaceDnsSettings`), and re-applies them if Windows drops them.
//! Removal needs no code here: on stop the engine flushes the interface's DNS and deletes the adapter.
//! Metric is the engine's (0 for a default route, automatic otherwise); no NRPT rule, so a silent DNS server
//! or a stopped tunnel never leaves the system without resolution.
//!
//! Why here and not a one-line `SetDNS` patch of the Go engine: the DLL's root package is upstream's embeddable
//! service, which leaves DNS to the embedding client (its `configureInterface` drops the `SetDNS` call that the
//! `tunnel/` package of the same commit has, at `tunnel/addressconfig.go:174`); `engine/build.ps1`
//! builds the pinned upstream commit unmodified and `tunnel.dll` is pinned by SHA-256 in the program and the signed
//! update manifest. A local patch would make every engine upgrade a rebase and a re-pin of a forked DLL; the keeper
//! needs neither and uses the same Windows call as upstream's `SetDNS`.
//!
//! Lifetime: the keeper writes only while the engine is not stopping. The engine reports `StopPending` before it
//! flushes DNS and deletes the adapter, so the keeper ends there and never re-applies DNS to an adapter on its way out.
//! If the service process is killed, Wintun removes the adapter with it; the DNS value stays in the registry under the
//! adapter GUID, which the engine uses for every tunnel (fixed GUID). The next tunnel's keeper overwrites or clears it
//! within one [`TICK`] after the adapter appears; until then queries to the old server fail over to other adapters.
//!
//! Messages: the tunnel service process has no pipe to the core; its other problems reach the core as service exit
//! codes. DNS problems are not fatal and must not stop the tunnel, so the keeper hands its notes to the core through a
//! per-tunnel file ([`Outbox`] → [`Inbox`], `C:\ProgramData\AmneziaWG UI Dark\tunnel-dns\<tunnel>.notes`) and the core
//! puts them into the app's event log. Only if that file cannot be written does a note go to the Windows Application log.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::i18n::trf;

/// Poll period: the adapter appears within a second of the start; a dropped setting is restored within it.
pub const TICK: Duration = Duration::from_secs(2);

/// DNS of one address family as Windows stores it: comma-separated servers and search domains.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FamilyDns {
    pub servers: String,
    pub search: String,
}

/// What the profile asks for: IPv4 servers with the search domains, IPv6 servers.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Plan {
    pub v4: FamilyDns,
    pub v6: FamilyDns,
}

impl Plan {
    /// From the profile text, by the engine's rule: an IP address is a server, anything else a search domain.
    pub fn of(conf_text: &str) -> Plan {
        let (mut v4, mut v6, mut search) = (Vec::new(), Vec::new(), Vec::new());
        for item in crate::conf::parse(conf_text).dns {
            // An IPv4-mapped IPv6 address is an IPv4 server, as Go's `To4` in the engine's `SetDNS` treats it.
            match item.parse::<IpAddr>().map(|a| a.to_canonical()) {
                Ok(IpAddr::V4(a)) => v4.push(a.to_string()),
                Ok(IpAddr::V6(a)) => v6.push(a.to_string()),
                Err(_) => search.push(item),
            }
        }
        let search = search.join(",");
        Plan {
            v4: FamilyDns {
                servers: v4.join(","),
                search: search.clone(),
            },
            v6: FamilyDns {
                servers: v6.join(","),
                search,
            },
        }
    }

    fn family(&self, v6: bool) -> &FamilyDns {
        if v6 {
            &self.v6
        } else {
            &self.v4
        }
    }
}

/// The interface's DNS through the Windows API (`Adapter` in tests is a fake). `id` is the adapter's GUID.
pub trait Adapter {
    /// The adapter with this alias, if it exists now.
    fn find(&self, alias: &str) -> Option<u128>;
    fn get(&self, id: u128, v6: bool) -> Result<FamilyDns, String>;
    fn set(&self, id: u128, v6: bool, dns: &FamilyDns) -> Result<(), String>;
}

/// One message of the keeper for the log; `warn` — something the owner should see.
#[derive(Debug, PartialEq)]
pub struct Note {
    pub warn: bool,
    pub text: String,
}

/// Keeper state between ticks: what was applied to which adapter and the last error (logged once until it changes).
#[derive(Default)]
pub struct Keeper {
    applied: [Option<u128>; 2],
    last_error: Option<String>,
}

impl Keeper {
    /// One pass: find the adapter, compare each family with the plan, set what differs. Returns messages to log.
    pub fn tick(&mut self, plan: &Plan, alias: &str, adapter: &dyn Adapter) -> Vec<Note> {
        let mut notes = Vec::new();
        let Some(id) = adapter.find(alias) else {
            self.applied = [None, None];
            return notes;
        };
        for v6 in [false, true] {
            let want = plan.family(v6);
            let family = if v6 { "IPv6" } else { "IPv4" };
            // No DNS key for the family yet (Windows answers "file not found") reads as empty; once applied to this
            // adapter, an unreadable family is left alone rather than rewritten every tick.
            let have = match adapter.get(id, v6) {
                Ok(have) => Ok(have),
                Err(_) if self.applied[usize::from(v6)] == Some(id) => continue,
                Err(_) => Ok(FamilyDns::default()),
            };
            let result = have.and_then(|have: FamilyDns| {
                if normalize(&have) == normalize(want) {
                    return Ok(None);
                }
                adapter.set(id, v6, want).map(|()| Some(have))
            });
            match result {
                Ok(None) => self.applied[usize::from(v6)] = Some(id),
                Ok(Some(had)) => {
                    let again = self.applied[usize::from(v6)] == Some(id);
                    self.applied[usize::from(v6)] = Some(id);
                    if want.servers.is_empty()
                        && want.search.is_empty()
                        && had.servers.is_empty()
                        && had.search.is_empty()
                    {
                        continue;
                    }
                    notes.push(Note {
                        warn: again,
                        text: changed_text(alias, family, want, &had, again),
                    });
                    self.last_error = None;
                }
                Err(e) => {
                    let text = trf("dns.failed", &[alias, family, &want.servers, &e]);
                    self.note_error_once(&mut notes, text);
                }
            }
        }
        notes
    }

    /// An error is logged once until it changes: the keeper retries every tick, the log must not.
    fn note_error_once(&mut self, notes: &mut Vec<Note>, text: String) {
        if self.last_error.as_deref() != Some(&text) {
            notes.push(Note {
                warn: true,
                text: text.clone(),
            });
            self.last_error = Some(text);
        }
    }
}

fn changed_text(
    alias: &str,
    family: &str,
    want: &FamilyDns,
    had: &FamilyDns,
    again: bool,
) -> String {
    let search = if want.search.is_empty() {
        String::new()
    } else {
        trf("dns.search", &[&want.search])
    };
    let key = if again { "dns.reapplied" } else { "dns.set" };
    trf(key, &[alias, family, &want.servers, &had.servers, &search])
}

/// Windows returns lists separated by commas or spaces; compare item by item, order kept (it is the preference).
fn normalize(d: &FamilyDns) -> (Vec<String>, Vec<String>) {
    let split = |s: &str| {
        s.split([',', ' ', ';'])
            .filter(|x| !x.is_empty())
            .map(str::to_ascii_lowercase)
            .collect()
    };
    (split(&d.servers), split(&d.search))
}

/// The keeper loop of the tunnel service: a pass every [`TICK`] until the engine is stopping (`stopping` says so;
/// an error there is logged once and the keeper goes on, DNS is what the profile asked for). Then it returns: the
/// engine flushes DNS and deletes the adapter, and a pass in between would put the DNS back on a dying adapter.
pub fn keep(
    plan: &Plan,
    alias: &str,
    adapter: &dyn Adapter,
    stopping: &dyn Fn() -> Result<bool, String>,
    log: &mut dyn FnMut(&Note),
    sleep: &dyn Fn(Duration),
) {
    let mut keeper = Keeper::default();
    // Its own "once": a DNS write that succeeds clears the keeper's error, not this one.
    let mut state_error: Option<String> = None;
    loop {
        let mut notes = Vec::new();
        match stopping() {
            Ok(true) => return,
            Ok(false) => state_error = None,
            Err(e) => {
                let text = trf("dns.state_unknown", &[alias, &e]);
                if state_error.as_deref() != Some(&text) {
                    notes.push(Note {
                        warn: true,
                        text: text.clone(),
                    });
                    state_error = Some(text);
                }
            }
        }
        notes.extend(keeper.tick(plan, alias, adapter));
        for note in &notes {
            log(note);
        }
        sleep(TICK);
    }
}

/// Folder of the keepers' notes for the core: under the core's data folder (SYSTEM and administrators only).
pub fn notes_dir() -> PathBuf {
    crate::daemon::data_dir().join("tunnel-dns")
}

/// How many recent notes a keeper's file holds: the core reads it every supervision tick, the keeper writes at most
/// two notes per [`TICK`], so the core misses none unless it stalls for a minute.
const KEEP_NOTES: usize = 32;

/// The keeper's side of the hand-over: the file `<tunnel>.notes` rewritten atomically with an instance (the keeper's
/// start, ms since the epoch) and the last [`KEEP_NOTES`] numbered notes.
pub struct Outbox {
    path: PathBuf,
    instance: u64,
    seq: u64,
    recent: VecDeque<(u64, Note)>,
}

impl Outbox {
    pub fn new(dir: &Path, tunnel: &str, instance: u64) -> Outbox {
        Outbox {
            path: dir.join(format!("{tunnel}.notes")),
            instance,
            seq: 0,
            recent: VecDeque::new(),
        }
    }

    pub fn push(&mut self, note: &Note) -> std::io::Result<()> {
        self.seq += 1;
        self.recent.push_back((
            self.seq,
            Note {
                warn: note.warn,
                text: note.text.replace(['\r', '\n'], " "),
            },
        ));
        while self.recent.len() > KEEP_NOTES {
            self.recent.pop_front();
        }
        let mut text = format!("{}\n", self.instance);
        for (seq, n) in &self.recent {
            text.push_str(&format!(
                "{seq}\t{}\t{}\n",
                if n.warn { "W" } else { "I" },
                n.text
            ));
        }
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        crate::fsutil::write_atomic(&self.path, text.as_bytes())
    }
}

/// The core's side: reads every keeper's file and returns the notes it has not taken yet.
pub struct Inbox {
    /// The keepers' folder ([`notes_dir`]); `None` — not read (cores of tests).
    dir: Option<PathBuf>,
    /// The core's start (ms since the epoch): a keeper that started earlier wrote its old notes before this core ran;
    /// they were taken by the previous core or are stale, so on first sight only its later notes are taken.
    started: u64,
    seen: HashMap<String, (u64, u64)>,
    /// Files of vanished tunnels that [`Inbox::prune`] could not remove (reported once each).
    unremovable: HashSet<String>,
    last_error: Option<String>,
}

impl Inbox {
    pub fn new(dir: Option<PathBuf>, started: u64) -> Inbox {
        Inbox {
            dir,
            started,
            seen: HashMap::new(),
            unremovable: HashSet::new(),
            last_error: None,
        }
    }

    /// New notes per tunnel, in order. An unreadable folder is reported once (as a note with an empty tunnel name).
    pub fn take(&mut self) -> Vec<(String, Note)> {
        let mut out = Vec::new();
        let Some(dir) = self.dir.clone() else {
            return out;
        };
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return out,
            Err(e) => {
                let text = crate::fsutil::io_ctx(&dir, e);
                if self.last_error.as_deref() != Some(&text) {
                    out.push((
                        String::new(),
                        Note {
                            warn: true,
                            text: text.clone(),
                        },
                    ));
                    self.last_error = Some(text);
                }
                return out;
            }
        };
        self.last_error = None;
        for path in entries.flatten().map(|e| e.path()) {
            let Some(tunnel) = path
                .file_name()
                .and_then(|f| f.to_str())
                .and_then(|f| f.strip_suffix(".notes"))
            else {
                continue;
            };
            // A file being replaced right now reads on the next tick; nothing is lost, the keeper keeps its notes.
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            out.extend(
                self.take_file(tunnel, &text)
                    .into_iter()
                    .map(|n| (tunnel.to_string(), n)),
            );
        }
        out
    }

    /// Removes the files of tunnels the store no longer has (deleted or renamed while no keeper ran): nothing else
    /// removes them, and the folder would grow with every renamed tunnel. `known` — every tunnel that exists or runs;
    /// the caller passes it only when it has just read both lists, so a failed read never deletes a live tunnel's
    /// file. Names compare case-insensitively like Windows file names. A file that cannot be removed is reported once
    /// as a warning note (with the tunnel name) until it goes or the tunnel returns.
    pub fn prune(&mut self, known: &[String]) -> Vec<(String, Note)> {
        let mut out = Vec::new();
        let Some(dir) = self.dir.clone() else {
            return out;
        };
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return out;
        };
        for path in entries.flatten().map(|e| e.path()) {
            let Some(tunnel) = path
                .file_name()
                .and_then(|f| f.to_str())
                .and_then(|f| f.strip_suffix(".notes"))
            else {
                continue;
            };
            if known.iter().any(|k| k.eq_ignore_ascii_case(tunnel)) {
                self.unremovable.remove(tunnel);
                continue;
            }
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    self.seen.remove(tunnel);
                    self.unremovable.remove(tunnel);
                }
                // The keeper's pass or an earlier prune removed it first.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    if self.unremovable.insert(tunnel.to_string()) {
                        out.push((
                            tunnel.to_string(),
                            Note {
                                warn: true,
                                text: crate::fsutil::io_ctx(&path, e),
                            },
                        ));
                    }
                }
            }
        }
        out
    }

    fn take_file(&mut self, tunnel: &str, text: &str) -> Vec<Note> {
        let mut lines = text.lines();
        let Some(instance) = lines.next().and_then(|l| l.trim().parse::<u64>().ok()) else {
            return Vec::new();
        };
        let notes: Vec<(u64, Note)> = lines
            .filter_map(|l| {
                let mut parts = l.splitn(3, '\t');
                let seq = parts.next()?.parse().ok()?;
                let warn = parts.next()? == "W";
                Some((
                    seq,
                    Note {
                        warn,
                        text: parts.next()?.to_string(),
                    },
                ))
            })
            .collect();
        let last = notes.last().map_or(0, |(s, _)| *s);
        let after = match self.seen.get(tunnel) {
            Some(&(seen_instance, seq)) if seen_instance == instance => seq,
            None if instance < self.started => last,
            _ => 0,
        };
        self.seen
            .insert(tunnel.to_string(), (instance, last.max(after)));
        notes
            .into_iter()
            .filter(|(s, _)| *s > after)
            .map(|(_, n)| n)
            .collect()
    }
}

/// The real keeper of a tunnel service: DNS through `iphlpapi`, the engine's state from the service manager, notes to
/// the core's [`Inbox`] (or, if that file fails, to the Windows Application log). Runs on its own thread.
pub fn run(plan: Plan, tunnel: &str, stopping: &dyn Fn() -> Result<bool, String>) {
    let mut outbox = Outbox::new(&notes_dir(), tunnel, now_ms());
    let mut log = |note: &Note| {
        if let Err(e) = outbox.push(note) {
            log_to_windows(&Note {
                warn: true,
                text: format!("{} ({})", note.text, crate::fsutil::io_ctx(&outbox.path, e)),
            });
        }
    };
    keep(
        &plan,
        tunnel,
        &Windows::load(),
        stopping,
        &mut log,
        &std::thread::sleep,
    );
}

/// Milliseconds since the epoch: the instance of a keeper and the start of the core's [`Inbox`].
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// The real adapter: `iphlpapi.dll`. `SetInterfaceDnsSettings` exists since Windows 10 1809; it is resolved at run
/// time so the program still starts on older builds (there the keeper logs that DNS is unsupported).
pub struct Windows {
    get: Option<GetFn>,
    set: Option<SetFn>,
    free: Option<FreeFn>,
}

type Settings = windows_sys::Win32::NetworkManagement::IpHelper::DNS_INTERFACE_SETTINGS;
type GetFn = unsafe extern "system" fn(windows_sys::core::GUID, *mut Settings) -> u32;
type SetFn = unsafe extern "system" fn(windows_sys::core::GUID, *const Settings) -> u32;
type FreeFn = unsafe extern "system" fn(*mut Settings);

impl Windows {
    pub fn load() -> Windows {
        use windows_sys::Win32::System::LibraryLoader::{
            GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32,
        };
        unsafe {
            let lib = LoadLibraryExW(
                crate::win::wide("iphlpapi.dll").as_ptr(),
                std::ptr::null_mut(),
                LOAD_LIBRARY_SEARCH_SYSTEM32,
            );
            if lib.is_null() {
                return Windows {
                    get: None,
                    set: None,
                    free: None,
                };
            }
            let get = GetProcAddress(lib, c"GetInterfaceDnsSettings".as_ptr().cast())
                .map(|f| std::mem::transmute::<_, GetFn>(f));
            let set = GetProcAddress(lib, c"SetInterfaceDnsSettings".as_ptr().cast())
                .map(|f| std::mem::transmute::<_, SetFn>(f));
            let free = GetProcAddress(lib, c"FreeInterfaceDnsSettings".as_ptr().cast())
                .map(|f| std::mem::transmute::<_, FreeFn>(f));
            Windows { get, set, free }
        }
    }
}

fn guid_of(id: u128) -> windows_sys::core::GUID {
    windows_sys::core::GUID::from_u128(id)
}

fn flags(v6: bool) -> u64 {
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        DNS_SETTING_IPV6, DNS_SETTING_NAMESERVER, DNS_SETTING_SEARCHLIST,
    };
    u64::from(
        DNS_SETTING_NAMESERVER | DNS_SETTING_SEARCHLIST | if v6 { DNS_SETTING_IPV6 } else { 0 },
    )
}

unsafe fn pwstr_to_string(p: windows_sys::core::PWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    let len = (0..).take_while(|&i| *p.add(i) != 0).count();
    String::from_utf16_lossy(std::slice::from_raw_parts(p, len))
}

impl Adapter for Windows {
    fn find(&self, alias: &str) -> Option<u128> {
        use windows_sys::Win32::NetworkManagement::IpHelper::{
            ConvertInterfaceAliasToLuid, ConvertInterfaceLuidToGuid,
        };
        use windows_sys::Win32::NetworkManagement::Ndis::NET_LUID_LH;
        unsafe {
            let mut luid: NET_LUID_LH = std::mem::zeroed();
            if ConvertInterfaceAliasToLuid(crate::win::wide(alias).as_ptr(), &mut luid) != 0 {
                return None;
            }
            let mut guid: windows_sys::core::GUID = std::mem::zeroed();
            if ConvertInterfaceLuidToGuid(&luid, &mut guid) != 0 {
                return None;
            }
            Some(
                (u128::from(guid.data1) << 96)
                    | (u128::from(guid.data2) << 80)
                    | (u128::from(guid.data3) << 64)
                    | u128::from(u64::from_be_bytes(guid.data4)),
            )
        }
    }

    fn get(&self, id: u128, v6: bool) -> Result<FamilyDns, String> {
        let (Some(get), Some(free)) = (self.get, self.free) else {
            return Err("GetInterfaceDnsSettings: needs Windows 10 1809 or newer".into());
        };
        use windows_sys::Win32::NetworkManagement::IpHelper::{
            DNS_INTERFACE_SETTINGS_VERSION1, DNS_SETTING_IPV6,
        };
        unsafe {
            let mut s: Settings = std::mem::zeroed();
            s.Version = DNS_INTERFACE_SETTINGS_VERSION1;
            if v6 {
                s.Flags = u64::from(DNS_SETTING_IPV6);
            }
            let rc = get(guid_of(id), &mut s);
            if rc != 0 {
                return Err(format!(
                    "GetInterfaceDnsSettings: {}",
                    std::io::Error::from_raw_os_error(rc as i32)
                ));
            }
            let dns = FamilyDns {
                servers: pwstr_to_string(s.NameServer),
                search: pwstr_to_string(s.SearchList),
            };
            free(&mut s);
            Ok(dns)
        }
    }

    fn set(&self, id: u128, v6: bool, dns: &FamilyDns) -> Result<(), String> {
        let Some(set) = self.set else {
            return Err("SetInterfaceDnsSettings: needs Windows 10 1809 or newer".into());
        };
        use windows_sys::Win32::NetworkManagement::IpHelper::DNS_INTERFACE_SETTINGS_VERSION1;
        let mut servers = crate::win::wide(&dns.servers);
        let mut search = crate::win::wide(&dns.search);
        unsafe {
            let mut s: Settings = std::mem::zeroed();
            s.Version = DNS_INTERFACE_SETTINGS_VERSION1;
            s.Flags = flags(v6);
            s.NameServer = servers.as_mut_ptr();
            s.SearchList = search.as_mut_ptr();
            let rc = set(guid_of(id), &s);
            if rc != 0 {
                return Err(format!(
                    "SetInterfaceDnsSettings: {}",
                    std::io::Error::from_raw_os_error(rc as i32)
                ));
            }
        }
        Ok(())
    }
}

/// Last resort for a note the core could not be handed (its file failed): the Windows Application log, source
/// "AmneziaWG UI Dark". The engine's own ring log is not used: its format belongs to `tunnel.dll`.
pub fn log_to_windows(note: &Note) {
    use windows_sys::Win32::System::EventLog::{
        DeregisterEventSource, RegisterEventSourceW, ReportEventW, EVENTLOG_INFORMATION_TYPE,
        EVENTLOG_WARNING_TYPE,
    };
    unsafe {
        let source = RegisterEventSourceW(
            std::ptr::null(),
            crate::win::wide(crate::APP_TITLE).as_ptr(),
        );
        if source.is_null() {
            return;
        }
        let text = crate::win::wide(&note.text);
        let strings = [text.as_ptr()];
        let kind = if note.warn {
            EVENTLOG_WARNING_TYPE
        } else {
            EVENTLOG_INFORMATION_TYPE
        };
        ReportEventW(
            source,
            kind,
            0,
            0,
            std::ptr::null_mut(),
            1,
            0,
            strings.as_ptr(),
            std::ptr::null(),
        );
        DeregisterEventSource(source);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    const SPLIT: &str = "[Interface]\nAddress = 10.255.252.10/32\nDNS = 10.255.252.1\n[Peer]\nAllowedIPs = 10.255.252.1/32, 8.34.208.0/20\n";

    #[derive(Default)]
    struct Fake {
        present: RefCell<Option<u128>>,
        dns: RefCell<HashMap<(u128, bool), FamilyDns>>,
        sets: RefCell<Vec<(u128, bool, FamilyDns)>>,
        fail_set: RefCell<Option<String>>,
    }

    impl Adapter for Fake {
        fn find(&self, alias: &str) -> Option<u128> {
            assert_eq!(alias, "pc-sky.ams-my.primary.v4.opt");
            *self.present.borrow()
        }
        fn get(&self, id: u128, v6: bool) -> Result<FamilyDns, String> {
            Ok(self
                .dns
                .borrow()
                .get(&(id, v6))
                .cloned()
                .unwrap_or_default())
        }
        fn set(&self, id: u128, v6: bool, dns: &FamilyDns) -> Result<(), String> {
            if let Some(e) = self.fail_set.borrow().clone() {
                return Err(e);
            }
            self.sets.borrow_mut().push((id, v6, dns.clone()));
            self.dns.borrow_mut().insert((id, v6), dns.clone());
            Ok(())
        }
    }

    const ALIAS: &str = "pc-sky.ams-my.primary.v4.opt";

    #[test]
    fn plan_splits_servers_by_family_and_search_domains() {
        let plan = Plan::of("[Interface]\nDNS = 10.255.252.1, fd00::1, 1.1.1.1, corp.example\n");
        assert_eq!(
            plan.v4,
            FamilyDns {
                servers: "10.255.252.1,1.1.1.1".into(),
                search: "corp.example".into()
            }
        );
        assert_eq!(
            plan.v6,
            FamilyDns {
                servers: "fd00::1".into(),
                search: "corp.example".into()
            }
        );
        assert_eq!(
            Plan::of("[Interface]\nAddress = 10.0.0.2/32\n"),
            Plan::default()
        );
    }

    #[test]
    fn split_tunnel_dns_is_set_on_the_tunnel_interface() {
        // The defect: the engine never set `DNS =` on the interface (pc-dev, 2026-10-11, split AllowedIPs).
        let fake = Fake::default();
        let plan = Plan::of(SPLIT);
        let mut k = Keeper::default();
        assert!(
            k.tick(&plan, ALIAS, &fake).is_empty(),
            "no adapter yet — nothing to do, nothing to log"
        );
        assert!(fake.sets.borrow().is_empty());

        *fake.present.borrow_mut() = Some(7);
        let notes = k.tick(&plan, ALIAS, &fake);
        assert_eq!(fake.get(7, false).unwrap().servers, "10.255.252.1");
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(
            !notes[0].warn
                && notes[0].text == trf("dns.set", &[ALIAS, "IPv4", "10.255.252.1", "", ""]),
            "{notes:?}"
        );

        assert!(
            k.tick(&plan, ALIAS, &fake).is_empty(),
            "already applied — no writes, no log"
        );
        assert_eq!(
            fake.sets.borrow().len(),
            1,
            "IPv6 had nothing to set and nothing stale to clear"
        );
    }

    #[test]
    fn dropped_dns_is_re_applied_with_a_warning() {
        let fake = Fake {
            present: RefCell::new(Some(7)),
            ..Default::default()
        };
        let plan = Plan::of(SPLIT);
        let mut k = Keeper::default();
        k.tick(&plan, ALIAS, &fake);
        fake.dns.borrow_mut().remove(&(7, false));
        let notes = k.tick(&plan, ALIAS, &fake);
        assert!(
            notes.len() == 1
                && notes[0].warn
                && notes[0].text == trf("dns.reapplied", &[ALIAS, "IPv4", "10.255.252.1", "", ""]),
            "{notes:?}"
        );
        assert_eq!(fake.get(7, false).unwrap().servers, "10.255.252.1");
    }

    #[test]
    fn a_new_adapter_gets_the_dns_again() {
        let fake = Fake {
            present: RefCell::new(Some(7)),
            ..Default::default()
        };
        let plan = Plan::of(SPLIT);
        let mut k = Keeper::default();
        k.tick(&plan, ALIAS, &fake);
        *fake.present.borrow_mut() = None;
        k.tick(&plan, ALIAS, &fake);
        *fake.present.borrow_mut() = Some(8);
        let notes = k.tick(&plan, ALIAS, &fake);
        assert!(notes.len() == 1 && !notes[0].warn, "{notes:?}");
        assert_eq!(fake.get(8, false).unwrap().servers, "10.255.252.1");
    }

    #[test]
    fn stale_dns_is_cleared_for_a_profile_without_dns() {
        // The engine uses one fixed adapter GUID for every tunnel: a value left by another tunnel must not stay.
        let fake = Fake {
            present: RefCell::new(Some(7)),
            ..Default::default()
        };
        fake.dns.borrow_mut().insert(
            (7, false),
            FamilyDns {
                servers: "10.9.9.9".into(),
                search: String::new(),
            },
        );
        let notes = Keeper::default().tick(&Plan::default(), ALIAS, &fake);
        assert_eq!(fake.get(7, false).unwrap(), FamilyDns::default());
        assert!(
            notes.len() == 1
                && notes[0].text == trf("dns.set", &[ALIAS, "IPv4", "", "10.9.9.9", ""]),
            "{notes:?}"
        );
    }

    #[test]
    fn same_list_in_another_spelling_is_not_rewritten() {
        let fake = Fake {
            present: RefCell::new(Some(7)),
            ..Default::default()
        };
        fake.dns.borrow_mut().insert(
            (7, false),
            FamilyDns {
                servers: "10.255.252.1 1.1.1.1".into(),
                search: String::new(),
            },
        );
        let plan = Plan::of("[Interface]\nDNS = 10.255.252.1, 1.1.1.1\n");
        assert!(Keeper::default().tick(&plan, ALIAS, &fake).is_empty());
        assert!(fake.sets.borrow().is_empty());
    }

    #[test]
    fn unreadable_family_reads_as_empty_and_is_not_rewritten_every_tick() {
        struct NoV6(Fake);
        impl Adapter for NoV6 {
            fn find(&self, alias: &str) -> Option<u128> {
                self.0.find(alias)
            }
            fn get(&self, id: u128, v6: bool) -> Result<FamilyDns, String> {
                if v6 {
                    Err("file not found".into())
                } else {
                    self.0.get(id, v6)
                }
            }
            fn set(&self, id: u128, v6: bool, dns: &FamilyDns) -> Result<(), String> {
                self.0.set(id, v6, dns)
            }
        }
        let a = NoV6(Fake {
            present: RefCell::new(Some(7)),
            ..Default::default()
        });
        let plan = Plan::of(
            "[Interface]
DNS = 10.255.252.1, fd00::1
",
        );
        let mut k = Keeper::default();
        assert_eq!(k.tick(&plan, ALIAS, &a).len(), 2, "both families set");
        assert!(
            k.tick(&plan, ALIAS, &a).is_empty(),
            "IPv6 unreadable after it was set: left alone"
        );
        assert_eq!(a.0.sets.borrow().len(), 2);
    }

    #[test]
    fn a_failing_set_is_logged_once_until_it_changes() {
        let fake = Fake {
            present: RefCell::new(Some(7)),
            fail_set: RefCell::new(Some("access denied".into())),
            ..Default::default()
        };
        let plan = Plan::of(SPLIT);
        let mut k = Keeper::default();
        let first = k.tick(&plan, ALIAS, &fake);
        assert!(
            first.len() == 1
                && first[0].warn
                && first[0].text
                    == trf(
                        "dns.failed",
                        &[ALIAS, "IPv4", "10.255.252.1", "access denied"]
                    ),
            "{first:?}"
        );
        assert!(
            k.tick(&plan, ALIAS, &fake).is_empty(),
            "the same error is not repeated every tick"
        );
        *fake.fail_set.borrow_mut() = None;
        let ok = k.tick(&plan, ALIAS, &fake);
        assert!(
            ok.len() == 1 && ok[0].text == trf("dns.set", &[ALIAS, "IPv4", "10.255.252.1", "", ""]),
            "{ok:?}"
        );
    }

    #[test]
    fn ipv6_servers_go_to_the_ipv6_family_and_mapped_ipv4_to_ipv4() {
        let fake = Fake {
            present: RefCell::new(Some(7)),
            ..Default::default()
        };
        let plan = Plan::of("[Interface]\nDNS = fd00::53, ::ffff:10.0.0.53\n");
        assert_eq!(plan.v4.servers, "10.0.0.53");
        assert_eq!(plan.v6.servers, "fd00::53");
        let notes = Keeper::default().tick(&plan, ALIAS, &fake);
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert_eq!(fake.get(7, true).unwrap().servers, "fd00::53");
        assert_eq!(fake.get(7, false).unwrap().servers, "10.0.0.53");
    }

    #[test]
    fn an_empty_dns_line_asks_for_no_servers() {
        assert_eq!(
            Plan::of("[Interface]\nDNS =\nAddress = 10.0.0.2/32\n"),
            Plan::default()
        );
    }

    /// `keep` against the fake: `stopping` answers from a script, one answer per pass; sleeping is a no-op.
    fn run_keep(fake: &Fake, answers: Vec<Result<bool, String>>) -> Vec<Note> {
        let answers = RefCell::new(answers.into_iter());
        let stopping = || answers.borrow_mut().next().unwrap_or(Ok(true));
        let mut notes = Vec::new();
        keep(
            &Plan::of(SPLIT),
            ALIAS,
            fake,
            &stopping,
            &mut |n: &Note| {
                notes.push(Note {
                    warn: n.warn,
                    text: n.text.clone(),
                })
            },
            &|_| {},
        );
        notes
    }

    #[test]
    fn keeper_ends_when_the_engine_is_stopping_and_never_writes_after_it() {
        // The engine flushes DNS after `StopPending`; a pass after that would put the DNS back on the dying adapter.
        let fake = Fake {
            present: RefCell::new(Some(7)),
            ..Default::default()
        };
        run_keep(&fake, vec![Ok(false)]);
        assert_eq!(fake.sets.borrow().len(), 1, "set while running");
        fake.dns.borrow_mut().clear(); // the engine's flush on stop
        run_keep(&fake, vec![Ok(true), Ok(false)]);
        assert_eq!(
            fake.sets.borrow().len(),
            1,
            "nothing written once the engine is stopping"
        );
    }

    #[test]
    fn unknown_engine_state_is_logged_once_and_dns_is_still_kept() {
        let fake = Fake {
            present: RefCell::new(Some(7)),
            ..Default::default()
        };
        let notes = run_keep(
            &fake,
            vec![Err("access denied".into()), Err("access denied".into())],
        );
        assert_eq!(fake.get(7, false).unwrap().servers, "10.255.252.1");
        let unknown: Vec<_> = notes
            .iter()
            .filter(|n| n.text == trf("dns.state_unknown", &[ALIAS, "access denied"]))
            .collect();
        assert_eq!(unknown.len(), 1, "{notes:?}");
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("awg-ui-tunnel-dns-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn note(warn: bool, text: &str) -> Note {
        Note {
            warn,
            text: text.into(),
        }
    }

    #[test]
    fn notes_reach_the_core_once_and_in_order() {
        let dir = temp_dir("handover");
        let mut inbox = Inbox::new(Some(dir.clone()), 1_000);
        assert!(
            inbox.take().is_empty(),
            "no folder yet — nothing, and no error"
        );
        let mut outbox = Outbox::new(&dir, ALIAS, 2_000);
        outbox.push(&note(false, "set")).unwrap();
        outbox.push(&note(true, "two\nlines")).unwrap();
        let taken = inbox.take();
        assert_eq!(
            taken,
            vec![
                (ALIAS.to_string(), note(false, "set")),
                (ALIAS.to_string(), note(true, "two lines"))
            ]
        );
        assert!(inbox.take().is_empty(), "taken notes are not repeated");
        outbox.push(&note(true, "third")).unwrap();
        assert_eq!(inbox.take(), vec![(ALIAS.to_string(), note(true, "third"))]);

        // The tunnel restarted: a new keeper numbers from 1 again, its notes are all new.
        let mut again = Outbox::new(&dir, ALIAS, 3_000);
        again.push(&note(false, "set again")).unwrap();
        assert_eq!(
            inbox.take(),
            vec![(ALIAS.to_string(), note(false, "set again"))]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_restarted_core_does_not_repeat_notes_of_a_keeper_older_than_itself() {
        let dir = temp_dir("restart");
        let mut outbox = Outbox::new(&dir, ALIAS, 1_000);
        outbox.push(&note(false, "old")).unwrap();
        let mut inbox = Inbox::new(Some(dir.clone()), 5_000);
        assert!(inbox.take().is_empty(), "written before this core started");
        outbox.push(&note(true, "new")).unwrap();
        assert_eq!(inbox.take(), vec![(ALIAS.to_string(), note(true, "new"))]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_outbox_keeps_only_the_recent_notes() {
        let dir = temp_dir("recent");
        let mut outbox = Outbox::new(&dir, ALIAS, 1_000);
        for i in 0..KEEP_NOTES + 5 {
            outbox.push(&note(false, &i.to_string())).unwrap();
        }
        let taken = Inbox::new(Some(dir.clone()), 0).take();
        assert_eq!(taken.len(), KEEP_NOTES);
        assert_eq!(taken[0].1.text, "5");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn notes_of_vanished_tunnels_are_removed_and_the_rest_kept() {
        let dir = temp_dir("prune");
        let mut inbox = Inbox::new(Some(dir.clone()), 0);
        assert!(inbox.prune(&[]).is_empty(), "no folder yet — nothing, and no error");
        for tunnel in ["old-name", "kept", "Running"] {
            Outbox::new(&dir, tunnel, 1_000).push(&note(false, "set")).unwrap();
        }
        std::fs::write(dir.join("readme.txt"), "not a notes file").unwrap();
        assert_eq!(inbox.take().len(), 3);

        let gone = inbox.prune(&["kept".to_string(), "running".to_string(), "new-name".to_string()]);
        assert!(gone.is_empty(), "{gone:?}");
        let mut left: Vec<String> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, ["Running.notes", "kept.notes", "readme.txt"], "only the vanished tunnel's file goes");

        // The tunnel comes back under the same name: its keeper's notes are taken from the start, not skipped.
        let mut again = Outbox::new(&dir, "old-name", 2_000);
        again.push(&note(true, "again")).unwrap();
        assert_eq!(inbox.take(), vec![("old-name".to_string(), note(true, "again"))]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_notes_file_that_cannot_be_removed_is_reported_once() {
        let dir = temp_dir("prune-blocked");
        // A directory named like a notes file: `read_dir` lists it, `remove_file` refuses it.
        std::fs::create_dir_all(dir.join("stuck.notes")).unwrap();
        let mut inbox = Inbox::new(Some(dir.clone()), 0);
        let first = inbox.prune(&[]);
        assert!(first.len() == 1 && first[0].0 == "stuck" && first[0].1.warn, "{first:?}");
        assert!(inbox.prune(&[]).is_empty(), "the same file is not reported every tick");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_notes_folder_is_reported_once() {
        let dir = temp_dir("blocked");
        std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
        std::fs::write(&dir, "a file where the folder should be").unwrap();
        let mut inbox = Inbox::new(Some(dir.clone()), 0);
        let first = inbox.take();
        assert!(
            first.len() == 1 && first[0].0.is_empty() && first[0].1.warn,
            "{first:?}"
        );
        assert!(
            inbox.take().is_empty(),
            "the same error is not repeated every tick"
        );
        let _ = std::fs::remove_file(&dir);
    }
}

#[cfg(test)]
mod windows_tests {
    use super::*;

    #[test]
    fn real_api_reads_without_writing() {
        // Read-only check of the FFI layout against the live system: a missing alias is `None`, the loopback's DNS
        // (if its alias is the English one on this machine) reads without error.
        let api = Windows::load();
        assert_eq!(api.find("awg-ui-no-such-adapter"), None);
        if let Some(id) = api.find("Loopback Pseudo-Interface 1") {
            // IPv6 is not read: the loopback has no IPv6 DNS key ("file not found"), which `Keeper` reads as empty.
            api.get(id, false).expect("IPv4 DNS of the loopback");
        }
    }
}
