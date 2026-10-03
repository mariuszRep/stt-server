---
name: install-scope-and-shared-access
title: Install Scope, One Data Folder, and Safe Shared Access
description: Make every way of running the server use one data folder per install, support per-user and machine-wide installs side by side, and let shared servers be used safely by other users, the local network, and Tailscale.
status: in_progress
type: feature
scope: stt-server only
attempt: 1
max_attempts: 8
last_result: all six slices implemented and gated; real-machine rehearsal (second Windows account, cross-account import-user, clean-VM service reinstall) still outstanding, tracked in hands-on-acceptance-tests
next_action: All six planned slices (install scope/data folder, port fallback/discovery, access levels, network modes including Tailscale, versions and moving models between scopes) are implemented, gated, and documented. What remains is real-machine rehearsal only: a genuine second Windows account exercising a machine-wide install's user.token, a real `models import-user` run between a per-user account and a machine-wide service install, and a clean-VM service install/uninstall recheck -- tracked in `ready/hands-on-acceptance-tests` sections B/D/E.
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

- `cancelled/ready-for-voice-typer` (service, LAN and clean-machine acceptance).
- `whisper-vibes`: `draft/switch-to-stt-server-next`; `stt-sdk`: `draft/stt-server-next-adapter`.

## Attempts

None yet.

## Verification Log

2026-09-27: Created from the user's decisions on install scope, one data folder, service role,
multi-user PCs, access levels, network safety and Tailscale.

2026-09-27: User decision -- browser CORS access is locked down by default: no browser origin is
allowed unless explicitly configured (`cors_allowed_origins` default `[]`, no
`Access-Control-Allow-Origin` header for any `Origin`). Configurable via `cors_allowed_origins` on
`PATCH /v1/local/config` (admin) and the CLI `--cors-origin <origin>` flag (repeatable) on
`run`/`start`/`restart`; precedence CLI > stored setting > default. `"*"` is accepted only when
explicitly set. Non-browser clients (Voice Typer, `stt-sdk`, CLI, curl) are unaffected either way.
This reverses the 2026-09-26 review (recorded in `docs/client-contract.md`) that had kept `*` as
the default; implemented in `src/api.rs`, `src/app.rs`, `src/store.rs`, `src/cli.rs`,
`src/bin/server.rs`, with tests covering the default rejection, an explicitly allowed origin's
headers and preflight, explicit `"*"`, no-`Origin`-header requests, and config validation. See
`README.md`'s "Browser access (CORS)" section and `docs/client-contract.md` section 3 for the
full writeup.

## Final Outcome

Not started.

2026-09-27: Slice 1 (install scope, one data folder, service only machine-wide) implemented. Orchestrator gates: fmt clean, clippy clean, cargo test 213 lib + 10 bin passed. Scope follows the exe location (machine-wide program folder or marker file); service install refuses unless elevated; old folder names are renamed, never deleted. Real service install not yet rehearsed.

2026-09-27: Slice 2 ("Several users on one PC": port fallback and discovery) implemented in
`src/api.rs::run_http_full` and `src/bin/server.rs`. A per-user install whose port was only
preferred (default or stored `bind_port` setting, not an explicit `--port`) now falls back to an
OS-assigned free loopback port on `AddrInUse`, logs the fallback, and updates the in-memory `App`
so `server.json`, `/health`, and `/v1/local/config` all report the port actually bound. A
machine-wide install, and any install given an explicit `--port`, fails clearly (`BindFailed`,
exit code 4) instead of falling back -- machine-wide has priority for the fixed port. `cmd_start`
no longer assumes the resolved port when confirming the spawned child is healthy or reporting it
to the caller; it now polls `server.json` the same way `status`/`stop`/`health`/`models *`/
`update *` already did, so it also picks up a port the child fell back to. The `/health` identity
check (`service` field, not just HTTP 200) that guards against a foreign process on the port was
already in place and is exercised again here. Added a CLI-level test,
`models_cli_tests::status_discovers_a_fallback_port_not_the_busy_preferred_one`, on top of the
three `api.rs::port_fallback_tests` (fallback on busy port, explicit `--port` busy fails,
machine-wide busy fails) that were already present from prior work. Updated `README.md`
("Several users on one PC") and `docs/client-contract.md` (new "1.3 Several users on one PC")
to document that clients must never assume the default port. Gates: `cargo fmt --check` clean,
`cargo clippy --all-targets -- -D warnings` clean, `cargo test` 216 lib + 11 bin passed (227
total). Not committed per instruction. Remaining slices: access levels; network modes including
Tailscale; version reporting and model import.

