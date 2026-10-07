<p align="right"><b>English</b> | <a href="structure.ru.md">Русский</a></p>

# Project structure

Related: [architecture](architecture.md), [modules](modules.md).

## Repository root

```
.
├── src/             program source (below)
├── tests/fixtures/  signed-manifest samples used by the update tests (src/update)
├── engine/          scripts that build and check the built-in engine (tunnel.dll, wintun.dll)
├── assets/icon/     PNG icons, one per size; build.rs embeds them in the exe
├── img/             screenshots for the README
├── docs/            this documentation
├── build.rs         embeds the icon and version info, pins the engine file hashes
├── Cargo.toml       package and dependencies (Cargo.lock pins them)
├── .cargo/          build flags: static C runtime, no VC++ Redistributable needed
├── .github/         CI, release and upstream-check workflows, Dependabot
├── README.md        user documentation (README.ru.md in Russian)
├── CHANGELOG.md     version history (CHANGELOG.ru.md in Russian)
└── CONTRIBUTING.md, LICENSE
```

## `src/`

Top-level files, by layer. Layers are explained in [modules](modules.md).

| Path | Purpose |
|---|---|
| `main.rs` | Entry point: picks the role from the command line (window, core, agent, tunnel service, helpers, install and status commands) |
| `app.rs`, `app/` | The window (egui): state, tables, dialogs, menus; see below |
| `monitor.rs` | One-second poll: tunnel list and state, graph history, transition events, tray state. In the core it polls the tunnels (`poll`, `core_state`); in the window it mirrors the core state (`spawn`) and the agent state (`spawn_agent`); `Stamp` records since when the window sees the core and the agent in their current state (for the diagnostics report) |
| `daemon/` | The core service, the agent and their protocols; see below |
| `update/` | Updates and rollbacks of three components; see below |
| `backend.rs` | `TunnelHost`: tunnel start, stop and state for mode 1 (`Real`), mode 2 (`EngineHost`) and the demo (`Demo`) |
| `engine.rs` | Mode 2: tunnel services, engine file install and verification |
| `store.rs` | Mode 2 config store (DPAPI-encrypted files) |
| `archive.rs` | Import from zip and password-protected backup archives |
| `native.rs` | Driving the original AmneziaWG window through UI Automation |
| `uapi.rs` | Tunnel state over the WireGuard UAPI pipe |
| `conf.rs`, `conf/check.rs` | Tunnel details parsed from a `.conf` file; config check with the engine's parser rules (keys, addresses, Endpoint, AmneziaWG parameters), shown under the editor with line numbers |
| `scm.rs` | Windows service control behind one interface, with a fake for tests |
| `win.rs` | Windows specifics: administrator check, title bar and window border per theme, manager service, autostart, single instance |
| `elevated.rs` | Start our exe elevated (UAC) and get its result back |
| `events.rs` | Event log: model, in-memory ring, `events.log` file with rotation |
| `stats.rs` | Cumulative per-tunnel statistics model and `Stats.ini` format |
| `ping.rs` | ICMP ping through the VPN and its state; ping host check (`valid_host`) |
| `groups.rs` | Tunnel book: nested groups, assignments, selection |
| `settings.rs` | Window settings (`Settings.ini`), the working-mode type and the window theme (`Theme`: `graphite`, `slate`, `daylight`, `system`; unknown value → `graphite`) and the graph range (`GraphRange`, `[layout] graph_range`: `2m`, `10m`, `1h`, `day`, `month`, `year`; unknown value → `2m`; the 0.5.3 key `graph_period` in seconds is read once) |
| `health.rs` | Tunnel state to level (dot color, tray icon, events) and text |
| `i18n.rs` | Localization: English default, Russian built in, other languages from `lang\*.lng` |
| `ini.rs` | Minimal INI reader and writer |
| `fmt.rs` | Number and time formatting |
| `fsutil.rs` | Atomic write, error text with a path, name checks, bounded waiting |
| `crash.rs` | Panic hook, isolated execution, lock recovery, non-fatal thread loops |
| `tray.rs`, `taskbar.rs`, `icon.rs`, `shortcut.rs` | Tray icon and notifications, taskbar button and overlay badge, program icon, desktop shortcut |

