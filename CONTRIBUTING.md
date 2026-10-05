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

The built-in engine (working mode 2) is built separately: run `.\engine\build.ps1` **before** `cargo build`. It
downloads Go, llvm-mingw and wintun (checked by SHA-256), builds `tunnel.dll` from a pinned
[amneziawg-windows](https://github.com/amnezia-vpn/amneziawg-windows) tag and puts the files into `engine\out`.
`build.rs` embeds the SHA-256 of both DLLs into `awg-ui.exe`; the app copies only DLLs with these checksums for the
tunnel service, and a build made without `engine\out` simply has no built-in engine. Put `tunnel.dll` and `wintun.dll`
next to `awg-ui.exe` to use the mode.

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

Every push and pull request is checked by the CI workflow: tests and a warning-free release build on Windows.

## Releases

1. Raise `version` in `Cargo.toml` (and run `cargo build` so `Cargo.lock` follows).
2. Add a row on top of the table in `CHANGELOG.md` and `CHANGELOG.ru.md`: version, date, changes separated by `<br>`.
3. Push a tag `vX.Y.Z`. The release workflow checks that the tag matches `Cargo.toml`, runs the tests, builds
   `awg-ui.exe` and publishes a GitHub Release with the exe, a zip, checksums and notes taken from the changelog rows.
   The release also carries `tunnel.dll`, `wintun.dll` and the signed update manifest (see below).

## Upstream updates

The `Upstream` workflow runs daily and compares the pins in `engine/build.ps1` with upstream: the newest stable
amneziawg-windows tag, the amneziawg-go version that tag asks for in its `go.mod`, wintun and the latest patch of the
pinned Go minor line. For every outdated item it opens one issue titled `Upstream update: <item> <version>`; the same
title is never opened twice (a closed issue counts too, so close it only once you have decided about that version).
A failed check (network, rate limit, unexpected page) turns the job red instead of passing silently. The same
workflow runs `cargo audit` on `Cargo.lock` every Monday, and Dependabot proposes weekly updates of Cargo
dependencies and GitHub Actions.

To bump a pin:

1. Edit the pins in `engine/build.ps1`: `$EngineTag` and `$EngineCommit` (`git rev-parse <tag>^{commit}` in the
   amneziawg-windows clone), and the `Url` / `Sha` of Go or wintun (take the checksum from the publisher's page, not
   from the file you just downloaded). Also update the wintun version in the `ENGINE.txt` line of the same script.
2. Run `.\engine\build.ps1` and then `cargo test --release`; the script fails if a hash or the commit does not match.
3. Check the engine on a test machine (connect in mode 2), then release as described in *Releases*: the new
   `tunnel.dll` / `wintun.dll` hashes go into the signed update manifest automatically.

Check the pins by hand: `powershell -File engine\check-upstream.ps1` prints a JSON report; exit code 0 means fresh,
10 outdated, 2 the check failed. llvm-mingw is not tracked by default (its pin mirrors the official client's build);
add `-IncludeLlvmMingw` to see it.

## Update signing key

The app updates itself and its engine only from releases with a valid signature. The release workflow writes
`update-manifest.json` (versions, sizes and SHA-256 of `awg-ui.exe`, `tunnel.dll` and `wintun.dll`) and signs it with
`ssh-keygen -Y sign -n awg-ui-update` into `update-manifest.json.sig`. The app checks the signature against the public
key `UPDATE_KEY` in `src/update/sign.rs`, then checks every downloaded file against the manifest.

- The private key is the repository secret `UPDATE_SIGNING_KEY`: the full contents of an OpenSSH ed25519 private
  key file without a passphrase. A release fails if the secret is empty. Never commit the private key.
- Forks build their own releases: generate your own key pair and put your public key into `UPDATE_KEY`.
- Rotation: create a new key pair (`ssh-keygen -t ed25519 -N "" -C awg-ui-update -f awg-ui-update`), put the new
  public key into `UPDATE_KEY` and publish that build as a release **signed with the old key**; only after that
  replace the secret with the new private key. Builds released before the switch accept only the old key, so
  without that intermediate release they would stop accepting updates.

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
конфиги. Для крупных изменений сначала откройте issue. Манифест обновлений подписывается ключом из секрета
`UPDATE_SIGNING_KEY`, открытый ключ - `UPDATE_KEY` в `src/update/sign.rs` (смена ключа - в разделе
«Update signing key»). Вклад распространяется на тех же условиях, что и проект
(бесплатно для некоммерческого использования, коммерческое - с письменного разрешения автора).
