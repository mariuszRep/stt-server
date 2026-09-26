# Client discovery and API contract

The authoritative guide for any client (the Whisper Vibes desktop app, the
`stt-sdk` package, or a third party) that talks to `stt-server-next`. It
supersedes the old `stt-server`/faster-whisper/sherpa-onnx provider-lifecycle
contract entirely -- there is one engine, one selected model at a time, and
no provider descriptors. See the bottom of this document for the exact
old-call-to-new-call mapping.

All request/response shapes here are read from this repository's source
(`src/api.rs`, `src/run_plan.rs`, `src/format.rs`, `src/capabilities.rs`,
`src/errors.rs`, `src/discovery.rs`, `src/cli.rs`), not invented.

## 1. Two ways a client finds the server

### 1.1 Default (app-owned) mode

The app spawns the server as its own child process and owns its lifecycle:

```
stt-server-next run --port <p> --data-dir <d>
```

(or `stt-server-next --port <p> --data-dir <d>`, or omit both flags for the
CLI defaults `127.0.0.1:54321` and `crate::app::data_dir()`.)

Steps:

1. Spawn the process with the desired `--port`/`--data-dir` (and optionally
   `--host` for LAN mode, see 3 below).
2. Poll `GET http://<host>:<port>/health` until it returns `200 {"status":
   "ok"}`. `/health` needs no token and is the only route that doesn't.
3. Read the bearer token from `<data-dir>\auth.token` (plain text, written
   once by the server on first start at that data dir; stable across
   restarts at the same data dir).
4. Use the token as `Authorization: Bearer <token>` on every other route.
5. On app quit: either kill the child process, or call
   `POST /v1/local/shutdown` (loopback-only; the server exits its own event
   loop cleanly, unlocks `server.lock`, and removes `server.json`) and wait
   for the process to exit before killing it as a fallback.

This is the process model the desktop app's Tauri sidecar (`lib.rs`, the
`stt run --port` spawn around lines 430-910) already assumes; only the
binary name and CLI surface change (`stt run --port` -> `stt-server-next run
--port <p> --data-dir <d>`).

### 1.2 Standalone mode

The server runs independently (started via `stt-server-next start`, a
per-user autostart entry, or a Windows Service). The app attaches without
starting or stopping it:

1. Read `<data dir>\server.json`:
   ```json
   { "pid": 12345, "host": "127.0.0.1", "port": 54321,
     "version": "0.1.0", "started_at": 1758859200000 }
   ```
   (`started_at` is Unix epoch milliseconds.) This file is written
   atomically at startup and removed on graceful shutdown -- its absence, or
   a `pid` that `tasklist`/`/proc` shows as dead, means "not running."
2. Read `<data dir>\auth.token` the same way as 1.1.
3. Confirm liveness with `GET /health` on `server.json`'s `host`/`port`
   (a dead-but-unremoved `server.json`, e.g. after a crash, fails this
   check -- treat it as not running).
4. Attach: use the token for all further calls. Never send
   `POST /v1/local/shutdown` or kill the PID in this mode.

`stt-server-next status --json` is the CLI-side equivalent of steps 1 and 3,
for a client that can shell out instead of reading the file directly:
```json
{ "running": true, "pid": 12345, "host": "127.0.0.1", "port": 54321,
  "data_dir": "C:\\Users\\...\\STT Server Next", "version": "0.1.0" }
```
(exact key set per `Command::Status`'s JSON in `src/cli.rs`/`src/bin/server.rs`
-- treat `status --json`'s `running: false` the same as a missing/dead
`server.json`.)

## 2. Data directory defaults and the drop-in folder

From `crate::app::data_dir()`:

- **Non-service (per-user) run:** `%LOCALAPPDATA%\STT Server Next`, or
  `%STT_NEXT_DATA_DIR%` if set (test/override hook only -- a real client
  should not rely on this env var).
- **Windows Service run** (`stt-server-next service run`, launched by SCM):
  `%PROGRAMDATA%\OpenVibeAI\STT Server Next` (falls back to
  `C:\ProgramData\...` if `PROGRAMDATA` is unset).

