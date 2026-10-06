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
5. **0.5.0** (2026.10.06): the core does only the VPN, everything else runs in a separate helper
   process (`awg-ui.exe --agent`) that the core restarts; tunnels in the tray menu; event log filter, search and
   copy; config check in the editor; copy values from the tunnel card and copy diagnostics; daily reminder about an
   available update; eframe 0.36 and wgpu 30; fixes and dependency updates.
6. **0.5.1** (2026.10.06): protection against disconnecting a tunnel by accident (asks first, connect-only
   double click and Enter); the core restarts a tunnel whose handshake stopped updating; a Settings window with
   OK / Cancel / Apply instead of the drop-down menu; keyboard access to the menus (F10, Alt, Alt+letter, arrows,
   Ctrl+N, Ctrl+I, F1, F5); screen reader support (accessibility tree, painted rows, graph and status dots are named).

## Next

1. **Accessibility**: check the screen reader support with Narrator and NVDA and fix what they find.

## Later, not scheduled

Also listed in the [README](../README.md): code-signed builds, an extended built-in engine, more translations.
Ideas and requests: open an [issue](https://github.com/ssv555/AmneziaWG-UI-Dark/issues).
