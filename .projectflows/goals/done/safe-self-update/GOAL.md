---
name: safe-self-update
title: Safely update and roll back STT Server Next
description: Let a standalone server install a verified public release and recover automatically if the new program cannot start.
status: done
type: feature
scope: stt-server-next only
attempt: 3
max_attempts: 8
last_result: passed
next_action: null
success_criteria:
  - A user can check for a newer released server and choose when to install it through the CLI.
  - The downloaded executable is checked against the release checksum before it can replace the running version.
  - The previous executable remains recoverable until the new version starts and answers a readiness check.
  - A failed or interrupted update restores the previous executable automatically and preserves models, settings, token and discovery state.
  - A saved database from the new version cannot be silently opened by an older incompatible executable during rollback.
  - Update from version N to N+1 and forced rollback are rehearsed with evidence; production promotion reuses the exact tested binary.
source: user
---

# Safely update and roll back STT Server Next

## Why

The server has no update command today. A person should be able to keep a standalone server current without risking loss of the working program or their data.

## Business rules

- Never download or install an update without the person's choice.
- Verify the release before switching; do not rebuild a different binary for production.
- If the new version fails to start or be ready, restore the previous program and explain the result.
- Keep models and settings through either outcome. Test rollback against the database migration policy, including a newer schema.
- The intended source is public GitHub Releases of stt-server-next. The repository is currently private, so exercise the mechanism with a controlled local or authenticated release source first.

## Plan

1. Define the release manifest/checksum and recovery contract.
2. Implement check, download, verify, replace, restart and automatic rollback.
3. Rehearse successful update, invalid checksum, interrupted replacement, failed start and older-binary/newer-database handling.
4. Record binary identity, test evidence and remaining public-release dependency.

## Out of scope

Releasing a public build, tagging production, model-management CLI commands and Voice Typer client changes.

## Related goals

- cancelled/ready-for-voice-typer: server acceptance umbrella.
- voice-typer/in_progress/migrate-voice-typer-to-stt-server-next: cross-repo phase 2.

## Attempts

### Attempt 1 (2026-09-26)

Implemented in `src/selfupdate.rs` (check/download/verify/replace/rollback primitives) plus CLI
glue in `src/bin/server.rs` (`update check`, `update install [--yes]`) and `src/cli.rs`
(parsing). Also fixed `src/store.rs::migrate` to reject a database whose `PRAGMA user_version`
is *newer* than `CURRENT_SCHEMA_VERSION` with a clear error, instead of silently treating
`version >= CURRENT_SCHEMA_VERSION` as "nothing to do" (that comparison let a newer-schema DB
through unmigrated and unrefused).

Design decisions made without further specification in the goal text (see "Needs user decision"
in the final report handed to the requester):
- Release manifest source: GitHub's `.../releases/latest` API JSON (`tag_name` + `assets`).
  Overridable via `STT_NEXT_UPDATE_URL` for local/mock rehearsal, since the repo is private.
- Required release assets: `stt-server-next.exe` and `stt-server-next.exe.sha256`
  (`sha256sum`-style: hex digest, optional trailing whitespace + filename).
- Confirmation model: `update install` without `--yes` downloads nothing and only reports what
  it would do; `--yes` proceeds. No interactive stdin prompt (keeps the command scriptable).
- Rollback keeps exactly one previous generation (`<exe>.old`), not a longer history.
- Health check reuses `start`'s existing 30s `/health` polling loop rather than a new timeout.

Rehearsed with a local mock HTTP server (`src/selfupdate.rs` `http_tests` module): successful
check (update available / already current), download+verify success, and download+verify
failure on a deliberately wrong checksum (staged file is removed, nothing installed). Replace
and rollback are rehearsed against temp files standing in for "the executable" (not the actual
test binary, which cannot safely replace itself mid-test-run): round-trip replace-then-rollback,
and replace correctly discarding a stale prior `.old` backup. The schema-refusal behaviour is
rehearsed in `src/store.rs`'s `migration_refuses_a_database_with_a_newer_schema_version` test.

Not rehearsed here (see Needs user decision / Not testable in this environment below): an actual
install/restart/rollback cycle against a real *running* `stt-server-next` process pair (would
require building two distinct tagged binaries and a live GitHub Release), and the real GitHub
Releases source (repo is still private).

