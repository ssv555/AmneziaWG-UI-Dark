<p align="right"><b>English</b> | <a href="protocols.ru.md">Русский</a></p>

# Protocols: core pipe and agent pipe

The window never touches tunnels itself. It talks to two background processes over two local named pipes:

| Pipe | Name | Served by | Modules |
|---|---|---|---|
| Core | `\\.\pipe\AmneziaWG-UI-Dark-Core` | the core service `AwgUiCore` (LocalSystem): tunnels, mode, supervision | `src/daemon/server.rs`, `src/daemon/proto.rs` |
| Agent | `\\.\pipe\AmneziaWG-UI-Dark-Agent` | the agent (`awg-ui.exe --agent`, started and watched by the core, runs as SYSTEM): ping, statistics, event log file, tunnel configs, updates | `src/daemon/agent/mod.rs`, `src/daemon/agent/proto.rs` |

Both pipes use the same transport (`src/daemon/pipe.rs`) and differ only in message types. The core and the agent also call each other
(see [Internal calls](#internal-calls)).

## Transport

1. **Framing.** One request per connection: the client writes one line of JSON terminated by `\n`, the server answers with one line of JSON
   and closes the connection. Messages are serde_json externally tagged enums, for example `"Hello"` or
   `{"State":{"events_after":5}}`. A message never contains a raw newline.
2. **Mode.** Byte pipe, overlapped I/O, 64 KiB buffers, remote clients rejected (`PIPE_REJECT_REMOTE_CLIENTS`).
3. **Size limits.** Request at most 8 MiB, response at most 64 MiB. A longer line fails with `pipe: message over N bytes`; a connection closed
   before any byte arrives fails with `pipe: closed`.
4. **Server timeouts.** The request line must arrive within 10 s after the client connects, and the reply must be taken within 10 s. A client
   that connects and stays silent is cut off, so it cannot hold a thread or a connection slot.
5. **Client timeouts** (time to send, time to wait for the reply):

   | Caller and request | Send | Reply |
   |---|---|---|
   | Window to core, all requests | 10 s | 300 s (longer than the longest legitimate request: two actions in the native window, 120 s each) |
   | Window to agent, regular requests (state, events, ping, updates) | 2 s | 5 s |
   | Window to agent, `Tunnel(..)` requests (the helper in the native window runs up to 120 s) | 10 s | 300 s |
   | Agent to core, `State` poll once a second | 2 s | 3 s |
   | Agent to core, the ping's `State` read | 5 s | 15 s |
   | Agent to core, `Hello`, `Forget`, `Footprint` | 10 s | 15 s |
   | Updates manager (in the agent) to core, `HoldNative`, `Release`, `ReconnectEngine` | 10 s | 15 s plus 75 s for each tunnel switch the core may have to do first |
   | Core watchdog to agent, `Hello` | 5 s | 5 s |

6. **Connecting.** The client opens the pipe at the `SECURITY_IDENTIFICATION` level: the server learns who calls but cannot act as the
   caller. When all instances are busy the client waits 500 ms and retries, up to 20 times. A missing or permanently busy pipe is reported as
   "core unavailable".

## Who may connect

1. **Pipe security descriptor** (`pipe_sddl`): `O:SYD:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x12019b;;;<owner SID>)`.
   - SYSTEM and Administrators: full access.
   - The owner account (SID from the `owner_sid` key of `core.ini`, written when the core is installed): only `0x12019b` - read and write of data,
     attributes and extended attributes, `READ_CONTROL`, `SYNCHRONIZE`. No `WRITE_DAC`, `WRITE_OWNER` or
     `FILE_CREATE_PIPE_INSTANCE`: the owner's window works without administrator rights but cannot change the pipe's rights or create its own
     instance with the same name.
   - Nobody else on the machine can open the pipe. An `owner_sid` that is not a valid SID is not put into the descriptor (the pipe stays
     for administrators and SYSTEM) and the rejected text is written to the event log.
2. **The pipe's owner is SYSTEM, and both sides check it.** The server creates the first instance with `FILE_FLAG_FIRST_PIPE_INSTANCE`; if the
   name is already held by another program it reports "name taken" (the update restart does not blame the new build for that, see
   [Updates](updates.md)). After creating every instance the server verifies the object's owner. The client verifies the owner
   before sending anything and refuses a pipe created by someone else, so a look-alike program of the user does not receive requests.
3. **Caller identity.** For every connection the server impersonates the client at identification level only to read its token, then reverts
   at once. It learns the account SID, the Windows session, and whether the token is elevated (UAC) with the Administrators group active.
   These feed the access rules below.
4. **SYSTEM-only requests** (core): `HoldNative`, `Release`, `ReconnectEngine`, `Forget`, `Footprint`. They are accepted only when the client SID is
   `S-1-5-18` (the agent). Anyone else gets `Refused`, and a warning with the caller's SID goes to the event log. Reason: otherwise any
   program of the owner could take tunnels off supervision.
5. **Tunnel names** in a request are checked before the request is handled (`engine::valid_name`; the agent uses the same rule through
   `store::valid_name`). A name ends up in paths and command lines, so a path instead of a name gets `Refused`.
6. **Agent rules.** The agent has no SYSTEM-only requests but has its own checks:
   - `Updates(Restore(..))` only from an elevated client; otherwise `Err`, plus a warning with the caller's SID (an audit entry).
   - A tunnel config with script directives (`PreUp`, `PostUp` and the like, which a tunnel service would run as SYSTEM) is refused. `Write` and `Import` are refused as a whole before the store is touched; `store::import` repeats the check for every path (including `TakeNative`) and names the refused entries in `ImportReport.scripts`, so the window shows them instead of dropping them silently.
   - Actions in the native AmneziaWG window run only when the caller's session and SID are known, in that session, one at a time.

## Connection budgets

1. **Core**: 32 simultaneous connections for the owner's programs and 8 for SYSTEM clients (the agent), plus one reserve slot used only for
   the core's own start-up `Hello` self-check: it is answered even when the 8 are taken, and any other request on that slot is refused. The budgets
   are separate: a hung agent cannot cut the window off, and 32 connections of the owner's programs cannot cut the agent off. Over budget the answer is
   `Refused("The core is busy, try again")`.
2. **Agent**: 16 simultaneous requests for the owner's programs and 4 for SYSTEM clients (the core watchdog's `Hello`), plus one reserve slot answered only for `Hello`. The budgets are separate: silent connections of an owner's program cannot make the watchdog miss `Hello` and kill a live agent. Over budget the answer is `Refused("agent busy")`. The counters are shared with the core (`src/daemon/budget.rs`).
3. Every request runs in its own thread; the slot is returned when the thread ends, also after a panic.
4. A panic inside a handler is isolated: the core logs it and answers `Err`, the agent answers `Refused` with the panic text. The process
   keeps running.
5. **Accept errors.** The core pauses 1 s after the first failure to wait for a client and doubles the pause up to 60 s; the same error is logged when it
   changes and then at most once an hour (a permanent error such as "name taken" would otherwise write 86 400 lines a day). The agent logs the same
   error at most once a minute and pauses 1 s.

## Core pipe

### Requests (`Request`, `src/daemon/proto.rs`)

Grouped by purpose.

1. **Session and state**
   1. `Hello` - core version and mode. Also the core's own liveness probe at start. The window asks it for **Copy diagnostics**.
   2. `State { events_after }` - everything the window shows (`CoreState`) and the events newer than `events_after`. The agent passes `u64::MAX` to
      read the state without events.
   3. `SetLanguage(code)` - window language (ISO 639-2 code); the core writes its event log in it and stores it in `core.ini`.
2. **Tunnel control**
   1. `Switch { tunnel, plan, multiple }` - `plan` is `Connect`, `Disconnect` or `Reconnect`. Without `multiple` the other connected tunnels are
      disconnected; with it only those that cannot run together (same address, or both route all traffic). A user command also updates
      the desired set (see [Supervisor](supervisor.md)).
   2. `Retry(tunnel)` - restart the reconnect schedule of a desired tunnel from the beginning.
   3. `SetMode(mode)` - switch between mode 1 (`Overlay`, on top of an installed AmneziaWG) and mode 2 (`Engine`, built-in). The tunnels of the
      previous mode are disconnected, the desired set is emptied, the window is not restarted.
3. **Mode 2 storage operations kept in the core** (they touch the tunnel service and the desired set under the core's `switching` lock):
   1. `Delete(tunnel)` - mode 2 only; in mode 1 the answer is `Refused` (moved to the agent).
   2. `Rename { old, new }` - mode 2, tunnel not running; otherwise an error.
4. **Moved to the agent.** `Read`, `Write`, `Details`, `Import`, `ExportAll`, `NewTunnel`, `TakeNative` and `Native(NativeOp)` stay in the enum so that
   an older window gets an explanation instead of a parse error: the core answers `Refused` ("now handled by the secondary service; restart the
   window"). `SetPing { enabled, host }` and `Updates(..)` are refused the same way, each with its own text.
5. **Internal, SYSTEM only** (the agent is the only caller):
   1. `HoldNative { lease_s }` and `Release { tunnels }` - take mode 1 tunnels off supervision for the time of an AmneziaWG installation and give them
      back; which tunnels is decided by the core, the answer is `Held(tunnels)` (see [Supervisor](supervisor.md)).
   2. `ReconnectEngine` - the engine was replaced: reconnect the mode 2 tunnels on the next supervisor tick; the answer is immediate.
   3. `Forget(tunnel)` - the agent removed a mode 1 tunnel in the native window: drop it from the desired set and forget its details.
   4. `Footprint { tunnel, info }` - details of a mode 1 tunnel read from the native window (`None` - the config changed, old details are stale);
      the core uses them to find conflicts when connecting.

`NativeOp` (inside `Native` and the agent's `TunnelRequest::Native`): `Open`, `Edit(name)`, `Import(optional file)`, `Close`.

### Responses (`Response`)

The core's handler returns only the first six variants below; the others stay in the enum and the current core does not produce them.

1. `Ok` - done.
2. `Err(text)` - the request was executed and failed. For `Switch` the core has already written the error to its event log.
3. `Refused(text)` - the core did not take the request (busy, unreadable line, unknown variant, bad name, moved, SYSTEM-only) and did not log it
   as an action. The client must show the refusal itself, otherwise a click in the window would leave no trace.
4. `Hello { version, mode }`.
5. `State(CoreState)`.
6. `Held(tunnels)` - the answer to `HoldNative`: the tunnels the core took off supervision (empty in mode 2); the agent returns exactly these with `Release`.
7. `Text`, `Info`, `Entries`, `Report`, `Updates` - payload shapes shared with the agent.

`CoreState` carries `mode`, `tunnels`, `running` (per tunnel: the status read from the tunnel's own pipe, or an error text), `error`, `events` with
their numbers, `events_instance` (changes on every core start, so the window notices a restart), `events_loaded`, `service` (the AmneziaWG manager
service, mode 1), `busy` (tunnels being switched or held), `retries` (per tunnel: `attempt`, `next_in_s`, `last_error`, `slow`) and `agent`
(`Starting`, `Up { pid }`, `Down { since, last_exit }` where the exit is `Code`, `Hung` or `NotStarted`; a start-grace timeout is sent as `NotStarted` with the reason text, so a released window decodes it). `stats` and `ping` are always empty in the
current core.

## Agent pipe

### Requests (`AgentRequest`, `src/daemon/agent/proto.rs`)

1. **State**
   1. `Hello` - agent version; the core's watchdog uses it as the liveness probe. The window asks it for **Copy diagnostics**.
   2. `State` - `AgentState { ping, stats }`; the window asks once a second next to the core's `State`.
   3. `Events { after }` - the agent's event log (core events and its own) newer than a number: history when the window opens, live
      events afterwards. The answer carries `instance`, `loaded` and `core` (how far the core's log is already contained, so the window does not
      show an event twice).
2. **Ping and statistics**
   1. `SetPing { enabled, host }` - stored in `agent.ini`; a failed write is an `Err` with the path.
   2. `StatsRename { old, new }` and `StatsForget(tunnel)` - the window reports a rename or a removal after the core has answered.
3. **Updates.** `Updates(UpdateOp)` with `State`, `Check`, `Apply(list of component and confirmed version)` and `Restore(history row)`; the answer is
   `Updates(UpdatesState)`. See [Updates](updates.md).
4. **Tunnel configs and the native window.** `Tunnel(TunnelRequest)`: `Read`, `Write`, `Details`, `Delete` (mode 1 only; deleting a mode 2 tunnel stays in
   the core), `Import`, `ExportAll`, `NewTunnel`, `TakeNative` (the last three mode 2 only) and `Native(NativeOp)` (mode 1 only). The agent asks the core for
   the mode (`Hello`) and refuses what does not fit it. Mode 2 `Write` saves only an existing tunnel: renamed or deleted by the core meanwhile -
   `Err`, nothing written (store lock: see [architecture](architecture.md)).

### Responses (`AgentResponse`)

`Hello { version }`, `Ok`, `Err(text)`, `Refused(text)` (unreadable line, unknown variant - the serde message names it - busy, bad name, refused
script directive, no updates manager), `State`, `Events`, `Updates`, `Text`, `Info`, `Entries`, `Report`.

## Internal calls

1. **Core to agent**: only `Hello`, from the watchdog (`src/daemon/agent_watch.rs`), every 5 s with a 5 s timeout. Once the agent has answered, three misses in a row mean it
   hangs; it is killed and started again. A fresh agent has a 60 s start grace: until its first answer a missing pipe is not a miss (a first run of a
   just-updated exe can be slowed by antivirus scanning); no answer within 60 s kills it and the journal says it did not start (`NotStarted`, not `Hung`). After an exit or a kill the restart waits 1 s, doubling up to 60 s (the series resets after 5 minutes of
   stable work). The process lives in a job object with a 512 MiB memory limit. The watchdog takes none of the core's locks, and an agent that is down
   never affects tunnels.
2. **Agent to core**: `State` once a second (statistics and the event log by cursor), `Hello` for the mode, `Forget` and `Footprint` after actions in the
   native window, `HoldNative`, `Release` and `ReconnectEngine` from the updates manager. The agent runs as SYSTEM, which is why the core accepts them.

## Compatibility rules

The window, the core and the agent are updated at different moments (the window updates itself after the core), so adjacent versions must
understand each other. The rules, with tests in `src/daemon/proto.rs` and `src/daemon/agent/proto.rs`:

1. **Never rename or remove an enum variant or a field** that a released window sends or reads. Test
   `requests_of_a_0_4_0_window_still_decode` keeps the request lines of window 0.4.0 verbatim; `state_of_a_0_4_0_core_still_decodes` does the same
   for the core's `State`.
2. **New fields are `#[serde(default)]`.** In `CoreState`: `stats`, `ping`, `events_instance`, `events_loaded`, `retries`, `agent`. The whole
   `AgentState` and `AgentEvents` structs are `#[serde(default)]`. A peer that does not send a field yields an empty value, not an unreadable answer.
3. **Unknown fields are ignored** (no `deny_unknown_fields`): an older window reads a newer state.
4. **A field the current side no longer fills stays in the type.** The core still sends an empty `stats` and `ping`, because a window of the
   previous version cannot parse the answer without them.
5. **A request moved to another process stays in the enum and is refused with an explanation** (`core.moved_to_agent`, `core.updates_moved`,
   `core.ping_moved`), so an old window shows "restart the window" instead of "parse error".
6. **An unreadable request or an unknown variant** gets `Refused` carrying the parser's message, from the core and from the agent alike (the
   agent's message names the unknown variant).
7. Messages are single-line: tests check that no serialized message contains `\n`, including text fields with line breaks inside.
