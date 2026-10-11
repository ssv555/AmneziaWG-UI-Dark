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
//! puts them into the app's event log; what the core took is recorded in [`TAKEN_FILE`] next to them, so a core restart
//! neither repeats nor loses notes. Only if that file cannot be written does a note go to the Windows Application log.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::i18n::trf;

/// Poll period: the adapter appears within a second of the start; a dropped setting is restored within it.
pub const TICK: Duration = Duration::from_secs(2);

/// Passes after the first write during which a wiped setting is put back without a log line: the engine finishes the
/// adapter about 2 s after it appears and wipes our first write (seen on pc-dev at every connect). Later a wipe is
/// news (another tunnel or a Windows component reset it) and is logged as a warning.
pub const SETTLE_TICKS: u32 = 5;

/// After the settle window a re-apply is a warning at most once per this many passes (10 minutes) per family; the
/// re-applies in between are counted and the count goes into the next warning (or into [`Keeper::finish`]). Without
/// the limit, a component that fights over the DNS would write one warning per [`TICK`].
pub const REAPPLIED_WARN_TICKS: u64 = 300;

/// Consecutive passes with the engine's state unreadable after which [`keep`] ends (5 minutes). Without the state the
/// keeper cannot tell a running tunnel from one on its way out, where a write would put DNS back on a dying adapter;
/// a service whose own state stays unreadable that long is broken beyond what the keeper can mend. The DNS already set
/// stays; the end is logged as a warning.
pub const STATE_ERROR_LIMIT: u32 = 150;

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

/// Keeper state between ticks: what was applied to which adapter and the last error per family (logged once until it
/// changes; per family, so an IPv4 and an IPv6 error do not take turns and repeat every tick).
#[derive(Default)]
pub struct Keeper {
    applied: [Option<u128>; 2],
    /// Passes since the first write to the current adapter, per family (saturates; compared with [`SETTLE_TICKS`]).
    since_set: [u32; 2],
    last_error: [Option<String>; 2],
    /// Passes with the adapter present (saturates): the clock of the re-apply warnings' limit.
    ticks: u64,
    reapplied: [Reapplied; 2],
}

/// Rate limit of one family's re-apply warnings ([`REAPPLIED_WARN_TICKS`]).
#[derive(Default)]
struct Reapplied {
    warned_at: Option<u64>,
    suppressed: u32,
}

impl Keeper {
    /// One pass: find the adapter, compare each family with the plan, set what differs. Returns messages to log.
    pub fn tick(&mut self, plan: &Plan, alias: &str, adapter: &dyn Adapter) -> Vec<Note> {
        let mut notes = Vec::new();
        let Some(id) = adapter.find(alias) else {
            self.applied = [None, None];
            return notes;
        };
        self.ticks = self.ticks.saturating_add(1);
        for v6 in [false, true] {
            let slot = usize::from(v6);
            self.since_set[slot] = self.since_set[slot].saturating_add(1);
            let want = plan.family(v6);
            let family = if v6 { "IPv6" } else { "IPv4" };
            let applied = self.applied[slot] == Some(id);
            let have = match adapter.get(id, v6) {
                Ok(have) => have,
                // No DNS key for the family (Windows answers "file not found") reads as empty: before the first apply,
                // and for a family the plan leaves empty (a missing key is exactly what it asks for).
                Err(_) if !applied || is_empty(want) => FamilyDns::default(),
                // After the apply the key of a non-empty plan must exist: unreadable means a reset deleted it or the
                // API fails. The set is idempotent, so it runs every pass; the read error is logged once.
                Err(e) => {
                    let text = match adapter.set(id, v6, want) {
                        Ok(()) => trf("dns.unreadable", &[alias, family, &want.servers, &e]),
                        Err(set) => trf("dns.failed", &[alias, family, &want.servers, &set]),
                    };
                    self.note_error_once(slot, &mut notes, text);
                    continue;
                }
            };
            if normalize(&have) == normalize(want) {
                self.applied[slot] = Some(id);
                continue;
            }
            if let Err(e) = adapter.set(id, v6, want) {
                let text = trf("dns.failed", &[alias, family, &want.servers, &e]);
                self.note_error_once(slot, &mut notes, text);
                continue;
            }
            self.applied[slot] = Some(id);
            if !applied {
                self.since_set[slot] = 0;
            } else if self.since_set[slot] <= SETTLE_TICKS {
                self.last_error[slot] = None;
                continue;
            }
            if is_empty(want) && is_empty(&have) {
                continue;
            }
            if applied {
                notes.extend(self.reapplied_note(slot, alias, family, want, &have));
            } else {
                notes.push(Note {
                    warn: false,
                    text: changed_text(alias, family, want, &have, false),
                });
            }
            self.last_error[slot] = None;
        }
        notes
    }

