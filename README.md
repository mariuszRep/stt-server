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
`GET /readiness`, `GET /v1/models`, and OpenAI-style `POST /v1/audio/transcriptions` for
mono/stereo WAV from 8–192 kHz (16/24-bit PCM or 32-bit float). Audio is converted to 16 kHz
mono before inference. The `/v1/local/*` routes expose fixed recommendations, per-model capability
matrices, installed models, explicit install/import/verification, selection/load/removal, operation progress
and cancellation, and CPU/Vulkan preference. Unsupported optional transcription fields return
`unsupported_capability`. Every catalog model is installable (`installable: true`); admission is
catalog membership plus a resolvable quant/hash, not a hard-coded model ID.

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
to `state.db.bak-v<old>` first when it already had data.

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
