---
name: safe-stop-startup-model-recovery
title: Safe stopping, responsive startup and model-file recovery
description: Make STT Server Next safe to stop after crashes and resilient to temporarily unreadable model files without scanning every model at startup.
status: done
type: bugfix
scope: stt-server-next only
attempt: 1
max_attempts: 8
last_result: passed
next_action: null
success_criteria:
  - Stopping after a crash never terminates unrelated processes or relies on a saved process number as identity.
  - Startup does not hash every installed model; changed or unverified files cannot be automatically loaded.
  - Temporary file-access failures preserve model files, registrations and selected preferences and report a retryable failure.
  - Refresh continues past unreadable individual files and reports their errors while registering other valid files.
  - Explicit successful verification restores eligibility; actual corruption remains rejected.
  - Regression tests and repository build checks pass with evidence recorded.
source: user
---

# Safe stopping, responsive startup and model-file recovery

## Business rules

Stopping must affect only this server. If graceful shutdown cannot be confirmed, report failure without killing a process based on stale discovery information. A stopped instance is identified through its exclusive data-folder lock.

Installed models must not all be read in full whenever the server starts. Record verified file size and modification time; require explicit verification when these change or no verified fingerprint exists. Preserve the saved selection while verification is needed. A temporary file-access error is not proof of corruption and must not uninstall or quarantine a model.

Refreshing a model folder must process healthy files even when another file is locked or unreadable. Report individual failures, preserve user files, and allow retry. Failure to read the folder itself must not masquerade as a successful empty scan.

## Plan

1. Replace PID-based forced stopping with lock-aware graceful shutdown and regression coverage.
2. Persist verified file fingerprints; replace startup hashing with metadata checks and preserve inaccessible files.
3. Make explicit verification distinguish unreadability from corruption and restore verified state on success.
4. Isolate refresh errors per file, including existing registrations; add locked-file and recovery tests.
5. Run formatting, strict lint, release tests and static build; record evidence and remaining limitations.

## Out of scope

Self-update, release publishing, SDK/app changes, CLI feature expansion, model catalog expansion and VISION edits.

## Related goals

- cancelled/ready-for-voice-typer: this goal addresses its startup/file resilience subset.
- Workspace migration and build-stt-server-next goals remain the overall acceptance record.

## Ready For Execution

User explicitly requested this goal, planning and implementation on 2026-09-26.

## Attempts

- 2026-09-26, attempt 1: inspected the independent review and current stop, startup, verification and refresh paths. Implementation and validation completed; evidence below.

## Do Not Repeat

- Never force-kill a process solely because its PID appears in server.json.
- Never treat a file-access error as evidence of a hash mismatch.

## Verification Log

## 2026-09-26: safe stop, startup and model-file recovery

Implemented lock-aware graceful stop without PID force termination; lightweight startup
fingerprints recorded after install/verification; preservation of inaccessible model files,
registrations and selected preferences; startup load exclusion for unverified models;
per-file refresh failures with retry and changing-file detection; explicit verification
recovery while retaining confirmed-corruption rejection.

Validation: cargo fmt --check; cargo clippy --release --all-targets --offline -- -D warnings;
cargo test --release --offline: 173 tests passed (171 library + 2 CLI). New regression tests
cover stale live PID, held lock with unavailable shutdown, managed startup without hashing,
locked startup selection, mixed locked/readable refresh and retry, verification access-error
recovery, and actual corruption quarantine. Static release build passed with the repository
build environment. Real binary check passed: isolated start, health, status, restart, stop,
and repeated stop, on port 54471. Test server stopped; no user data directory was used.

Binary size: 66224128 bytes. SHA-256: 2A421EE00E9E65DDA8BD14798694C50E3D32E00228B89A24497DDB41C7D71307.

Limits: metadata checks are not continuous hash verification; an unchanged size/timestamp
is trusted after prior verification. Older installations without fingerprints require one
explicit verification (no download). Selected-model loading cost still applies. No clean-PC,
full-disk, CPU-only, LAN or transcription-quality claims are made by these checks. No release
or commit was made as part of this goal.

## Final Outcome

Implemented and verified all scoped recovery fixes. Models and preferences survive temporary file-access failures; unverified files cannot load; refresh continues past individual errors; stop never force-kills a saved PID. Changes remain uncommitted for review.
