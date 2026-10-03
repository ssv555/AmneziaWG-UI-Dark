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
- Sortable, resizable columns: downloaded total, uploaded total, peak speed, share of connected time.

**Live status**
- Is there a handshake and how long ago, are packets flowing, download / upload speed, ping through the VPN.
  Numbers use a monospaced font in fixed-width cells, so nothing jumps around when values change.
- Speed graph (2 min / 10 min / 1 h) with a ping strip underneath.
- Per-tunnel statistics since the first launch: traffic, peak speed, time connected and its share of the total.
- Event log: connections, disconnections, lost and restored links.

**Convenience**
- A coloured dot on the tray icon and on the taskbar button shows the state; Windows notifications when a link
  drops or comes back.
- Autostart at sign-in **without a UAC prompt** (via a scheduled task) and a desktop shortcut that also skips UAC.
- Every panel can be switched on or off in the **View** menu; behaviour is configured in **Settings**.
- Sharp on any screen: the interface follows the Windows display scale of each monitor (Full HD, 2K, 4K, 8K) and
  can be made larger or smaller on top of it with **View -> Interface scale** or `Ctrl+Plus` / `Ctrl+Minus`.
- Dark theme with a dark title bar. English by default, Russian built in, easy to add your own language.

**Config editing**
- Edit, import and delete tunnels through the native AmneziaWG window, driven for you with UI Automation.
- Link a tunnel to its plain `.conf` source file and synchronise in both directions.
- Built-in `.conf` editor with an "Import into AmneziaWG" button.

## How it works

AmneziaWG UI Dark does not replace or patch the official client; it sits next to it and uses what is already there:

1. **Tunnel list** - the encrypted `*.conf.dpapi` files in `C:\Program Files\AmneziaWG\Data\Configurations\`.
   The configs are encrypted for SYSTEM; the UI never reads them, it only passes the path to the service.
2. **Connect / disconnect** - `amneziawg.exe /installtunnelservice <path>` and `/uninstalltunnelservice <name>`,
   exactly what the original client's manager does.
3. **Status** - the tunnel service's UAPI named pipe (`\\.\pipe\ProtectedPrefix\Administrators\AmneziaWG\<name>`),
   queried once per second. A running tunnel is a tunnel whose pipe exists.
4. **Ping** - ICMP echo to a host of your choice (default `1.1.1.1`) every 10 seconds while any tunnel is up.
5. **Edit, import, delete** - Windows UI Automation operates the native AmneziaWG window. Saving and encrypting is
   always done by the original client; the UI never touches your private keys.

Administrator rights are required to reach the tunnel services. The app restarts itself elevated; a second launch
simply brings the already open window to the front.

## Requirements

- Windows 10 or Windows 11 (x64).
- The official [AmneziaWG for Windows](https://github.com/amnezia-vpn/amneziawg-windows-client) client installed
  (the demo mode works without it).
- Administrator rights (a UAC prompt on first launch, or none at all after you create the launch task).

## Installation

### Download

Prebuilt releases are **coming soon** on the [Releases](https://github.com/ssv555/AmneziaWG-UI-Dark/releases) page.
The app is a single portable `awg-ui.exe`; settings, statistics, logs and languages live next to it.

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
| `Enter` | Connect / disconnect a tunnel, or collapse / expand a group |
| `F2` | Rename the selected group |
| `Delete` | Delete the selected group or tunnel (with confirmation) |
| `Ctrl+S` | Save in the built-in config editor |
| `Ctrl+Plus` / `Ctrl+Minus` / `Ctrl+0` | Larger / smaller interface / back to 100 % |
| `Enter` / `Esc` in dialogs | Primary button / cancel |

### Groups

- Right-click a group: new subgroup, rename, move up / down, move to the top level, delete (its subgroups and
  tunnels move one level up; tunnels are **not** removed from AmneziaWG).
- Right-click a tunnel: **Move to group** (a tree of submenus).
- Drag a tunnel onto a group to move it there; drop it on "No group" or on the empty area to take it out. Drag a
  group onto another group to make it a subgroup. Invalid drops get a red frame.

### Tray and autostart

- Closing the window hides it to the tray; **File -> Exit** quits only the UI. **Exit with AmneziaWG** also closes
  the native window and its tray icon. Services and tunnels keep running either way.
- **Settings -> Start at Windows sign-in** creates a scheduled task (`awg-ui`, elevated, `--tray`) - no UAC prompt at
  sign-in. The same switch is available from the command line: `awg-ui.exe --autostart on|off`.
- **Settings -> Create desktop shortcut** also creates the `awg-ui-launch` task, so the shortcut starts the
  elevated app without a UAC prompt.

### Editing configs and source files

- **Edit in AmneziaWG** opens the native editor of the tunnel for you.
- **Add config...** remembers a plain `.conf` as the tunnel's *source* and opens the native import dialog with the
  file already selected; you only press "Open".
- **Edit source** opens the file in your editor; after saving you are offered to import it into AmneziaWG.
- **Synchronize with source** (once a source is linked): `AmneziaWG -> source` or `Source -> AmneziaWG`, always with
  a confirmation before anything is overwritten.
- **Delete tunnel...** selects the tunnel in the native window and confirms the removal there. If no source file is
  known, you can save a copy of the full config first.

### Languages

English is the default and Russian is built in. To add a translation:

1. **Language -> Add language...** creates a template `lang\<ISO 639-2 code>.lng` (for example `deu.lng`,
   `fra.lng`) with all keys and the English text, and opens it in Notepad.
2. Translate the values and save the file under the language's code.
3. Pick the language in the **Language** menu. A missing key or an empty value falls back to English.

Pull requests with new translations are very welcome - see [CONTRIBUTING.md](CONTRIBUTING.md).

## Files

Everything is stored **next to the exe**, so the app is fully portable:

| Path | Purpose |
|------|---------|
| `Settings.ini` | Window position and size, interface scale, panel and column widths, graph height, View / Settings switches, groups, tunnel source links. Saved 0.3 s after the last change. |
| `Stats.ini` | Accumulated per-tunnel statistics, written every 15 s and on exit. WireGuard counters reset when a tunnel service restarts; the UI notices this and keeps adding up correctly. |
| `logs\events.log` | Event log, rotated to `events.1.log` at 1 MiB. The folder can be changed with `log_dir` in the `[options]` section of `Settings.ini`. |
| `lang\*.lng` | Additional interface languages. |

Time connected is counted while the UI is running - that is why autostart is recommended.

## Command line

| Flag | Description |
|------|-------------|
| `--tray` | Start hidden in the tray (this is what autostart uses) |
| `--demo` | Invented tunnels and statistics; no AmneziaWG, no administrator rights. Demo settings live in `%TEMP%\awg-ui-demo` |
| `--about` | Open the About window at start |
| `--snapshot <file.png>` | Once the speed graph covers its whole period (about 2 minutes), save a screenshot of a 1600×1100-point window at the Windows display scale and exit (used for the images in this README) |
| `--status` | No window: print the tunnel list and the state of the running ones |
| `--autostart on\|off` | Enable or disable autostart |
| `--launch-task` | Create only the `awg-ui-launch` task (start without UAC) |
| `--edit-native <tunnel>` | Open the native AmneziaWG editor of a tunnel (automation check) |
| `--import-native <file.conf>` | Open the native import dialog with the file selected |
| `--native-details <tunnel>` | Print tunnel details read from the native window |
| `--sync-roundtrip <tunnel>` | Read the config from the native editor, write it back and compare; prints only the length |
| `--help`, `-h` | Short usage line |

## Roadmap

- [ ] Signed release builds on GitHub Releases.
- [ ] Standalone mode that does not need the native AmneziaWG window for editing and import.
- [ ] More translations.
- [ ] Your ideas - open an issue!

## Contributing

Contributions of every size are welcome: bug reports, feature ideas, UI and UX suggestions, code, documentation and
translations. If you use AmneziaWG on Windows and something could be nicer, please join in - open an
[issue](https://github.com/ssv555/AmneziaWG-UI-Dark/issues) or send a pull request. How to build, test and submit a
change is described in [CONTRIBUTING.md](CONTRIBUTING.md).

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