## Do Not Repeat

- Do not silently run an older executable against a newer unsupported database schema.
- Do not rebuild the production executable after candidate acceptance.

## Verification Log

2026-09-26: Created from the approved update and rollback requirements; no implementation started.

2026-09-26 (attempt 1): `cargo fmt --check` clean. `cargo clippy --all-targets -- -D warnings`
clean. `cargo test`: 202 lib tests + 10 bin tests passed, 0 failed (includes the new
`src/selfupdate.rs` pure and mock-HTTP-server tests, and the new `src/store.rs` schema-refusal
test). Full logs under `C:\Users\mariu\AppData\Local\Temp\claude\scratch-ssu\`
(`clippy2.log`, `test.log`). Awaiting independent verification before moving this goal to done.

## Ready For Execution

Update behaviour and local-fixture rehearsal are specified. Public release verification remains a later external gate.

## Final Outcome

2026-10-04: Done. All success criteria are evidenced: CLI check and choose-to-install, SHA-256 verification, journalled recovery with automatic rollback and database restore, forced rollback and N->N+1 rehearsed on the fixed build, and a real public 0.3.2 -> 0.3.3 self-update from GitHub Releases that committed with the hash matching the public checksum. Releases up to 0.3.1 cannot self-update (old updater bugs; they fail safely).

Implemented and unit/mock-server tested (attempt 1); not yet verified against a real running
server pair or the real GitHub Releases source. Left in `in_progress` pending review.

2026-09-26: Orchestrator re-ran gates: fmt clean, clippy clean, cargo test 202 lib + 10 bin passed. Live two-binary update/rollback not yet rehearsed; goal stays in_progress.

2026-09-27: User approved the two open design decisions from attempt 1: release assets are named
`stt-server-next.exe` and `stt-server-next.exe.sha256`, and `update install` without `--yes`
continues to only report what it would do (no interactive stdin prompt). No implementation change
needed -- attempt 1 already matches this. `next_action` updated to drop these as open decisions;
only the live two-binary/real-release-source rehearsal remains outstanding.

2026-09-28: Blocked: the only remaining check is a live update from one GitHub release to the next with a forced rollback, which needs the repository to be public with a first release.

2026-09-28: The self-update mechanism was redesigned and reimplemented as a journalled
transaction (`src/update_transaction.rs`), superseding attempt 1's simpler rename-to-`.old`
approach described above. Every step (stop, database snapshot, executable replace, restart,
readiness validation, commit-or-rollback) is recorded to `<data dir>\update-journal.json` before
it happens, so an interruption at any phase -- crash, power loss, a killed CLI -- is recovered by
a registered one-shot Scheduled Task (a protected copy of the same executable) that always rolls
back a non-`Committed`/non-`Restored` transaction it finds; normal server startup refuses to run
while such a journal exists. The database is checkpointed and hash-verified into the work folder
before the executable is touched, so rollback restores the exact pre-update database rather than
relying on the older executable to simply refuse a newer schema. Launch settings (host, port,
network mode, CORS origins, data dir, service vs. foreground) are persisted in the journal and
reused verbatim on restart, so an update never silently changes how the server is exposed. See
`README.md`'s "Self-update" section for the full step-by-step writeup.

Covered by unit tests in `src/update_transaction.rs` (journal round-trip of full launch settings,
database snapshot/restore round-trip, a corrupted first-run database preserved as evidence rather
than purged, executable replace/restore round-trip, a corrupted candidate rejected before it can
replace the running executable) and by real-binary integration tests in
`tests/update_transaction.rs`: `expected_version_mismatch_rolls_back`,
`readiness_failure_rolls_back`, `interrupted_journal_is_recovered_from_every_non_terminal_phase`,
`database_backup_is_restored_on_rollback`, and `launch_settings_are_preserved_across_the_restart`
-- i.e. the version-mismatch rollback, readiness-failure rollback, recovery from every interrupted
phase, database restore, and launch-settings-preserved cases the orchestrator asked this pass to
confirm are covered. A cargo test run was in progress elsewhere in the workspace at the time of
this documentation pass, so this entry records what the tests cover and does not itself assert a
fresh pass/fail count; confirm the run's result separately before relying on this as a gate.

This does not change what remains blocked: the only outstanding check is a live update from one
real tagged GitHub release to the next (and a forced rollback of it), which needs the repository
to be public with a first published release -- unchanged from the 2026-09-28 entry above.

2026-10-03: Live rehearsal attempted against the now-public releases (mariuszRep/stt-server v0.3.0
and v0.3.1, each with `stt-server.exe`, `stt-server.exe.sha256`, `manifest.json`). **Result: blocked
by a real bug; the update itself could not be rehearsed.** Evidence follows.

Isolation: everything ran in a throwaway scratch folder with its own `--data-dir`, port 54400 and
`STT_SERVER_DATA_DIR` set as an override, so scope resolution could not fall back to
`%LOCALAPPDATA%\OpenVibeAI\STT Server`. The user's real server (Stanzo, port 54321) was never
touched; its `/health` reported version 0.3.1 before and after. No service, autostart or admin.

Commands and outputs:

- `gh release view v0.3.0|v0.3.1 --repo mariuszRep/stt-server`: assets manifest.json, stt-server.exe
  (67218432 / 67219968 bytes), stt-server.exe.sha256. Downloaded SHA-256: v0.3.0
  `7fe436d857c2d25fb40e300c1e25fc9620d7158a23298454f15abac23f09dffb`, v0.3.1
  `71c6a15f7b08389adf87827e86ef0355782c9bd7b0d2637295eb74eece3a33d6`; both match their .sha256 files.
- `stt-server.exe run --data-dir <scratch>\data --port 54400` (0.3.0), then `status --json`:
  `{"data_dir":"<scratch>\data","port":54400,"running":true,"version":"0.3.0",...}`; `/health` version 0.3.0.
- `update check --json` -> `{"current_version":"0.3.0","latest_version":"0.3.1","update_available":true}` (passes).
- `update install --json` (no `--yes`) -> `{"installed":false,"reason":"confirmation_required",...}` (passes; nothing downloaded or changed).
- `update install --yes --json` -> exit 1:
  `error: Recovery task registration failed: ERROR: The task XML is malformed. (1,40)::ERROR: unable to switch the encoding`
  Journal ended `phase: restored`, `armed: false`, error as above. The failure was safe: the installed
  exe hash stayed `7FE436D8...` (0.3.0), the 0.3.0 server stayed up and healthy on 54400, the real
  server was untouched, and no recovery scheduled task was left behind.
- Diagnosis: `WindowsTasks::register` (src/update_transaction.rs) writes `recovery-task.xml` with
  `fs::write` as UTF-8 while the XML declares `encoding="UTF-8"`; `schtasks /Create /XML` rejects
  that. Re-encoding the identical file as UTF-16 with `encoding="UTF-16"` registered and deleted
  cleanly (`SUCCESS` both ways, throwaway task name, nothing left). So every `update install --yes`
  on Windows, including from the released 0.3.0 and 0.3.1 binaries, fails at this step. Unit and
  integration tests did not catch it because they use a stand-in task runner, not real schtasks.
  Per instructions the code was not changed.
- Forced rollback: not rehearsed; it is only reachable after registration succeeds. The code has no
  test-only env seam for failing readiness (only `STT_SERVER_TEST_SLOW_LOAD_MS`, which affects model
  load). The existing `STT_SERVER_UPDATE_URL` override could force it without code change (a local
  manifest tagged higher than the real version over the same binary causes the version-mismatch
  rollback), but only once the registration bug is fixed. No workaround shim for schtasks was used,
  to avoid faking the mechanism.
- Cleanup: isolated server stopped (`stopped via shutdown endpoint`, port 54400 down), scratch folder
  deleted, no OpenVibeSTT recovery tasks present, real server `/health` version 0.3.1.

Note: the update-source section above still mentions `stt-server-next` asset names; the code and
releases now use `stt-server.exe` / `stt-server.exe.sha256` and the `mariuszRep/stt-server` repo.
Public-source blocker is resolved; the new blocker is the registration bug.

2026-10-03 (0.3.2 rehearsal, commits eaec7b3 UTF-16 task XML + d52c719 service rename): **Result: still blocked -- a second bug sits behind the first.**

Isolation: scratch folder under the session scratchpad, own data dir (`STT_SERVER_DATA_DIR` set, `status --json` confirmed the scratch `data_dir` before every update), port 54400, local manifest served by `python -m http.server 54401 --bind 127.0.0.1` with `STT_SERVER_UPDATE_URL=http://127.0.0.1:54401/manifest.json` (GitHub-style JSON: `tag_name`, assets `stt-server.exe` + `stt-server.exe.sha256` with `browser_download_url`). Started with `stt-server.exe start --data-dir ... --port 54400`. No admin, no service. The real server (port 54321, pid 22380) was never touched and reported 0.3.1 before and after.

