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
next_action: Set up a Windows 11 Hyper-V VM on an External switch, copy the release exe into it, then run sections A to F in order and paste results back.
success_criteria:
  - The single exe runs on a clean Windows machine with no GPU and falls back to CPU.
  - Another device reaches the laptop's server on a private network with the token and is refused without it.
  - Tailscale mode works from the phone on mobile data and is invisible on Wi-Fi/Ethernet addresses.
  - On a machine-wide install, a standard Windows user can transcribe but cannot change models or settings.
  - A user's already-downloaded models move into a machine-wide install without a new download.
  - Uninstall leaves no service and no program folder behind, and keeps models.
source: user
---

# Hands-On Acceptance Tests

All development for these areas is done. Only testing remains. Paste each section's output back
to Claude; results go into the Verification Log below.

**Build under test:** `D:\Users\mariu\Projects\stt-server-next\s\release\stt-server-next.exe`
(record its SHA-256 from the build output before starting).

Replace `<LAPTOP-IP>` with the laptop's Wi-Fi/Ethernet IPv4 (`ipconfig`), and `<TS-NAME>` with its
Tailscale machine name (`tailscale status`).

## A. Clean machine, no GPU (inside the VM)

Copy `stt-server-next.exe` to `C:\stt\` in the VM. Install nothing else. In a normal PowerShell:

```powershell
cd C:\stt
.\stt-server-next.exe start
.\stt-server-next.exe status                 # expect: running, version and api_level shown
.\stt-server-next.exe models install whisper-tiny --wait
.\stt-server-next.exe models select whisper-tiny
.\stt-server-next.exe health                 # expect: ready; backend CPU with a fallback reason
.\stt-server-next.exe stop
```

Pass: every command succeeds; the backend is CPU with a clear reason, not an error.

## B. Second device on the local network (laptop serves, VM calls)

On the laptop (normal PowerShell):

```powershell
$exe = 'D:\Users\mariu\Projects\stt-server-next\s\release\stt-server-next.exe'
& $exe start --network lan
& $exe health                                # expect: network mode lan, effective lan
Get-Content "$env:LOCALAPPDATA\OpenVibeAI\STT Server\auth.token"
Get-Content "$env:LOCALAPPDATA\OpenVibeAI\STT Server\user.token"
```

Windows Firewall may ask to allow the app on private networks: allow it.
In the VM (use a short WAV copied into `C:\stt\clip.wav`):

```powershell
curl.exe -s http://<LAPTOP-IP>:54321/health                               # expect: status ok
curl.exe -s -o NUL -w "%{http_code}`n" http://<LAPTOP-IP>:54321/v1/models  # expect: 401
curl.exe -s -H "Authorization: Bearer <USER-TOKEN>" -F file=@C:\stt\clip.wav -F model=default http://<LAPTOP-IP>:54321/v1/audio/transcriptions   # expect: text
curl.exe -s -o NUL -w "%{http_code}`n" -X POST -H "Authorization: Bearer <USER-TOKEN>" http://<LAPTOP-IP>:54321/v1/local/models/refresh   # expect: 403
```

Optional: switch the laptop's network to Public in Windows settings, wait 30 s, repeat the
transcription call; expect `403 network_not_private`. Switch back to Private.
Afterwards on the laptop: `& $exe stop`.

## C. Tailscale from the phone

On the laptop: `& $exe start --network tailscale`, then `& $exe health`
(expect: effective tailscale, with the 100.x address).
On the phone, Wi-Fi off, Tailscale on, open in the browser:

- `http://<TS-NAME>:54321/health` → expect `status: ok`.
- `http://<LAPTOP-IP>:54321/health` from the VM → the page may load, but any other route must give `403`.

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
Remove it from the machine-wide install first, then import:

```powershell
& $svc models remove whisper-tiny --data-dir $data
& $svc models import-user --from "$env:LOCALAPPDATA\OpenVibeAI\STT Server" --wait --data-dir $data
& $svc models list --data-dir $data          # expect: whisper-tiny installed, no download happened
```

## F. Uninstall (inside the VM)

```powershell
& $svc service uninstall
Start-Sleep 15
Get-Service OpenVibeSttNext -ErrorAction SilentlyContinue      # expect: nothing
Test-Path 'C:\Program Files\OpenVibeAI\STT Server'             # expect: False
Get-ChildItem "$data\models" | Select Name                     # expect: model file kept
```

## Already passed on the laptop (2026-09-27)

Service install, status, model install with progress, transcription on Vulkan, restart, crash
recovery with the model reloaded, uninstall keeping models, old-folder migration.

## Attempts

None yet.

## Verification Log

2026-09-28: Created. All server development for these areas is committed.

## Final Outcome

Not started.
