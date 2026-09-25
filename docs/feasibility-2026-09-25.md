# Single-executable feasibility evidence — 2026-09-25

Build: Windows x64, Rust 1.98.1, Visual Studio Build Tools 2022, CMake 4.4.3,
Vulkan SDK 1.4.357.0. `transcribe-cpp`/`transcribe-cpp-sys` 0.2.3 are pinned by
`Cargo.lock`; `vulkan` is enabled and `shared`/`dynamic-backends` are absent.
`GGML_NATIVE=OFF` was used for a portable x64 CPU baseline. A short Cargo target path
and `LIB` pointing to the Vulkan SDK were required on this machine.

Test model: Handy Parakeet Unified EN 0.6B Q8 at immutable revision
`7e948f21b7bdbac698d3318db9d350f1096f3b6c`. Local SHA-256:
`4B50B6DD862BF6E346929AAF4F5EAACEC003BFA3F56462D6C874B41EF2F38795`,
matching Handy's catalog. Audio: existing nested `stt-server` 16 kHz mono WAV fixture.

| Test | Observed result |
|---|---|
| Preferred backend on Intel Iris Xe | `observed_backend=Vulkan0`; produced transcript |
| Explicit `--cpu` | `observed_backend=CPU`; produced the same transcript |
| Invalid `VK_ICD_FILENAMES` and `VK_DRIVER_FILES` | `observed_backend=CPU`, `fallback_reason=Vulkan backend unavailable`; produced the same transcript |

Transcript: “Well, I don't wish to see it any more, observed Phoebe, turning away her eyes it is certainly very like the old portrait”.

Release executable size: 57,476,608 bytes. SHA-256:
`E9C6EC3D48C7808B8190A48D3FAAE5770D90C0144E83963AA4933E75169F03E5`.
`dumpbin /DEPENDENTS` reports Windows API libraries, `vulkan-1.dll`, and MSVC C/C++
runtime libraries; it reports no separate transcribe/ggml/backend inference DLL.
No DLL is staged alongside the executable. Fresh-machine MSVC runtime availability,
older CPU instruction compatibility, Vulkan fallback on other GPU/driver combinations,
and model-family breadth remain to be verified.

Static-CRT correction: a first `RUSTFLAGS=-C target-feature=+crt-static` attempt failed at
link because the CMake native build retained mixed CRT defaults. Setting
`TRANSCRIBE_CMAKE_ARGS='-DGGML_NATIVE=OFF -DCMAKE_MSVC_RUNTIME_LIBRARY=MultiThreaded'`
and rebuilding `transcribe-cpp-sys` produced a 64,034,304-byte server executable with SHA-256
`D4C6E65DB22FAE1EA2611E9AACD0E7954E1ED96EBEA0D91E21C2106C94A2D3D9`.
`dumpbin /DEPENDENTS` shows only Windows system DLLs and `vulkan-1.dll`: no MSVC C++ runtime
or inference DLL. Its two unit tests passed. A local HTTP smoke test loaded the existing
verified Parakeet Q8 model on `Vulkan0` and reproduced the same fixture transcript.
Handy already uses `transcribe-cpp` 0.2.3: its x64 Windows configuration enables
`dynamic-backends` plus Vulkan to ship per-ISA backend DLLs, while its Windows ARM
configuration links the CPU backend statically. This server uses the same inference crate
with Vulkan and no `dynamic-backends`, then aligns Rust and CMake on the static CRT to
meet its stricter one-executable packaging requirement.

## Verify-endpoint E2E and stereo transcription — 2026-09-25 (update)

`POST /v1/local/models/{id}/verify` was exercised end to end for the first time, on a fresh
`STT_NEXT_DATA_DIR`, against the Parakeet Q8 fixture: import (HTTP 201, operation completed),
verify on an intact install (HTTP 202, operation `completed`), select (Vulkan0), and transcription
of `test-data/stereo48.wav` (HTTP 200, non-empty transcript matching the known fixture text) all
passed. Failure paths: deselecting then corrupting one byte of the installed copy and restarting
caused restart reconciliation to remove it and quarantine it as `<uuid>-invalid.gguf`; re-importing
and corrupting one byte while the server was running, then calling verify, produced operation state
`failed` with error "Installed model size or SHA-256 mismatch", removed the model from `installed`,
and moved the file to `quarantine/<op>-verify-failed.gguf`. `verify` on a non-installed model
returned HTTP 404 `model_not_installed`; a second concurrent `verify` call while one was
queued/running returned HTTP 409 `operation_conflict`. No bugs were found in the verify path;
`cargo fmt --check`, `cargo clippy --release --all-targets --offline -- -D warnings`, and
`cargo test --release --offline --bin stt-server-next` (6 tests) all passed. Rebuilt release
executable: 65,178,624 bytes, SHA-256
`283163FA48FF1C02D1BE54143FBF0680528FD8CD0CB564C55DE802590ADB9C2E`.

## Every catalog model installable, versioned migration — 2026-09-25 (update)

