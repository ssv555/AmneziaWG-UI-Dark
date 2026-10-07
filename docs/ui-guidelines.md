<p align="right"><b>English</b> | <a href="ui-guidelines.ru.md">Русский</a></p>

# UI guidelines

Rules for every window, dialog and notice in the app. The goal is a window that behaves like a native Windows 11
(Fluent) one. Read this before changing the interface; the code that enforces it is named below.

## Windows and dialogs

1. Every window, dialog and notice has a caption in a title bar, can be dragged by it, has a close button (X) in the
   title bar, closes on Esc (Cancel/close) and confirms on Enter (the default button).
2. No anchored, fixed or title-less popups. A default start position is fine (centre, bottom-right for notices);
   a locked one is not.
3. Esc and Enter go only to the topmost window, as in Windows where the active window receives the key. A held Enter
   (key repeat) does not confirm a dialog. The keys follow the Windows rules (see "Buttons and keyboard").
4. Modal only when the action cannot continue without an answer. Notices are non-modal toasts with a close X, and
   they also go to the event log. A modal dialog blocks everything under it, as in Windows: clicks on the main
   window do nothing, Tab walks only the dialog, and focus leaves the main window when the dialog opens. Dialogs of
   the `Modals` stack build their window with `Turn::window`; `make_modal` in `src/app/modals.rs` puts an invisible
   backdrop that takes the clicks under the top dialog window and makes that window egui's modal layer for focus
   (`Modals::run`; tests `a_modal_dialog_blocks_the_window_under_it`,
   `modal_dialogs_build_their_window_through_the_turn`).
5. Details (release notes, logs, long text) never expand inside tables or lists: a button opens them in their own
   window.
6. A dialog never extends past the main window: `dialog_window` keeps every window inside the main window's content
   area with a 12 px margin (`window_bounds`), position and maximum size both, also after the main window is made
   smaller. The size limit holds a resizable window; an auto-sized window grows with its content, so its body goes
   through `dialog_body(ui, width, ..)`: the width is capped by the bounds (at UI scale 200 % a 760 px window has
   356 pt for dialogs), the body scrolls when it is taller than the space left under the title bar, and the button
   row drawn right after it is always visible. A dialog's width is set only there (guard test
   `dialog_widths_go_through_dialog_body`); windows with their own `ScrollArea` (updates, release notes, the .conf
   editor) manage their height themselves. Tests `dialogs_stay_inside_the_main_window`,
   `dialog_body_keeps_the_buttons_reachable`, `fit::dialogs_fit_the_minimum_window_and_zoom_200`.
7. The main window is never smaller than 760x480 pt at any UI scale: the minimum size is re-sent to the window in
   points after every scale change (`WindowState::min_size_due`), so a 200 % window is at least 1520x960 px. The
   layout is checked at that size without a GPU by `src/app/fit.rs`: menu, core banner, tunnel list, tunnel card,
   event log and status bar are laid out and no painted shape may leave the window or its clip rectangle. The
   tunnel list is capped at `left_panel_max` so the card keeps at least 420 pt (its buttons, a readable name and the
   4-column grid); the right column (card, graph, details) is one vertical `ScrollArea` (`details` in
   `src/app/details.rs`), so in a low window it scrolls instead of being cut at the panel bottom; the card header
   lays out its buttons first and truncates the name in the rest; the core banner wraps its text and keeps the fix
   button at the right edge; the event log toolbar wraps onto a second line when the window is narrow.
8. Tables use auto-sized columns: a column is as wide as its widest content (header or cell), the header is aligned
   like the column's values (text left, numbers right), spare width stays after the last column, never between
   columns. Only when the window is narrower than the table does the name column shrink and truncate with a tooltip
   (`column_widths` in `src/app/updates.rs`, test `columns_take_the_width_of_their_widest_content`).
   The tunnel list (`src/app/list.rs`) follows the same rule with one difference: its numeric columns are as wide as
   the widest value they can ever show (`widest_value`) or their header plus the sort mark, so they do not move as
   traffic grows; when the panel is too narrow, numeric columns hide from the right (Share first) and the name keeps
   at least 120 pt (`fit_columns`). Every cell is painted clipped to itself; a truncated name has a tooltip. Tests
   `columns_that_do_not_fit_hide_from_the_right`, `measured_columns_hold_their_widest_value`.

