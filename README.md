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

`GET /health` is unauthenticated. Other routes require a bearer token. The server implements
`GET /readiness`, `GET /v1/models`, and OpenAI-style `POST /v1/audio/transcriptions` and
`POST /v1/audio/translations` for mono/stereo WAV from 8–192 kHz (16/24-bit PCM or 32-bit float),
both sharing one pipeline (parse → decode → plan → queue → run → format) and a 40 MiB body limit.
Audio is converted to 16 kHz mono before inference. Accepted multipart fields are `file`, `model`,
`language`, `prompt`, `temperature`, `response_format` (`json`/`text`/`verbose_json`), and repeatable
`timestamp_granularities`/`timestamp_granularities[]`; `prompt` is passed to the engine verbatim
(no server-side composition or trimming) and is only accepted on whisper-family models. An
unresolvable `language` hint is never an error: the server falls back to auto-detection, then
English, then the model's first language (matching Handy), and reports whether the hint was
actually applied via `x_diagnostics.language_hint_applied`/`applied_language`/`language_evidence`.
The `/v1/local/*` routes expose fixed recommendations, per-model capability matrices (a `catalog`
view when unloaded, an `effective` view computed from the live model plus `catalog_mismatch` once
loaded), installed models, explicit install/import/verification, selection/load/removal, operation
progress and cancellation, and CPU/Vulkan preference. Unsupported optional transcription fields
return `unsupported_capability`. Every catalog model is installable (`installable: true`); admission
is catalog membership plus a resolvable quant/hash, not a hard-coded model ID.

`POST /v1/local/models/{id}/install` takes an optional JSON body `{"quant":"Q8_0"}`; an absent or
empty body installs the model's `default_quant`. An unknown quant returns 400 `invalid_quant`;
an already-installed model returns 409 `already_installed`. `POST /v1/local/models/import`
accepts an optional multipart `quant` field before `file` to preselect the expected catalog file;
without it, the uploaded file's size and SHA-256 are matched against any of the model's files.
`GET /v1/local/models` and `GET /v1/local/recommendations` list every catalog file
(`quant`, `size_bytes`, `sha256`) plus `installed_quant` (the recorded quant, or `null`).
`GET /v1/local/operations/{id}` also reports `created_at`, `updated_at`, and `finished_at`
(unix milliseconds). Verify, restart reconciliation, select, and remove all operate on the quant
actually installed, not the catalog's current default.

State is stored in a versioned SQLite schema (`PRAGMA user_version`). Opening an older,
unversioned database migrates it to the current schema in one transaction, backing up the file
to `state.db.bak-v<old>` first when it already had data. Schema v3 adds a machine-readable
`error_code` on operations (`insufficient_disk_space`, `stalled`, `source_unavailable`,
`hash_mismatch`, `cancelled`), returned alongside the free-text `error` by
`GET /v1/local/operations/{id}`. Schema v4 adds `installed.source`
(`catalog_download`/`import`/`user_folder`) plus nullable `custom_name`/`custom_arch`/
`custom_languages`/`custom_claims`/`mtime_ms`/`needs_verification` columns for drop-in models, and
`operations.progress_items`/`total_items`/`result` (a JSON blob) for item-counted, durable
operations such as refresh; existing rows backfill `import` (from a completed `import` operation)
or `catalog_download` (everything else).

## Drop-in models and refresh

A user may copy `.gguf` files directly into a user-writable folder instead of using
install/import. The folder is `%LOCALAPPDATA%\OpenVibeAI\STT Server\models` of the user who is
running the server (Local, not Roaming, so multi-gigabyte files are never synced with a roaming
profile). It is the `user_models_dir` setting: visible on `GET /v1/local/config` (defaulted when
unset, in a normal non-service run, to the path above) and settable via `PATCH /v1/local/config`
with `{"user_models_dir": "<absolute path>"}`; a relative or nonexistent path returns 400
`invalid_user_models_dir`. In service mode there is no default (`LocalSystem` has no useful
`LOCALAPPDATA`), so an unconfigured folder makes refresh fail with `error_code:
"user_models_dir_not_configured"`; `service::install` records the installing user's folder
explicitly (creating it if missing, without touching its ACLs) so a fresh service install already
has one configured. The protected `ProgramData` store still holds downloaded/imported models,
state, and the token; only this folder is user-writable.