Removed the hard-coded `parakeet-unified-en-0.6b` admission check from install and import; every
catalog entry is now `installable: true`. `POST /v1/local/models/{id}/install` takes an optional
JSON body `{"quant":"..."}` (validated by a pure `catalog::resolve_quant`, 400 `invalid_quant` on
an unknown value); import accepts an optional multipart `quant` field to preselect the expected
file, otherwise matches the uploaded size+SHA-256 against any of the model's files. State moved
to a versioned SQLite schema keyed on `PRAGMA user_version`: v1 is the original unversioned
schema, v2 adds `installed.quant/filename/size_bytes` (backfilled by SHA-256 match against the
catalog) and `operations.created_at/updated_at/finished_at` (unix ms). A non-empty older DB is
checkpointed and copied to `state.db.bak-v1` before migrating, all in one transaction. Verify,
restart reconciliation, and model views now use the recorded installed quant/size/sha instead of
the catalog's `default_quant`. `capability_matrix` was rewritten to derive every field from
catalog metadata (`model_claim` plus a `not_implemented`/`model_lacks` reason) instead of a
slug-keyed hard-code; `streaming` stays always-unsupported.

Gates: `cargo fmt --check`, `cargo clippy --release --all-targets --offline -- -D warnings`, and
`cargo test --release --offline` (14 tests, all lib-crate) all passed. `tower 0.5.3` (pinned to
match the version axum 0.8.9 already resolves) was added as the only new dependency, as a
dev-dependency only, via one online `cargo update -p tower --precise 0.5.3`. Release build via
`scripts/build-local.ps1 -Offline`: **65,394,688 bytes**, SHA-256
`6B5F7FE3B02C42DD7E1047B5BD577FE0B94875901C7A8AF95BE6F97B31F8E73E`.

Real E2E on a fresh `STT_NEXT_DATA_DIR`:
1. Imported the Parakeet fixture (multipart `model`/`file`) — HTTP 201. `/v1/local/models` showed
   `installed_quant: "Q8_0"`. Selected (Vulkan0) and transcribed `stereo48.wav` with
   `model=default` — HTTP 200, text starting "Well, I don't wish to see it any more, observed
   Phoebe, ...", matching the known fixture transcript.
2. Installed `whisper-tiny.en` at the non-default quant `Q4_K_M` (43,545,248 bytes) over the
   network: `POST .../install {"quant":"Q4_K_M"}` returned HTTP 202; polling
   `GET /v1/local/operations/{id}` showed `state` progressing `running` → `completed` with
   populated `created_at`/`updated_at`/`finished_at`. Selected it (backend `Vulkan0`, no
   fallback) and transcribed the same WAV: HTTP 200, text "Well, I don't wish to see it anymore,
   observe Phoebe, turning away her eyes. It is certainly very like the old portrait." — a
   plausible small-Whisper transcript of the same English clip, no load or inference errors.
   `/v1/local/models` confirmed `installed_quant: "Q4_K_M"` against `default_quant: "Q8_0"`.
3. Migration on a real old DB: built a throwaway helper binary (not committed) that wrote a
   `state.db` with the original unversioned `CREATE TABLE` statements (no `quant`/`filename`/
   `size_bytes`/timestamp columns) plus one `installed` row pointing at a copy of the Parakeet
   fixture and a `selected_model` setting, in a fresh data dir. Starting the new
   `stt-server-next.exe` against that dir produced `state.db.bak-v1` (pre-migration backup),
   backfilled the installed row to `installed_quant: "Q8_0"` (matched by SHA-256), and reloaded
   the previously selected model on `Vulkan0` — `GET /v1/local/models` and
   `GET /v1/local/models/selected` both confirmed it stayed installed and loaded after restart.

No bugs were found in the install/import/verify/migration paths exercised. Deviations: none from
the task scope; download hardening, queue, CORS, and language/prompt controls were intentionally
left out as separate later tasks. All temp data directories created for this run were deleted
after the checks above.

## Download hardening and CORS — 2026-09-25 (update)

Added: stall timeout (60s, connect and per-chunk, via `tokio::time::timeout`; replaces the old
3600s blanket client timeout so a 48 GB download is never killed for merely taking a long time);
resume edge cases (a `.part` already at the expected size skips the network and verifies
directly; HTTP 416 discards the partial and restarts once; a 206 whose `Content-Range` doesn't
start at our offset is rejected); a mirror fallback list (HuggingFace, then each catalog
`mirrors` entry in order — the catalog's top-level `mirrors: ["https://blob.handy.computer"]`,
previously undeserialized, is now read into `Catalog.mirrors` and `App.mirrors`); per-source
retries (3 attempts, 2s/5s/15s backoff, interruptible every 200ms); a disk-space preflight
(`fs4::available_space`, requiring `(remaining bytes) * 1.05 + 64 MiB` free, failing with
`insufficient_disk_space`); progress writes throttled to 250ms/4 MiB; a new `operations.error_code`
column (schema v3) surfaced on `GET /v1/local/operations/{id}`.

