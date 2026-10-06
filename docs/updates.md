<p align="right"><b>English</b> | <a href="updates.ru.md">Русский</a></p>

# Updates and rollbacks

The window **Help → Check for updates...** manages three components, each separately or all together (`src/update/mod.rs`, window in `src/app/updates.rs`):

| Component | What it is | Source | Installed by |
|---|---|---|---|
| Native | the original AmneziaWG client (mode 1) | GitHub releases of the AmneziaWG Windows client (`src/update/feed.rs`, `NATIVE_REPO`) | `msiexec` (`src/update/native.rs`) |
| Engine | the built-in engine of mode 2: `tunnel.dll` and `wintun.dll` | releases of this project, signed manifest | file set swap (`src/update/ours/`) |
| App | the program itself, `awg-ui.exe` | releases of this project, signed manifest | file set swap, then a core restart |

When all are updated together, the order is Native, Engine, App (`ORDER`): the app is last because it restarts the core.

## Where it runs

1. The updates manager (`src/update/manager.rs`) lives in the **agent** (`src/daemon/agent/updates.rs`), not in the core and not in the window. The window sends
   `Updates(UpdateOp)` to the agent pipe; the core refuses `Updates` from old windows with an explanation. See [Protocols](protocols.md).
2. The manager touches tunnels only through three internal core requests, `HoldNative`, `Release` and `ReconnectEngine` (`src/update/core_link/`); see
   [Supervisor](supervisor.md).
3. One job at a time (check, update or restore); a second command during a job gets "busy". The job runs in its own thread and the window sees its progress
   (`busy`) in the state. A panic inside a job is an event in the log, not a stop of the process.
4. Network: HTTPS through WinHTTP (`src/update/net.rs`), system proxy, redirects never downgrade to HTTP, 30 s network timeouts, a total deadline of 2 min
   for a small request and 30 min for a download. Limits: GitHub API answer 2 MiB, release notes cut to 2000 characters, manifest 64 KiB, MSI 64 MiB,
   a file of our release at most 256 MiB and at most its size in the manifest.
