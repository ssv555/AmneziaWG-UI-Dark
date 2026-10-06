<p align="right"><b>English</b> | <a href="roadmap.ru.md">Русский</a></p>

# Roadmap

Full per-version lists are in the [changelog](../CHANGELOG.md). Only the headline of each version is here.

## Done

1. **0.1.0** (2026.10.03): tunnel window on top of the installed AmneziaWG: groups, connect/disconnect, live status
   and graph, dark theme, demo mode.
2. **0.2.0** (2026.10.03): tunnel table with search and statistics, speed graph with ping, event log, tray icon and
   notifications, autostart.
3. **0.3.0** (2026.10.03): ready-to-run `awg-ui.exe` on GitHub Releases; config sources and editor; Russian built in
   and own languages as `.lng` files; interface scale.
4. **0.4.0** (2026.10.05): the core service `AwgUiCore` (VPN keeps working with the window closed, no UAC in the
   window); built-in engine (working mode 2) without an installed AmneziaWG; several tunnels at once; automatic
   reconnect; updates and rollbacks window with signed manifests and backups; upstream check.
5. **0.5.0** (not released yet; in `main`): the core does only the VPN, everything else runs in a separate helper
   process (`awg-ui.exe --agent`) that the core restarts; daily reminder about an available update;
   eframe 0.36 and wgpu 30; fixes and dependency updates.

## Next

1. **Usability items**:
   1. tray menu with the tunnels (connect/disconnect from the tray; done, ships with the next release);
   2. event log: filter, search, copy (done, ships with the next release);
   3. config check in the editor before saving (done, ships with the next release);
   4. copy diagnostics (one button that collects what an issue report needs, without keys).

## Later, not scheduled

Also listed in the [README](../README.md): code-signed builds, an extended built-in engine, more translations.
Ideas and requests: open an [issue](https://github.com/ssv555/AmneziaWG-UI-Dark/issues).
