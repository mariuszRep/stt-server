---
name: ready-for-voice-typer
title: Make STT Server Next Release-Ready for Voice Typer
description: Close the remaining server-side work so Voice Typer can depend on stt-server-next as its only speech server, with safe updates and proven reliability.
status: cancelled
type: feature
scope: stt-server-next only
attempt: 0
max_attempts: 8
last_result: cancelled
next_action: null
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
  covered by `in_progress/hands-on-acceptance-tests` sections B, D, E.
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
- `in_progress/hands-on-acceptance-tests`: the manual VM/phone/Tailscale/shared-machine script. Sections
  A-F are runnable today against the release build; sections G (full disk) and H (power loss /
  interrupted download) are recorded as unit-tested-only pending an optional real rehearsal.

## Remaining acceptance checklist (owner goal for each)

- [ ] **Clean machine, no GPU/CPU-only startup** -- `in_progress/hands-on-acceptance-tests` section A.
- [ ] **Second device on the local network (LAN)** (partial 2026-10-03: LAN mode bound to 0.0.0.0,
      `/v1/models` 200 with token / 401 without, sent from the laptop to its own LAN IP -- not a
      second device; genuine second-device LAN transcription still needed in the Hyper-V VM) -- `in_progress/hands-on-acceptance-tests` section B.
- [ ] **Tailscale, including an authenticated transcription (not just health)** (partial
      2026-10-03: from the laptop, `/v1/models` via the Tailscale IP 200 with token / 401 without,
      LAN IP in tailscale mode 403 `network_not_private`; Pixel 7 phone reached `/health` as a real
      tailnet peer -- phone part DONE, no further phone testing; authenticated transcription from a
      peer and the plain-LAN 403 on a transcription route still needed in the Hyper-V VM, which
      joins the tailnet and uses curl) --
      `in_progress/hands-on-acceptance-tests` section C.
- [ ] **Real multi-user (machine-wide install, standard user access level, cross-account model
      import)** -- `in_progress/hands-on-acceptance-tests` sections D-E, plus
      `in_progress/install-scope-and-shared-access`'s remaining real-machine rehearsal.
- [ ] **Model-per-request end-to-end on a real device** -- `in_progress/hands-on-acceptance-tests`
      sections A/B (a second downloaded model served via `model=<id>` with no select call).
- [ ] **Full-disk behaviour** -- `in_progress/hands-on-acceptance-tests` section G (unit-tested only
      today; no dedicated real-hardware goal exists, and none is needed unless the VM rehearsal
      finds a real gap).
- [ ] **Power loss / interrupted download** -- `in_progress/hands-on-acceptance-tests` section H
      (unit-tested only today, same as full-disk).
- [ ] **Clean-VM Windows Service install/uninstall recheck** -- `in_progress/hands-on-acceptance-tests`
      section F, closing the cosmetic empty-folder issue noted in
      `in_progress/install-scope-and-shared-access`'s 2026-09-27 log entry.
- [ ] **Upgrade from an older version (live N -> N+1 update and forced rollback)** --
      `blocked/safe-self-update`, gated on the first public GitHub release.
- [ ] **Public release + exact-binary promotion rehearsal** -- `blocked/safe-self-update`'s
      "production promotion reuses the exact tested binary" success criterion; same external gate
      (repository must be public) as the item above.

No item above is without a home: every one is either already a section of
`in_progress/hands-on-acceptance-tests` or a stated success criterion of `blocked/safe-self-update` or
`in_progress/install-scope-and-shared-access`. No new goal was created.

## Out of scope

Client changes (SDK and app have their own goals), streaming, multiple resident models.

## Related goals

- Workspace: `voice-typer/.projectflows/goals/in_progress/build-stt-server-next` and
  `migrate-voice-typer-to-stt-server-next`.
- `stt-sdk`: `draft/stt-server-next-adapter`.
- `whisper-vibes`: `draft/switch-to-stt-server-next`.

## Attempts

None. No attempt was started; closed as cancelled on 2026-10-03.

## Verification Log

2026-09-26: Drafted from the migration plan (phases 2 and 5, server side).
2026-09-26: Reconciled with the recovery goal and independent review; focused ready goals created. No remaining acceptance gate was marked passed without new evidence.

2026-09-28: Documentation-only pass: reconciled this goal's stale 2026-09-26 status text against
current source (per-request model choice in `src/api.rs`, the journalled self-updater in
`src/update_transaction.rs`) and the focused goals' own Verification Logs. Replaced the old
narrative status with a per-goal summary and an explicit remaining-acceptance checklist mapping
every outstanding check to its owner goal. No code was changed; no new tests were run.

2026-10-03: Live acceptance evidence recorded (real laptop, Windows 11 "rhs-surface", Tailscale
100.125.201.68, home LAN 10.0.0.18; stt-server 0.3.1 run by Stanzo 0.3.4 -- Stanzo's LAN-mode bug of
connecting to 0.0.0.0 was fixed in whisper-vibes 36fa22a and released as Stanzo v0.3.4).
- LAN mode (health: mode lan, effective lan, bound 0.0.0.0:54321): `GET /v1/models` via
  10.0.0.18 with user.token -> 200; without token -> 401. Caveat: sent from the laptop itself to its
  own LAN address, NOT from a second device, so this does not close the genuine second-device LAN
  check.
- Tailscale mode (health: mode tailscale, effective tailscale, addresses [100.125.201.68]): from
  the laptop, `/v1/models` via the Tailscale IP with token -> 200; without token -> 401; via the LAN
  IP 10.0.0.18 with token -> 403 `network_not_private`.
- Real tailnet peer: Android phone (Pixel 7) on Tailscale, `GET http://100.125.201.68:54321/health`
  in Chrome -> status ok, effective tailscale. Proves an off-machine tailnet peer reaches the server.
- Switching Local/LAN/Tailscale from the Stanzo Settings UI works on Stanzo 0.3.4.

Net effect on the checklist: no item is fully checked. LAN and Tailscale are partially evidenced;
a genuine second-device LAN transcription and a peer-side authenticated Tailscale transcription are
deferred to the Hyper-V VM session.

## Final Outcome

Cancelled 2026-10-03: superseded by shipped releases; remaining checks tracked in
hands-on-acceptance-tests / safe-self-update. Voice Typer (Stanzo) has shipped on stt-server since
0.3.0 (whisper-vibes tags v0.3.0, v0.3.2, v0.3.3, v0.3.4). Success criteria were not all met, so this
is `cancelled`, not `done`. Open items (owner goal in `in_progress/` or `blocked/`):

- Clean machine, no GPU/CPU-only startup -> in_progress/hands-on-acceptance-tests section A.
- Second device on the LAN (genuine second-device transcription) -> hands-on-acceptance-tests section B.
- Tailscale peer-side authenticated transcription and plain-LAN 403 on a transcription route -> section C.
- Real multi-user (machine-wide install, standard user, cross-account import) -> sections D-E and in_progress/install-scope-and-shared-access.
- Model-per-request end-to-end on a real device -> sections A/B.
- Full-disk behaviour -> section G; power loss / interrupted download -> section H (unit-tested only).
- Clean-VM Windows Service install/uninstall recheck -> section F.
- Live N -> N+1 update with forced rollback -> blocked/safe-self-update.
- Public release + exact-binary promotion rehearsal -> blocked/safe-self-update.
