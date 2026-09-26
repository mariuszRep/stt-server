---
name: safe-self-update
title: Safely update and roll back STT Server Next
description: Let a standalone server install a verified public release and recover automatically if the new program cannot start.
status: in_progress
type: feature
scope: stt-server-next only
attempt: 1
max_attempts: 8
last_result: implemented; unit/mock-server rehearsal green; real GitHub Releases source untested (repo private)
next_action: Rehearse a live N to N+1 update and forced rollback with two real tagged binaries once a release source exists; confirm asset naming and non-interactive install behaviour with the user.
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

- draft/ready-for-voice-typer: server acceptance umbrella.
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

Implemented and unit/mock-server tested (attempt 1); not yet verified against a real running
server pair or the real GitHub Releases source. Left in `in_progress` pending review.

2026-09-26: Orchestrator re-ran gates: fmt clean, clippy clean, cargo test 202 lib + 10 bin passed. Live two-binary update/rollback not yet rehearsed; goal stays in_progress.