    /// A re-apply after the settle window: a warning, unless one for this family went out less than
    /// [`REAPPLIED_WARN_TICKS`] passes ago — then it is only counted, and the count rides on the next warning.
    fn reapplied_note(
        &mut self,
        slot: usize,
        alias: &str,
        family: &str,
        want: &FamilyDns,
        had: &FamilyDns,
    ) -> Option<Note> {
        let now = self.ticks;
        let r = &mut self.reapplied[slot];
        if r.warned_at.is_some_and(|at| now - at < REAPPLIED_WARN_TICKS) {
            r.suppressed += 1;
            return None;
        }
        let mut text = changed_text(alias, family, want, had, true);
        if r.suppressed > 0 {
            text.push_str(&trf("dns.reapplied_more", &[&r.suppressed.to_string()]));
        }
        *r = Reapplied {
            warned_at: Some(now),
            suppressed: 0,
        };
        Some(Note { warn: true, text })
    }

    /// The keeper ends: re-applies counted under the warning limit and not reported yet go out as one line per family.
    pub fn finish(&mut self, alias: &str) -> Vec<Note> {
        let mut notes = Vec::new();
        for (slot, family) in [(0, "IPv4"), (1, "IPv6")] {
            let r = &mut self.reapplied[slot];
            if r.suppressed > 0 {
                let count = r.suppressed.to_string();
                notes.push(Note {
                    warn: true,
                    text: trf("dns.reapplied_tail", &[alias, family, &count]),
                });
                r.suppressed = 0;
            }
        }
        notes
    }

    /// An error is logged once until it changes: the keeper retries every tick, the log must not.
    fn note_error_once(&mut self, slot: usize, notes: &mut Vec<Note>, text: String) {
        if self.last_error[slot].as_deref() != Some(&text) {
            notes.push(Note {
                warn: true,
                text: text.clone(),
            });
            self.last_error[slot] = Some(text);
        }
    }
}

