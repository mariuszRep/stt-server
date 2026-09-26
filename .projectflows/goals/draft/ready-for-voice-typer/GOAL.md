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
next_action: Review with the user, then move to ready.
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
  start, the previous version comes back automatically. Models, settings, and history survive.
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

## Final Outcome

Not started.