2026-09-27: Real service rehearsal on the dev laptop (user-run, admin). Passed: machine-wide install from the release build, status/health, whisper-tiny install with progress, select, transcription on Vulkan, Restart-Service, forced kill recovered by the SCM with a new PID and the model reloaded, uninstall removed the service and kept models. Bugs found and fixed: (1) SCM start timed out because the service was registered with bare "service", which the CLI rejected; now registers "service run" and accepts bare "service". (2) Install created the new data folder before migrating, leaving old "STT Server Next" folders; migration now runs first and the old program folder is removed. (3) Uninstall from the installed copy failed with Access denied deleting its own running exe; the program folder is now removed after exit. Fixes 2 and 3 need one more real install/uninstall pass. Transcription speed under the service (8.5 s for 8.9 s audio, whisper-tiny on Vulkan) to be rechecked after the catalog sweep releases the GPU.

2026-09-27: Service re-rehearsal after fixes (user-run): old folders migrated/removed, reinstall kept whisper-tiny and came back ready, uninstall ran without errors, service removed, models kept, program files deleted. Remaining cosmetic issue: an empty program folder was left behind (contents removed); recheck on the clean VM.

2026-09-27: "Access levels on a shared server" slice implemented. Two tokens now exist per data
folder: `auth.token` (admin, unchanged path/name) and a new `user.token`, both created the same
way (`app::token_file`/`app::user_token_file`, sharing one `token_file_named` generator). Every
route is classified in `src/auth.rs` (`AccessLevel::User`/`Admin`, checked by `authorize`;
`authorized` is now `authorize(.., Admin)`, kept for the many still-admin call sites). User-level:
`/health` (unauthenticated), `readiness`, `v1/models`, `v1/local/models`, `.../selected` (GET),
`v1/local/system`, `v1/local/recommendations`, `v1/local/operations/{id}` (GET), transcriptions/
translations. Everything else (config GET/PATCH, install/verify/select/unload/remove/refresh/
import, operation cancel, shutdown) stays admin-only. A valid user token on an admin route is
`403 {"code":"admin_required"}`, distinct from `401 unauthorized` for a missing/wrong token; the
`/v1/local/shutdown` loopback-only check still runs first, unchanged. `service.rs::install`'s
ACLs changed to match the approved design: `auth.token` dropped the installing user's standing
read grant (SYSTEM/Administrators only now), and the new `user.token` is granted to the built-in
Users group by well-known SID (`*S-1-5-32-545`, not the localized name) alongside SYSTEM/
Administrators -- the ACL rule lists are pulled into `admin_token_acl_args`/`user_token_acl_args`/
`data_dir_acl_args` so they're unit-testable without touching real ACLs. `model_cli::connect`
(`src/model_cli.rs::read_token`) now prefers `auth.token`, falling back to `user.token` only when
`auth.token` can't be read; `format_error` gained an `admin_required` hint ("admin access
required (run as administrator)"), and any command hitting it already exits non-zero via the
existing generic error path. Updated `README.md` ("Two tokens, two access levels") and
`docs/client-contract.md` (section 3: token/ACL description, full route-classification table,
`admin_required` added to the 403 row of the error table). Tests added: `src/api.rs` router tests
iterating the full user/admin route lists with both tokens (`user_token_is_allowed_on_every_user_
route`, `user_token_is_forbidden_with_admin_required_on_every_admin_route` incl. shutdown/import's
multipart-body edge cases, `admin_token_is_allowed_on_every_route_user_and_admin`); `src/app.rs`
(`user_token_file_creates_a_distinct_stable_token`, `open_app_populates_both_admin_and_user_
tokens`); `src/service.rs` (three tests on the extracted ACL-arg-list functions, no real icacls
run); `src/model_cli.rs` (three `read_token` fallback/error tests). Gates: `cargo fmt --check`
clean, `cargo clippy --all-targets -- -D warnings` clean, `cargo test` 228 lib + 11 bin passed
(239 total; one transient Windows file-lock failure in the new admin-token router test was fixed
by giving `refresh`'s background `tokio::spawn` task a moment to finish before the test deletes
its data dir, matching the existing `refresh_returns_202_and_the_operation_completes` pattern).
Not committed per instruction. Remaining slices: network modes including Tailscale; version
reporting and model import.

