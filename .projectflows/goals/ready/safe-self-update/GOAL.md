---
name: safe-self-update
title: Safely update and roll back STT Server Next
description: Let a standalone server install a verified public release and recover automatically if the new program cannot start.
status: ready
type: feature
scope: stt-server-next only
attempt: 0
max_attempts: 8
last_result: none
next_action: Implement the update and rollback path using a local release fixture first; then test against the intended public release source when available.
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

None yet.

## Do Not Repeat

- Do not silently run an older executable against a newer unsupported database schema.
- Do not rebuild the production executable after candidate acceptance.

## Verification Log

2026-09-26: Created from the approved update and rollback requirements; no implementation started.

## Ready For Execution

Update behaviour and local-fixture rehearsal are specified. Public release verification remains a later external gate.

## Final Outcome

Not started.
