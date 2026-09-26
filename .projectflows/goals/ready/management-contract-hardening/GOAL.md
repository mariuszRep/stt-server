---
name: management-contract-hardening
title: Finish server management and security contract hardening
description: Resolve the remaining independently reviewed management gaps before standalone or SDK clients depend on the server.
status: ready
type: bugfix
scope: stt-server-next only
attempt: 0
max_attempts: 8
last_result: none
next_action: Resolve token-file permissions and start/status identity first, then import-progress contention and client-contract accuracy.
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

- draft/ready-for-voice-typer: server acceptance umbrella.
- done/safe-stop-startup-model-recovery: resolved high-priority stop/startup/refresh findings.

## Attempts

None yet.

## Do Not Repeat

- Do not claim full OpenAI audio compatibility when the implemented parser accepts WAV only.
- Do not silently delete user-owned drop-in files to make removal appear permanent.

## Verification Log

2026-09-26: Created from unresolved findings in docs/reviews/2026-09-26-independent-review.md; no implementation started.

## Ready For Execution

The review names concrete source paths and behaviours. Media-format expansion remains a later scope decision, but the current limits can be documented and tested now.

## Final Outcome

Not started.
