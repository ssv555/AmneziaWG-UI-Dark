# Contributing to AmneziaWG UI Dark

Thank you for your interest! Bug reports, ideas, UI suggestions, code, documentation and translations are all
welcome, from a one-word typo fix to a whole new feature. Please be kind and constructive.

*Краткое резюме на русском - в конце файла.*

## Ways to help

1. **Report a bug** or **suggest an idea** - open an [issue](https://github.com/ssv555/AmneziaWG-UI-Dark/issues).
2. **Translate** the interface into your language (see [Translations](#translations)).
3. **Send a pull request** - code, tests, documentation, screenshots.
4. **Test** on your setup (Windows 10 / 11, different AmneziaWG versions) and tell us what you find.

For a larger change, please open an issue first so we can agree on the approach before you spend time on it.

## Building

You need Windows, [Rust](https://rustup.rs) with the MSVC toolchain, the Visual Studio Build Tools and the
Windows SDK (the build script uses `rc.exe` to embed the icon).

```powershell
git clone https://github.com/ssv555/AmneziaWG-UI-Dark.git
cd AmneziaWG-UI-Dark
cargo build --release
```

The binary is `target\release\awg-ui.exe`.

You do **not** need AmneziaWG installed or administrator rights to try the interface: run it in demo mode.

```powershell
target\release\awg-ui.exe --demo
```

Demo mode uses invented tunnels and keeps its settings in `%TEMP%\awg-ui-demo`. To capture a screenshot of the
window for a pull request: `awg-ui.exe --demo --snapshot out.png` (add `--about` for the About window). Before
committing images, compress them losslessly with [oxipng](https://github.com/shssoichiro/oxipng):
`oxipng -o max -Z --strip safe img/*.png` (the screenshots in `img/` shrink by about two thirds).

## Testing

Run the unit tests before every pull request:

```powershell
cargo test --release
```

- Add a test for every bug fix (a failing test first, then the fix) and for new logic.
- Tests must not touch anything outside the project and the current user: no system services, no other accounts,
  no real tunnels. Logic that needs the real AmneziaWG should be covered with mocks, or described as a manual
  check in the pull request.
- If you change the interface, run the app in demo mode and look at it. Say in the pull request what you checked.

## Code style

- Rust 2021, formatted with `rustfmt` (`cargo fmt`). No new warnings from `cargo build --release`.
- Keep functions small and single-purpose; prefer composition over inheritance-like patterns; do not add an
  abstraction until the project actually needs one.
- Keep a change focused: fix what the pull request is about, do not mix in unrelated refactoring.
- Fix the cause of a problem, not the symptom (no blanket `try`/retry/fallback to hide a bug).
- Comments in the existing code are written in Russian. Comments in English are equally welcome; write in the
  language you are comfortable with, but keep one language within one comment.
- User-visible text goes through the translation table in `src/i18n.rs`; do not hard-code strings in the UI code.
- Do not commit secrets, real tunnel names, endpoints or keys. Use documentation addresses
  (`203.0.113.0/24`, `192.0.2.0/24`) in tests and screenshots.
- Update the documentation (`README.md`, `README.ru.md`) in the same pull request as the code it describes.

## Commit messages

- Short imperative subject line, up to about 72 characters: `Fix group drag onto its own subgroup`.
- Optionally a blank line and a body explaining *why*, not just *what*.
- One logical change per commit; reference the issue as `Fixes #123` when there is one.

## Translations

The interface is English by default, with Russian built in. Other languages are plain text files
`lang\<ISO 639-2 code>.lng` (`deu.lng`, `fra.lng`, `spa.lng`, ...) next to the exe.

1. Run `awg-ui.exe --demo`, open **Language -> Add language...** - a template with all keys and the English text
   is created and opened in Notepad.
2. Translate the values, keep the keys and the `{0}`, `{1}` placeholders unchanged, save the file with the
   language's code as its name.
3. Choose the language in the **Language** menu and look through the dialogs. Missing keys and empty values fall
   back to English.
4. Send the `.lng` file in a pull request (or attach it to an issue if you are not comfortable with Git).

## Filing issues

Please include:

1. Windows version and the AmneziaWG client version.
2. AmneziaWG UI Dark version (**Help -> About**).
3. What you did, what you expected and what happened; a screenshot helps.
4. Relevant lines from `logs\events.log` - **with tunnel names, endpoints and keys removed if they are private**.

Never paste private keys or full configs into an issue.

## Pull requests

1. Fork the repository and create a branch from `main` (`fix/group-drag`, `feat/spanish-translation`).
2. Make the change, run `cargo test --release`, run the app in demo mode.
3. Open a pull request: describe what changed and why, link the issue, attach a screenshot for UI changes.
4. Be ready for a friendly review. Small, focused pull requests are merged fastest.

## License of contributions

The project is free for non-commercial use; commercial use requires the author's written permission (see
[LICENSE](LICENSE)). By submitting a contribution you agree that it is distributed under the same terms.

## Кратко по-русски

Помощь приветствуется любая: ошибки и идеи - в [issues](https://github.com/ssv555/AmneziaWG-UI-Dark/issues),
код и переводы - pull request'ами. Сборка: Rust (MSVC) + Windows SDK (`rc.exe`), `cargo build --release`;
проверка - `cargo test --release`; посмотреть интерфейс без AmneziaWG - `awg-ui.exe --demo`. Код - Rust 2021,
`cargo fmt`; комментарии в коде русские, английские тоже подходят; тексты интерфейса - только через таблицу
`src/i18n.rs`. Сообщения коммитов - короткая строка в повелительном наклонении. Перевод - файл
`lang\<код ISO 639-2>.lng` (меню «Язык -> Добавить язык...»). В issue не публикуйте приватные ключи и полные
конфиги. Для крупных изменений сначала откройте issue. Вклад распространяется на тех же условиях, что и проект
(бесплатно для некоммерческого использования, коммерческое - с письменного разрешения автора).
