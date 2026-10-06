<p align="right"><b>English</b> | <a href="README.ru.md">Русский</a></p>

<h1 align="center">AmneziaWG UI Dark</h1>

<p align="center">
  A dark, fast, unofficial Windows interface for your <a href="https://amnezia.org">AmneziaWG</a> tunnels:
  groups, live status, speed graph, ping, statistics and tray - without touching the original client.
</p>

<p align="center">
  <img alt="Platform: Windows" src="https://img.shields.io/badge/platform-Windows%2010%20%7C%2011-0078D6?logo=windows&logoColor=white">
  <img alt="Rust" src="https://img.shields.io/badge/built%20with-Rust-dea584?logo=rust&logoColor=white">
  <img alt="UI: egui" src="https://img.shields.io/badge/UI-egui-4c8bf5">
  <img alt="License: non-commercial" src="https://img.shields.io/badge/license-non--commercial-orange">
  <img alt="Status: unofficial" src="https://img.shields.io/badge/status-unofficial-lightgrey">
</p>

<p align="center">
  <img src="img/main.png" alt="AmneziaWG UI Dark - main window with nested groups, a connected tunnel and the speed graph" width="900">
</p>

> The screenshots use the built-in demo mode (`--demo`): invented tunnels and statistics, no AmneziaWG and no
> administrator rights required. Try it yourself: `awg-ui.exe --demo`.

## Features

**Tunnel list**
- Nested groups of any depth (`Europe/Netherlands`), shown as a tree or a flat list. A group row shows totals for
  its whole subtree: active / total, downloaded, uploaded, peak speed and time share.
- Drag and drop tunnels and groups, a right-click menu, keyboard navigation, instant search (`Ctrl+F`).
- Screen readers (Narrator, NVDA): tunnel and group rows, the speed graph and status dots are read with their name and state.
- Sortable, resizable columns: downloaded total, uploaded total, peak speed, share of connected time.

**Live status**
- Is there a handshake and how long ago, are packets flowing, download / upload speed, ping through the VPN.
  Numbers use a monospaced font in fixed-width cells, so nothing jumps around when values change.
- Speed graph (2 min / 10 min / 1 h) with a ping strip underneath.
- Per-tunnel statistics since the first launch: traffic, peak speed, time connected and its share of the total.
- Event log: connections, disconnections, lost and restored links; filter by severity and tunnel, search, copy, save to a file.
- If the core stops answering, the tunnels show "Unknown: no connection to the core" instead of a guess, the tray icon and
  the status bar warn, and everything returns by itself once the core answers again.

**Convenience**
- A coloured dot on the tray icon and on the taskbar button (overlay badge) shows the state; Windows notifications when a link
  drops or comes back. Clicking a notification opens the window on that tunnel.
- **Tray menu** (right click on the tray icon): Open window, the tunnels (in their groups when the window shows groups;
  connected ones are checked) and Exit. Clicking a tunnel connects or disconnects it exactly like the window does (disconnecting asks first),
  including the "several tunnels at once" setting. Without a connection to the core the state is unknown: the menu
  says so on top, no tunnel is checked and none can be clicked (the window shows "Unknown: no connection to the core"
  and greys out the Connect / Disconnect button).
- **The working mode is visible at a glance**: in the built-in engine mode the icon (window, taskbar, tray) is yellow,
  separators and frames are neon yellow and the status bar says so.
- **Several tunnels at once** (Settings): independent tunnels run side by side; a tunnel that cannot run together with
  a connected one (same address, or both route all traffic) replaces it, and the event log says why.
- **The VPN does not depend on the window**: tunnels, statistics and the event log live in the core service; close the
  window and everything keeps working. The window needs no administrator rights - no UAC prompts after the core is
  installed.
- **Tunnels come back after a reboot, power loss or a drop**: the core reconnects a tunnel you left connected
  every 10 s for 3 minutes, every minute up to 10 minutes, then every 10 minutes, and at once when the network changes.
- **Exit asks about the VPN** when tunnels are connected: disconnect and exit, or exit and keep the VPN running. The
  choice can be remembered; **Settings → Settings… → Show hidden dialogs again** brings the question back.
- **No accidental disconnects**: a double click or `Enter` on a tunnel row only connects; on a connected tunnel they do
  nothing. **Disconnect** (card button, row menu, tray) asks first, with **Don't ask again** (**Settings → Settings…
  → Show hidden dialogs again** brings the question back). With "several tunnels at once" off, the tooltip of **Connect** names the
  tunnels it will disconnect.