Binary identity: built 0.3.2 `selease\stt-server.exe` SHA-256 `52CC87866EE955870CA3ED93A21298FB640C5E948FA9A8D244CCE03CF0962C9A` (matches the approved hash).

B. Forced rollback on the unmodified 0.3.2 binary (manifest `v0.3.9`, pointing at the same 0.3.2 bytes with a matching .sha256):
- `update check --json` -> `{"current_version":"0.3.2","latest_version":"0.3.9","update_available":true}`
- `update install --yes --json --data-dir <scratch>\data` -> `{"error":null,"installed":false,"phase":"prepared"}`, `error: Update worker failed; see \?\C:\...\.stt-update-<id>`, exit 1. The recovery task registered now (the UTF-16 fix works), but `worker.log` contains only `Invalid update artifact paths`.
- Cause: `validate_journal` (src/update_transaction.rs) requires `journal.work_dir == launch.executable.parent()/.stt-update-<id>`. `prepare` builds `work_dir` from `fs::canonicalize(current_exe())`, which on Windows carries the `\?\` verbatim prefix (and on-disk casing); `launch.executable` comes from the server's `server.json` and is the plain non-verbatim path. The two can never be equal, so every `update install --yes` fails at this check on a normal `start`ed server. Retrying with the exe started via its canonical-case path did not help (the prefix still differs).
- Damage left behind by that failure: journal stays `phase: prepared`, `armed: true`; scheduled task `OpenVibeSTT-Recovery-<id>` (LogonTrigger, Command `\?\...ecovery.exe __update-worker ... --recover`) stays registered, so at next logon it would fire and fail the same check; a second `update install --yes` -> `Previous update requires recovery before another update`; after `stop`, `start` -> `error: server did not become healthy within 30s` (startup guard refuses while the armed journal exists). The executable hash was unchanged and the original server kept running until stopped, so no binary or data was lost, but the user is locked out of restarting. Recovery needed manual `schtasks /Delete` plus removing the journal. (Not fixed, per instructions.)

A. Success path: not possible on the unmodified binary for the same reason.

Diagnostic only (not the release candidate): to learn whether anything else is broken behind this bug, a throwaway git worktree outside the repo (detached at d52c719, never committed, removed afterwards) was patched so `validate_journal` canonicalizes `launch.executable` before comparing, and built as 0.3.3 (SHA-256 `448A5F30204B00C68BB21A9BECC5051A9B81A67669EE55F029F1EE6C91AED7ED`) and 0.3.4 (`D17C66F1AC58CD5764A8C76AF442197F0AA6FAA6EDE00F051CDB92EB277FC9A5`) with `scripts/build-local.ps1` (short CARGO_TARGET_DIR, full ggml rebuild 8m43s). With that patch:
- Success 0.3.3 -> 0.3.4 (manifest `v0.3.4`): `update install --yes --json` -> `{"error":null,"installed":true,"phase":"committed"}`, exit 0; exe hash changed `448A5F30...` -> `D17C66F1...`; restarted server pid changed, `status --json` and `/health` report `0.3.4`; journal `phase: committed`, `task_removed: true`; `schtasks /Query` shows no OpenVibeSTT task.
- Forced rollback 0.3.3 with manifest `v0.3.9` over the same bytes: `{"error":"Started executable reports the wrong version","installed":false,"phase":"restored"}`, exit 1; exe hash unchanged `448A5F30...`; server restarted (new pid) healthy at 0.3.3; journal `phase: restored`, `task_removed: true`; no OpenVibeSTT task left.
So the transaction logic (download, verify, stop, replace, restart, validate, commit, rollback, task removal) works end to end; only the path comparison is wrong. These patched binaries are not release candidates; the exact fixed release binary must be rehearsed again.

Cleanup: isolated servers stopped (ports 54400/54401 free), scratch folder, patched worktree (`git worktree remove` + `prune`) and the extra build directory deleted, the two leftover scratch recovery tasks deleted by name, no OpenVibeSTT tasks present, real server `/health` version 0.3.1 on 54321.

2026-10-03 (fix + rehearsal on the fixed 0.3.2 build; commits 8580cf7 and 1a92ace): **Result: A, B and C pass. Only the live public N->N+1 check remains.**

Fixes (src/update_transaction.rs, with tests): `simplify_path`/`same_path` helpers (strip the `\\?\` / `\\?\UNC\` prefix when a plain form exists; ignore separator style, trailing slash and, on Windows, case) used by `validate_journal`, by `prepare` (executable, data dir, work dir, launch.executable) and for the recovery task command and arguments. A worker whose validation fails while the journal is still `Prepared` now removes the task and disarms. A failed `prepare` removes its work folder. A terminal phase clears `armed`. `update install` marks its own stdout/stderr non-inheritable before spawning the worker (found during this rehearsal: the restarted server kept the caller's redirect file open, so a caller waiting on pipe EOF would block until the server exited). New tests: path helper (verbatim, UNC, no-plain-form, long path, case), canonical work_dir with a plain launch.executable passes validation, task XML has no `\\?\`, post-arm failure removes the task and disarms (startup works again), a failure after the executable was touched stays armed, terminal phases disarm. The real-schtasks test from eaec7b3 still passes.
Gates on 1a92ace: `cargo fmt --check` clean; `cargo clippy --release --all-targets --offline -- -D warnings` clean; `cargo test --release --offline` 330 lib + 11 + 1 + 5 integration tests passed, 0 failed; `scripts\build-local.ps1 -Offline` OK.

Binary identity: 0.3.2 (unmodified, committed 1a92ace) `s\release\stt-server.exe`, 67231744 bytes, SHA-256 `87E86057C64F5DC0407EE61C719F82D3F5FFE462922D0CB7118B3694BD9CD47A`. The "N+1" binary was built from a throwaway detached worktree at 1a92ace with only the `Cargo.toml` version changed 0.3.2 -> 0.3.3 (`CARGO_TARGET_DIR=C:\vtb`, build-local.ps1 -Offline): SHA-256 `A60A092B895379B8CB516F7F14DED17AABF072BD56A6F376017AF8211F4D8DE5`. Neither is published; the 0.3.3 one is a rehearsal artifact only.

Isolation: scratch folder under the session scratchpad (`rh\bin\stt-server.exe` copy, `rh\data`, `rh\srv`), `STT_SERVER_DATA_DIR` and `--data-dir` set to the scratch data dir, port 54400, manifest served by `python -m http.server 54401 --bind 127.0.0.1` with `STT_SERVER_UPDATE_URL=http://127.0.0.1:54401/manifest.json` (GitHub-style JSON, assets `stt-server.exe` + `stt-server.exe.sha256`), server started with `stt-server.exe start --data-dir <rh\data> --port 54400`. No admin, no service. The user's real server (pid 22380, `C:\Users\mariu\AppData\Local\Stanzo\stt-server.exe`, port 54321) was never touched: `/health` reported 0.3.1 before every scenario and after the last. Each scenario started from a fresh scratch folder and ended with `stop`, `start` (healthy) and `stop`.