`POST /v1/local/models/refresh` is a durable operation (`kind: "refresh"`, 202 + operation ID,
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

The operation's `result` (visible on `GET /v1/local/operations/{id}`) lists `registered`,
`duplicates`, `unsupported`, `removed`, and `changed`.

Startup reconciliation (`app::reconcile_installed`) treats `user_folder` rows differently from
catalog/import rows: it never quarantines or moves them (they are outside the managed store by
design), only checking existence/size/mtime -- a disappeared file is unregistered, and a
changed-on-disk file is flagged `needs_verification` (blocking selection until an explicit refresh
re-hashes it) without itself re-hashing anything.

`GET /v1/local/models` lists custom (non-catalog) installed models alongside catalog entries, with
`source`, `custom: true`, a capability view built from the GGUF header claims
(`evidence: "gguf_header"`), `installable: false` (they can only arrive via refresh), and
`recommended_rank: null`; `GET /v1/models` includes any installed custom model too. Select/load and
transcription work the same regardless of source. `DELETE /v1/local/models/{id}` on a
`user_folder` model unregisters it only -- the file is never deleted (`file_deleted: false` in the
response); catalog/import models keep the previous move-then-delete behavior
(`file_deleted: true`). `POST /v1/local/models/{id}/verify` on a `user_folder` model re-hashes it
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
default binary (`stt-server-next.exe`, not the `service`/`install`/`uninstall` subcommands) also
accepts `--queue-max-waiting <n>`, `--queue-wait-timeout-ms <n>`, and `--inference-timeout-ms <n>`
flags, which override the stored settings for that process only; an unrecognized flag or a
non-positive value prints an error to stderr and exits with code 2.

Multipart parsing, validation, and `decode_wav` all happen before a request joins the queue. If
the client disconnects while queued or running, the handler future is dropped and the queue slot
/ cancellation token are released via RAII. A successful transcription's JSON response gets an
additive `x_diagnostics` object: `queue_wait_ms`, `inference_ms`, `audio_ms` (post-resample
duration at 16 kHz), `model`, `backend`, and `fallback_reason`.

`POST /v1/local/models/{id}/select` (and its `/load` alias) no longer holds the inference slot
while the new model loads: the old model keeps serving in-flight and newly queued transcriptions
off its already-loaded handle while the new model loads on a blocking thread, and only the final
swap of `app.loaded` is briefly exclusive. A failed load leaves the previous selection in place.
Note that both models are briefly resident in memory during the swap window; this is accepted as
a tradeoff for non-blocking switching. `DELETE /v1/local/models/selected` now waits up to 30s for
a running inference to finish before unloading, returning 409 `model_in_use` only if that timeout
elapses. When transcription's readiness check fails (`server_not_ready`), the error includes
`error.details.operation_id` when an install/import/verify operation is currently queued or
running.

## CORS

`GET /v1/local/config` and `PATCH /v1/local/config` expose `cors_allowed_origins` (JSON array,
default `["*"]`; `preferred_backend` is independently optional on PATCH so either can be patched
alone). Each entry must be `*` or an `http(s)://host[:port]` origin; an invalid entry returns 400
`invalid_cors_origins`. A CORS `PATCH` returns `restart_required: true` and takes effect on the
next server start, when the `tower_http::cors::CorsLayer` is built from the persisted setting.
`OPTIONS` preflight requests succeed without a bearer token; every other route (except `/health`)
still requires one. When origins are restricted (not `*`), `Authorization` and `Content-Type` are
explicitly allowed.

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

Set `STT_NEXT_DATA_DIR` to a test directory and run `stt-server-next.exe` for a local instance
on `127.0.0.1:54321`. `install` and `uninstall`
register or remove the Windows Service with elevation. Installation copies the same executable
under `%ProgramFiles%\\OpenVibeAI\\STT Server Next` and keeps state, models, and a protected
token under `%ProgramData%\\OpenVibeAI\\STT Server Next`. Uninstall
preserves data. The server attempts Vulkan, falls back to CPU if it cannot load, and reports the
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
