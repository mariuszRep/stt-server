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
next_action: Run ready/hands-on-acceptance-tests (sections A-F); finish in_progress/openai-model-per-request's /models/manage rename; after the first public release, run blocked/safe-self-update's live N-to-N+1 rehearsal; then move this goal to ready/done. See the "Remaining acceptance checklist" below for every outstanding check and its owner goal.
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

## Current status (2026-09-28)

Everything that can be proven by source review, unit tests, and real-model checks on this
development laptop is done. What remains is (a) hands-on rehearsal that needs a second device or a
clean VM, (b) one more implementation slice (the `/models/manage` rename), and (c) checks that are
gated on the repository going public with a first release.

- `done/management-contract-hardening`, `done/model-management-cli`,
  `done/handy-gguf-parity-evidence`, `done/safe-stop-startup-model-recovery`: closed, no further
  work in this repository.
- `in_progress/install-scope-and-shared-access`: all six planned slices (install scope/data
  folder, port fallback, access levels, network modes including Tailscale, versions and moving
  models between scopes) are implemented, gated, and documented. Remaining: real-machine rehearsal
  only (second Windows account, cross-account `import-user`, clean-VM service reinstall) --
  covered by `ready/hands-on-acceptance-tests` sections B, D, E.
- `in_progress/openai-model-per-request`: per-request model choice, default model, the restricted
  `GET /v1/models`, and `default_model`/`loaded_model` health fields are implemented and gated.
  Remaining: the `/models/manage` path rename and the `models select` -> `models default` CLI
  rename (success criteria not yet met until this lands).
- `blocked/safe-self-update`: the journalled updater (stop, database snapshot, verified binary
  swap, restart with identical launch settings, readiness validation, automatic rollback, crash
  recovery via a one-shot Scheduled Task) is implemented and covered by unit tests
  (`src/update_transaction.rs`) and real-binary integration tests
  (`tests/update_transaction.rs`: version-mismatch rollback, readiness-failure rollback, recovery
  from every interrupted phase, database restore, launch-settings-preserved). Remaining: a live
  update from one real tagged GitHub release to the next with a forced rollback, which needs the
  repository to be public with a first published release.
- `ready/hands-on-acceptance-tests`: the manual VM/phone/Tailscale/shared-machine script. Sections
  A-F are runnable today against the release build; sections G (full disk) and H (power loss /
  interrupted download) are recorded as unit-tested-only pending an optional real rehearsal.

## Remaining acceptance checklist (owner goal for each)

- [ ] **Clean machine, no GPU/CPU-only startup** -- `ready/hands-on-acceptance-tests` section A.
- [ ] **Second device on the local network (LAN)** -- `ready/hands-on-acceptance-tests` section B.
- [ ] **Tailscale, including an authenticated transcription (not just health)** --
      `ready/hands-on-acceptance-tests` section C.
- [ ] **Real multi-user (machine-wide install, standard user access level, cross-account model
      import)** -- `ready/hands-on-acceptance-tests` sections D-E, plus
      `in_progress/install-scope-and-shared-access`'s remaining real-machine rehearsal.
- [ ] **Model-per-request end-to-end on a real device** -- `ready/hands-on-acceptance-tests`
      sections A/B (a second downloaded model served via `model=<id>` with no select call).
- [ ] **Full-disk behaviour** -- `ready/hands-on-acceptance-tests` section G (unit-tested only
      today; no dedicated real-hardware goal exists, and none is needed unless the VM rehearsal
      finds a real gap).
- [ ] **Power loss / interrupted download** -- `ready/hands-on-acceptance-tests` section H
      (unit-tested only today, same as full-disk).
- [ ] **Clean-VM Windows Service install/uninstall recheck** -- `ready/hands-on-acceptance-tests`
      section F, closing the cosmetic empty-folder issue noted in
      `in_progress/install-scope-and-shared-access`'s 2026-09-27 log entry.
- [ ] **Upgrade from an older version (live N -> N+1 update and forced rollback)** --
      `blocked/safe-self-update`, gated on the first public GitHub release.
- [ ] **Public release + exact-binary promotion rehearsal** -- `blocked/safe-self-update`'s
      "production promotion reuses the exact tested binary" success criterion; same external gate
      (repository must be public) as the item above.

No item above is without a home: every one is either already a section of
`ready/hands-on-acceptance-tests` or a stated success criterion of `blocked/safe-self-update` or
`in_progress/install-scope-and-shared-access`. No new goal was created.

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

2026-09-28: Documentation-only pass: reconciled this goal's stale 2026-09-26 status text against
current source (per-request model choice in `src/api.rs`, the journalled self-updater in
`src/update_transaction.rs`) and the focused goals' own Verification Logs. Replaced the old
narrative status with a per-goal summary and an explicit remaining-acceptance checklist mapping
every outstanding check to its owner goal. No code was changed; no new tests were run.

## Final Outcome

Not started.
