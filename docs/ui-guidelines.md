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
   (key repeat) does not confirm a dialog.
4. Modal only when the action cannot continue without an answer. Notices are non-modal toasts with a close X, and
   they also go to the event log.
5. Details (release notes, logs, long text) never expand inside tables or lists: a button opens them in their own
   window.

## Look

1. Windows 11 Fluent: rounded corners (8 px windows, 4 px controls), Segoe UI Variable / Segoe UI, the system accent
   colour, the system dark/light theme, a soft shadow, no hard borders. The one exception is the mode 2 accent
   (yellow icon, taskbar button and frames).
2. DPI: per-monitor scale; nothing is clipped at 100-300 %.

## Buttons and keyboard

1. Buttons sit bottom-right in the Windows order: the primary (accent-filled) one first, then Cancel; a property
   window adds Apply last (OK / Cancel / Apply). Minimum width about 80 px, one default button per dialog. The order
   lives in one place, `button_row` in `src/app/dialog.rs` (tests `property_sheet_buttons_are_ok_cancel_apply`,
   `button_row_lays_out_in_list_order`).
2. Keyboard: sensible Tab order, visible focus, Alt mnemonics in menus, a context menu on right click and on
   Shift+F10 / Menu key, built only through `menu::context_menu` (guard test `context_menus_go_through_the_helper`).
3. Menu bar keyboard lives in one place, `MenuNav` in `src/app/menu.rs`: F10 or a lone Alt (`AltTap`) focuses the
   first menu, Alt+letter opens the menu whose title has `&` before that letter (`&File`), Left / Right switch menus,
   Up / Down and Enter work inside, Right on an item with a submenu opens it, Left or Esc in a submenu closes it, Esc
   steps back. A window subclass swallows `SC_KEYMENU` (except Alt+Space), so F10 and Alt never enter the native
   system-menu mode (`swallows_system_key`). Cyrillic mnemonics are mapped to their key on the ЙЦУКЕН layout,
   so they work in any keyboard layout. Command shortcuts (Ctrl+N, Ctrl+I, F1, F5) come from one table, `SHORTCUTS`,
   which also gives the text shown next to the menu item. Tests: `menu_mnemonics_are_unique_per_language` (fails on
   a letter collision in a built-in language), `shortcuts_map_to_commands`, `menu_bar_keys_walk_like_windows`,
   `arrows_open_and_close_submenus_like_windows`, `window_filter_takes_only_the_keyboard_menu_command`.

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
   error text.
2. Dates are always `YYYY.MM.DD`, time `HH:MM`, through the one shared formatter `src/fmt.rs` (`date`, `date_time`,
   `date_time_sec`). Do not format dates in UI code.
3. All user-visible strings go through `src/i18n.rs`, English and Russian both ([CONTRIBUTING.md](../CONTRIBUTING.md)).

## Build UI only from the shared helpers

1. Windows are created only by `dialog_window` in `src/app/dialog.rs`; it sets the title bar, close X, drag and
   position. Button rows (`dialog_buttons`, `dialog_choice`, `dialog_ok_cancel_apply`) and keys (`window_escape`, `window_keys`) live there too.
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
