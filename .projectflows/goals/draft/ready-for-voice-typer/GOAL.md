---
name: ready-for-voice-typer
title: Make STT Server Next Release-Ready for Voice Typer
description: Close the remaining server-side work so Voice Typer can depend on stt-server-next as its only speech server, with safe updates and proven reliability.
status: draft
type: feature
scope: stt-server-next only
attempt: 0
max_attempts: 8
last_result: none
next_action: Run hands-on-acceptance-tests; after the first public release, the self-update rehearsal; then move this goal to ready/done.
success_criteria:
  - A user can keep the server up to date from public releases without losing models or settings, and a failed update rolls back automatically.
  - The server installs and runs correctly on a clean Windows machine, on a machine without a GPU or Vulkan, and when upgrading from an earlier version.
  - The server survives crashes, reboots, interrupted downloads, and full disks without corrupting state or losing installed models.
  - LAN mode is confirmed from a second device, and every remote request without the token is refused.
  - Transcription quality on real Voice Typer dictation is confirmed against human-checked text, not only against the old server's output.
  - Every release is the exact binary that was tested.
source: user
---

# Make STT Server Next Release-Ready for Voice Typer

## Why

Voice Typer will switch from its old provider-based speech server to this one. Before that
happens, the new server must be something ordinary users can install, keep updated, and trust
with their models and settings. Features are in place; this goal is about reliability, updates,
and proof.

## Business rules

- **Updates.** The server checks public releases, downloads a newer version only when asked,
  confirms the download is genuine, replaces itself, and restarts. If the new version does not
  start, the previous version comes back automatically. Models, settings, and operation history survive.
- **Where it runs.** It must work on a fresh Windows install, on machines with no usable GPU
  (falling back to CPU without failing), and when upgrading over an older version.
- **Resilience.** A crash, reboot, power loss, interrupted download, or full disk must never
  leave a broken model marked as usable or damage saved state. After any of these, the server
  comes back to a usable state or reports clearly what the user needs to do.
- **Network use.** When shared on the local network, other devices can use it only with the
  token; without it they get nothing but a health check.
- **Quality proof.** Speed has been shown to beat the old server. Accuracy must now be checked
  against what the speaker actually said, using the clips where the old and new servers
  disagree most.
- **Trustworthy releases.** What users download is byte-for-byte what was tested. Releases are
  made deliberately, never automatically on merge.

## Current status and remaining acceptance gates (2026-09-26)

- The safe-stop/startup/model-file recovery goal is done locally, with 173 tests and a real start/restart/stop check. Its source and evidence are still uncommitted in this repository; review and commit them first. Older installed models need one explicit verification to establish their recorded fingerprint.
- (Updated 2026-09-28) Focused goals: model-management-cli, management-contract-hardening, handy-gguf-parity-evidence and safe-stop-startup-model-recovery are done; safe-self-update is blocked on the first public release; install-scope-and-shared-access is implemented and waits on hands-on-acceptance-tests.
- Prove a fresh Windows installation and CPU-only startup without a usable GPU or Vulkan loader. Check supported CPU instruction sets and additional GPU/driver combinations.
- Rehearse a real Windows Service install, run, restart and uninstall, including the user drop-in folder.
- Test authenticated LAN use from another device. Confirm unauthenticated requests cannot reach protected routes and token-file permissions are restrictive in both service and user mode, including custom data folders.
- Exercise power loss/interrupted work, full disk, database migration and old-version rollback. A newer schema must not silently be accepted by an older executable.
- Complete the management API contract: select-needs-verification returns 409; document the error catalog and absence of an idle timeout. Protect user-mode tokens and verify process identity on the configured port. Throttle large-import progress writes and clarify drop-in removal/refresh.
- Build dispatch-only candidate/release automation and rehearse promotion of the exact tested binary. The repository is still private, so public self-update verification remains an external gate.
- Record a human-checked accuracy verdict and a model/quantisation coverage matrix before claiming full Handy GGUF functional parity.

## Focused goals

- `done/management-contract-hardening`
- `done/model-management-cli`
- `done/handy-gguf-parity-evidence`
- `done/safe-stop-startup-model-recovery`
- `blocked/safe-self-update` — only the live release-to-release rehearsal remains.
- `in_progress/install-scope-and-shared-access` — implemented; waits on hands-on tests.
- `ready/hands-on-acceptance-tests` — VM, phone, Tailscale, shared-machine checks.

## Out of scope

Client changes (SDK and app have their own goals), streaming, multiple resident models.

## Related goals

- Workspace: `voice-typer/.projectflows/goals/in_progress/build-stt-server-next` and
  `migrate-voice-typer-to-stt-server-next`.
- `stt-sdk`: `draft/stt-server-next-adapter`.
- `whisper-vibes`: `draft/switch-to-stt-server-next`.

## Attempts

None yet.

## Verification Log

2026-09-26: Drafted from the migration plan (phases 2 and 5, server side).
2026-09-26: Reconciled with the recovery goal and independent review; focused ready goals created. No remaining acceptance gate was marked passed without new evidence.

## Final Outcome

Not started.