A. Success 0.3.2 -> 0.3.3 (manifest `v0.3.3` -> the patched binary):
- before: exe SHA `87E86057...`, `/health` 0.3.2, no journal, no task. `update check --json` -> `{"current_version":"0.3.2","latest_version":"0.3.3","update_available":true}`.
- `update install --yes --json --data-dir <rh\data>` -> `{"error":null,"installed":true,"phase":"committed"}`, exit 0.
- after: exe SHA `A60A092B...`, `/health` and `status --json` 0.3.3 (new pid), journal `phase=committed armed=False task_removed=True`, `schtasks /Query` shows no OpenVibeSTT task. The CLI's captured-output file was writable straight after (handle not leaked). `stop`, then `start` -> healthy 0.3.3.

B. Forced rollback (manifest `v0.3.9` advertising the same 0.3.2 bytes):
- `update install --yes --json` -> `{"error":"Started executable reports the wrong version","installed":false,"phase":"restored"}`, exit 1.
- after: exe SHA unchanged `87E86057...`, `/health` 0.3.2 (server restarted and healthy), journal `phase=restored armed=False task_removed=True`, no task. `stop`, then `start` -> healthy 0.3.2.

C1. Failure before arming (manifest with a wrong `.sha256`): `error: downloaded executable's SHA-256 (a60a092b...) does not match the release's checksum (000...0)`, exit 1; exe unchanged, 0.3.2 still running, no journal, no task, no leftover `.stt-update-*` folder. Retry with a good manifest -> `installed:true, phase:committed`, 0.3.3, no task.