- **Copy diagnostics** (**Help → Copy diagnostics**, also a button in **About**): a plain-text report for bug reports goes to the clipboard - versions, mode, Windows, core and agent status, tunnel names with state, the last 50 events, component versions. Keys, addresses, host names, user and computer names are removed, the profile path becomes `%USERPROFILE%`.
- **Updates and rollbacks** (**Help → Check for updates…**): the original AmneziaWG client, the built-in engine and
  the app itself, each separately or all at once. Before every update and every rollback the current version is
  backed up automatically; the history table has a **Restore** button for each backup (a rollback asks for
  administrator rights through UAC, so no other program can silently downgrade a component). Downloads are checked before
  installing: the AmneziaWG installer by its SHA-256 and its publisher signature, our engine and app by a signed
  manifest. If the updated core does not start, the previous build is put back automatically together with the engine
  files replaced with it, and the history shows the rollback. Once a day the app checks for updates by itself and marks
  the **Help** menu; it never installs on its own. A new version is announced once: a small notice appears at the
  bottom right (**Open** / **Later**; it can be dragged by its title, the close button and `Esc` mean **Later**) without
  stealing focus or blocking the window; if the window is hidden in the
  tray, Windows shows a notification as well.
- Every panel can be switched on or off in the **View** menu; behaviour is configured in the **Settings…** window (sections General with the theme choice, Notifications and tray,
  Network, Working mode; **OK** / **Cancel** / **Apply**, **Reset to defaults** with a confirmation). The ping host is
  checked there: a host name or an IPv4 address, otherwise the window shows the error and does not save.
- Sharp on any screen: the interface follows the Windows display scale of each monitor (Full HD, 2K, 4K, 8K) and
  can be made larger or smaller on top of it with **View -> Interface scale** or `Ctrl+Plus` / `Ctrl+Minus`.
- Three themes: Graphite (dark, the default), Slate (soft blue-grey dark) and Daylight (light), or Follow Windows;
  switched live in **Settings…**, the title bar follows the theme. English by default, Russian built in, easy to add
  your own language.

**Config editing**
- Edit, import and delete tunnels through the native AmneziaWG window, driven for you with UI Automation.
- Link a tunnel to its plain `.conf` source file and synchronise in both directions.
- Built-in `.conf` editor with an "Import into AmneziaWG" button. Closing it with unsaved changes asks
  **Save** / **Don't save** / **Cancel**.