2026-09-27: "Network modes" slice implemented in new `src/network.rs` plus glue in
`src/app.rs`/`src/api.rs`/`src/cli.rs`/`src/bin/server.rs`. A `network_mode` setting
(`local`/`lan`/`tailscale`, default `local`) is settable via `PATCH /v1/local/config` (admin) or
CLI `--network` on `run`/`start`/`restart`/`autostart enable` (CLI > stored setting > default); an
existing explicit `--host` (or a previously-stored `bind_host`) still wins over it entirely and is
now reported as network mode `"custom"`. `local` forces a loopback-only bind, as before.

Enforcement design (the "LAN guard" question): both `lan` and `tailscale` bind every interface
(`0.0.0.0`) once, rather than rebinding sockets as Windows' network category or the Tailscale
interface changes -- rebinding would need a second concurrent listener plus a shutdown signal
shared across both (`axum::serve` owns one `TcpListener`) and risks dropping in-flight connections
mid-transition. Instead a `network_gate` middleware (attached only in `run_http_full`'s serving
router, not the shared test `router()`, so it never touches `ConnectInfo`-less unit tests) rejects
every non-loopback, non-`/health` request with `403 network_not_private` unless a live
`NetworkReport` (in `App.network_state`, an `RwLock`, refreshed at startup and every 30s by a
background task) currently says the mode is active: for `lan`, every active Windows connection
profile is Private/DomainAuthenticated (`(Get-NetConnectionProfile).NetworkCategory` via a
PowerShell subprocess, 5s timeout, polled with `try_wait` so a hung subprocess is killed rather
than hanging startup/recheck); for `tailscale`, this PC has a detected Tailscale IPv4
(`tailscale ip -4`, same subprocess pattern, parsed for a `100.64.0.0/10` CGNAT address) *and* the
calling peer's own source address is itself in that CGNAT range -- a connection arriving over the
Tailscale virtual interface always carries the caller's own Tailscale address as its source
address, so this peer-address check is equivalent to having bound only the Tailscale interface,
without a second listener. Detection failure/timeout/no-active-profile all fall back to
local-only the same way a Public profile does. `/health` (still unauthenticated) gained a
`network: {mode, effective, reason?, addresses?}` object so a client can show the real state
without guessing; addresses never include the token.

