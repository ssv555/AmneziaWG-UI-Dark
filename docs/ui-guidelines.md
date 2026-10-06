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

1. Buttons sit bottom-right: the primary (accent-filled) one first, then Cancel. Minimum width about 80 px, one
   default button per dialog.
2. Keyboard: sensible Tab order, visible focus, Alt mnemonics in menus, a context menu on right click and on
   Shift+F10 / Menu key, built only through `menu::context_menu` (guard test `context_menus_go_through_the_helper`).

## Errors and text

1. Errors of actions go to the event log (and `window-errors.log`); the status bar shows only a link to the log, no
   error text.
2. Dates are always `YYYY.MM.DD`, time `HH:MM`, through the one shared formatter `src/fmt.rs` (`date`, `date_time`,
   `date_time_sec`). Do not format dates in UI code.
3. All user-visible strings go through `src/i18n.rs`, English and Russian both ([CONTRIBUTING.md](../CONTRIBUTING.md)).

## Build UI only from the shared helpers

1. Windows are created only by `dialog_window` in `src/app/dialog.rs`; it sets the title bar, close X, drag and
   position. Button rows (`dialog_buttons`, `dialog_choice`) and keys (`window_escape`, `window_keys`) live there too.
2. The guard test `windows_follow_the_standard` in `src/app/dialog.rs` fails the build of the tests if `src/` creates
   an egui window outside `dialog_window`, or if `src/app/` uses `.anchor(`, `.movable(false)` or `.title_bar(false)`.
3. Need a pattern the helper does not cover: extend the helper, do not write a one-off window.

## Check before you send

Walk through the change in the live window (`awg-ui.exe --demo` is enough):

1. Drag it by the title bar, close it with X and with Esc, confirm with Enter.
2. Tab through it and look at the focus.
3. Both working modes (mode 2 is yellow), at 100 % and 150 % Windows scale.

Say in the pull request what you checked.
