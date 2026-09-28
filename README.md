# STT Server Next

Standalone Windows-first prototype of a single-process GGUF speech server. This repository is
independent of the shipping Voice Typer component worktrees. The current executable hosts an
authenticated batch API, SQLite model state, verified model installation/import, and a Windows
Service. SDK and Voice Typer integration remain part of a separate cutover.

The intended product starts without downloading any model. It presents a fixed, Handy-informed
recommendation order with model languages, licence, sizes, capabilities, and sourced benchmark
data. A user explicitly chooses a model to install. Backend probing happens when loading that
model, and diagnostics report the backend actually used and any CPU fallback reason.

## Current API

`GET /health` is unauthenticated and reports `{"status","service","version","api_level","network"}`
-- `version` is this build's `CARGO_PKG_VERSION`, and `api_level` is an integer, starting at 1,
bumped only when a client must change to keep working (a breaking shape change, a removed route, a
new required field -- never for an additive, backward-compatible one). A client should refuse or
warn when the server's `api_level` is lower than the level it requires; `stt-server-next health`
and `status` also print both fields (`status --json`'s `server.json`-derived `api_level` defaults
to `0` for a `server.json` written before this field existed, meaning "older than any level a
client requires"). `GET /v1/local/system`'s `server` section reports the same two fields.
Other routes require a bearer token. The server implements
`GET /readiness`, `GET /v1/models`, and OpenAI-style `POST /v1/audio/transcriptions` and
`POST /v1/audio/translations` for mono/stereo WAV from 8–192 kHz (16/24-bit PCM or 32-bit float),
both sharing one pipeline (parse → decode → plan → queue → run → format) and a 40 MiB body limit.
Audio is converted to 16 kHz mono before inference. Accepted multipart fields are `file`, `model`,
`language`, `prompt`, `temperature`, `response_format` (`json`/`text`/`verbose_json`), and repeatable
`timestamp_granularities`/`timestamp_granularities[]`; `prompt` is passed to the engine verbatim
(no server-side composition or trimming) and is only accepted on whisper-family models. `model` is
optional: an absent field is treated as `model=default` (whichever model is currently selected)
rather than a 400, since OpenAI-shaped clients typically send it but this server's own SDK client
often doesn't. An unresolvable `language` hint is never an error: the server falls back to
auto-detection, then English, then the model's first language (matching Handy), and reports whether
the hint was actually applied via
`x_diagnostics.language_hint_applied`/`applied_language`/`language_evidence` (`user_selected`,
`model_constrained`, `model_detected`, `translated_to_english`, or `unknown`; a `translations`
request whose source is already English transcribes instead of translating but reports whichever of
`user_selected`/`model_constrained` decided that language, never the stale `translated_to_english`
label for a run that didn't translate anything).
The `/v1/local/*` routes expose fixed recommendations, per-model capability matrices (a `catalog`
view when unloaded, an `effective` view computed from the live model plus `catalog_mismatch` once
loaded), installed models, explicit install/import/verification, selection/load/removal, operation
progress and cancellation, and CPU/Vulkan preference. Unsupported optional transcription fields
return `unsupported_capability`. Every catalog model is installable (`installable: true`); admission
is catalog membership plus a resolvable quant/hash, not a hard-coded model ID.
`GET /v1/local/system` (authenticated) reports the server's own OS/CPU/memory/GPU/process
info for a client's hardware/health card -- see `docs/client-contract.md` for the full shape,
field provenance, and a mapping from the old provider-lifecycle client calls to the new ones.

`POST /models/manage/{id}/download` takes an optional JSON body `{"quant":"Q8_0"}`; an absent or
empty body installs the model's `default_quant`. An unknown quant returns 400 `invalid_quant`;
an already-installed model returns 409 `already_installed`. `POST /models/manage/import`
accepts an optional multipart `quant` field before `file` to preselect the expected catalog file;
without it, the uploaded file's size and SHA-256 are matched against any of the model's files.
`GET /models/manage` and `GET /models/manage/recommendations` list every catalog file
(`quant`, `size_bytes`, `sha256`) plus `installed_quant` (the recorded quant, or `null`).
`GET /models/manage/operations/{id}` also reports `created_at`, `updated_at`, and `finished_at`
(unix milliseconds). Verify, restart reconciliation, select, and remove all operate on the quant
actually installed, not the catalog's current default.

State is stored in a versioned SQLite schema (`PRAGMA user_version`). Opening an older,
unversioned database migrates it to the current schema in one transaction, backing up the file
to `state.db.bak-v<old>` first when it already had data. Schema v3 adds a machine-readable
`error_code` on operations (`insufficient_disk_space`, `stalled`, `source_unavailable`,
`hash_mismatch`, `cancelled`), returned alongside the free-text `error` by
`GET /models/manage/operations/{id}`. Schema v4 adds `installed.source`
(`catalog_download`/`import`/`user_folder`) plus nullable `custom_name`/`custom_arch`/
`custom_languages`/`custom_claims`/`mtime_ms`/`needs_verification` columns for drop-in models, and
`operations.progress_items`/`total_items`/`result` (a JSON blob) for item-counted, durable
operations such as refresh; existing rows backfill `import` (from a completed `import` operation)
or `catalog_download` (everything else).

## Drop-in models and refresh

A user may copy `.gguf` files directly into a user-writable folder instead of using
install/import. The folder defaults to `<data dir>\models` of this install's single data folder
(see "Install scope and data folder" above) -- `%LOCALAPPDATA%\OpenVibeAI\STT Server\models`
per-user, `%ProgramData%\OpenVibeAI\STT Server\models` machine-wide (shared by every user of the
machine, not tied to whichever user happened to run `service install`), the same folder the
managed store itself uses (Local, not Roaming, so multi-gigabyte files are never synced with a
roaming profile). It is the `user_models_dir` setting: visible on `GET /v1/local/config` (defaulted
when unset to the path above) and settable via `PATCH /v1/local/config` with
`{"user_models_dir": "<absolute path>"}`; a relative or nonexistent path returns 400
`invalid_user_models_dir`.

`POST /models/manage/refresh` is a durable operation (`kind: "refresh"`, 202 + operation ID,
observable and cancellable like install/import/verify) that non-recursively scans the drop folder
for `*.gguf` files (ignoring `.part`). For each file:

1. A file already registered at the same path, size, and mtime is skipped.
2. Otherwise it is hashed (`spawn_blocking`) and its item progress is reported via the operation's
   `progress_items`/`total_items`.
3. A size+SHA-256 match against any catalog file registers it as that catalog model/quant,
   `source: "user_folder"`, in place (never moved or copied). If that catalog model is already
   installed from the managed store, the drop-in copy is reported as a duplicate and left alone.
4. Otherwise its GGUF header is probed (`src/gguf_probe.rs`, ported from Handy's
   `gguf_meta.rs`/`model_capabilities.rs`, MIT, commit `8f9cf53`) for `general.architecture`. A
   known speech architecture registers a custom model: ID `custom-<slug>-<first 8 hex of sha256>`
   (slug from `general.name`, else the file stem, lowercased to `[a-z0-9-]`), with name,
   architecture, languages, and capability claims read from the header. An unknown/unsupported/
   unreadable file is listed with a reason and left untouched.
5. A previously registered file that disappeared is unregistered (deselecting/unloading it first
   if it was active). One whose size or mtime changed is re-hashed and re-probed by this same
   refresh (registering it under a possibly new identity if its content changed architecture/hash).

The operation's `result` (visible on `GET /models/manage/operations/{id}`) lists `registered`,
`duplicates`, `unsupported`, `removed`, and `changed`.

Startup reconciliation (`app::reconcile_installed`) treats `user_folder` rows differently from
catalog/import rows: it never quarantines or moves them (they are outside the managed store by
design), only checking existence/size/mtime -- a disappeared file is unregistered, and a
changed-on-disk file is flagged `needs_verification` (blocking selection until an explicit refresh
re-hashes it) without itself re-hashing anything.

`GET /models/manage` lists custom (non-catalog) installed models alongside catalog entries, with
`source`, `custom: true`, a capability view built from the GGUF header claims
(`evidence: "gguf_header"`), `installable: false` (they can only arrive via refresh), and
`recommended_rank: null`; `GET /v1/models` includes any installed custom model too. Select/load and
transcription work the same regardless of source. `DELETE /models/manage/{id}` on a
`user_folder` model unregisters it only -- the file is never deleted (`file_deleted: false` in the
response); catalog/import models keep the previous move-then-delete behavior
(`file_deleted: true`). `POST /models/manage/{id}/verify` on a `user_folder` model re-hashes it
in place; a mismatch marks it `needs_verification` rather than quarantining the user's file.

Downloads are hardened: a 60s stall timeout applies to connect and every chunk (not the whole
transfer, so a 48 GB file is never killed just for taking a long time); a `.part` already at the
expected size skips the network and goes straight to hash verification; HTTP 416 discards the
partial and restarts once; each source gets up to 3 attempts with 2s/5s/15s backoff before moving
to the next; a disk-space preflight refuses to start when free space is under
`(remaining bytes) * 1.05 + 64 MiB`, failing the operation with `insufficient_disk_space`;
progress is written to SQLite at most every 250 ms or 4 MiB. **HuggingFace is the only download
source.** We do not use Handy's `blob.handy.computer` mirror without that project's permission, so
there is no mirror fallback: when HuggingFace fails after the retry schedule above, the operation
fails with `error_code: "source_unavailable"` and a message noting the operation can be retried.
The catalog's `mirrors` field is still deserialized (so the embedded catalog JSON stays
byte-identical to upstream) but is otherwise unused.

## Inference queue and model switching

Exactly one transcription runs at a time, matching the single resident model. A request that
arrives while another is running joins a FIFO queue (`src/queue.rs`). By default the queue is
unbounded and a waiter never times out, matching the current shipping server. Three settings make
these optional and bounded:

- `queue_max_waiting` (positive integer or `null`): once this many requests are already waiting,
  the next one gets 429 `queue_full` immediately instead of joining the queue.
- `queue_wait_timeout_ms` (positive integer or `null`): a waiter that sits this long without a
  turn gets 503 `queue_timeout`.
- `inference_timeout_ms` (positive integer or `null`): a run that exceeds this many milliseconds
  is cancelled (its cancel token fires) and the request gets 504 `inference_timeout`.

All three are visible on `GET /v1/local/config` and settable via `PATCH /v1/local/config`
(`null` clears a setting back to unbounded/no-timeout; a non-positive value is rejected with 400).
They apply **live**: the server keeps an in-memory copy (`App::limits`, a `std::sync::RwLock`,
read at request time), so a `PATCH` takes effect on the very next request with no restart. The
`run`/`start`/`restart` commands (see "CLI" below; not `service`/`autostart`) also accept
`--queue-max-waiting <n>`, `--queue-wait-timeout-ms <n>`, and `--inference-timeout-ms <n>` flags,
which override the stored settings for that process only; an unrecognized flag or a non-positive
value prints an error to stderr and exits with code 2.

Multipart parsing, validation, and `decode_wav` all happen before a request joins the queue. If
the client disconnects while queued or running, the handler future is dropped and the queue slot
/ cancellation token are released via RAII. A successful transcription's JSON response gets an
additive `x_diagnostics` object: `queue_wait_ms`, `inference_ms`, `audio_ms` (post-resample
duration at 16 kHz), `model`, `backend`, and `fallback_reason`.

The server and `server.json` are available immediately at startup; a previously selected model
(a huge one that falls back to CPU, e.g. Voxtral-Small-24B, can take minutes) reloads on a
detached background thread that startup never waits for or joins. `GET /health` is `ok`
throughout, so `stop`/`status`/`models list` all work while it's loading -- previously
`server.json` did not exist until the load finished, so those commands failed as if the server
weren't running, and only killing the process worked. While it's loading, `GET /readiness` reports
`503 {"status": "not_ready", "reason": "loading model", "model": id, "elapsed_ms": n}`, and a
transcription/translation request gets `503 model_loading` (details: `model`, `elapsed_ms`) rather
than the generic `server_not_ready`, so a client can tell "still coming up" apart from "nothing
selected." Selecting a *different* model while this background load is in progress is rejected
with `409 model_loading` rather than queued (the in-progress load can't be cancelled); retry once
`/readiness` clears. `stop`/shutdown during the load returns promptly -- it is never blocked on the
loader thread, which is torn down with the process. See `docs/client-contract.md` section 4.2 for
the full behaviour.

`POST /models/manage/{id}/default` no longer holds the inference slot
while the new model loads: the old model keeps serving in-flight and newly queued transcriptions
off its already-loaded handle while the new model loads on a blocking thread, and only the final
swap of `app.loaded` is briefly exclusive. A failed load leaves the previous selection in place.
Note that both models are briefly resident in memory during the swap window; this is accepted as
a tradeoff for non-blocking switching. `DELETE /models/manage/default` now waits up to 30s for
a running inference to finish before unloading, returning 409 `model_in_use` only if that timeout
elapses. When transcription's readiness check fails (`server_not_ready`), the error includes
`error.details.operation_id` when an install/import/verify operation is currently queued or
running.

## Browser access (CORS)

CORS ("Cross-Origin Resource Sharing") is the browser rule that decides whether a web page loaded
from one origin (e.g. `http://localhost:3000`) is allowed to read the response of a request it
makes to a different origin (e.g. this server on `http://127.0.0.1:54321`). It only ever applies
to requests a **browser** makes on a web page's behalf; it has no effect on Voice Typer, `stt-sdk`,
the CLI, curl, or any other non-browser client, since none of those send the `Origin` header a
browser attaches automatically, and the server's CORS layer only ever reacts to that header.

**Secure by default (decision 2026-09-27):** out of the box, no browser origin is allowed --
`GET /v1/local/config`'s `cors_allowed_origins` defaults to `[]`, and no response ever carries an
`Access-Control-Allow-Origin` header. A web page cannot call this server's API at all unless an
operator explicitly opts an origin in. The bearer token is still required regardless of CORS: an
allowed origin only lets a browser *see* the response, it doesn't bypass `Authorization`.

**Allowing a browser origin:**

- CLI, at start: `stt-server-next run --cors-origin http://localhost:3000` (repeatable for more
  than one origin; wins for that process's lifetime over the stored setting). Also accepted by
  `start`/`restart`.
- Config API, persisted across restarts:
  ```
  PATCH /v1/local/config
  Authorization: Bearer <admin token>
  Content-Type: application/json

  {"cors_allowed_origins": ["http://localhost:3000"]}
  ```
  Each entry must be `*` or an exact `http(s)://host[:port]` origin (no path/query/credentials);
  an invalid entry returns 400 `invalid_cors_origins`. The response includes
  `"restart_required": true` -- a CORS change takes effect on the next server start, when the
  `tower_http::cors::CorsLayer` is built from the resolved allow-list (CLI `--cors-origin` >
  stored setting > default `[]`).

**Avoid `"*"`:** it is accepted, but only when set explicitly -- never as an implicit default --
because it lets *any* web page probe this server, including `GET /health` (the one unauthenticated
route), from a victim's browser. Prefer listing the exact origin(s) that need access (e.g. your
Electron/Tauri app's dev server, or a local web UI you trust) instead.

`OPTIONS` preflight requests succeed without a bearer token for an allowed origin, with the
expected `Access-Control-Allow-Methods`/`-Headers` (including `Authorization` and `Content-Type`);
a disallowed origin gets no CORS headers at all, and every other route (except `/health`) still
requires the token exactly as before.

## CLI

`stt-server-next.exe` is hand-parsed (no argument-parsing dependency); `--help`/`-h`/`help`
prints usage, and an unknown command or flag prints an error and exits with code 2.

```
stt-server-next [run] [flags]              foreground (default when no command given)
stt-server-next start [flags]              detached background process; no-op if already running
stt-server-next stop [--data-dir <path>]   graceful stop of the running instance
stt-server-next restart [flags]            stop then start
stt-server-next status [--json] [--data-dir <path>]
stt-server-next autostart enable [flags]   per-user "start with Windows" (no admin)
stt-server-next autostart disable
stt-server-next autostart status
stt-server-next service install|uninstall|run   existing Windows Service host
stt-server-next health [--json] [--data-dir <path>]
stt-server-next models list [--json] [--data-dir <path>]
stt-server-next models recommended [--json] [--data-dir <path>]
stt-server-next models selected [--json] [--data-dir <path>]
stt-server-next models download <id> [--wait] [--json] [--data-dir <path>]  (alias: install)
stt-server-next models import <path> --model <id> [--quant <q>] [--wait] [--json] [--data-dir <path>]
stt-server-next models import-user [--from <per-user data dir>] [--wait] [--json] [--data-dir <path>]
stt-server-next models verify <id> [--wait] [--json] [--data-dir <path>]
stt-server-next models cancel <operation_id> [--data-dir <path>]
stt-server-next models default <id> [--json] [--data-dir <path>]  (alias: select)
stt-server-next models unload [--json] [--data-dir <path>]
stt-server-next models remove <id> [--json] [--data-dir <path>]
stt-server-next models refresh [--wait] [--json] [--data-dir <path>]
stt-server-next update check [--json]
stt-server-next update install [--yes] [--json] [--data-dir <path>]
```

`run`/`start`/`restart`/`autostart enable` share: `--port <n>` (default 54321), `--host <addr>`
(default `127.0.0.1`), `--network <local|lan|tailscale>` (default `local`, or the stored
`network_mode` setting), `--data-dir <path>`, plus the existing `--queue-max-waiting`,
`--queue-wait-timeout-ms`, `--inference-timeout-ms`. The top-level `install`/`uninstall`
spellings and bare `service` (no subcommand vs. `service run`) keep working as aliases for
`service install`/`service uninstall`/`service run`.

**Bind precedence**: `--port`/`--host` (CLI flag) > the stored `bind_host`/`bind_port` settings
(readable/settable via `GET`/`PATCH /v1/local/config`; a bind change reports
`restart_required: true` and takes effect on the next start) > the hard default
`127.0.0.1:54321`. An explicit `--host` (or a stored `bind_host`) is an *advanced override* that
always wins over `--network`/the stored `network_mode` setting entirely; `/health` then reports
network mode `"custom"` instead of `local`/`lan`/`tailscale`. `--data-dir` overrides
`STT_NEXT_DATA_DIR` and the `%LOCALAPPDATA%`-based default the same way.

**Network modes** (used instead of `--host` by anyone who doesn't need the advanced override):
`--network local` (default) binds loopback only. `--network lan` binds every interface but only
actually accepts a non-loopback caller while Windows reports every active network connection
profile as Private or DomainAuthenticated (`Get-NetConnectionProfile`); a Public profile, a mixed
set, or a detection failure/timeout rejects non-loopback callers with `403 network_not_private`
and falls back to local-only, rechecked every 30s so moving onto (or off) a trusted network takes
effect without a restart. `--network tailscale` binds every interface but only accepts a
non-loopback caller that is itself reachable from a genuine Tailscale address
(`100.64.0.0/10`) while this PC's own Tailscale IPv4 address (`tailscale ip -4`) is detected;
otherwise it also falls back to local-only, rechecked the same way. Both modes bind all
interfaces up front rather than rebinding sockets as the network changes -- see
`docs/client-contract.md`'s "Network modes" section for why that's the chosen, more robust
mechanism, and for exactly what each mode's `/health` report looks like. `network_mode` is also
settable via `PATCH /v1/local/config` (admin only; takes effect on the next start, like
`bind_host`/`bind_port`).

**Single instance and discovery**: on startup the server writes `<data dir>\server.json`
(`{pid, host, port, version, started_at}`, atomically) and holds an exclusive
`<data dir>\server.lock`; a second instance pointed at the same data directory exits immediately
with a clear error (exit code 3). `status` checks the saved PID and health endpoint. `stop`
uses the exclusive lock to identify an absent instance and safely clear stale discovery.
A bind failure (e.g. port already in use) exits with code 4. Every CLI command that talks to a
running server (`status`/`stop`/`restart`/`health`/`models`/`update`) discovers it through
`server.json`, never by assuming the default port -- `health`'s `service` field is also checked,
not just a 200 status, so a foreign process already listening on that port is never mistaken for
this server (see "Several users on one PC" below).

**Several users on one PC**: a per-user install (data under `%LOCALAPPDATA%`) whose port was not
given explicitly (no `--port`, whether the effective port came from the hard default or a stored
`bind_port` setting) falls back to an OS-assigned free loopback port if the preferred one is
already taken -- typically by another user's server, or a machine-wide one -- and logs the
fallback; the port actually bound is what `server.json` (and `/health`, `/v1/local/config`)
report, and that's what every CLI command finds. A machine-wide install always keeps its fixed
port and has priority for it: if that port is taken, it fails clearly (exit code 4) instead of
moving. An explicit `--port` also always fails clearly rather than silently falling back to a
port the caller didn't ask for, on either install scope.

**`start`** spawns the same executable with `run` and the given flags as a detached background
process (`CREATE_NO_WINDOW | DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP` on Windows), waits up
to 30s for `/health`, then prints the pid and port. Calling `start` again while already running
is a no-op success. **`stop`** calls the authenticated `POST /v1/local/shutdown` (using the token
from `<data dir>\auth.token`) to trigger graceful shutdown and waits up to 15s for the
data-folder lock to be released. An unavailable endpoint or a shutdown timeout returns
failure without forcibly terminating any process.

**LAN mode**: binding to anything other than a loopback address (`127.0.0.1`/`::1`) -- including
`0.0.0.0`/`::` or a specific LAN IP -- logs a warning that the server is reachable from the
network and requires a usable bearer token (fails closed if the token file can't be read/created).
`/health` stays unauthenticated; every other route, including the new shutdown endpoint, still
requires a token. The shutdown endpoint additionally only accepts callers connecting from a
loopback address, regardless of token, even when the server itself is bound to a LAN address.

**Two tokens, two access levels**: `<data dir>\auth.token` is the admin token (full access to
every route); `<data dir>\user.token` is a user token that only reaches the read-only/transcribe
routes (see `docs/client-contract.md` section 3 for the full route table) -- a valid user token
on an admin-only route gets `403 admin_required`, not `401`. Both files exist for every install,
but only a machine-wide (service) install gives them different ACLs: `auth.token` is readable
only by `SYSTEM`/Administrators, `user.token` is additionally readable by every local user of the
machine (the well-known Users-group SID, so this also works on non-English Windows). The CLI
prefers `auth.token`, falling back to `user.token` only when `auth.token` can't be read.

**Autostart**: `enable` writes a per-user (no admin) `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`
value named `OpenVibeSttServer` set to `"<exe path>" start --data-dir "<data dir>"` (plus any
given `--port`/`--host`); `disable` removes it; `status` reports the stored command line or
"disabled". Implemented with the built-in `reg.exe` (the same pattern `service.rs` already uses
for `icacls`), so no new registry-access dependency was added. Windows-only; other platforms
report "not supported" and exit 2.

**`models`/`health`**: thin CLI wrappers over an already-*running* server's existing
authenticated API (`docs/client-contract.md` sections 4/4.1/7) -- they discover the server and
read its token exactly like `stop`/`status` do, and never start it or duplicate any business
rule.

- `models list`/`recommended`/`selected` are read-only `GET`s (catalog + installed state,
  the curated recommendation order, and the currently loaded model's capability matrix).
- `models download <id>` (alias: `install`)/`models verify <id>`/`models refresh` start a long-running operation
  and print its `operation_id`/`state`; add `--wait` to poll `GET /models/manage/operations/{id}`
  every 500ms, printing byte/item progress, until it reaches `completed`/`failed`/`cancelled`.
  `models cancel <operation_id>` aborts one. `models refresh --wait` additionally reports each
  drop-in file's outcome (registered/duplicate/changed/removed, or a retryable failure reason).
- `models import <path> --model <id> [--quant <q>]` uploads a local GGUF file as
  `multipart/form-data` (`model` field, optional `quant`, then the file) to
  `POST /models/manage/import`, then behaves like `install`/`verify` above.
- `models import-user [--from <dir>]` (admin only) copies GGUFs from another install's models
  folder into this one -- the machine-wide-install-on-a-PC-with-existing-per-user-models case: an
  admin setting up a shared server can pull in models a user already downloaded instead of
  downloading again. `--from` defaults to the invoking OS user's own per-user data folder
  (`%LOCALAPPDATA%\OpenVibeAI\STT Server`); it also works per-user -> per-user with an explicit
  `--from`. Every file under `<from>\models` is hashed and matched against the catalog (the same
  check `models refresh` uses for drop-in files) *before* being copied and re-verified in place --
  a file that doesn't match any catalog entry, or one already installed here, is reported and left
  alone; the source is never modified or deleted either way. `--wait` reports each model's
  imported/skipped/failed outcome, like `models refresh` does for drop-in files.
- `models default <id>` (alias: `select`) loads a model (`409 needs_verification` prints a hint to verify or
  refresh first); `models unload` deselects the current one; `models remove <id>` unregisters a
  managed or drop-in model (a drop-in file's disk copy is never deleted -- the next `refresh`
  re-registers it, matching `CONVENTIONS.md`'s "refresh and removal never delete a user's file").
- `health` combines `/health`, `/readiness`, `/models/manage/default` and `/v1/local/system`
  into one report and exits non-zero when `/readiness` itself isn't `ready`.
- Every subcommand accepts `--data-dir` and `--json` (raw server JSON, for automation); without
  `--json` output is a short human-readable rendering. An absent/dead server or a `{"error":
  {"code","message"}}` response from the API prints a clear message to stderr (or, under
  `--json`, the error body) and exits non-zero -- these commands never crash on a down server.

**Self-update** (`src/selfupdate.rs` for the release check/download, `src/update_transaction.rs`
for the journalled apply/recovery): the release source is GitHub Releases of this repo,
overridable via `STT_NEXT_UPDATE_URL` (used to rehearse against a local/mock server while the
repo is private). A release must publish exactly two assets, under these exact names:
`stt-server-next.exe` and `stt-server-next.exe.sha256` (a `sha256sum`-style text file: the hex
digest, optionally followed by whitespace and a filename). `update install` without `--yes`
downloads and verifies nothing -- it only reports whether a newer version exists and what
installing it would do.

- `update check` fetches the release manifest and reports whether a newer version is available.
  It never downloads anything.
- `update install --yes` runs the update as a **journalled transaction**: every step is recorded
  to `<data dir>\update-journal.json` (`Phase`: `Prepared` -> `Stopping` -> `Snapshot` ->
  `Replacing` -> `Validating` -> `Committed`, or `RollingBack` -> `Restored` on any failure) before
  it happens, so an interruption at any point -- crash, power loss, a killed process -- leaves
  enough on disk to finish the job correctly rather than guessing. The sequence:
  1. **Check and download.** Re-checks the release, downloads the executable and checksum assets
     into a protected per-update work folder beside the executable (`.stt-update-<uuid>`, ACL'd to
     the installing user or `SYSTEM` for a service install), and verifies the executable's SHA-256
     against the checksum asset before anything existing is touched. A preflight also confirms
     enough free disk space beside the executable and beside the data folder for the swap and the
     database snapshot.
  2. **Stop.** Stops the running server the same way `stop` does (graceful, authenticated
     shutdown, or the Windows Service `stop` command for a service install).
  3. **Snapshot the database.** `state.db` is checkpointed and copied to the work folder
     (`state.snapshot`) before the executable is touched, so a rollback can restore the exact
     pre-update database contents, hash-verified on the way back in.
  4. **Replace the executable.** The current executable is moved aside (`retired.exe`) and the
     verified candidate takes its place, both steps hash-checked.
  5. **Restart and validate.** The new executable is started with the *same* launch settings
     (host, port, network mode, CORS origins, data dir, service vs. foreground) recorded at
     prepare time -- an update never changes how the server is exposed. It must then report its
     expected version, an API level at least as high as required, the same network mode, and (if a
     model was loaded before the update) that model ready again, within a bounded readiness
     timeout.
  6. **Commit or roll back.** If validation succeeds, the transaction is marked `Committed` and the
     server is left running the new version (stopped again afterwards if it wasn't running before
     the update). If any step from Stop onward fails -- download/verify failure never gets this
     far -- the previous executable and database snapshot are restored, the previous version is
     started back up and validated the same way, and the transaction is marked `Restored`; the CLI
     reports the update as rolled back, not failed silently.
  - **Recovery after interruption.** A registered, one-shot Scheduled Task (a protected copy of
    this same executable, not a separate program) runs at the next boot/logon if the process
    doing the update is killed or the machine loses power mid-transaction. It always rolls back
    any transaction it finds not yet `Committed`/`Restored` -- it never assumes an interrupted
    update succeeded. Normal server startup refuses to run while a non-terminal, armed journal
    exists ("Update recovery is pending"), so nothing can start against half-updated state; only
    the exact recovery worker (or the exact update-transaction server instance mid-validation) may
    proceed. The task is removed once recovery finishes either way.
- **Models are never touched.** Models, settings, the auth token, and operation history live in
  `<data dir>` and are untouched by any of this; only the executable file itself is replaced, and
  the database snapshot/restore exists purely to protect against a schema migration by the new
  version that a rollback would otherwise leave in place.
- An older executable refuses to open a database with a newer `PRAGMA user_version` (see
  `src/store.rs::migrate`) with a clear error instead of silently reading it, so a rolled-back
  older binary can never misinterpret state a newer version already migrated -- the database
  snapshot/restore above is what makes the rollback path avoid this case in the first place.

## Local build

Requires Rust MSVC, Visual Studio C++ Build Tools, CMake, and the Vulkan SDK. Build the service
with `./scripts/build-local.ps1 -Offline` after dependencies are cached (omit `-Offline` to
allow a normal dependency fetch). The script sets a short Cargo target path, finds the Vulkan
SDK library, and makes both Rust and CMake use the static C++ runtime. See the feasibility
document for the import audit and test evidence.
The `transcribe-cpp` dependency is pinned to 0.2.3 with `vulkan` enabled and without
`dynamic-backends` or `shared`, so the native library should link into the executable.

The separate developer proof executable accepts an existing, trusted model and 16 kHz mono WAV:

```powershell
cargo run --release --bin stt-proof -- "C:\path\to\model.gguf" "C:\path\to\sample.wav"
cargo run --release --bin stt-proof -- "C:\path\to\model.gguf" "C:\path\to\sample.wav" --cpu
```

Set `STT_NEXT_DATA_DIR` to a test directory and run `stt-server-next.exe start` (or plain `run`
for the foreground default) for a local instance on `127.0.0.1:54321`; see "CLI" above for the
full command surface.

### Install scope and data folder

Every install has exactly one data folder, and every mode of that install -- `run`/`start`/
`stop`/`status`/`models`/`update`, `autostart`, and the Windows Service -- resolves to it, so a
model is never stored twice. `--data-dir`/`STT_NEXT_DATA_DIR` always overrides this resolution.

- **Per-user (default, no admin):** data under `%LOCALAPPDATA%\OpenVibeAI\STT Server`. Use
  `autostart enable` for "start with Windows".
- **Machine-wide:** program under `%ProgramFiles%\OpenVibeAI\STT Server`, data under
  `%ProgramData%\OpenVibeAI\STT Server`, shared by every user of the machine. Only a machine-wide
  install offers the Windows Service.

Scope is decided from where the running executable lives, not from how it was launched: a process
running from the machine-wide program folder (or carrying the `.machine-wide-install` marker
`service install` writes there) is machine-wide; everything else is per-user. `service install`
(alias: `install`) refuses with a clear error unless run from an elevated (Administrator) prompt,
since that is what setting up a machine-wide install requires; `service uninstall` (alias:
`uninstall`) removes the service and executable but preserves data. The drop-in models folder
(below) is likewise inside that single data folder for either scope: per-user under
`%LOCALAPPDATA%\...`, machine-wide under `%ProgramData%\...`, rather than tied to whichever user
happened to install the service.

An install that predates this unification is migrated forward automatically and non-destructively:
if the old folder name (`...\STT Server Next`) is the only one present, it is moved (not copied) to
the new name the first time this version opens it; if a rename isn't possible (e.g. a file inside
is open), the old folder is used as-is rather than losing data or starting a second, empty one.

The server attempts Vulkan, falls back to CPU if it cannot load, and reports the
observed backend and reason. The package contains no separate inference DLL, although the
machine's Vulkan loader/driver remains a dependency. The audited static-CRT build does not
import `MSVCP140.dll` or `VCRUNTIME140.dll`.

Local tests have covered first start without a download, explicit verified download/import,
CPU and Vulkan transcription, forced fallback, authentication, service restart, and uninstall.
Other audio containers, model families, fault injection, fresh-machine portability, upgrade and
rollback, full dictation parity, and the candidate/release rehearsal remain open. See
`docs/parity-ledger.md` and `docs/service-recovery-result.json`. No replacement verdict has
been made.

## Reference and provenance

Handy's MIT-licensed catalog at local commit
`8f9cf53cd1410cda26beea39ff802ac306e39585` is the initial recommendation metadata
source. Its display speed/accuracy scores are editorial transforms of model-card results;
they are not local Voice Typer benchmark results. A cached Parakeet Q8 GGUF at the model's
immutable revision was checked against the Handy catalog SHA-256 during the feasibility setup.
The `transcribe-cpp`/`transcribe-cpp-sys` 0.2.3 crates and bundled ggml sources retain their
respective upstream attribution and licences in their package sources.

The four Voice Typer dictation clips in the nested server's completed Parakeet benchmark goal
form a seed corpus. Broader language, long-dictation, and technical-vocabulary evidence is
required before any replacement verdict.

## Startup and model-file recovery

Startup checks installed model size and modification time against the fingerprint recorded
after a successful install or verification; it does not hash every managed model on each
launch. Changed, missing or temporarily inaccessible managed files stay registered with
their saved selection, but are marked as needing verification and are not automatically
loaded. Existing installations without a recorded modification time need one explicit
verification to establish that fingerprint. No model download is required for this step.
The selected model's normal loading cost still applies; this change removes catalog-wide
hashing, not inference-engine initialization.

Explicit verification distinguishes an unreadable or concurrently changing file from a
confirmed size/hash mismatch. Read failures preserve the file and registration and report
a retryable failure. A successful retry records the fingerprint and clears the verification
flag. Confirmed corruption retains the existing rejection/quarantine policy for managed
files; user-folder files are never moved or deleted.

Folder refresh continues past individual unreadable files, listing their reasons and
retryable: true in the existing unsupported result list. Existing registrations are
preserved and flagged for verification on read errors. A later refresh retries flagged
files even if their size and timestamp are unchanged. An unreadable folder itself fails
the operation instead of reporting a successful empty scan.

The CLI stop command uses the data-folder lock and authenticated shutdown. It never
force-kills a saved PID. Stale discovery is cleared only while holding that lock. If
shutdown cannot be requested or confirmed, stop returns failure and leaves processes alone;
retry after the server finishes starting or its outstanding work completes.