- **Built-in engine mode** - works without an installed AmneziaWG: its own encrypted tunnel storage, import from
  `.conf` / `.zip` or straight from AmneziaWG in one click, password-protected backups (see [Working modes](#working-modes)).

## How it works

The app has two parts:

- **The core** - a Windows service (`AwgUiCore`, runs as SYSTEM, starts with Windows). It keeps the working mode,
  connects and disconnects tunnels, polls their state every second, keeps statistics and the event log, pings. It keeps
  working when the window is closed. If one of its internal threads fails, the core stops itself and Windows restarts the
  service within seconds; the reason is written to `events.log`.
- **The window** - only the interface. It talks to the core over a named pipe that only your account, administrators
  and SYSTEM can open, and needs **no administrator rights** at all. Closing it does not touch the VPN. If the window itself
  hits an internal error, it shows a message, writes `logs\crash.log` and exits; the core and the tunnels are not affected.

On top of AmneziaWG (the default mode) the core does not replace or patch the official client; it uses what is already
there:

1. **Tunnel list** - the encrypted `*.conf.dpapi` files in `C:\Program Files\AmneziaWG\Data\Configurations\`.
   The configs are encrypted for SYSTEM; the UI never reads them, it only passes the path to the service.
2. **Connect / disconnect** - `amneziawg.exe /installtunnelservice <path>` and `/uninstalltunnelservice <name>`,
   exactly what the original client's manager does.
3. **Status** - the tunnel service's UAPI named pipe (`\\.\pipe\ProtectedPrefix\Administrators\AmneziaWG\<name>`),
   queried once per second. A running tunnel is a tunnel whose pipe exists.
4. **Ping** - ICMP echo to a host of your choice (default `1.1.1.1`) every 10 seconds while any tunnel is up.
5. **Edit, import, delete** - Windows UI Automation operates the native AmneziaWG window. The core starts a helper
   copy of the app in your session for that (the native window runs with administrator rights). Saving and encrypting
   is always done by the original client; the app never reads its encrypted store. These actions need your account to
   be an administrator (the native window itself requires it); everything else works for a standard account too.

The core needs administrator rights once, to be installed: on the first start the window shows **Install the core**
(one Windows UAC prompt). After an update of the app the window offers **Update the core** the same way. A second
launch of the window simply brings the already open one to the front.

## Requirements

- Windows 10 or Windows 11 (x64).
- The official [AmneziaWG for Windows](https://github.com/amnezia-vpn/amneziawg-windows-client) client installed -
  for the default mode only (the built-in engine mode and the demo mode work without it).
- Administrator rights once - to install the core service (one UAC prompt). The window itself runs without them.

## Installation

### Download

Download `awg-ui.exe` from the [latest release](https://github.com/ssv555/AmneziaWG-UI-Dark/releases/latest), put it
in any folder and run it. Nothing to install: it is a single portable file with no runtime to install (no Visual C++
Redistributable, no .NET); settings, statistics, logs and languages live next to it. What changed in each version is
listed in the [changelog](CHANGELOG.md).

The exe is not code-signed yet, so Windows SmartScreen may warn on the first start: **More info -> Run anyway**.

### Build from source

1. Install [Rust](https://rustup.rs) with the **MSVC** toolchain (`stable-x86_64-pc-windows-msvc`).
2. Install the Visual Studio Build Tools and the **Windows SDK** - the build needs `rc.exe` to embed the icon.
3. Clone and build:

   ```powershell
   git clone https://github.com/ssv555/AmneziaWG-UI-Dark.git
   cd AmneziaWG-UI-Dark
   cargo test --release
   cargo build --release
   ```

4. The result is `target\release\awg-ui.exe`.

## Usage

Start `awg-ui.exe`. Select a tunnel and use **Connect / Disconnect** (or **Reconnect**) at the top right.

### Keyboard shortcuts

| Keys | Action |
|------|--------|
| `Ctrl+F` | Focus the tunnel search (`Esc` clears it) |
| `Up` / `Down` | Move between rows |
| `Left` / `Right` | Collapse / expand a group, or jump to the parent |
| `Enter`, double click | Connect a tunnel (never disconnects; use the button or the menu), or collapse / expand a group |
| `F2` | Rename the selected group |
| `Delete` | Delete the selected group or tunnel (with confirmation) |
| `Shift+F10` / Menu key | Context menu of the selected row, or of the focused card value or log line (`Tab` moves the focus) |
| `F10` or `Alt` alone | Focus the menu bar (first menu highlighted); `Left` / `Right` move between menus, `Down` or `Enter` opens one, `Esc` steps back |
| `Alt+F`, `Alt+V`, `Alt+S`, `Alt+L`, `Alt+H` | Open **File**, **View**, **Settings**, **Language**, **Help** (the underlined letter); inside a menu `Up` / `Down` move, `Enter` runs the item, `Right` opens a submenu, `Left` closes it |
| `Ctrl+N` / `Ctrl+I` | New tunnel / import tunnels (built-in engine mode) |
| `F1` / `F5` | About / check for updates |
| `Ctrl+S` | Save in the built-in config editor |
| `Ctrl+Plus` / `Ctrl+Minus` / `Ctrl+0` | Larger / smaller interface / back to 100 % |
| `Enter` / `Esc` in dialogs | Primary button / cancel; `Esc` and the title-bar close button cancel or close any window |

### Groups

- Right-click a group: new subgroup, rename, move up / down, move to the top level, delete (its subgroups and
  tunnels move one level up; tunnels are **not** removed from AmneziaWG).
- Right-click a tunnel: **Move to group** (a tree of submenus).
- Drag a tunnel onto a group to move it there; drop it on "No group" or on the empty area to take it out. Drag a
  group onto another group to make it a subgroup. Invalid drops get a red frame.

### Tray and autostart

- Closing the window hides it to the tray; **File -> Exit** quits only the UI. **Exit with AmneziaWG** also closes
  the native window and its tray icon. Services and tunnels keep running either way.
- **Settings -> Settings… -> Start at Windows sign-in** starts the window hidden in the tray (`--tray`) when you sign in. The core
  does not need it: it is a service and starts with Windows. The same switch from the command line:
  `awg-ui.exe --autostart on|off`.
- **Settings -> Create desktop shortcut** puts a shortcut to the window on the desktop.
- **Settings -> Core (service)** - reinstall or remove the core.

### Editing configs and source files

- **Edit in AmneziaWG** opens the native editor of the tunnel for you.
- **Add config...** remembers a plain `.conf` as the tunnel's *source* and opens the native import dialog with the
  file already selected; you only press "Open".
- **Edit source** opens the file in your editor; after saving you are offered to import it into AmneziaWG.
- **Synchronize with source** (once a source is linked): `AmneziaWG -> source` or `Source -> AmneziaWG`, always with
  a confirmation before anything is overwritten.
- **Delete tunnel...** selects the tunnel in the native window and confirms the removal there. If no source file is
  known, you can save a copy of the full config first.

### Working modes

**Settings -> Settings… -> Working mode** switches between two modes. After **OK**, a window explains the chosen mode and
lists the tunnels that will be disconnected; the core switches without restarting the window.

1. **On top of AmneziaWG** (default). The official client runs alongside and receives its own updates; this app shows
   and controls its tunnels. Import and editing go through the AmneziaWG window.
2. **Built-in engine.** The app works on its own: the tunnel engine `tunnel.dll` (built from
   [amneziawg-windows](https://github.com/amnezia-vpn/amneziawg-windows), the same version as in the official client
   3.1) and the signed `wintun.dll` driver come in the release zip. Each connected tunnel is a Windows service created
   by the core and started with Windows, so a connected tunnel comes back by itself after a reboot; status, graph and
   statistics work exactly as in the first mode. The app remembers the selected tunnel separately for each mode.
   - Tunnels are stored in `C:\Program Files\AmneziaWG UI Dark\tunnels\`, encrypted by Windows (DPAPI): no password
     in the app, and a copied file cannot be read on another computer. Only SYSTEM and administrators can open this
     folder, so no ordinary program can read the keys or swap a config the tunnel service runs. The engine files are
     copied there as well - only if their checksums match the ones built into the app.
   - **File -> Import tunnels** takes `.conf` files and `.zip` archives (several at once); **Take all tunnels from
     AmneziaWG** exports them from an installed AmneziaWG in one click. Configs with `PreUp` / `PostUp` / `PreDown` /
     `PostDown` commands are not imported by either path; the notice names them.
   - **File -> Backup** saves all tunnels into one zip encrypted with AES-256 and your password - for a new PC or a
     reinstalled Windows; **Restore from backup** brings them back. The zip also opens in 7-Zip and WinRAR.
   - **File -> New tunnel** creates a config with a fresh private key; the tunnel menu has **Edit**, **Rename** and
     **Delete**.

A third mode - the built-in engine with extra features of our own - is planned.

### Languages

English is the default and Russian is built in. To add a translation:

1. **Language -> Add language...** creates a template `lang\<ISO 639-2 code>.lng` (for example `deu.lng`,
   `fra.lng`) with all keys and the English text, and opens it in Notepad.
2. Translate the values and save the file under the language's code.
3. Pick the language in the **Language** menu. A missing key or an empty value falls back to English. In menu titles
   `&` marks the Alt letter (`&File`); keep the letters of the five menus different.

Pull requests with new translations are very welcome - see [CONTRIBUTING.md](CONTRIBUTING.md).

## Files

The window keeps its own settings **next to the exe**, so it stays portable:

| Path | Purpose |
|------|---------|
| `Settings.ini` | Window position and size, interface scale, panel and column widths, graph height, View / Settings switches, theme (`theme` in `[options]`: `graphite` by default, `slate`, `daylight`, `system` = follow Windows; an unknown value means `graphite`), groups, tunnel source links. Saved 0.3 s after the last change. |
| `lang\*.lng` | Additional interface languages. |
| `logs\crash.log` | Why the window ended after an internal error or could not start (time, version, place). Folder set by `log_dir` in `Settings.ini`. |
| `logs\window-errors.log` | Errors of the window's actions (the same lines as in its event log, so none is lost while it is hidden in the tray); rotated to `.1.log` at 1 MiB. |
| `*.unreadable-<date>` | A settings file that could not be read (`Settings.ini`, `core.ini`) is not overwritten: it is moved aside under this name, defaults are used and the log says so. |
| `tunnel.dll`, `wintun.dll` | The built-in engine (working mode 2), from the release zip; installed together with the core. |

The core keeps its data where only SYSTEM and administrators can reach it:

| Path | Purpose |
|------|---------|
| `C:\Program Files\AmneziaWG UI Dark\` | The core and the engine; `tunnels\` - the encrypted tunnels of the built-in engine. |
| `C:\ProgramData\AmneziaWG UI Dark\core.ini` | Working mode and ping settings of the core. |
| `C:\ProgramData\AmneziaWG UI Dark\Stats.ini` | Accumulated per-tunnel statistics, written every 15 s. WireGuard counters reset when a tunnel service restarts; the core notices this and keeps adding up correctly. |
| `C:\ProgramData\AmneziaWG UI Dark\logs\events.log` | Event log of the core (also its internal failures and rejected engine files), rotated to `events.1.log` at 1 MiB. |
| `C:\ProgramData\AmneziaWG UI Dark\updates\` | Updates: history, last check, backups (at most 5 per component), msiexec logs. Administrators only. |

On the first install of the core, statistics, the event log and the working mode are taken over from the window's
folder. Statistics and time connected are counted by the core all the time, whether the window is open or not.

## Command line

| Flag | Description |
|------|-------------|
| `--tray` | Start hidden in the tray (this is what autostart uses) |
| `--demo` | Invented tunnels and statistics; no AmneziaWG, no administrator rights. Demo settings live in `%TEMP%\awg-ui-demo` |
| `--about` | Open the About window at start |
| `--snapshot <file.png>` | Once the speed graph covers its whole period (about 2 minutes), save a screenshot of a 1600×1100-point window at the Windows display scale and exit (used for the images in this README) |
| `--status` | No window: print the tunnel list and the state of the running ones |
| `--autostart on\|off` | Enable or disable autostart |
| `--edit-native <tunnel>` | Open the native AmneziaWG editor of a tunnel (automation check) |
| `--import-native <file.conf>` | Open the native import dialog with the file selected |
| `--native-details <tunnel>` | Print tunnel details read from the native window |
| `--sync-roundtrip <tunnel>` | Read the config from the native editor, write it back and compare; prints only the length |
| `--export-native <file.zip>` | Export all tunnels through the native AmneziaWG window into a zip; prints only their number |
| `--engine-import <file.conf>` | Built-in engine: put a config into the encrypted storage |
| `--engine-connect <tunnel>`, `--engine-disconnect <tunnel>` | Built-in engine: connect / disconnect a stored tunnel without the window |
| `--install-core`, `--uninstall-core` | Install (or update) / remove the core service (asks for administrator rights) |
| `--core-status` | Ask the core for its version, mode and tunnels, as the window does |
| `--core-mode overlay\|engine` | Switch the working mode through the core |
| `--core-updates state\|check` | Show the updates state / check for updates now through the core |
| `--core-updates restore <id>` | Roll back to the backup of history entry `<id>`; the core accepts it only from a process with administrator rights |
| `--core-details <tunnel>` | Details of a disconnected tunnel through the core (prints only the number of addresses and peers) |
| `--core-take-native` | Built-in engine: take all tunnels from AmneziaWG through the core |
| `--core-connect <tunnel>`, `--core-disconnect <tunnel>` | Connect / disconnect a tunnel through the core, with the window's "Several tunnels at once" setting |
| `--tunnel-service <config>`, `--core`, `--native-op <task>` | Internal: tunnel service, core service and the core's helper (started by Windows / the core) |
| `--help`, `-h` | Short usage line |

## Roadmap

- [x] Ready-to-run builds on GitHub Releases ([changelog](CHANGELOG.md)).
- [ ] Code-signed builds.
- [x] Working modes: on top of AmneziaWG and the built-in engine ([details](#working-modes)).
- [ ] Extended engine: the built-in engine with extra features of our own.
- [ ] More translations.
- [ ] Your ideas - open an issue!

## Contributing

Contributions of every size are welcome: bug reports, feature ideas, UI and UX suggestions, code, documentation and
translations. If you use AmneziaWG on Windows and something could be nicer, please join in - open an
[issue](https://github.com/ssv555/AmneziaWG-UI-Dark/issues) or send a pull request. How to build, test and submit a
change is described in [CONTRIBUTING.md](CONTRIBUTING.md). Documentation: [docs/](docs/README.md).

<p align="center">
  <img src="img/about.png" alt="The About window" width="640">
</p>

## License

Free for **non-commercial** use. Commercial use is possible only with the author's written permission. See
[LICENSE](LICENSE) for the exact terms.

Author: **ssv555** - [github.com/ssv555](https://github.com/ssv555) - ssv555ssv@gmail.com

## Disclaimer

This is an **unofficial** project. It is not affiliated with, endorsed by or sponsored by Amnezia. The names
"AmneziaWG" and "Amnezia", and the AmneziaWG logo and icon, belong to their respective owners and are mentioned only
to describe compatibility. The software is provided "as is", without warranty of any kind.