## Look

1. Windows 11 Fluent: rounded corners (8 px windows, 4 px controls), Segoe UI Variable / Segoe UI, the system accent
   colour, a soft shadow, no hard borders. The one exception is the mode 2 accent (yellow icon, taskbar button and frames).
2. Three themes: Graphite (dark, the default), Slate (soft blue-grey dark) and Daylight (light), plus Follow Windows
   (Daylight when Windows apps use the light theme, otherwise Graphite). Chosen in Settings… - General - Theme
   (option `theme` in `Settings.ini`). A change applies on the next frame, without a restart; the Windows title bar
   follows the theme.
3. Colours come only from the active theme's `Palette` in `src/app/theme.rs` (`palette()`); the test
   `colours_come_only_from_the_palette` forbids colour literals and egui base visuals elsewhere in `src/app`. On
   Daylight the selected row also gets a 3 px accent bar on the left: the light selection fill alone is hard to see.
   Every text colour, weak text included, has contrast of at least 4.5 (WCAG AA) on the theme's backgrounds; weak text
   stays dimmer than plain text (test `every_palette_meets_wcag_contrast_targets`). Controls (buttons, check boxes,
   radio buttons, combo boxes, text fields) have a 1 px edge `control_edge` with contrast of at least 3 (WCAG 1.4.11)
   on the window and panel backgrounds in Daylight and Slate; Graphite keeps egui's edge-less dark controls by the
   owner's decision (tests `controls_have_visible_boundaries`, `graphite_is_todays_look`).
   An active tunnel stands out in the list: its status dot is slightly larger and its name is drawn in the dot's
   colour - green when connected, yellow while connecting, reconnecting or when pings fail (`name_color` in
   `src/app/list/rows.rs`). Disconnected tunnels and an unknown state (no link to the core) keep plain text. A tunnel
   with an error has its name in the error colour (plain on the selected row, where red is unreadable) and a ring
   instead of a filled dot, so an error differs from "disconnected" by shape, not only by colour (`status_mark`, test
   `error_tunnel_name_reads_on_unselected_rows`). These two status colours are saturated (CIELAB chroma at least
   45, colour difference CIE76 from the grey of disconnected tunnels at least 45) and readable at 4.5 on the selected
   row too (tests `active_status_colours_stand_out_from_idle`, `active_tunnel_name_reads_on_every_row_background`).
   Icons on the taskbar and in the tray do not depend on the theme.
4. DPI: per-monitor scale; nothing is clipped at 100-300 %.

## Buttons and keyboard

1. Buttons sit bottom-right in the Windows order: the primary (accent-filled) one first, then Cancel; a property
   window adds Apply last (OK / Cancel / Apply). Minimum width about 80 px, one default button per dialog: the
   primary one, filled with the theme accent (`Palette::accent`, text `Palette::on_accent`, at least 4.5:1 in every
   theme). The order and the look live in one place, `button_row` in `src/app/dialog.rs` (tests
   `property_sheet_buttons_are_ok_cancel_apply`, `button_row_lays_out_in_list_order`,
   `exactly_one_primary_button_is_accent_filled`). Notices use it too (the update notice: Open / Later).
