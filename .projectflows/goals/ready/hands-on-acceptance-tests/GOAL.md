---
name: hands-on-acceptance-tests
title: Hands-On Acceptance Tests (VM, Phone, Tailscale, Shared Machine)
description: The manual tests only a person with a second device can run, against the final release build, with copy-paste commands and expected results.
status: ready
type: validation
scope: stt-server-next only
attempt: 0
max_attempts: 3
last_result: none
next_action: Set up a Windows 11 Hyper-V VM on an External switch, copy the release exe into it, then run sections A to F in order and paste results back; sections G and H are recorded as unit-tested-only unless a real rehearsal is also done.
success_criteria:
  - The single exe runs on a clean Windows machine with no GPU and falls back to CPU.
  - Another device reaches the laptop's server on a private network with the token and is refused without it, and an authenticated Tailscale transcription (not just /health) succeeds from a real tailnet peer while the same route from a plain-LAN address is refused.
  - Tailscale mode works from the phone on mobile data and is invisible on Wi-Fi/Ethernet addresses.
  - On a machine-wide install, a standard Windows user can transcribe but cannot change models or settings.
  - A user's already-downloaded models move into a machine-wide install without a new download.
  - Uninstall leaves no service and no program folder behind, and keeps models.
  - A request naming a second downloaded model is served by that model without a separate select call (model-per-request).
  - Full-disk and interrupted-download/power-loss behavior is at least unit-test-covered, with a real rehearsal recorded if performed.
source: user
---

# Hands-On Acceptance Tests

All development for these areas is done. Only testing remains. Paste each section's output back
to Claude; results go into the Verification Log below.

**Build under test:** `D:\Users\mariu\Projects\stt-server-next\s\release\stt-server-next.exe`
(record its SHA-256 from the build output before starting).

Replace `<LAPTOP-IP>` with the laptop's Wi-Fi/Ethernet IPv4 (`ipconfig`), and `<TS-NAME>` with its
Tailscale machine name (`tailscale status`).