The drop-in models folder (manually copied `.gguf` files, picked up by
`POST /v1/local/models/refresh`) defaults to
`%LOCALAPPDATA%\OpenVibeAI\STT Server\models` for a normal run (note: a
different vendor/product path segment than the data dir above -- this is
intentional, matching `crate::app::default_user_models_dir`) and has no
default at all under a LocalSystem service run; a service install instead
records the installing user's path explicitly as the `user_models_dir`
setting (`PATCH /v1/local/config`). A client that wants to tell the user
where to drop files should read `GET /v1/local/config`'s
`user_models_dir` rather than hardcoding either path.

## 3. Auth, CORS, LAN mode

- **Auth header:** `Authorization: Bearer <token>` from `auth.token`. Every
  route except `GET /health` requires it (`src/auth.rs::authorized`); a
  missing/wrong token is `401 {"error": {"code": "unauthorized", ...}}`.
- **CORS:** `GET /v1/local/config`'s `cors_allowed_origins` (default `["*"]`)
  controls `Access-Control-Allow-Origin`; set via `PATCH /v1/local/config`.
  An origin must be `*` or an exact `http(s)://host[:port]` (no path/query/
  credentials) -- see `store::is_valid_cors_origin`.
- **LAN mode:** binding `--host` to anything non-loopback (not `127.0.0.1`/
  `::1`) requires a non-empty token to exist already; the server refuses to
  start on a non-loopback bind with an empty/missing token
  (`api::run_http_full`'s LAN guard) and prints a warning that every route
  except `/health` now requires the token. A client offering LAN mode should
  surface that same warning and never disable the token for a LAN bind.

## 4. Model-centric flow (replaces provider lifecycle)

There is one engine and at most one loaded model; no provider IDs, variants,
or descriptors.

- **List catalog + installed state:** `GET /v1/local/models` -- catalog
  entries plus any drop-in/custom models, each with `id`, `name`,
  `installed`, `source`, capability info.
- **OpenAI-shaped list:** `GET /v1/models` -- `{"object": "list", "data":
  [{"id", "object": "model", "owned_by": "local"}, ...]}`, covering both
  catalog models and custom (drop-in) ones.
- **Recommendations:** `GET /v1/local/recommendations` -- the curated,
  hardware-independent recommended subset in fixed rank order (`recommended`
  + `recommended_rank` from the catalog; per `CONVENTIONS.md`, hardware
  probing never reorders this list).
- **Install:** `POST /v1/local/models/{id}/install` -> `202 {"operation_id",
  "state": "queued"}` (or `409 already_installed` / `operation_conflict`).
  Poll `GET /v1/local/operations/{id}` until `state` is terminal
  (`completed`/`failed`/`cancelled`), reading `progress_bytes`/`total_bytes`
  for a progress bar; `POST /v1/local/operations/{id}/cancel` aborts it.
- **Verify:** `POST /v1/local/models/{id}/verify` -- same operation-polling
  shape; checks the immutable revision/size/SHA-256 before a model is
  trusted (`CONVENTIONS.md`'s trust rule).
- **Select (load):** `POST /v1/local/models/{id}/select` (alias:
  `.../load`) -> `200 {"model": id, "backend": {"observed_backend",
  "fallback_reason"}}`, or `409 needs_verification` if the model needs
  verification first. Verify a managed model, or refresh a drop-in model, then select again. Loading never triggers a download (`CONVENTIONS.md`:
  "First start and transcription have no download side effects").
- **Deselect / current selection:**
  `GET /v1/local/models/selected` -> the loaded model's `id` plus its full
  effective-capability matrix (see 4.1), or `{"model": null,
  "effective_capabilities": null}` when nothing is loaded.
  `DELETE /v1/local/models/selected` unloads it (`{"model": null, "loaded":
  false}`).
- **Remove:** `DELETE /v1/local/models/{id}` -- removes the managed-store
  file (drop-in/`user_folder` models keep their file on disk:
  `{"removed": true, "file_deleted": false}`).
- **Refresh (drop-in scan):** `POST /v1/local/models/refresh` -> `202
  {"operation_id", "state": "queued"}`, polled the same way as
  install/verify.

### 4.1 Selected-model capability matrix and the "omit unless supported" rule

`GET /v1/local/models/selected`'s `effective_capabilities` (and the
per-model view in `/v1/local/models`) reports a `ControlCapability` object
per optional control (`prompt`, `temperature`, `language_hint`,
`word_timestamps`, `translation`, ...):

```json
{ "status": "supported" | "unsupported" | "unknown",
  "reason": "model_lacks" | null,
  "evidence": "loaded_model",
  "mechanism": "whisper_initial_prompt" | null,
  "extra": { "...": "..." } }
```

**Rule (from `CONVENTIONS.md`: "Advertise a control only after verifying
its model-side effect... Clients omit unsupported or unknown controls
rather than sending defaults"):** before sending `prompt`, `language`,
`temperature`, or `timestamp_granularities`, check this matrix; if a
control's `status` isn't `"supported"`, omit the field entirely rather than
sending a default or empty value. Sending an unsupported optional field is
rejected with `422 unsupported_capability` (see the error table in 6).
`model` itself is never gated this way -- see 5's "missing model" rule.

## 5. Transcription / translation requests

`POST /v1/audio/transcriptions` and `POST /v1/audio/translations`, both
`multipart/form-data`:

| Field | Required | Notes |
|---|---|---|
| `file` | yes | audio bytes |
| `model` | no | **Omit entirely, or send `"default"`, to use whichever model is currently selected.** A request with no `model` field at all is treated as `model=default` -- do not synthesize a value just to satisfy a "required" assumption; the field is genuinely optional now. |
| `language` | no | BCP-47-ish hint; omit unless the selected model's `language_hint` capability is `supported` |
| `prompt` | no | opaque, verbatim text; omit unless `prompt` capability is `supported` |
| `temperature` | no | `0.0..=1.0`; omit unless `temperature` capability is `supported` |
| `response_format` | no | `json` (default) \| `text` \| `verbose_json` |
| `timestamp_granularities` (repeatable) | no | `word` \| `segment`; only meaningful with `verbose_json`; omit `word` unless `word_timestamps` capability is `supported` |

Any other field name is rejected as `422 unsupported_capability` before any
model-aware planning happens; a field sent twice is `400 duplicate_field`.

### Response (`json`/default)

```json
{ "text": "...",
  "language": "en",
  "duration": 12.34,
  "segments": [ { "text", "start", "end", "avg_logprob", "no_speech_prob",
                  "compression_ratio", "words": [ { "word", "start", "end",
                  "probability" } ] } ] }
```
`text` only for `response_format: "text"` (plain body, not JSON). For
`verbose_json`, the same shape as `json` with `segments` (and `words`, if
timestamps allow) always populated.

### `x_diagnostics` (present on every JSON response)

```json
{ "queue_wait_ms", "inference_ms", "audio_ms", "model", "backend",
  "fallback_reason", "language_evidence", "prompt_applied",
  "engine_timings": { "mel_ms", "encode_ms", "decode_ms" },
  "language_hint_applied": true,      // omitted entirely if no hint was sent
  "applied_language": "en",           // omitted if nothing was resolved
  "truncated": true                    // omitted if false
}
```

`language_evidence` is one of:

- `"user_selected"` -- the request's `language` hint matched a language the
  model supports (this now also covers a `translations` request whose
  hint is `en`: transcribing instead of translating is correct because the
  source is already English, but the hint was explicit, so the evidence is
  `user_selected`, not a "translation" that never happened).
- `"model_constrained"` -- no hint (or an unmatched one) and the model isn't
  language-agnostic, so the Handy fallback picked English (or the model's
  first language) for it -- this also covers a single-language `en` model on
  `translations`.
- `"model_detected"` -- no hint, the model auto-detected the language at run
  time.
- `"translated_to_english"` -- reserved for an actual translation run
  (`task: "translate"`); a same-source-language "translation" no longer
  reports this label (fixed 2026-09-26; see `run_plan.rs`'s
  `resolve_language`/`plan`).
- `"unknown"` -- none of the above applied.

## 6. Error envelope and codes

Every non-2xx JSON error:
```json
{ "error": { "code": "unsupported_capability", "message": "...",
             "details": { "...": "..." } } }
```
(`details` omitted when absent.) Codes observed in this codebase, by status:

| Status | Codes |
|---|---|
| 400 | `missing_file`, `duplicate_field`, `invalid_multipart`, `invalid_temperature`, `invalid_bind_host`, `invalid_bind_port`, `invalid_cors_origins`, `invalid_queue_max_waiting`, `invalid_queue_wait_timeout_ms`, `invalid_inference_timeout_ms`, `invalid_user_models_dir`, `invalid_model`, `invalid_quant`, `invalid_backend` |
| 401 | `unauthorized` |
| 404 | `model_not_found` |
| 409 | `already_installed`, `operation_conflict` |
| 413 | `audio_too_long` (payload too large) |
| 422 | `unsupported_capability`, `engine_unsupported`, `engine_rejected_option` |
| 500 | internal errors (message only; not meant to be pattern-matched) |
| 503 | `server_not_ready` (no model loaded), `not_ready`, `engine_busy` |
| 507 | out-of-memory (insufficient storage) |

`missing_model` (400) no longer occurs -- see 5.

## 7. Health-card data sources

A client's health/status card should combine, all authenticated except the
first:

- `GET /health` -- liveness only, `{"status": "ok"}`, no token needed.
- `GET /readiness` -- `200 {"status": "ready", "model": id, "backend":
  {...}}` when a model is loaded, else `503 {"status": "not_ready",
  "reason": "..."}`.
- `GET /v1/local/models/selected` -- which model, and its full capability
  matrix (4.1).
- `GET /v1/local/system` -- hardware: see 8.

## 8. `GET /v1/local/system`

Authenticated. Reports the server's own hardware, since in LAN mode only the
server can see the machine it's running on. Fields are omitted (not
fabricated or reported as `null`) when unobtainable -- always check for a
key's presence before reading it:

```json
{ "os": { "name": "Windows 11", "version": "10.0.26200.7462" },
  "cpu": { "model": "...", "logical_cores": 32, "physical_cores": 16 },
  "memory": { "total_bytes": 68448595968, "available_bytes": 41000000000 },
  "gpu": { "vulkan_available": true,
           "devices": [ { "name": "NVIDIA GeForce RTX 4080", "backend": "vulkan",
                           "memory_bytes": 17179869184 } ] },
  "process": { "pid": 12345, "uptime_ms": 60000, "rss_bytes": 52428800 },
  "server": { "version": "0.1.0", "host": "127.0.0.1", "port": 54321,
              "data_dir": "C:\\Users\\...\\STT Server Next" } }
```

Notes:

- `os.version` is `<CurrentBuildNumber>.<UBR>` from the registry
  (`HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion`); `os.name` is
  "Windows 11" once the build number is >= 22000, else "Windows 10" (or the
  raw `ProductName` if the build number can't be read).
  `cpu.model` is read from
  `HKLM\HARDWARE\DESCRIPTION\System\CentralProcessor\0\ProcessorNameString`.
  `cpu.physical_cores` comes from `GetLogicalProcessorInformation`; either
  can be absent (key missing, permissions) and is then omitted.
- `memory` is `GlobalMemoryStatusEx`'s physical totals -- system RAM, not
  this process's usage (that's `process.rss_bytes`, this process's working
  set from `GetProcessMemoryInfo`).
- `gpu.vulkan_available` is `transcribe_cpp::backend_available(Backend::
  Vulkan)` -- whether the Vulkan backend can actually be used for a model
  load, independent of whether a device enumerated below.
  `gpu.devices` comes from `transcribe_cpp::devices()`, the native
  process-global compute-device registry, filtered to non-CPU entries. In
  this crate's static build that registry is already populated at process
  start, so **device enumeration does not require a loaded model** --
  `gpu.devices` reflects real hardware even before any model has ever been
  selected. (If a future build mode can't enumerate without a model loaded,
  it should add a `gpu.note` field explaining that and fall back to
  reporting only `vulkan_available` plus the loaded model's observed
  device -- no such fallback is needed on this build.)
- `process.uptime_ms` is `now - GetProcessTimes` process creation time (the
  real process start, correct even on the very first call after startup); on
  a platform where that's unavailable it falls back to measuring from the
  first time anything in this process asked for a snapshot.

## 9. Old call -> new call mapping

From `whisper-vibes/apps/web/src/lib/stt-server-client.ts` and
`stt-sdk/src/providers/local-runtime.ts`:

| Old call | New call |
|---|---|
| `getOrStartProviderDescriptor(providerId, opts)` | App-owned mode: spawn `stt-server-next run --port <p> --data-dir <d>` directly (1.1); no descriptor, no per-provider install |
| `installVariant` / `removeProviderVariant` | removed: no provider variants -- see model install/remove (4) instead |
| `cancelOperation` / `pollInstallOperation` (`/v1/install-operations/{id}`) | `POST /v1/local/operations/{id}/cancel` / `GET /v1/local/operations/{id}` (same polling shape, different path, model-scoped not provider-scoped) |
| `getProviders()` (`/v1/providers`) | removed: no provider catalog -- use `GET /v1/local/models` |
| `getModels()` (`/v1/models`) | `GET /v1/models` (kept, OpenAI-shaped) or `GET /v1/local/models` for the richer view |
| `getRecommendations()` (`/v1/recommendations`) | `GET /v1/local/recommendations` |
| `getHardware()` (`/v1/hardware`) | `GET /v1/local/system` (`os`/`cpu`/`memory`/`gpu`; no `hasNvidiaGpu`/`driverVersion` fields -- Vulkan device presence via `gpu.devices`/`gpu.vulkan_available` instead) |
| `getSystemMemory()` (`/v1/system/memory`) | `GET /v1/local/system`'s `memory` (queried live on every call, same as before; no separate VRAM figures) |
| `selectModel(providerId, modelId)` (`/v1/models/select`) | `POST /v1/local/models/{id}/select` |
| `switchModel` | `POST /v1/local/models/{id}/select` (same call now covers both "first load" and "switch"; response shape differs -- see 4) |
| `setModelLanguage` | removed: language is a per-request hint (`language` field), not a load-time model setting -- see 5 and `run_plan.rs`'s Handy fallback |
| `loadModel` | `POST /v1/local/models/{id}/select` |
| `pullModel` | `POST /v1/local/models/{id}/install` |
| `verifyModel` | `POST /v1/local/models/{id}/verify` |
| `removeModel(providerId, modelId)` | `DELETE /v1/local/models/{id}` |
| `stopProvider` / `pinProvider` / `unpinProvider` | removed: no provider lifecycle to stop/pin -- the server itself is stopped via `POST /v1/local/shutdown` (app-owned mode) or left running (standalone mode) |
| `getProviderStatus` (`/v1/providers/{id}/status`) | `GET /readiness` (server-wide, not per-provider) |
| `getProviderDescriptor` | removed: no descriptor object -- see 1's discovery contract |
| `getActiveProviderId` / `waitForDevRuntimeDescriptor` | removed: one server, one base URL from discovery (1) -- no "which provider is active" question |
| `LocalRuntimeProvider.listModels()` (`GET /v1/config`) | `GET /v1/local/models` or `GET /v1/models` |
| `LocalRuntimeProvider.transcribe()` (`POST /v1/audio/transcriptions`, `{ text }`) | same path, same multipart shape, richer response (`language`/`duration`/`segments`/`x_diagnostics`); **always send `model` going forward** even though the server now defaults it (see 5) |
| sherpa-onnx per-runtime quirks (`prompt` dropped, no per-request `language`) | not applicable: one engine (whisper-family via `transcribe-cpp`); use the capability matrix (4.1) instead of hardcoding per-runtime quirks |

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