2. Enter and Esc in a dialog, as in Windows; one implementation, `dialog_keys` in `src/app/dialog.rs`, used by
   `Turn::keys` (modal dialogs), `window_keys` / `window_escape` (other windows) and the update notice (tests
   `enter_on_a_focused_button_presses_that_button_only`, `enter_elsewhere_is_the_default_button`,
   `keys_with_an_open_list_stay_in_the_list`, `enter_on_the_active_notice_opens_the_updates_window`):
   1. Enter presses the focused control (a button, Cancel included, a check box, a list button), like Space.
      With focus anywhere else (nothing focused, a text field) Enter is the default (primary) button.
   2. Esc is Cancel/close.
   3. While a drop-down list or a menu is open, Enter and Esc belong to it: Esc closes only the list, the dialog
      stays. Whether a list is open is taken at the start of the frame (`note_popups`, called first in
      `App::ui`), because the list closes itself while it is drawn.
3. Keyboard: sensible Tab order, visible focus, Alt mnemonics in menus, a context menu on right click and on
   Shift+F10 / Menu key, built only through `menu::context_menu` (guard test `context_menus_go_through_the_helper`).
4. Menu bar keyboard lives in one place, `MenuNav` in `src/app/menu.rs`: F10 or a lone Alt (`AltTap`) focuses the
   first menu, Alt+letter opens the menu whose title has `&` before that letter (`&File`), Left / Right switch menus,
   Up / Down and Enter work inside, Right on an item with a submenu opens it, Left or Esc in a submenu closes it, Esc
   steps back. A window subclass swallows `SC_KEYMENU` (except Alt+Space), so F10 and Alt never enter the native
   system-menu mode (`swallows_system_key`). A lone Alt is detected from `ModifiersChanged`; on Windows winit sends
   that event before the key itself, and egui-winit delivers Alt as `Key::AltLeft` / `Key::AltRight`, so `AltTap`
   ignores Alt's own key events (any other key or click while Alt is held cancels the tap). Cyrillic mnemonics are
   mapped to their key on the ЙЦУКЕН layout, so they work in any keyboard layout. Command shortcuts (Ctrl+N, Ctrl+I,
   F1, F5) come from one table, `SHORTCUTS`, which also gives the text shown next to the menu item. Tests:
   `menu_mnemonics_are_unique_per_language` (fails on a letter collision in a built-in language),
   `shortcuts_map_to_commands`, `menu_bar_keys_walk_like_windows`, `arrows_open_and_close_submenus_like_windows`,
   `window_filter_takes_only_the_keyboard_menu_command`, `lone_alt_becomes_f10`,
   `alt_own_key_events_do_not_cancel_the_tap`.

## Actions that drop the VPN

1. Activating a row (double click, `Enter`) does only the safe action: it connects a tunnel and never disconnects it.
   The decision is `Primary::activation` in `src/app/list.rs`, the same `Primary` the card button, the row menu and
   the tray use.
2. Disconnecting (card button, row menu, tray) asks first, with "Don't ask again". The one rule is `list::asks_first`
   (window and tray); the answer is a `DialogId` in `hidden_dialogs`, and Settings… - "Show hidden dialogs again"
   resets it.
3. A tooltip says what else an action changes when the window knows it: Connect names the tunnels the core will
   disconnect (`Primary::connect_hint`). The rules themselves live in the core; the window only shows them.

## Options

1. Options live in the Settings window (`src/app/settings_dialog.rs`), not as check boxes and fields in a menu: a menu
   closes when it loses focus and has no place for an input error. Menus hold commands only.
2. The window edits a draft. Cancel, X and Esc drop it; OK and Apply apply everything at once and only when the input
   is valid: an invalid ping host (`ping::valid_host`: a host name or an IPv4 address) shows the error under the field
   and disables OK and Apply. Nothing is half applied.
