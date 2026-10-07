<p align="right"><b>English</b> | <a href="development.ru.md">Русский</a></p>

# Development

How to build, test and release the project. Contribution rules (issues, pull requests, code style, commit messages,
translations) are in [CONTRIBUTING.md](../CONTRIBUTING.md) and are not repeated here.

## Toolchain

1. Windows 10/11 x64.
2. Rust 1.95 or newer (the minimum of eframe 0.36), stable channel, MSVC target (`x86_64-pc-windows-msvc`),
   edition 2021. CI installs the latest stable.
3. Visual Studio Build Tools and the Windows SDK: `build.rs` uses `rc.exe` to embed the icon and version info.
4. `.cargo/config.toml` links the C runtime statically (`+crt-static`), so the exe needs no VC++ Redistributable.
   Setting `RUSTFLAGS` replaces these flags; do not set it for release builds.
5. The engine build (below) additionally needs PowerShell and `git`; Go, llvm-mingw and wintun are downloaded by the
   script itself.

## Build

```powershell
cargo build --release
```

Result: `target\release\awg-ui.exe`. The release profile is `opt-level = 3`, thin LTO, stripped. The build must be
free of warnings (CI fails on any line starting with `warning`).

`awg-ui.exe --demo` runs the window without AmneziaWG and without administrator rights, on invented tunnels.
`--demo --snapshot out.png` writes a screenshot of the window; the full command-line reference is in the
[README](../README.md).

## Engine build

The built-in engine (working mode 2) is `tunnel.dll` (from a pinned tag of amneziawg-windows) and `wintun.dll`:

```powershell
.\engine\build.ps1     # before cargo build
```

1. `engine/build.ps1` downloads Go, llvm-mingw and wintun into `engine\.deps`, checking each archive by SHA-256,
   clones the pinned amneziawg-windows tag into `engine\.src`, verifies the commit hash and builds the DLLs into
   `engine\out`. All pins (`$EngineTag`, `$EngineCommit`, download URLs and hashes) are at the top of the script.
2. `build.rs` embeds the SHA-256 of both DLLs from `engine\out` into `awg-ui.exe`. The app copies the DLLs for the
   tunnel service only if their hashes match. Without `engine\out` the app builds, just without the built-in engine.
3. To run mode 2 from a local build, put `tunnel.dll` and `wintun.dll` next to `awg-ui.exe`.
4. `engine/check-upstream.ps1` compares the pins with the newest upstream versions and prints a JSON report. Exit
   code: 0 fresh, 10 outdated, 2 the check failed. How to bump a pin: *Upstream updates* in
   [CONTRIBUTING.md](../CONTRIBUTING.md).

## Tests

```powershell
cargo test --release
```

CI runs `cargo test --release --locked`. Tests are unit tests next to the code (`#[cfg(test)]` modules in `src/`);
fixtures are in `tests/fixtures/`. They use mocks and temporary folders and do not touch services, other accounts or
real tunnels.

Tests marked `#[ignore]` are not run by default:

| Test | What it needs |
|------|---------------|
| `live_latest_native` (`src/update/feed.rs`) | Network: the latest release of the original AmneziaWG client has an amd64 MSI with a SHA-256 |
| `live_msi_identity` (`src/update/native.rs`) | Network: downloads a real MSI and checks its identity |
| `live_installed` (`src/update/native.rs`) | An installed AmneziaWG client on this machine |
| `live_download_small`, `live_max_enforced` (`src/update/net.rs`) | Network: download progress and the size limit |
| `fake_agent_process` (`src/daemon/server.rs`) | Not a test: the entry point of a fake helper process that the core-isolation tests start themselves |

Run the live ones on purpose, with output:

```powershell
cargo test --release -- --ignored --nocapture live_
```

Do not run `--ignored` blindly: `fake_agent_process` does nothing without its argument, but the `live_*` tests use
the network and the real machine state. They never install, uninstall or restart anything.

Logic that needs a live system (service install, update, MSI, tunnels) is checked by hand and described in the pull
request.

### Flaky test logs

The core-isolation tests (`isolation_*` in `src/daemon/server.rs`) record a timing mark for every phase of each
Switch: client connect, pipe accept, request thread start, core locks, config write, engine calls
(`src/daemon/phase_trace.rs`). If a Switch fails or takes 1 s or more, the test writes the trace with machine facts
(CPU count, CPU load, thread count, process and binary age) to `target/test-logs/<test>-<unix time>.log` and puts
the path into the failure message. Developers only: the trace exists only in test builds, the release exe has none.

## CI workflows

All in `.github/workflows/`:

| Workflow | Trigger | What it does |
|----------|---------|--------------|
| `ci.yml` | push to `main`, pull request | On Windows: `cargo test --release --locked`, then a release build that fails on any warning |
| `release.yml` | tag `v*` | Builds and publishes a release (see below) |
| `upstream.yml` | daily, Mondays, manual | Daily: runs `engine/check-upstream.ps1` and opens one issue per outdated pin (`Upstream update: <item> <version>`). Mondays: `cargo audit` over `Cargo.lock`, fails on vulnerabilities |

`.github/dependabot.yml` proposes weekly updates of Cargo dependencies and GitHub Actions.

## Release

1. Raise `version` in `Cargo.toml` (and build once so `Cargo.lock` follows).
2. Add a row on top of the table in `CHANGELOG.md` and `CHANGELOG.ru.md`: version, date, changes separated by `<br>`.
   The row text becomes the release notes.
3. Push the tag `vX.Y.Z`. `release.yml` then:
   1. checks that the tag equals the `Cargo.toml` version and that the secret `UPDATE_SIGNING_KEY` is set;
   2. builds the engine, runs the tests, builds `awg-ui.exe`;
   3. packs a zip (exe, licence, README and CHANGELOG in both languages, engine DLLs);
   4. writes `update-manifest.json` (versions, sizes and SHA-256 of `awg-ui.exe`, `tunnel.dll`, `wintun.dll`) and signs
      it with `ssh-keygen -Y sign -n awg-ui-update` into `update-manifest.json.sig`;
   5. writes `SHA256SUMS.txt` and publishes a GitHub Release with all of these and the notes from both changelogs.
4. The app updates itself only from a manifest whose signature matches the public key `UPDATE_KEY` in
   `src/update/sign.rs`, and checks every downloaded file against the manifest (see [updates](updates.md)). A fork
   needs its own key pair; key rotation is described in the *Update signing key* section of
   [CONTRIBUTING.md](../CONTRIBUTING.md).

The release is not code-signed yet; the notes mention the SmartScreen warning.

## Before you push

1. `cargo fmt`, `cargo test --release`, `cargo build --release` without warnings.
2. UI changes: follow [ui-guidelines.md](ui-guidelines.md) and look at the result in the window.
3. Update the affected pages in this folder in the same commit ([index](README.md)).