### `src/app/` (window)

| File | Purpose |
|---|---|
| `window.rs` | Window state: size and position, closing, settings saving, scaling, `--snapshot` |
| `list.rs`, `list/rows.rs` | Tunnel table: columns, keyboard, group and tunnel rows, menus, drag and drop; the primary action of a tunnel (`Primary`: button, menus, tray, double click and `Enter` only connect) and the disconnect confirmation rule (`asks_first`) |
| `details.rs`, `graph.rs`, `status.rs` | Selected tunnel details (right click on a value: Copy; private and preshared keys are never shown or copied), speed graph with ping strip (speed scale on the right; pause button: a frozen snapshot of the graph and the ping strip for the window session, `GraphPause`, dropped when another tunnel or a range with another source is shown; range drop-down: 2 min, 10 min and 1 h from the window's samples, Day, Month and Year from the agent's history), status bar |
| `history_feed.rs`, `history_graph.rs` | Speed history for the graph's Day, Month and Year ranges: `HistoryFeed` asks the agent (`AgentApi::history`) in a background thread at once on a tunnel or range change, then once a minute while shown; the frame only reads the last answer. `history_graph` draws averages as lines and peaks as thin lines, a gap where an interval is missing, a time axis in local hours (Day), local dates (Month) or UTC months (Year), a tooltip with the interval time, averages, peaks and connected time; "Loading history…", "No history yet" or "History unavailable" (reason in the tooltip) instead of the curves |
| `event_log.rs` | Event log panel: severity and tunnel filter, search, copy, "Save as…"; the filter is a pure function (`visible`) |
| `menu.rs`, `theme.rs` | Menu bar with its keyboard (`MenuNav`: F10, lone Alt, Alt+letter mnemonics, arrows and submenus, command shortcuts; window filter for `SC_KEYMENU`), the one context-menu helper (right click, Shift+F10, Menu key); theme palettes (`Palette` by role: Graphite, Slate, Daylight; the active one from `palette()`, `resolve` turns "follow Windows" into Daylight or Graphite, `Palette::visuals` builds the egui look) and fonts |
| `a11y.rs` | Screen reader names for custom-painted elements (tunnel and group rows, speed graph, ping strip, status dots): one helper `describe` |
| `diagnostics.rs` | Help → Copy diagnostics: a plain-text report (versions, mode, Windows, core and agent status, tunnel states, last 50 events, component versions) built by a pure function and passed through one scrub that removes keys, addresses, bare host names and the host of any URL (`scheme://host…` keeps scheme and path, the query and fragment become `?<query>` / `#<fragment>`; a `label.label` token with an alphabetic last label, except file names with known extensions, the app's own hosts and tunnel names), user and computer names and the profile path |
| `dialog.rs`, `modals.rs`, `group_dialog.rs`, `about.rs`, `editor.rs` | The shared dialog-window constructor and the dialogs built with it (the About window has the **Copy diagnostics** button) |
| `settings_dialog.rs` | Settings window: a draft of every option in sections (General with the theme combo, Notifications and tray, Network, Working mode), OK / Cancel / Apply (`dialog::dialog_ok_cancel_apply`), Reset to defaults with a confirmation; applied through `push_options`, `Action::Autostart` and `Action::ChooseMode` |
| `core_ui.rs` | Core install and update strip, install through one UAC prompt, mode from the core, window look per mode and theme (`apply_look`) |
| `engine_mode.rs` | Mode 2 in the window: mode switch, import, take over from AmneziaWG, backup, new tunnel, rename |
| `updates.rs`, `reminder.rs`, `markdown.rs` | "Updates and rollbacks" window, update reminder timing, release notes rendering |
| `native_reopen.rs` | Reopens the AmneziaWG window an AmneziaWG update or restore closed, by the agent's `native_ui` mark |
| `sources.rs`, `watcher.rs` | Source `.conf` files opened for editing and the config details cache |
| `exit.rs`, `errors.rs` | Exit with the "keep the VPN" question, action errors into the event log |
| `tray_menu.rs` | Tray menu with the tunnels (groups as submenus) and its click: the same switch request and the same primary action (`list::Primary`) as the window; a disconnect is confirmed in the window first (`list::asks_first`); without the core a note on top, tunnels unchecked and grey; a click on a tunnel notification selects the tunnel |
| `demo_core.rs` | Fake core for `--demo` |

### `src/daemon/` (core and agent)

| File | Purpose |
|---|---|
| `mod.rs` | Core constants, `Config` (`core.ini`), the `CoreApi` trait, the core fence test |
| `service.rs` | Windows service entry: dispatcher, status, stop and shutdown |
| `server.rs` | Core runtime: threads, pipe server, request handling, switching, mode change, hold and release |
| `retry.rs`, `deadwatch.rs`, `restore.rs`, `netwatch.rs` | Reconnect schedule, dead-tunnel restarts, desired-set rules, network-change signal |
| `agent_watch.rs` | Agent watchdog: Job object, restart backoff, `Hello` probes |
| `pipe.rs`, `proto.rs` | Core pipe (access rules, deadlines) and the window-to-core protocol |
| `budget.rs` | Connection slots (user, SYSTEM, reserve for `Hello`) shared by the core and the agent pipes |
| `install.rs` | Install, upgrade and removal of the core service |
| `session.rs`, `helper.rs` | Starting the helper in the user's session and the helper itself |
| `fake.rs` | Fake core for tests |
| `phase_trace.rs` | Test-only timing trace of the Switch path; empty in a non-test build (see [development](development.md#flaky-test-logs)) |
| `agent/mod.rs` | Agent process: pipe server, request dispatch |
| `agent/proto.rs`, `agent/client.rs` | Agent protocol; window-side client and request routing |
| `agent/core_poll.rs` | One-second poll of the core state shared by the language follower, statistics, speed history and the journal |
| `agent/language.rs` | The agent's language: the core's `language` from `core.ini`, re-read when the file changes |
| `agent/journal.rs` | Writes `events.log` |
| `agent/stats.rs`, `agent/ping.rs`, `agent/settings.rs` | Statistics, ping, `agent.ini` |
| `agent/history.rs` | Per-tunnel speed history (day, month and year series) and `history.bin` |
| `agent/updates.rs` | Starts the update manager and forwards its log to the journal |
| `agent/tunnels.rs` | Config requests: mode 2 store and the helper for the original window |

### `src/update/` (updates and rollbacks)

| File | Purpose |
|---|---|
| `mod.rs` | Update operations and the state shown in the window |
| `manager.rs` | Manager: checks, backup before each update and rollback, install, history, daily check |
| `component.rs`, `component/native.rs` | Per-component strategy: original AmneziaWG (MSI), engine, app; MSI under a tunnel hold |
| `native.rs` | Original AmneziaWG: installed version, installer signature check, install and removal |
| `ours.rs`, `ours/` | Our releases: download, verification, file-set transaction, engine and app backups, `--restart-core` and rollback of the set |
| `sources.rs`, `feed.rs`, `net.rs` | Update sources, GitHub releases, HTTPS download over WinHTTP |
| `sign.rs` | Signed release manifest (Ed25519, SSHSIG) and SHA-256 |
| `history.rs`, `backup.rs`, `jsonstore.rs`, `restore_target.rs` | History model, backup folders, JSON files with atomic write, what a "Restore" button restores |
| `rows.rs`, `busy.rs`, `clock.rs`, `engine_tag.rs` | Window rows model, running-job model, scheduler clock, engine versions shared with `build.rs` |
| `core_link.rs`, `core_link/pipe_core.rs` | What updates ask of the core (`HoldNative`, `Release`, `ReconnectEngine`) and its pipe implementation |