**Mirror URL shape — unconfirmed assumption**: the catalog publishes mirror *hosts* only
(`https://blob.handy.computer`), no path template, and no second real mirror was available to
test against. `candidate_urls` guesses `{mirror}/{id}/{revision}/{filename}`, mirroring the
HuggingFace resolve path. This is documented in code and here; it should be verified against a
real mirror response before being relied on.

New dependencies: `fs4 =1.1.0` (disk free-space query, `sync` feature only, no libc) and
`tower-http =0.7.1` (feature `cors`, compatible with the pinned `axum =0.8.9`). Both are pure
Rust; the DLL-dependents audit below confirms no new non-system DLL imports.

Adapted from Handy's `src-tauri/src/managers/model/download.rs` (commit `8f9cf53`, MIT — see
`THIRD_PARTY_NOTICES.md`): the `DOWNLOAD_STALL_TIMEOUT` value and rationale, the
`Content-Range` start parser, and the shape of the resume/416/oversized-partial decision logic.
Retries-with-backoff and the disk preflight are new (Handy has neither, per the earlier parity
research).

CORS: `tower_http::cors::CorsLayer` built at router construction from a new
`cors_allowed_origins` setting (JSON array, default `["*"]`), validated as `*` or
`http(s)://host[:port]`. `preferred_backend` was made independently optional on
`PATCH /v1/local/config` (still `deny_unknown_fields`) so either field can be patched alone.

Gates: `cargo fmt --check`, `cargo clippy --release --all-targets --offline -- -D warnings`, and
`cargo test --release --offline` (45 lib unit/integration tests, plus empty bin/doc test
harnesses) all passed. New unit/integration coverage: pure functions (candidate URL list with/without mirrors,
retry schedule, disk math, resume/416/range-mismatch decisions, progress-throttle decision) and
an in-process axum test server exercising happy-path download+hash, a stall (server sends a
prefix then hangs, caught within a shortened injectable timeout), resume via Range from a
pre-written partial, first-source-failure-then-mirror-success, and hash-mismatch quarantine.
A v2→v3 migration test confirms `error_code` is added and `state.db.bak-v2` is written.

Build via `scripts/build-local.ps1 -Offline`: **65,529,344 bytes**, SHA-256
`ED5236F2CD0D3DE4A2C105ABE08DFDE4824C346646F0C236953C902BE3137415`. `dumpbin /DEPENDENTS` shows
only Windows system DLLs (`ws2_32.dll`, `advapi32.dll`, `ntdll.dll`, `bcrypt.dll`,
`bcryptprimitives.dll`, `kernel32.dll`, `api-ms-win-core-synch-l1-2-0.dll`) plus `vulkan-1.dll` —
no new non-system DLL import from `fs4` or `tower-http`.

Real E2E on a fresh `STT_NEXT_DATA_DIR`:
1. `GET /v1/local/config` showed the new default `"cors_allowed_origins":["*"]`.
2. Installed `whisper-tiny.en` quant `Q4_K_M` (43,545,248 bytes); cancelled a second attempt via
   `POST .../operations/{id}/cancel` before any bytes were written — HTTP 200,
   `{"state":"cancelled","error_code":"cancelled"}`; `GET .../operations/{id}` confirmed
   `state: "cancelled"`, `error_code: "cancelled"`. The `.part` file was 0 bytes at cancel time (a
   local/CDN-fast 43 MB transfer on this link completes inside the 250ms progress-flush interval,
   so an intermediate non-zero `progress_bytes` was not observable over the network in this run —
   resume from a non-zero partial is instead exercised directly by
   `download::http_tests::resumes_via_range_after_partial_write`). Re-running install completed
   normally (HTTP 202 → `state: "completed"`, 43,545,248 bytes). Selected the model
   (`observed_backend: "Vulkan0"`) and transcribed `test-data/stereo48.wav`: HTTP 200, text
   "Well, I don't wish to see it anymore, observe Phoebe, turning away her eyes. It is certainly
   very like the old portrait."
3. CORS: `curl -i -X OPTIONS .../v1/audio/transcriptions` with `Origin: http://tauri.localhost`,
   `Access-Control-Request-Method: POST`, `Access-Control-Request-Headers: authorization` → HTTP
   200 with `access-control-allow-origin: *`, `access-control-allow-methods: *`,
   `access-control-allow-headers: authorization,content-type` (no token sent or required). A plain
   `GET /v1/local/config` with `Origin` and a valid token → HTTP 200 with
   `access-control-allow-origin: *`. `PATCH /v1/local/config {"cors_allowed_origins":["not-a-url"]}`
   → HTTP 400; with `["http://tauri.localhost"]` → HTTP 200,
   `{"cors_allowed_origins":["http://tauri.localhost"],"restart_required":true}`.
4. Disk preflight: not exercised live (hard to fake a real low-disk condition safely); covered by
   `download::pure_tests::disk_requirement_*` and `has_enough_disk_space_true_and_false`.

Deviations/gaps: the mirror URL path template is an unconfirmed assumption (see above); the
disk-full path is unit-tested only; a genuinely slow/throttled link was not available to observe
a non-zero resume offset over real network E2E (covered by unit test instead). The server
process and its temp data directory used for this run were both stopped/deleted afterward.
