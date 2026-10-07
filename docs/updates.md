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
   every job and at start), `logs\` (`msiexec` logs), `started.json` (a restore of AmneziaWG in progress), `restore\configs\` (the tunnel files of that
   restore, kept until they are back in AmneziaWG's folder), `after_change.json` (an engine reconnect not yet accepted by the core), `swap.json` (a file set swap in progress, see [Installing the engine and the app](#installing-the-engine-and-the-app)), `app\` (the new build for the window).

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
   The same request without the window, for a scripted check of a live update: `awg-ui.exe --core-updates apply native|engine|app <version>`
   (`parse_cli` in `src/update/mod.rs`). It goes to the agent like the window's Update button, with the same checks; the command waits up to
   10 minutes while the job runs (2 minutes for `check`) and prints the state and the last history rows.
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
   success, error text and `prior_backup`. A file that cannot be parsed is moved aside as `history.json.unreadable-<date>` (the same for `state.json`),
   reported in the event log with the new name, and the history starts empty: the rows with the backup names stay in the moved file instead of being
   overwritten by the next job. Rows with impossible field combinations are repaired and each repair is logged.
   At start every folder in `backups\` with a valid `component.json` that no row refers to (rows lost with an unreadable file, for example) becomes a
   `Backup` row again (`adopt_orphans` in `src/update/backup.rs`, `History::adopt` in `src/update/history.rs`): the row number comes from the folder name if it is free, otherwise a new one; the rows
   are saved at once and each one is logged. A folder without a valid `component.json` is left untouched, is not offered and is logged with its path.
   Row numbers found in folder names are never given to new rows.
3. Limits, applied after every job and at start: at most 200 rows (the last update or restore of each component and the backup before it
   stay even beyond that), at most 5 backups per component (the oldest by row number go first, **except** the backup made
   before the last update or restore of that component, which is never deleted), at most 20 `msiexec` logs. Backup folders of dropped rows are deleted.
4. The **Restore** button of a row goes to (`src/update/restore_target.rs`; one rule for the window and for the command):
   1. a `Backup` row: its own copy;
   2. an `Update` row: the copy made right before the update (the "was" version);
   3. a `Restore` row: the copy of the version that restore replaced.
   It is unavailable if the target version is already installed or its copy is no longer on disk. An App row older than `0.5.0` (`MIN_APP_RESTORE`, the
   first build with the agent) is refused with an explanation: such a core has no agent pipe, and this window talks to the agent for updates and tunnel
   configs - after the restore the owner could neither roll forward nor edit tunnels from the window.

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
   "interrupted" error at the next start. The merged tunnel files wait in `updates\restore\configs\` (not in `downloads\`, which every start cleans) until
   they are back in AmneziaWG's folder; after an interrupted restore the next start of the agent copies them there itself when AmneziaWG is installed, and
   otherwise keeps them and names the folder in the event log (install AmneziaWG, and the next start puts them back). Without this the supervisor, once the
   lease ended, would find the tunnels without their files and drop them from the desired set. If a Windows installer is still running when the agent
   starts (the `msiexec` of the dead agent lives in the core's job object and may still be uninstalling AmneziaWG; its registry row goes last), nothing is
   copied: the uninstall would wipe the copied files and the staged folder would already be gone. The files wait, the log says so once, and the manager
   retries on its 30 s tick until the installer has finished.
4. While an MSI runs, the core takes the mode 1 tunnels off supervision (`HoldNative`, a lease of 15 minutes, renewed by a repeated `HoldNative` every
   5 minutes while `msiexec` runs; the core picks the running and desired tunnels itself) and gives them back afterwards (`Release`) whatever the outcome;
   if the core is unreachable or the lease cannot be taken, the MSI does not start. A failed renewal is a warning only (the lease still holds). `msiexec` is
   waited for at most 60 minutes (the core's `MAX_LEASE`): then the job fails with an error naming the installer's log, the lease is released and the
   process is left alone (a killed installer leaves a half-done transaction).
5. The MSI closes the AmneziaWG window (the `amneziawg.exe` process in the user's session) at its very start, and AmneziaWG does not open it
   again. The core and the agent run as SYSTEM in session 0 and start no windows, so **the window of this program** reopens it
   (`src/app/native_reopen.rs`):
   1. The agent, the side that runs the MSI, looks right before it (before the install, or before the uninstall of a restore) whether an
      `amneziawg.exe` process runs in any non-zero session (`update::native::ui_open`, process list with session ids only; session 0 holds its
      services). A whole MSI takes 1-3 s, so a window polling once a second could see neither the job nor the open AmneziaWG window.
   2. When the job ends (success or failure), the agent stores `NativeUiMark { seq, was_open }` in `state.json`: `seq` grows by one per job that ran
      an MSI. The agent's `State` carries it as `native_ui`; the window reads it once a second, also while hidden in the tray.
   3. The window decides once per new `seq`, even if it never saw the job running: `was_open`, the AmneziaWG process is not back in its session and
      AmneziaWG is installed - it asks the agent `NativeOp::Reopen`, the same start as the **AmneziaWG window** menu item but allowed in either mode
      (what the user had open comes back, in mode 2 too). A failure goes to the event log.
   4. Not opened: the AmneziaWG window was closed before the MSI; the first `seq` the window sees after its own start (a past job); the window of
      this program is not running at all. A mark stored by an agent that an app update restarted right after the job is still seen by the window.

## Installing the engine and the app

1. **File set swap** (`src/update/ours/fileset.rs`), in three phases so that a power loss or a killed agent at any point leaves a set that can be made whole:
   1. every new file is copied next to its target as `<name>.new-<random>` and flushed to disk (`FlushFileBuffers`), its SHA-256 and size are taken from the copy;
   2. the plan (folder, files, their `.new-` and `.old-` names, sums, files to remove) is written atomically to `updates\swap.json` (`src/update/journal.rs`,
      a durable step journal shared by multi-step actions of the updates subsystem; `src/update/ours/swap.rs` holds the plan and its replay);
   3. each file to replace is renamed to `<name>.old-<random>` (a loaded DLL and a running exe can be renamed) and its `.new-` copy is renamed into place; every
      rename is write-through (`MOVEFILE_WRITE_THROUGH`), so after a power loss the disk holds the steps in order. The journal is removed when the set is in place.
   Any error - everything put in place is removed, everything moved aside is returned, the journal is removed; a file that cannot be returned is renamed to
   `<name>.keep-<random>` and named in the error (`.keep-` files are never deleted). A new swap is refused while a journal of an unfinished one exists.
2. **Replay** (`swap::recover`): the core at its start (before tunnels, the agent and the cleanup of `.old-`), the agent at its start and `--install-core` read
   `swap.json` and bring the set to a consistent state: **forward** if every new file is intact (in place or in `.new-`), otherwise **back** to the previous files;
   if neither is possible (a previous file is missing or cannot be put back), the surviving previous files are kept as `.keep-`, the plan is removed and the event
   log names them - the next update or a restore from a backup installs a full set again. Only when even that fails the plan stays and the next start tries
   again. Replaying a consistent set changes nothing. The outcome is one event in the log. Leftover `.old-` and `.new-` files are removed at the next install and
   service start, except the ones a pending plan still names. A `swap.json` that cannot be parsed is moved aside as `swap.json.unreadable-<date>`, the files
   are left as they are and the `.old-`/`.new-` cleanup of that start is skipped: without the plan nobody knows which copy is the only good one, and the
   `.old-` files next to the quarantined plan are the way back for a manual fix. The plan carries a `version` field (1) so that a build reading another
   build's plan can tell it apart; new fields are added with defaults.
3. **App update**: the new `awg-ui.exe` is verified and swapped in the program folder; if the installed engine DLLs do not match the new manifest they are
   downloaded and swapped in the same set; the manifest and signature are always installed with it. The build is also published for the window
   (`updates\app`, readable by the owner account), then a detached helper `awg-ui.exe --restart-core <replaced set>` is started outside the agent's job object
   (the agent dies with the core it restarts). If publishing or starting the helper fails, the set is rolled back and the core stays on the previous build.

## Self-update and the fallback of `--restart-core`

`src/update/ours/fallback.rs`. The helper runs only as SYSTEM (otherwise exit code 2).

1. First it turns the service manager's restart-on-failure actions of the core service off and turns them back on at every exit (the core also re-applies them
   when it reaches "running", `install::reapply_failure_actions`, in case the helper was killed): otherwise, after the first failed start of the new build, the
   service manager would start it again on its own 5 s later - concurrently with the helper's retries, in the middle of the file rollback or the previous build
   before the history is written. Then, after 2 s, it stops the core service (waits up to 30 s) and starts it again: up to 4 attempts, 5 s apart, each waiting
   up to 30 s for the "running" state, which the core reports only after its own pipe answers `Hello` with the same version.
2. If the pipe name is held by another program, the new build is not blamed (otherwise any program of the owner could roll an update back without UAC): up to
   8 rounds of starts, about 10 minutes, are made; if the name does not free up, the core stays on the new build (code 10).
   Whenever the helper would leave the core stopped (codes 7-10), it makes one more start after the failure actions are back on: every start before ran
   with them paused, so a transient failure (an antivirus holding the just-renamed exe, a busy service manager) would otherwise leave the service stopped
   until a reboot; now the failure lands under the service manager, which restarts the core every 5 s. The log says so. If that start succeeds the exit
   code is 0 (new build) or 6 (previous build); a mixed set (8) keeps its code, since the set is not the one the history describes.
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
