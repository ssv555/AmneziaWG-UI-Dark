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
7. **0.5.2** (2026.10.06): three themes - Graphite, Slate and Daylight - plus Follow Windows, switched live
   in the Settings window.
8. **0.5.3** (2026.10.07): pause button and speed scale on the graph; actions in the AmneziaWG window work again
   and its window reopens after an AmneziaWG update; dialogs fit the main window, aligned tables in the Updates
   window; a background thread failure no longer breaks the window; a lone Alt opens the menu; brighter faint text in
   Graphite; the helper process follows the program language.
9. **0.5.4** (2026.10.07): graph range as a drop-down - 2 min, 10 min, 1 h, day, month, year - remembered between
   starts; speed history kept by the helper process; an active tunnel stands out in the list in every theme.

10. **0.5.5** (2026.10.07): fixes from a full audit - Enter acts on the focused button, dialogs and the card fit the
   window at 760x480 and 200 %, readable controls in Daylight, human error texts, list columns sized to content, units
   in Latin letters; mode 1 tunnels stay wanted after a failed reconnect or an outside AmneziaWG install, journaled
   app and engine updates, interrupted AmneziaWG rollbacks keep the tunnels.

## Next

Nothing scheduled.

## Later, not scheduled

Deferred to testers: check the screen reader support with Narrator and NVDA and fix what they find.

Also listed in the [README](../README.md): code-signed builds, an extended built-in engine, more translations.
Ideas and requests: open an [issue](https://github.com/ssv555/AmneziaWG-UI-Dark/issues).