Every command below discovers the server through `status`/`health`/`server.json` rather than
assuming port 54321 -- the CLI does this for you, but where a step talks to the server directly
(curl, the phone browser) it first reads the real port. A per-user install whose port was not
given explicitly falls back to an OS-assigned free port if 54321 is already taken (e.g. by another
user's own server); a machine-wide install never falls back. See `docs/client-contract.md` section
1.3.

## A. Clean machine, no GPU (inside the VM)

Copy `stt-server-next.exe` to `C:\stt\` in the VM. Install nothing else. In a normal PowerShell:

```powershell
cd C:\stt
.\stt-server-next.exe start
.\stt-server-next.exe status --json          # expect: running: true, version and api_level shown; note the "port" field
.\stt-server-next.exe models install whisper-tiny --wait
.\stt-server-next.exe models select whisper-tiny   # sets whisper-tiny as the default AND loads it
.\stt-server-next.exe health                 # expect: ready; backend CPU with a fallback reason; default_model and loaded_model both whisper-tiny
.\stt-server-next.exe models install moonshine-tiny --wait   # a second downloaded model (whisper-tiny stays the default)
.\stt-server-next.exe stop
```

Pass: every command succeeds; the backend is CPU with a clear reason, not an error; `health`
reports a loaded model (not just installed) before moving on -- transcription tests always need a
loaded default, not just an installed one.

## B. Second device on the local network (laptop serves, VM calls)

On the laptop (normal PowerShell):

```powershell
$exe = 'D:\Users\mariu\Projects\stt-server-next\s\release\stt-server-next.exe'
& $exe start --network lan
& $exe status --json                         # note the actual "port" -- do not assume 54321
& $exe health                                # expect: network mode lan, effective lan
Get-Content "$env:LOCALAPPDATA\OpenVibeAI\STT Server\auth.token"
Get-Content "$env:LOCALAPPDATA\OpenVibeAI\STT Server\user.token"
& $exe models install whisper-tiny --wait; & $exe models install moonshine-tiny --wait
& $exe models select whisper-tiny            # default model, and confirms something is loaded
& $exe health                                # expect: ready, default_model and loaded_model both whisper-tiny
```

Windows Firewall may ask to allow the app on private networks: allow it. Replace `<PORT>` below
with the port `status --json` actually reported (usually, but not necessarily, 54321).
In the VM (use a short WAV copied into `C:\stt\clip.wav`):

```powershell
curl.exe -s http://<LAPTOP-IP>:<PORT>/health                               # expect: status ok
curl.exe -s -o NUL -w "%{http_code}`n" http://<LAPTOP-IP>:<PORT>/v1/models  # expect: 401
curl.exe -s -H "Authorization: Bearer <USER-TOKEN>" -F file=@C:\stt\clip.wav -F model=default http://<LAPTOP-IP>:<PORT>/v1/audio/transcriptions   # expect: text, model=whisper-tiny in x_diagnostics
curl.exe -s -H "Authorization: Bearer <USER-TOKEN>" -F file=@C:\stt\clip.wav -F model=moonshine-tiny http://<LAPTOP-IP>:<PORT>/v1/audio/transcriptions   # expect: text, model=moonshine-tiny in x_diagnostics -- the server swaps models itself, no select call
curl.exe -s -o NUL -w "%{http_code}`n" -X POST -H "Authorization: Bearer <USER-TOKEN>" http://<LAPTOP-IP>:<PORT>/v1/local/models/refresh   # expect: 403
```

Optional: switch the laptop's network to Public in Windows settings, wait 30 s, repeat the
transcription call; expect `403 network_not_private`. Switch back to Private.
Afterwards on the laptop: `& $exe stop`.

## C. Tailscale (health, then an authenticated transcription)

On the laptop: `& $exe start --network tailscale`, then `& $exe health`
(expect: effective tailscale, with the 100.x address); note the port from `status --json` and read
`user.token` as in section B. Find the laptop's Tailscale machine name with `tailscale status`.

From another tailnet device -- the VM (if it also runs Tailscale) or a phone with an HTTP client
app (e.g. an app that can send a `multipart/form-data` POST with a header, not just a browser tab,
since a browser alone cannot attach `Authorization` or upload a file to an arbitrary URL):

```
GET  http://<TS-NAME>:<PORT>/health                                                    -> expect status: ok, no token needed
POST http://<TS-NAME>:<PORT>/v1/audio/transcriptions  (Authorization: Bearer <USER-TOKEN>, file=clip.wav, model=default)
                                                                                         -> expect 200 with transcribed text
POST http://<TS-NAME>:<PORT>/v1/audio/transcriptions  (no Authorization header, file=clip.wav)
                                                                                         -> expect 401 unauthorized
```

Then, from the VM or another device reachable only over plain Wi-Fi/Ethernet (not Tailscale),
using the laptop's LAN address while it is still in `--network tailscale` mode:

```
POST http://<LAPTOP-IP>:<PORT>/v1/audio/transcriptions  (Authorization: Bearer <USER-TOKEN>, file=clip.wav)
                                                                                         -> expect 403 network_not_private
```

(`/health` alone would still return 200 from the LAN address even in tailscale mode -- the 403
check above must be a real authenticated route, not just `/health`, to prove the tailnet
restriction actually applies.)

Afterwards: `& $exe stop`.

## D. Shared machine: access levels (inside the VM, machine-wide)

Admin PowerShell in the VM:

```powershell
C:\stt\stt-server-next.exe service install
$svc = 'C:\Program Files\OpenVibeAI\STT Server\stt-server-next.exe'
$data = 'C:\ProgramData\OpenVibeAI\STT Server'
& $svc models install whisper-tiny --wait --data-dir $data
& $svc models select whisper-tiny --data-dir $data
net user tester Test1234! /add               # a standard (non-admin) user
```

Sign in as `tester` (or "Run as different user" for PowerShell):

```powershell
$svc = 'C:\Program Files\OpenVibeAI\STT Server\stt-server-next.exe'
$data = 'C:\ProgramData\OpenVibeAI\STT Server'
& $svc health --data-dir $data               # expect: ready
& $svc models list --data-dir $data          # expect: list shown
& $svc models remove whisper-tiny --data-dir $data   # expect: "admin access required"
Get-Content "$data\auth.token"               # expect: access denied
```

## E. Moving a user's models into the machine-wide install (inside the VM)

As the VM's own admin user, the per-user install from section A already has whisper-tiny.
`models remove` refuses to remove the current **default** model (`409 model_in_use`) -- whisper-tiny
was set as the machine-wide default in section D, so it must be unloaded (or a different model made
default) *before* it can be removed; unloading only clears the default/loaded state, it never
deletes the file:

```powershell
& $svc models unload --data-dir $data        # clears the default/loaded model; required before remove
& $svc models remove whisper-tiny --data-dir $data
& $svc models import-user --from "$env:LOCALAPPDATA\OpenVibeAI\STT Server" --wait --data-dir $data
& $svc models list --data-dir $data          # expect: whisper-tiny installed, no download happened
& $svc models select whisper-tiny --data-dir $data   # re-establish a default/loaded model before section F
```

## F. Uninstall (inside the VM)

```powershell
& $svc service uninstall
Start-Sleep 15
Get-Service OpenVibeSttNext -ErrorAction SilentlyContinue      # expect: nothing
Test-Path 'C:\Program Files\OpenVibeAI\STT Server'             # expect: False
Get-ChildItem "$data\models" | Select Name                     # expect: model file kept
```

## G. Full disk

Not automated here -- unit-tested only (`insufficient_disk_space`'s preflight check in
`src/download.rs`/`src/update_transaction.rs`). If a real full-disk rehearsal is done: shrink a VM
disk or fill it with a large dummy file until under ~100 MiB free, then attempt `models install`
on an uninstalled model and `update install --yes` on a pending update; expect a clear
`insufficient_disk_space` failure (or the update's own preflight refusal) in both cases, with no
partial/corrupted state left registered as installed or as the active executable. Record actual
free-space thresholds observed, since the preflight math is a heuristic
(`(remaining bytes) * 1.05 + 64 MiB` for downloads; roughly 4x the executable size plus 64 MiB, and
2x the database size plus 16 MiB, for a self-update).

## H. Power loss / interrupted download

Not automated here -- unit-tested only (stall timeout, retry/backoff, `.part` resume-from-partial
in `src/download.rs`; journalled recovery in `src/update_transaction.rs`). If a real interruption
rehearsal is done: start a large model's `models install`, then hard-kill the server process (or
the VM itself) partway through; on restart, confirm via `models list`/`operations` that the
interrupted operation is reported `failed` (not silently resumed), that the `.part` file under
`<data dir>\staging` is still present, and that a fresh `models install` of the same model reuses
it (verify via a visibly shorter download / an immediate "resuming" indication) rather than
starting from zero. Separately, kill the process mid-`update install --yes` and confirm the
one-shot recovery Scheduled Task rolls the executable and database back to the previous version on
the VM's next boot/logon, per `blocked/safe-self-update`.

## Already passed on the laptop (2026-09-27)

Service install, status, model install with progress, transcription on Vulkan, restart, crash
recovery with the model reloaded, uninstall keeping models, old-folder migration.

## Attempts

None yet.

## Verification Log

2026-09-28: Created. All server development for these areas is committed.

2026-09-28: Reviewed against current source and corrected: section E now unloads whisper-tiny
before removing it (`models remove` refuses the current default with `409 model_in_use`, checked
against `src/api.rs::remove_model`/`selected_id`, not the resident-but-not-default case); sections
A-C now discover the port from `status --json`/`server.json` instead of assuming 54321 (per
`docs/client-contract.md` section 1.3's port-fallback rule); section A now installs and loads a
default model and confirms `health` reports it loaded before any transcription step is attempted;
section B/C now include a second downloaded model sent via `model=<id>` to exercise model-per-
request; section C now includes an authenticated Tailscale transcription (not just `/health`) from
a real tailnet peer, a no-token request expecting 401, and a plain-LAN-address request in
tailscale mode expecting 403. Added sections G (full disk) and H (power loss / interrupted
download), recorded as unit-tested-only pending a real rehearsal, so every acceptance check from
the umbrella goal's checklist has an explicit home in this file.

## Final Outcome

Not started.