Real checks on this laptop (temp `--data-dir`, spare ports, `curl`/`Invoke-RestMethod
/health`, stopped afterward): `--network lan` on port 54471 reported
`{"mode":"lan","effective":"lan"}` (this laptop's active network profile is genuinely Private);
`--network tailscale` on port 54472 reported
`{"mode":"tailscale","effective":"tailscale","addresses":["100.125.201.68"]}`, matching this
laptop's real `tailscale ip -4` output (Tailscale is installed and running here). Both processes
were stopped and their temp data dirs removed after the check.

Tests added: `src/network.rs` (pure, no real subprocess/network calls) covers mode
parse/precedence, `Get-NetConnectionProfile`-output parsing for a single Private/Public/
DomainAuthenticated profile, mixed profiles (any Public -> not private; Private+DomainAuthenticated
-> private), no/blank/unrecognized lines, `tailscale ip -4`-output parsing (CGNAT line picked out
of extra non-CGNAT lines, boundary octets of `100.64.0.0/10`), the `peer_allowed_non_loopback`
decision for all four cases (custom/local/lan/tailscale, including a tailscale-mode peer whose own
address is a plain LAN address being rejected even while the server's own Tailscale address is up),
and `NetworkReport::to_json` field omission. `src/cli.rs`: `--network` parses all three values and
rejects an invalid one. `src/app.rs`: mode/custom resolution (default local, explicit `--host` wins
over a `--network` override, a previously-stored `bind_host` also counts as custom). `src/api.rs`:
`/health`'s `network` object for default (local) and custom-host cases; `network_gate` behaviour
(health always open, loopback always allowed, LAN peer rejected under `local`/allowed once the
live report says `lan`, a custom-host bind allows any peer); `PATCH`/`GET /v1/local/config`
round-trip and rejection of an invalid `network_mode`. Gates: `cargo fmt --check` clean, `cargo
clippy --all-targets -- -D warnings` clean, `cargo test` 266 lib + 11 bin passed (277 total).
Updated `README.md` (new "Network modes" paragraph, `--network` flag, advanced-override note) and
`docs/client-contract.md` (new "3.1 Network modes" section with the enforcement-design
justification, `network_not_private`/`invalid_network_mode` added to the error table, `/health`'s
`network` object documented in section 7). Not committed per instruction. Remaining slices: version
reporting and model import.

2026-09-27: Final slice ("Versions and moving between scopes") implemented.

- **Version and API level**: `api::API_LEVEL` (currently `1`) is a documented constant, bumped
  only for a breaking client-facing change, never for an additive one. `GET /health`
  (unauthenticated) now reports `version` (`CARGO_PKG_VERSION`) and `api_level` alongside the
  existing `status`/`service`/`network` fields; `GET /v1/local/system`'s `server` section reports
  the same two. `server.json` (`discovery::ServerInfo`) gained an `api_level` field (`#[serde(default)]`
  so an old file without it still parses, reading back as `0`); `stt-server status` and
  `stt-server health` both print version/api_level (JSON and human output). `status --json`'s
  `api_level` is therefore this build's constant if the running server wrote it, or `0` for a
  `server.json` from before this change.
- **`models import-user`** (admin-only, `src/import_user.rs`, new module): CLI
  `stt-server models import-user [--from <dir>] [--wait] [--json] [--data-dir <path>]` and
  `POST /v1/local/models/import-user` `{"from": path}` (from defaults server-side to
  `app::per_user_data_dir()`, the invoking OS user's own per-user data folder). Deliberately does
  *not* open the source's `state.db`: since the managed store and the drop-in folder already share
  one models folder per install (`<data dir>/models`), it scans `<from>/models/*.gguf`, hashes each
  file off the async runtime, and matches it against the catalog with the same
  `catalog::catalog_match_by_hash` the drop-in refresh (`dropin.rs`) already uses for exactly this
  kind of untrusted-file verification -- simpler and more robust than depending on another
  process's SQLite file/schema/locking. A match not already installed here is copied to staging,
  re-hashed (verifies the *copy*, not just trust in the source), and promoted via the existing
  `store::promote_verified_model_with_source` with a new `source: "import_user"`; a hash mismatch
  after copying quarantines the copy (never the source); a file matching no catalog entry, or a
  model already installed, is reported and left untouched. The source install's files and database
  are never written to. Runs as a durable operation (`kind: "import_user"`, `progress_items`/
  `total_items`, and a `result: {"imported","skipped","unsupported"}` JSON blob on completion),
  polled/cancelled the same way as `refresh`/`install`/`verify`; `--wait` prints a per-model
  imported/skipped/failed line, mirroring `models refresh --wait`.
- Tests added (`src/import_user.rs`, 4; `src/api.rs`, 1; `src/cli.rs`, 1): `/health` reports
  `version`/`api_level`; import-user copies two catalog-matched files and verifies both, a second
  run against the same source skips both as `already_installed`, source files are byte-identical
  after the copy; a file matching no catalog entry is reported `unsupported` and left in place
  (never copied, never quarantined -- it was never matched); a missing source `models` folder
  completes with nothing imported (same "missing dir = empty" rule as `refresh`); a user-token
  caller gets `403 admin_required`; CLI parses `models import-user` with/without `--from` and
  rejects an unknown flag.
- Updated `README.md` (`/health`'s new fields under "Current API", the `models import-user` CLI
  entry) and `docs/client-contract.md` (new "1.4 Versions and moving between scopes" section,
  `POST /v1/local/models/import-user` added to the admin-only route table).
- Gates: `cargo fmt --check` clean, `cargo clippy --all-targets -- -D warnings` clean, `cargo test`
  271 lib + 11 bin passed (282 total, 0 failed). Not committed per instruction.

2026-10-03: Network modes checked live (no code change) (real laptop, Windows 11 "rhs-surface", Tailscale
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
  Still outstanding for this goal: second Windows account, cross-account `import-user`, clean-VM
  service reinstall, and a genuine second-device LAN check (all in `in_progress/hands-on-acceptance-tests`).

Every success criterion in this goal's slices (install scope/one data folder, several users on one
PC, access levels, network modes including Tailscale, versions and moving models between scopes)
now has an implementation and passing tests. Status stays `in_progress`: the real multi-user and
clean-VM rehearsals called for in the goal (a genuine second Windows account exercising a
machine-wide install's `user.token`, a real `models import-user` run between a per-user account's
real models and a machine-wide service install, and the clean-VM service install/uninstall
recheck noted above) have not been done.
