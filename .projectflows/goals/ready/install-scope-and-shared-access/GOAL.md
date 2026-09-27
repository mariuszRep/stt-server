---
name: install-scope-and-shared-access
title: Install Scope, One Data Folder, and Safe Shared Access
description: Make every way of running the server use one data folder per install, support per-user and machine-wide installs side by side, and let shared servers be used safely by other users, the local network, and Tailscale.
status: ready
type: feature
scope: stt-server-next only
attempt: 0
max_attempts: 8
last_result: none
next_action: Implement in slices: install scope and data folder; port fallback and discovery; access levels; network modes including Tailscale; version reporting and model import.
success_criteria:
  - Each install has exactly one data folder, and the app, CLI, start-with-Windows and service all use it; a model is never stored twice within one install.
  - A per-user install needs no admin; a machine-wide install needs admin once and serves every user on the PC.
  - The Windows Service is offered only for machine-wide installs.
  - Several users on one PC can each run their own server at the same time without clashing, and clients always find the right one.
  - On a machine-wide server, ordinary local users can transcribe and see models and health, but only the admin can change models, settings, or update.
  - A shared server can be reached on the local network only on networks Windows marks as private, or over Tailscale only, and never without the token.
  - Clients can tell the server's version and API level before relying on it.
  - Models a user already downloaded can be moved into a machine-wide install without downloading again.
source: user
---

# Install Scope, One Data Folder, and Safe Shared Access

## Why

Today a service uses a different data folder from normal runs, so models can be stored twice and
settings drift apart. The server must also work on PCs with several users, and as a shared server
reached from other devices, without anyone being able to damage it or reach it from a public network.

## Two roles

- **Part of an application (default).** An app such as Voice Typer ships the server, starts it,
  stops it on exit, and manages it through the API. It uses the CLI only for what the API cannot
  do (starting the process, updating the executable, autostart). "Start with Windows" is available
  for someone who wants it always on for themselves.
- **A shared server.** The server runs on its own on a machine and is used by several users or
  devices. This is the case for the Windows Service: it starts at boot, keeps running for whoever
  is signed in, and restarts after a crash.

## Business rules

### Install scope and data folder
- **Per user (default):** data in the user's `%LOCALAPPDATA%\OpenVibeAI\STT Server`. No admin.
- **Machine-wide:** program in Program Files, data in `%ProgramData%\OpenVibeAI\STT Server`.
  Admin needed once, at install.
- Every mode within an install uses that install's single folder. An explicit data folder
  chosen by the user still overrides it.
- The Windows Service exists only in machine-wide installs. Per-user installs use start-with-Windows.

### Several users on one PC
- Each user's own server prefers the default port. If the port is taken (another user's server,
  or a machine-wide server), it picks a free one and records it where that user's clients look.
- Clients always find the server through that record, never by assuming the default port.
- Each user's server has its own token; one user cannot use another's without it.
- A machine-wide server keeps the fixed default port and takes priority for it.

### Access levels on a shared server
- **User access:** transcribe, translate, list models, see the selected model, health, readiness,
  and hardware. Every local user of the PC gets this without handling a secret file.
- **Admin access:** install, import, verify, select, unload and remove models, change settings,
  refresh the drop-in folder, update, and shut down. Only holders of the admin token.
- Per-user installs behave as today: the owner has full access.
- Remote (network) callers always need a token; the access level follows the token they present.

### Network modes
- **Local only (default):** reachable from this PC only.
- **Local network:** reachable from other devices only while Windows marks the current network as
  private. On a public network it falls back to local only and says so in health.
- **Tailscale:** reachable only through this PC's Tailscale address, from anywhere on the user's
  tailnet, and not on Wi-Fi or Ethernet. Clients may use the machine's Tailscale name. If Tailscale
  is not running, the server stays local only and says so in health.
- A token is always required for anything beyond the health check from another device.

### Versions and moving between scopes
- Health reports the server version and an API level that increases whenever clients need to
  change. Clients use it to refuse or warn about a server that is too old.
- When a machine-wide install is set up on a PC where a user already has models, the admin can
  import those models into the shared folder after verification, instead of downloading again.
  The user's own copy is left alone unless they remove it.

## Open decision (not in this goal)

- Using Tailscale's knowledge of who is calling to replace the token for tailnet devices.

## Out of scope

Voice Typer and SDK changes (offering to use a shared server, connecting to a remote server,
version warnings in the app), macOS and Linux, multiple servers behind one address, cloud hosting.

## Related goals

- `draft/ready-for-voice-typer` (service, LAN and clean-machine acceptance).
- `whisper-vibes`: `draft/switch-to-stt-server-next`; `stt-sdk`: `draft/stt-server-next-adapter`.

## Attempts

None yet.

## Verification Log

2026-09-27: Created from the user's decisions on install scope, one data folder, service role,
multi-user PCs, access levels, network safety and Tailscale.

## Final Outcome

Not started.