C2. Failure after arming (candidate with a matching SHA-256 that is not an executable, manifest `v0.3.3`): `update install --yes --json` -> `{"error":"This version of %1 is not compatible with the version of Windows you're running. ... (os error 216)","installed":false,"phase":"restored"}`, exit 1; exe unchanged `87E86057...`, 0.3.2 restarted and healthy, journal `phase=restored armed=False task_removed=True`, no task. Retry with a good manifest on the same scratch install -> `installed:true, phase:committed`, 0.3.3; `stop`/`start` healthy. So after a post-arm failure a later `update install` is not refused and `start` works.
- Not rehearsed with the real binaries: a worker validation failure while `Prepared` (cannot be injected between prepare and worker spawn); covered by the unit test `a_worker_failure_after_arming_removes_the_task_and_disarms` with a recording task runner.

Observation (not changed): finished transactions keep their `.stt-update-<id>` work folder (previous.exe, candidate.exe, recovery.exe, journal copy; about 200 MB) beside the executable, and each update adds one; nothing prunes them. Open decision for the user: prune old terminal work folders on the next successful update?

Cleanup: scratch folder (the token files needed an ACL grant before they could be deleted), throwaway worktree (`git worktree remove` + `prune`), `C:\vtb` build dir; http server and all scratch servers stopped (ports 54400/54401 idle), no OpenVibeSTT tasks, real server `/health` 0.3.1 on 54321.