3. Applied values take the same paths as before: `push_options` (the agent's `SetPing`, the tray), `Action::Autostart`,
   `Action::ChooseMode` with its confirmation window. A new option is added to `Choices` and to these paths, never
   given a second implementation.
4. Reset to defaults asks first and only resets the draft; the working mode and autostart stay as they are.

## Errors and text

1. Errors of actions go to the event log (and `window-errors.log`); the status bar shows only a link to the log, no
   error text. The link stays until the log is opened, so an error that arrived while the window was hidden in the
   tray is visible at the next show (`src/app/errors.rs`). A failure to read the tunnel state is logged once per
   change of its text and keeps its own link while it lasts (`monitor::set_poll_error`). The link stands at the right
   edge; a long notice is shortened with "…" (full text on hover) and never pushes it out (`src/app/status.rs`).
2. Dates are always `YYYY.MM.DD`, time `HH:MM`, through the one shared formatter `src/fmt.rs` (`date`, `date_time`,
   `date_time_sec`). Do not format dates in UI code.
3. All user-visible strings go through `src/i18n.rs`, English and Russian both ([CONTRIBUTING.md](../CONTRIBUTING.md)).
4. Units of measure are Latin in every language and never translated: `B`, `KiB`, `MiB`, `GiB`, `/s`, `ms`
   (`src/fmt.rs`: `bytes`, `rate`, `ms`). Durations (`2 min 15 s ago`) are text and follow the language.

5. Human sentence, technical detail apart. A window shows a short sentence in the program language; a known cause
   (no network, server name not found, timeout, secure connection failed, access denied, file in use, service not
   installed, server limits requests …) is added in words. The raw text (`os error 5`, `WinHttpSendRequest: error
   12007`, `HTTP status 403`, function names) goes only to the tooltip and the event log, at the end in parentheses.
   One place: `src/explain.rs` (`short`, `log_line`, `cause`); a new code goes into its table, its text is a `cause.*`
   key in `src/i18n.rs`. The tunnel card shows the state in the big line, the cause in words under it and the raw
   text on hover (`Health::detail`); the Updates window shows "Could not check: no connection to the server". A core
   or agent answer never goes into a text through `{:?}`: `explain::variant_name` (test `no_debug_dumps_in_error_texts`).
6. Notices in the status bar have a kind (`src/app/notice.rs`): progress (plain text, not logged), done (green,
   logged as information) and warning (yellow, logged as a warning: UAC declined, configs refused on import, rollback
   cancelled). Success and failure never share a colour; a closed notice stays in the event log.
7. Event log lines wrap at the panel width; errors are written in the error colour and warnings in the warning
   colour, not only marked by the dot.
8. One component, one name: the background helper process is the "helper service" everywhere in the window (test
   `helper_service_has_one_name`).
9. The tray tooltip lists whole lines (one per active tunnel, short state only); what does not fit is counted as
   "+N", a shortened line ends with "…" (`tray::fit_tip`).

## Build UI only from the shared helpers

1. Windows are created only by `dialog_window` in `src/app/dialog.rs`; it sets the title bar, close X, drag and
   position. The body (`dialog_body`), button rows (`dialog_buttons`, `dialog_choice`, `dialog_ok_cancel_apply`), the
   input error line under a field (`error_line`: one line reserved, grows with a long text) and keys
   (`window_escape`, `window_keys`) live there too.
2. The guard test `windows_follow_the_standard` in `src/app/dialog.rs` fails the build of the tests if `src/` creates
   an egui window outside `dialog_window`, or if `src/app/` uses `.anchor(`, `.movable(false)` or `.title_bar(false)`.
3. Need a pattern the helper does not cover: extend the helper, do not write a one-off window.
4. Custom-painted widgets (anything drawn with the painter instead of an egui widget: table rows, graphs, status
   dots) must call the a11y helper `describe` in `src/app/a11y.rs`, otherwise a screen reader sees an empty box. A new
   kind of painted element gets a new `Painted` variant there, not its own `widget_info` call.

## Check before you send

Walk through the change in the live window (`awg-ui.exe --demo` is enough):

1. Drag it by the title bar, close it with X and with Esc, confirm with Enter.
2. Tab through it and look at the focus.
3. Both working modes (mode 2 is yellow), at 100 % and 150 % Windows scale.

Say in the pull request what you checked.