5. Data in `C:\ProgramData\AmneziaWG UI Dark\updates`: `history.json`, `state.json` (last check result), `backups\`, `downloads\` (temporary, cleaned after
   every job and at start), `logs\` (`msiexec` logs), `started.json` (a restore of AmneziaWG in progress), `after_change.json` (an engine reconnect not yet accepted by the core), `app\` (the new build for the window).

## What a command does

1. `State` - the current state without the network.
2. `Check` - asks the sources: the latest release of the native client, the latest release of this project with its manifest, and for the engine row the newest
   stable tag of the upstream engine repository (shown as "current", "newer exists - comes with an app update" or "not checked"). The result goes to
   `state.json`; a source error shows in that row only. A release of this project without a signed manifest (before 0.4.0) is not an error: it can be installed
   only by hand and the window says so neutrally.
3. `Apply(list)` - a fresh check first, then, for each selected component in `ORDER`:
   1. the version found must equal the version the user confirmed (a different one - the component is not touched, a history row with the error);
   2. it must be newer than the installed one (numeric comparison by dot-separated parts, a leading `v` ignored); if not, only a log line;
   3. a **backup of the current version** (see below); a failed backup stops that component;
   4. install; a history row `Update` with `from`, `to` and the link to the backup made just before (`prior_backup`);
   5. on success, an event in the log; for the engine the core is asked to reconnect mode 2 tunnels (`ReconnectEngine`). The request is owed
      durably: `after_change.json` lists the engine before its files are replaced and is cleared only when the core accepted the request (or the
      replacement returned an error). A record left by a dead agent or a core that did not answer is re-sent on the next scheduler tick (every 30 s,
      not while a job runs); a failed retry is logged once until it succeeds.
   A failed component does not stop the others.
4. `Restore(row)` - see [Rollback](#rollback).

## Verification

Nothing is installed before every check below passes.

1. **Native AmneziaWG (MSI)** (`src/update/backup.rs`, `src/update/native.rs`):
   1. the release must carry a SHA-256 for the file (the `digest` field of the GitHub asset); no digest - refusal; size and SHA-256 of the download must match;
   2. Authenticode signature checked by Windows (`WinVerifyTrust`, revocation checked over the whole chain, no UI);
   3. the signer's certificate name must be exactly `Privacy Technologies OU` (`PUBLISHER`);
   4. the MSI's own `Property` table is read without installing anything: `UpgradeCode` must be `{876B57E4-4490-4442-A983-721EE141B00D}` (the amd64
      product) and `ProductVersion` must be the version that was asked for.
   The same checks run on the saved installer of a backup **before** anything is uninstalled during a restore.
2. **Our engine and app: the signed manifest** (`src/update/sign.rs`):
   1. the release carries `update-manifest.json` and `update-manifest.json.sig`;
   2. the signature is **SSHSIG** (`-----BEGIN SSH SIGNATURE-----`), made with `ssh-keygen -Y sign`, algorithm **Ed25519**, in the namespace
      **`awg-ui-update`**. The checker requires: SSHSIG version 1, the namespace exactly that string, the signing key equal to the public key
      embedded in the program (`UPDATE_KEY`), the message hash `sha512` or `sha256`, and a valid Ed25519 signature over the exact bytes of the JSON;
   3. the manifest is then validated: versions are 1 to 64 characters of letters, digits, `.`, `-`; the app entry is `awg-ui.exe`; file names are plain names;
      every SHA-256 is 64 lowercase hex digits; the engine files are exactly `tunnel.dll` and `wintun.dll`;
   4. every file is downloaded with a size cap and its size and SHA-256 are compared with the manifest entry;
   5. at install time the manifest is downloaded **again** and must be identical, as a whole, to the one checked at the search (a re-signed release with
      the same versions but other sums would otherwise leave installed files that do not match the manifest). The downloaded bytes are what gets installed,
      because the signature covers those exact bytes.
3. **Trust in the engine at run time** (`src/engine.rs`, `verify`): a `tunnel.dll` or `wintun.dll` is loaded only if its SHA-256 equals the sum pinned
   into the program at build time or is listed in the verified local manifest in the program folder (the one the engine update installs).
4. **The window's own update** takes `awg-ui.exe` from the folder the agent publishes (`updates\app`), checks the manifest signature with the embedded key,
   the manifest version equal to the core's, and the size and SHA-256 of the file; the verified bytes are installed (the file is not read a second time).

## Backups and history

1. Before every update and every restore the current version is backed up into `updates\backups\<row>-<component>-<version>\` with a `component.json`:
   1. Native: `installer.msi` (downloaded again from the release of the installed version and verified as above) and `configs\` - the tunnel files
      `*.conf.dpapi` copied as opaque files, never decrypted; links and other reparse points are skipped;
   2. Engine: both DLLs and the local manifest if there is one;
   3. App: the file set of the build and its version.
2. The history (`history.json`, written through a temporary file so a cut write does not corrupt it) has rows `Backup`, `Update` and `Restore` with `from`, `to`,
   success, error text and `prior_backup`. A corrupted file is reported in the event log and the history starts empty; rows with
   impossible field combinations are repaired and each repair is logged.
3. Limits, applied after every job and at start: at most 200 rows, at most 5 backups per component (the oldest by row number go first, **except** the backup made
   before the last update or restore of that component, which is never deleted), at most 20 `msiexec` logs. Backup folders of dropped rows are deleted.
4. The **Restore** button of a row goes to (`src/update/restore_target.rs`; one rule for the window and for the command):
   1. a `Backup` row: its own copy;
   2. an `Update` row: the copy made right before the update (the "was" version);
   3. a `Restore` row: the copy of the version that restore replaced.
   It is unavailable if the target version is already installed or its copy is no longer on disk.

## Rollback

1. **Needs UAC.** `Restore` is accepted only from a client whose token is elevated and has the Administrators group active (`updates_allowed`,
   checked by the agent for every restore, not only for a downgrade: versions are not always comparable, and a simple rule has nothing to go around). The window
   starts a helper `awg-ui.exe --core-updates restore <row>` with administrator rights (the UAC prompt); it sends the request to the agent and exits, the
   progress is visible in the window. A non-elevated `Restore` gets an error and leaves a warning with the caller's account in the event log. Without this a
   program of the owner could silently downgrade a component to a version with known holes. `Check` and `Apply` do not need elevation.
2. A restore first backs up the **current** state (so one can go back again), then installs from the chosen copy; the row records `prior_backup`.
3. **Native** restore: the saved installer is verified (signature, `UpgradeCode`, `ProductVersion` equal to the copy's version) before anything is removed; the
   tunnel files are merged (the current set wins, files only the old copy has are added); the current version is uninstalled, the old one installed, the tunnels
   put back. If the old version does not install, the pre-restore copy is installed again with its tunnels (the message says "rolled back" or
   "rollback failed" with the copy's path). The history row is written **before** the work with a marker file; if the process stops in the middle, the row becomes an
   "interrupted" error at the next start.
4. While an MSI runs, the core takes the mode 1 tunnels off supervision (`HoldNative`, a lease of 15 minutes; the core picks the running and desired
   tunnels itself) and gives them back afterwards (`Release`) whatever the outcome; if the core is unreachable or the lease cannot be taken, the MSI does
   not start.

## Installing the engine and the app

1. **File set swap** (`src/update/ours/fileset.rs`): every file to replace is renamed to `<name>.old-<random>` (a loaded DLL and a running exe can be renamed),
   the new one is copied in its place. Any error - everything put in place is removed, everything moved aside is returned; a file that cannot be returned is
   renamed to `<name>.keep-<random>` and named in the error (`.keep-` files are never deleted). Leftover `.old-` files are removed at the next install and
   service start.
2. **App update**: the new `awg-ui.exe` is verified and swapped in the program folder; if the installed engine DLLs do not match the new manifest they are
   downloaded and swapped in the same set; the manifest and signature are always installed with it. The build is also published for the window
   (`updates\app`, readable by the owner account), then a detached helper `awg-ui.exe --restart-core <replaced set>` is started outside the agent's job object
   (the agent dies with the core it restarts). If publishing or starting the helper fails, the set is rolled back and the core stays on the previous build.

## Self-update and the fallback of `--restart-core`

`src/update/ours/fallback.rs`. The helper runs only as SYSTEM (otherwise exit code 2).

1. After 2 s it stops the core service (waits up to 30 s) and starts it again: up to 4 attempts, 5 s apart, each waiting up to 30 s for the "running"
   state, which the core reports only after its own pipe answers `Hello` with the same version.
2. If the pipe name is held by another program, the new build is not blamed (otherwise any program of the owner could roll an update back without UAC): up to
   8 rounds of starts, about 10 minutes, are made; if the name does not free up, the core stays stopped on the new build and the service manager's failure
   action or a reboot brings it up (code 10).
3. If the new core does not start: stop it, **put the whole replaced set back** and start the previous build. Order: the new signed manifest moves aside
   first, then DLLs and exe one by one, then the previous manifest returns, so a mixed set never counts as trusted at any step, even if interrupted. The history
   gets the `Update` row as an error and a `Restore` row from the new version to the previous one, and the core's log gets the outcome. Exit codes: 0 new core
   works; 2 not SYSTEM; 3, 4 no access to the service; 5 service did not stop; 6 previous build restored and running; 7 nothing to restore; 8 only part restored
   (the core does not start, the set is mixed); 9 previous build restored but did not start; 10 pipe name never freed.
4. **The window** starts its own update at its next launch: if the folder `updates\app` holds a build equal to the core's version and newer than its own and it
   passes the manifest checks above, the window moves its `awg-ui.exe` aside to `awg-ui.exe.old`, writes the verified bytes in its place and starts the new process with the
   same arguments and the variable `AWG_UI_UPDATED_FROM=<pid>`. The new process waits up to 10 s for the old one to exit (it holds the single-instance mutex)
   and deletes `.old`. A failed write puts the old exe back.

## Daily check and reminder

1. **Check** (agent, `src/update/clock.rs`, `Manager::daily`): a thread wakes every 30 s. The first check is 120 s after the agent starts; later when 24 h have
   passed since the last one stored in `state.json` or there has been none. A clock moved backwards means "not due". If a job is running the step is skipped.
   The check only **looks**: nothing is installed without the user. Each new version gets one line in the event log (`announced` in `state.json`).
2. **Reminder** (window, `src/app/reminder.rs`):
   1. a new version is announced at once; a version already announced and still not installed is reminded of once a day (`REMIND_EVERY` = 24 h);
   2. only if the user is at the computer (input within the last 5 minutes) and Windows accepts notifications (no full-screen program, presentation,
      do-not-disturb or locked session); otherwise the check is repeated every 3 minutes. If Windows does not answer, the user is assumed present;
   3. the notice appears at the bottom right of the window (**Open** / **Later**; **Later**, the close button and `Esc` postpone the next reminder by a day);
      with the window hidden in the tray Windows shows a notification as well;
   4. after the update is installed the offered list empties and the reminders stop. The versions already told about and the time of the last reminder are kept in
      the window's settings.
3. The mark on the **Help** menu is set whenever an update is found, independent of the reminder.