2026-10-03: Goal moved from `blocked` to `ready`: the registration and path bugs are fixed and rehearsed. The only remaining item is the live check of a real public N->N+1 GitHub release after 0.3.2 ships.

2026-10-03 (live public check, real GitHub Releases source, no `STT_SERVER_UPDATE_URL`): **Result: public 0.3.1 -> 0.3.2 cannot self-update; it fails safely. The true public N->N+1 check is still outstanding (0.3.2 -> next release).**

Which binary does the swap: the OLD one. `update install` runs the installed binary's `prepare` (path normalisation, task XML, task registration) in-process, and the worker that performs stop/replace/restart/validate/commit-or-rollback is `recovery.exe`, a copy of the *currently installed (old)* executable (`copy_sync(&executable, work_dir/recovery.exe)`, spawned as `__update-worker`). The downloaded candidate is only copied into place and started. So the updater logic that matters is always the old version's; the fixes in 0.3.2 (UTF-16 task XML, `\?\` path normalisation, disarm/cleanup) take effect only for updates *from* 0.3.2 onward. Users on 0.3.0/0.3.1 need a manual or app-bundled upgrade (Stanzo bundles stt-server, so a Stanzo app update carries the new server).

Evidence (isolated: scratch under the session scratchpad `selfupdate3\`, `STT_SERVER_DATA_DIR` and `--data-dir` set, port 54400, no admin/service; real server pid 22380 on 54321 untouched and reporting 0.3.1 before and after): public v0.3.1 `stt-server.exe` downloaded with `gh release download` (SHA-256 `71c6a15f...33d6`, matches its .sha256) and started with `start --port 54400`; `update check --json` -> `{"current_version":"0.3.1","latest_version":"0.3.2","update_available":true}` against the real GitHub source; `update install --yes --json` -> exit 1, `error: Recovery task registration failed: ERROR: The task XML is malformed. (1,40)::ERROR: unable to switch the encoding` (the 0.3.0/0.3.1 UTF-8-vs-UTF-16 bug, as recorded on 2026-10-03 earlier). Safe failure: exe hash unchanged `71c6a15f...`, 0.3.1 server kept running and healthy, journal `phase=restored armed=false`, no OpenVibeSTT scheduled task registered (query count 0), so nothing needed manual cleanup. A leftover `.stt-update-<id>` work folder remained beside the 0.3.1 exe (old pruning behaviour) and was removed with the scratch folder.
Also confirmed with the public v0.3.2 binary (SHA-256 `643a07a9...9159`, matches its .sha256): `update check --json` -> `current_version 0.3.2, latest_version 0.3.2, update_available false`; `update install --json` -> `reason: already_up_to_date`. There is no public release newer than 0.3.2 yet, so 0.3.2 -> N+1 cannot be run.
Remaining item: the live check of public 0.3.2 -> next release (see next_action). Mechanism evidence otherwise stands from the 0.3.2-build rehearsal above.
Cleanup: isolated server stopped via the shutdown endpoint (54400 down), scratch folder deleted, no OpenVibeSTT tasks, real server `/health` 0.3.1 on 54321.

2026-10-04 (live public N->N+1 check, real GitHub Releases source, no `STT_SERVER_UPDATE_URL`): **Result: PASS. Public v0.3.2 self-updated to public v0.3.3.**

Isolation: scratch `selfupdate4\` under the session scratchpad, `STT_SERVER_DATA_DIR` and `--data-dir` set, port 54400, no admin/service. `status --json` confirmed `data_dir` was the scratch `...\selfupdate4\data` before the update. The user's real server (`C:\Users\mariu\AppData\Local\Stanzo\stt-server.exe`, port 54321, now 0.3.2) was never touched; `/health` ok before and after.

Commands and outputs:
- `gh release download v0.3.2|v0.3.3 --repo mariuszRep/stt-server`: v0.3.2 exe SHA-256 `643A07A9...D9159` (67239424 bytes) matches its .sha256; v0.3.3 exe SHA-256 `AB3C1872802EBFD90B2CD5822038A98EFA6A40BB81C7629D3AD2C25395993406` (67251712 bytes) matches its .sha256.
- v0.3.2 copy started with `start --data-dir <scratch>\data --port 54400` -> `started: pid 28536`; `/health` 0.3.2.
- `update check --json` -> `{"current_version":"0.3.2","latest_version":"0.3.3","update_available":true}`.
- `update install --json` (no `--yes`) -> `{"current_version":"0.3.2","installed":false,"latest_version":"0.3.3","reason":"confirmation_required"}`; exe hash unchanged.
- `update install --yes --json --data-dir <scratch>\data` -> exit 0, `{"error":null,"installed":true,"phase":"committed"}`.
- After: exe SHA `AB3C1872...3406` equals the public v0.3.3 .sha256; `status --json` and `/health` report 0.3.3 (pid changed 28536 -> 30660); journal `phase=committed armed=False task_removed=True`; zero `OpenVibeSTT` scheduled tasks.
- `stop` (`stopped via shutdown endpoint`), `status` -> `{"running":false}`, `start` -> healthy 0.3.3.
- Second `update check --json` -> `{"current_version":"0.3.3","latest_version":"0.3.3","update_available":false}`; `update install --json` -> `reason: already_up_to_date`.
- Work-folder behaviour: exactly one `.stt-update-<id>` folder (previous.exe, recovery.exe, retired.exe, state.snapshot, logs) was kept beside the exe after the commit, as designed (the code comment says it is pruned by the next update; `prune_finished_work_dirs` and its tests cover that). Prune-on-next-update was not run live because no release after 0.3.3 exists.
- Cleanup: isolated server stopped (54400 down), scratch deleted, no OpenVibeSTT tasks, real server `/health` 0.3.2 on 54321.
