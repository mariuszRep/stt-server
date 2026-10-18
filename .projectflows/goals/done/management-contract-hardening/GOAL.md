---
name: management-contract-hardening
title: Finish server management and security contract hardening
description: Resolve the remaining independently reviewed management gaps before standalone or SDK clients depend on the server.
status: done
type: bugfix
scope: stt-server-next only
attempt: 1
max_attempts: 8
last_result: passed
next_action: none
success_criteria:
  - Every auth token file is restricted to the intended user/service identity, including a custom data folder and ordinary user mode.
  - Start/status never mistake an unrelated program answering on the configured port for this server.
  - Importing a large model does not make transcription or other management requests unresponsive due to progress persistence.
  - The API and client contract agree on response statuses and structured errors, including needs_verification, operation progress and the absence of an idle timeout.
  - Drop-in removal and refresh behaviour is explicit to the user: their file is preserved, and the operator understands whether refresh will find it again.
  - CORS and request-size defaults have been reviewed for the intended local and LAN clients; any change is recorded with a compatibility check.
source: user
---

# Finish server management and security contract hardening

## Why

The independent review found smaller gaps that become visible when the server is used on its own or through the SDK. They are distinct from safe stopping, startup and individual model-file recovery, which were completed locally.

## Business rules

- A bearer token must not be exposed to other users merely because the server uses a custom data folder.
- Process status must identify this server, not accept any healthy program on the same port.
- Long imports must leave transcription and health/management responsive, with useful progress updates.
- Clients must be able to distinguish not-ready, missing model, needs-verification, busy, timeout and unsupported-option errors from the documented response.
- Removing a drop-in entry never deletes the user's file. The next refresh may discover that file again; document this honestly or provide a deliberate dismiss action if required by the product decision.
- The audio API currently accepts WAV and has a 40 MiB request limit. Decide whether those bounds satisfy the intended OpenAI-style clients before promising broader compatibility. Streaming remains outside the approved product scope.

## Plan

1. Fix token permissions and server identity checks; validate ordinary and custom data folders.
2. Throttle import progress writes and observe an import alongside transcription.
3. Pin the structured error catalog with contract checks and clarify drop-in removal, idle timeout, CORS and upload limits.
4. Record any OpenAI media-format expansion as a separate approved scope if clients require it.

## Out of scope

Self-update, CLI model-command implementation, new speech models, client migration and public release.

## Related goals

- cancelled/ready-for-voice-typer: server acceptance umbrella.
- done/safe-stop-startup-model-recovery: resolved high-priority stop/startup/refresh findings.

## Attempts

Attempt 1 (2026-09-26): Implemented all six success criteria in source and docs:
- M6 (token ACL): `src/app.rs` `token_file` now applies a restrictive, non-inherited icacls ACL (current user only) on every open, not just creation, skipped in service mode (service.rs already sets its own SYSTEM/Administrators/user ACL there). Covers a custom `--data-dir`.
- L2 (start/status identity): `src/api.rs` `/health` now returns `{"status":"ok","service":"stt-server-next"}` (new `api::SERVICE_ID` constant); `src/bin/server.rs` `health_ok` checks that field, not just the 200 status.
- M3 (import progress contention): `src/import.rs` now throttles `update_operation` writes with the existing `download::should_flush_progress` helper (same cadence as `download.rs`: every 250ms or 4 MiB) instead of once per multipart chunk, with a forced final flush after the stream ends.
- M2/error catalog/idle-timeout/format-limits/drop-in-rediscovery: `docs/client-contract.md` rewritten in the error table (full catalog of codes by status, including `needs_verification` 409, `model_in_use`, `model_not_active`, `queue_full`, `inference_timeout` 504, plus a note on per-operation `error_code`s), a new "Format and size limits (reviewed 2026-09-26)" section (WAV-only, 40 MiB, decision to keep as-is against Whisper Vibes/stt-sdk), a "No idle timeout" note, a CORS-default review note (kept `*`, reasoning recorded), and an expanded Remove/drop-in bullet stating the file reappears on the next refresh with no dismiss action.
- Tests added: `token_file_acl_is_restricted_to_current_user_only` (src/app.rs), `health_ok_requires_service_identity_not_just_200` (src/bin/server.rs), `import_completes_with_full_progress_recorded` (src/import.rs, injects a fake catalog model via `Arc::get_mut` before the first clone to drive a full import through the router without needing a real multi-GB catalog file).
- Added `reqwest`'s `json` feature (needed for `response.json()` in the new health-identity check) to Cargo.toml.

Gates were started (`cargo build --lib`, log at C:\Users\mariu\AppData\Local\Temp\claude\scratch-mch\build1.log) but the process was still compiling third-party dependencies (had not yet reached this crate's own code) when this attempt had to hand back. `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test` have not been run to completion. No pass/fail evidence yet -- do not treat this attempt as gate-clean.

## Do Not Repeat

- Do not claim full OpenAI audio compatibility when the implemented parser accepts WAV only.
- Do not silently delete user-owned drop-in files to make removal appear permanent.

## Verification Log

2026-09-26: Created from unresolved findings in docs/reviews/2026-09-26-independent-review.md; no implementation started.
2026-09-26: Attempt 1 implemented all six criteria (see Attempts). `cargo build --lib` was launched but not observed to completion before hand-back (still compiling dependencies, log at C:\Users\mariu\AppData\Local\Temp\claude\scratch-mch\build1.log). `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` NOT run. Goal stays in_progress; next attempt must run the gates to completion, fix anything they surface, and only then record pass evidence and move to done/.

## Ready For Execution

The review names concrete source paths and behaviours. Media-format expansion remains a later scope decision, but the current limits can be documented and tested now.

## Final Outcome

Not started.

2026-09-26: Orchestrator ran gates with build-local.ps1 environment: cargo fmt --check clean, cargo clippy --all-targets -D warnings clean, cargo test 173 lib + 3 bin passed, 0 failed. Moved to done.