fn is_empty(d: &FamilyDns) -> bool {
    d.servers.is_empty() && d.search.is_empty()
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
/// an error there is logged once and the keeper goes on, DNS is what the profile asked for, for at most
/// [`STATE_ERROR_LIMIT`] passes in a row). Then it returns: the engine flushes DNS and deletes the adapter, and a
/// pass in between would put the DNS back on a dying adapter. Re-applies still counted under the warning limit are
/// logged on the way out ([`Keeper::finish`]).
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
    let mut state_failures: u32 = 0;
    loop {
        let mut notes = Vec::new();
        match stopping() {
            Ok(true) => break,
            Ok(false) => {
                state_error = None;
                state_failures = 0;
            }
            Err(e) => {
                state_failures += 1;
                if state_failures >= STATE_ERROR_LIMIT {
                    let minutes = ((TICK * STATE_ERROR_LIMIT).as_secs() / 60).to_string();
                    log(&Note {
                        warn: true,
                        text: trf("dns.state_gave_up", &[alias, &minutes, &e]),
                    });
                    break;
                }
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
    for note in keeper.finish(alias) {
        log(&note);
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

/// The core's record of what it has taken, in [`notes_dir`]: `<tunnel>\t<keeper instance>\t<last note>` per line. Not
/// a `.notes` file, so [`Inbox::take`] and [`Inbox::prune`] never treat it as a keeper's. Tunnel names are Windows file
/// names and hold no tabs or line breaks.
pub const TAKEN_FILE: &str = "taken.core";

/// The core's side: reads every keeper's file and returns the notes no core has taken yet.
///
/// What was taken survives a core restart in [`TAKEN_FILE`]. At boot the tunnel services (auto-start) and their
/// keepers can start before the core, and a keeper logs a failure once until its text changes: a rule "skip what was
/// written before this core started" lost that warning forever (audit 2026-10-11, item 2). With the record, a keeper
/// the previous core never saw is new in full, and one it saw continues after its last taken note.
pub struct Inbox {
    /// The keepers' folder ([`notes_dir`]); `None` — not read (cores of tests).
    dir: Option<PathBuf>,
    /// The core's start (ms since the epoch). Used only without a record of the previous core (first start of this
    /// version, record unreadable): a keeper older than this core may have had its notes taken by the previous core,
    /// so on first sight its information notes are skipped and its warnings taken — a repeated warning costs less
    /// than a lost one.
    started: u64,
    /// Tunnel → (keeper instance, last note taken).
    seen: HashMap<String, (u64, u64)>,
    /// `seen` came from [`TAKEN_FILE`]: a keeper missing from it wrote notes no core took.
    seen_loaded: bool,
    /// `seen` changed since [`TAKEN_FILE`] was last written.
    dirty: bool,
    /// Tunnels whose `.notes` files the last [`Inbox::take`] listed: the only files [`Inbox::prune`] may remove.
    listed: Vec<String>,
    /// Tunnel → (error, reported) of a notes file that did not read on the last take.
    unreadable: HashMap<String, (String, bool)>,
    /// Files of vanished tunnels that [`Inbox::prune`] could not remove (reported once each).
    unremovable: HashSet<String>,
    last_error: Option<String>,
    /// Last error of [`TAKEN_FILE`] (reported once until it changes).
    record_error: Option<String>,
    /// Notes for the next take: the record could not be read when the core started.
    queued: Vec<(String, Note)>,
}

impl Inbox {
    pub fn new(dir: Option<PathBuf>, started: u64) -> Inbox {
        let mut inbox = Inbox {
            dir,
            started,
            seen: HashMap::new(),
            seen_loaded: false,
            dirty: false,
            listed: Vec::new(),
            unreadable: HashMap::new(),
            unremovable: HashSet::new(),
            last_error: None,
            record_error: None,
            queued: Vec::new(),
        };
        inbox.load_taken();
        inbox
    }

    fn load_taken(&mut self) {
        let Some(path) = self.dir.as_ref().map(|d| d.join(TAKEN_FILE)) else {
            return;
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                for line in text.lines() {
                    let mut parts = line.splitn(3, '\t');
                    let (Some(tunnel), Some(instance), Some(seq)) = (parts.next(), parts.next(), parts.next()) else {
                        continue;
                    };
                    if let (Ok(instance), Ok(seq)) = (instance.parse(), seq.parse()) {
                        self.seen.insert(tunnel.to_string(), (instance, seq));
                    }
                }
                self.seen_loaded = true;
            }
            // First start of this version, or no keeper has written yet.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                let text = crate::fsutil::io_ctx(&path, e);
                self.queued.push((String::new(), Note { warn: true, text: text.clone() }));
                self.record_error = Some(text);
            }
        }
    }

    /// New notes per tunnel, in order. An unreadable folder or record is reported once (as a note with an empty tunnel
    /// name); a notes file that fails to read on two takes in a row once (with the tunnel name): a single failure is a
    /// keeper replacing the file right now, which reads on the next tick.
    pub fn take(&mut self) -> Vec<(String, Note)> {
        let mut out = std::mem::take(&mut self.queued);
        self.listed.clear();
        let Some(dir) = self.dir.clone() else {
            return out;
        };
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return out,
            Err(e) => {
                let text = crate::fsutil::io_ctx(&dir, e);
                if self.last_error.as_deref() != Some(&text) {
                    out.push((String::new(), Note { warn: true, text: text.clone() }));
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
            self.listed.push(tunnel.to_string());
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(e) => {
                    out.extend(self.unreadable_file(tunnel, &path, e));
                    continue;
                }
            };
            self.unreadable.remove(tunnel);
            out.extend(
                self.take_file(tunnel, &text)
                    .into_iter()
                    .map(|n| (tunnel.to_string(), n)),
            );
        }
        let listed = &self.listed;
        self.unreadable.retain(|t, _| listed.contains(t));
        out.extend(self.save_taken());
        out
    }

    /// A notes file did not read: reported on the second failure in a row with the same error, then not again until it
    /// reads or the error changes.
    fn unreadable_file(&mut self, tunnel: &str, path: &Path, e: std::io::Error) -> Option<(String, Note)> {
        let text = crate::fsutil::io_ctx(path, e);
        match self.unreadable.get_mut(tunnel) {
            Some((last, reported)) if *last == text => {
                if *reported {
                    return None;
                }
                *reported = true;
                Some((tunnel.to_string(), Note { warn: true, text }))
            }
            _ => {
                self.unreadable.insert(tunnel.to_string(), (text, false));
                None
            }
        }
    }

    /// Writes [`TAKEN_FILE`] if `seen` changed. A failure is reported once until it changes and retried next tick.
    fn save_taken(&mut self) -> Option<(String, Note)> {
        let dir = self.dir.as_ref()?;
        if !self.dirty {
            return None;
        }
        let mut lines: Vec<String> = self.seen.iter().map(|(t, (i, s))| format!("{t}\t{i}\t{s}\n")).collect();
        lines.sort();
        let path = dir.join(TAKEN_FILE);
        match crate::fsutil::write_atomic(&path, lines.concat().as_bytes()) {
            Ok(()) => {
                self.dirty = false;
                self.record_error = None;
                None
            }
            Err(e) => {
                let text = crate::fsutil::io_ctx(&path, e);
                if self.record_error.as_deref() == Some(&text) {
                    return None;
                }
                self.record_error = Some(text.clone());
                Some((String::new(), Note { warn: true, text }))
            }
        }
    }

    /// Removes the files of tunnels the store no longer has (deleted or renamed while no keeper ran): nothing else
    /// removes them, and the folder would grow with every renamed tunnel. `known` — every tunnel that exists or runs;
    /// the caller passes it only when it has just read both lists, so a failed read never deletes a live tunnel's
    /// file. Only files listed by this tick's [`Inbox::take`] are candidates, and the caller takes before it reads the
    /// lists: a tunnel imported and connected while the lists are read writes its file after the listing and is not
    /// touched (audit 2026-10-11, item 13.3). Names compare case-insensitively like Windows file names. A file that
    /// cannot be removed is reported once as a warning note (with the tunnel name) until it goes or the tunnel returns.
    pub fn prune(&mut self, known: &[String]) -> Vec<(String, Note)> {
        let mut out = Vec::new();
        let Some(dir) = self.dir.clone() else {
            return out;
        };
        for tunnel in std::mem::take(&mut self.listed) {
            if known.iter().any(|k| k.eq_ignore_ascii_case(&tunnel)) {
                self.unremovable.remove(&tunnel);
                continue;
            }
            let path = dir.join(format!("{tunnel}.notes"));
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    self.dirty |= self.seen.remove(&tunnel).is_some();
                    self.unremovable.remove(&tunnel);
                }
                // The keeper's pass or an earlier prune removed it first.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    if self.unremovable.insert(tunnel.clone()) {
                        out.push((
                            tunnel,
                            Note {
                                warn: true,
                                text: crate::fsutil::io_ctx(&path, e),
                            },
                        ));
                    }
                }
            }
        }
        out.extend(self.save_taken());
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
        // `warnings_before`: notes up to `after` are skipped except warnings (see `started`).
        let (after, warnings_before) = match self.seen.get(tunnel) {
            Some(&(seen_instance, seq)) if seen_instance == instance => (seq, false),
            _ if self.seen_loaded || instance >= self.started => (0, false),
            _ => (last, true),
        };
        let entry = (instance, last.max(after));
        if self.seen.insert(tunnel.to_string(), entry) != Some(entry) {
            self.dirty = true;
        }
        notes
            .into_iter()
            .filter(|(s, n)| *s > after || (warnings_before && n.warn))
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
    fn a_drop_while_the_engine_settles_is_re_applied_without_a_log_line() {
        // Seen on pc-dev at every connect: the engine finishes the adapter ~2 s after our first write and wipes it.
        let fake = Fake {
            present: RefCell::new(Some(7)),
            ..Default::default()
        };
        let plan = Plan::of(SPLIT);
        let mut k = Keeper::default();
        assert_eq!(k.tick(&plan, ALIAS, &fake).len(), 1, "the first set is logged");
        fake.dns.borrow_mut().remove(&(7, false));
        assert!(k.tick(&plan, ALIAS, &fake).is_empty(), "expected reset, not news");
        assert_eq!(fake.get(7, false).unwrap().servers, "10.255.252.1");
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
        for _ in 0..SETTLE_TICKS {
            k.tick(&plan, ALIAS, &fake);
        }
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
        assert_eq!(k.tick(&plan, ALIAS, &a).len(), 2, "both families set: no key yet reads as empty");

        // The usual case of a missing IPv6 key: the plan has no IPv6 DNS, a missing key is exactly that — no writes.
        // (A non-empty family unreadable after the apply is set again: see the test after this one.)
        let a = NoV6(Fake {
            present: RefCell::new(Some(7)),
            ..Default::default()
        });
        let plan = Plan::of(SPLIT);
        let mut k = Keeper::default();
        assert_eq!(k.tick(&plan, ALIAS, &a).len(), 1, "IPv4 set");
        assert!(
            k.tick(&plan, ALIAS, &a).is_empty(),
            "IPv6 unreadable and asked empty: left alone"
        );
        assert_eq!(a.0.sets.borrow().len(), 1);
    }

    /// Audit 2026-10-11 item 4: after the first apply an IPv4 read error (key deleted by a reset, a persistent API
    /// error) left the tunnel without DNS for its whole life, silently.
    #[test]
    fn an_unreadable_family_after_the_apply_is_set_again_and_logged_once() {
        struct V4Fails(Fake, RefCell<bool>);
        impl Adapter for V4Fails {
            fn find(&self, alias: &str) -> Option<u128> {
                self.0.find(alias)
            }
            fn get(&self, id: u128, v6: bool) -> Result<FamilyDns, String> {
                if !v6 && *self.1.borrow() {
                    Err("access denied".into())
                } else {
                    self.0.get(id, v6)
                }
            }
            fn set(&self, id: u128, v6: bool, dns: &FamilyDns) -> Result<(), String> {
                self.0.set(id, v6, dns)
            }
        }
        let a = V4Fails(Fake { present: RefCell::new(Some(7)), ..Default::default() }, RefCell::new(false));
        let plan = Plan::of(SPLIT);
        let mut k = Keeper::default();
        assert_eq!(k.tick(&plan, ALIAS, &a).len(), 1, "the first set");
        *a.1.borrow_mut() = true;
        a.0.dns.borrow_mut().clear(); // a reset deleted the key: DNS is gone and unreadable
        let first = k.tick(&plan, ALIAS, &a);
        assert_eq!(a.0.get(7, false).unwrap().servers, "10.255.252.1", "set again");
        let unreadable = trf("dns.unreadable", &[ALIAS, "IPv4", "10.255.252.1", "access denied"]);
        assert!(first.len() == 1 && first[0].warn && first[0].text == unreadable, "{first:?}");
        assert!(k.tick(&plan, ALIAS, &a).is_empty(), "the same error is not repeated every tick");
        assert_eq!(a.0.sets.borrow().len(), 3, "re-applied on every pass while unreadable");
    }

    /// Audit 2026-10-11 item 13.2: one WARN per pass if something fights over the DNS after the settle window.
    #[test]
    fn re_applies_after_the_settle_window_are_rate_limited_with_a_count() {
        let fake = Fake { present: RefCell::new(Some(7)), ..Default::default() };
        let plan = Plan::of(SPLIT);
        let mut k = Keeper::default();
        k.tick(&plan, ALIAS, &fake);
        for _ in 0..SETTLE_TICKS {
            k.tick(&plan, ALIAS, &fake);
        }
        let drop_and_tick = |k: &mut Keeper| {
            fake.dns.borrow_mut().remove(&(7, false));
            k.tick(&plan, ALIAS, &fake)
        };
        let warned = drop_and_tick(&mut k);
        assert!(warned.len() == 1 && warned[0].warn, "{warned:?}");
        assert!(drop_and_tick(&mut k).is_empty(), "within the limit: set again silently");
        assert!(drop_and_tick(&mut k).is_empty());
        for _ in 0..REAPPLIED_WARN_TICKS {
            k.tick(&plan, ALIAS, &fake);
        }
        let next = drop_and_tick(&mut k);
        let text = trf("dns.reapplied", &[ALIAS, "IPv4", "10.255.252.1", "", ""]) + &trf("dns.reapplied_more", &["2"]);
        assert!(next.len() == 1 && next[0].warn && next[0].text == text, "{next:?}");
        assert!(drop_and_tick(&mut k).is_empty());
        assert_eq!(
            k.finish(ALIAS),
            vec![Note { warn: true, text: trf("dns.reapplied_tail", &[ALIAS, "IPv4", "1"]) }],
            "suppressed re-applies are not lost when the keeper ends"
        );
        assert!(k.finish(ALIAS).is_empty());
    }

    /// Audit 2026-10-11 item 13.4: a `stopping()` that fails for good kept the keeper looping blind forever.
    #[test]
    fn keeper_gives_up_when_the_engine_state_stays_unknown() {
        let fake = Fake { present: RefCell::new(Some(7)), ..Default::default() };
        let calls = RefCell::new(0u32);
        let stopping = || {
            *calls.borrow_mut() += 1;
            Err::<bool, String>("access denied".into())
        };
        let mut notes = Vec::new();
        keep(&Plan::of(SPLIT), ALIAS, &fake, &stopping, &mut |n: &Note| notes.push(n.text.clone()), &|_| {});
        assert_eq!(*calls.borrow(), STATE_ERROR_LIMIT, "bounded");
        let minutes = (TICK * STATE_ERROR_LIMIT).as_secs() / 60;
        assert_eq!(notes.last(), Some(&trf("dns.state_gave_up", &[ALIAS, &minutes.to_string(), "access denied"])), "{notes:?}");
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

    /// Audit 2026-10-11 item 2: at boot the tunnel service (and its keeper) starts before the core; the keeper's
    /// once-only warning predates the core and was skipped forever.
    #[test]
    fn a_warning_of_a_keeper_older_than_the_core_is_still_taken() {
        let dir = temp_dir("boot");
        let mut outbox = Outbox::new(&dir, ALIAS, 1_000);
        outbox.push(&note(false, "set")).unwrap();
        outbox.push(&note(true, "not set: access denied")).unwrap();
        let mut inbox = Inbox::new(Some(dir.clone()), 5_000);
        assert_eq!(inbox.take(), vec![(ALIAS.to_string(), note(true, "not set: access denied"))]);
        assert!(inbox.take().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What a core took is remembered on disk: its successor repeats nothing, warnings included.
    #[test]
    fn a_restarted_core_repeats_nothing_the_previous_core_took() {
        let dir = temp_dir("taken");
        let mut outbox = Outbox::new(&dir, ALIAS, 1_000);
        outbox.push(&note(true, "warn")).unwrap();
        assert_eq!(Inbox::new(Some(dir.clone()), 0).take().len(), 1);
        let mut next = Inbox::new(Some(dir.clone()), 5_000);
        assert!(next.take().is_empty(), "taken by the previous core");
        outbox.push(&note(true, "new")).unwrap();
        assert_eq!(next.take(), vec![(ALIAS.to_string(), note(true, "new"))]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A keeper started while no core ran (the previous core's record does not know it): every note of it is new.
    #[test]
    fn a_keeper_no_core_has_seen_is_taken_whole() {
        let dir = temp_dir("unseen");
        Outbox::new(&dir, "a", 500).push(&note(false, "a")).unwrap();
        assert_eq!(Inbox::new(Some(dir.clone()), 0).take().len(), 1);
        let mut b = Outbox::new(&dir, "b", 1_000);
        b.push(&note(false, "set")).unwrap();
        b.push(&note(true, "warn")).unwrap();
        let taken = Inbox::new(Some(dir.clone()), 5_000).take();
        assert_eq!(taken, vec![("b".to_string(), note(false, "set")), ("b".to_string(), note(true, "warn"))]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Audit 2026-10-11 item 13.1: an unreadable notes file was skipped silently on every tick.
    #[test]
    fn an_unreadable_notes_file_is_reported_once() {
        let dir = temp_dir("unreadable");
        // A directory named like a notes file: listed, but cannot be read as a file.
        std::fs::create_dir_all(dir.join("stuck.notes")).unwrap();
        let mut inbox = Inbox::new(Some(dir.clone()), 0);
        assert!(inbox.take().is_empty(), "one failure may be a replace in progress");
        let second = inbox.take();
        assert!(second.len() == 1 && second[0].0 == "stuck" && second[0].1.warn, "{second:?}");
        assert!(inbox.take().is_empty(), "reported once");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Audit 2026-10-11 item 13.3: a tunnel imported and connected while the tick read its lists had its file pruned.
    #[test]
    fn prune_spares_a_file_that_appeared_after_the_tick_listed_the_folder() {
        let dir = temp_dir("prune-race");
        Outbox::new(&dir, "old", 1_000).push(&note(false, "set")).unwrap();
        let mut inbox = Inbox::new(Some(dir.clone()), 0);
        inbox.take();
        // Between the listing and the reads of the tunnel lists: a new tunnel appears and its keeper writes.
        Outbox::new(&dir, "new", 2_000).push(&note(true, "warn")).unwrap();
        assert!(inbox.prune(&[]).is_empty());
        assert!(dir.join("new.notes").exists(), "not in this tick's listing: kept");
        assert!(!dir.join("old.notes").exists());
        assert_eq!(inbox.take(), vec![("new".to_string(), note(true, "warn"))]);
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
        assert_eq!(left, ["Running.notes", "kept.notes", "readme.txt", TAKEN_FILE], "only the vanished tunnel's file goes");

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
        assert!(inbox.prune(&[]).is_empty(), "nothing listed yet: prune works on the tick's listing only");
        inbox.take();
        let first = inbox.prune(&[]);
        assert!(first.len() == 1 && first[0].0 == "stuck" && first[0].1.warn, "{first:?}");
        inbox.take();
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
