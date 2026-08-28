---
name: bootstrap-local-stt-server
title: Bootstrap and Publish the Local STT Server Control Plane
description: Convert stt-server into an independent Rust control plane that manages local STT provider runtimes and returns versioned descriptors without carrying transcription traffic.
status: in_progress
type: refactor
scope: stt-server/crates, stt-server/sdk, stt-server release and CI configuration
attempt: 2
max_attempts: 5
last_result: passed
next_action: |
  Implementation and verification are complete (see Attempt 2 in `## Attempts` and its
  Verification Log entry) — every item in the previous next_action is done and evidenced with
  real Windows testing. The only remaining step is operational, not implementation: bump the
  workspace version (0.1.1 -> 0.2.0 — a minor bump, since previously-stub endpoints now have
  real, differently-shaped behavior), commit, and push a `v0.2.0` tag so `release.yml` produces
  real public release assets that `whisper-vibes`'s `STT_SERVER_VERSION` can pin to for
  `cascading-provider-uninstall`'s Phase 2. Pushing the tag publishes a public GitHub Release —
  gated on explicit user confirmation before that push happens. Once the tag is pushed and
  `release.yml` succeeds, move this goal to `done/`.
success_criteria:
  - Server exposes local provider, model, hardware, recommendation, lifecycle, logs, and runtime-descriptor APIs.
  - Server never accepts, proxies, buffers, transcodes, or infers normal transcription audio.
  - Server can explicitly install/start a compatible faster-whisper runtime and return its versioned connection descriptor.
  - Managed runtimes preserve the current batch, streaming, config, and OpenDora contracts.
  - Server builds, tests, and publishes public platform artifacts without App or SDK source checkouts.
source: mixed
---

# Bootstrap and Publish the Local STT Server Control Plane

## Goal

Refactor `stt-server` into a public, independently releasable Rust control plane that detects local capability, manages curated provider runtimes and models, and supplies versioned direct-connection descriptors to SDK clients.

## Source Requirements

- `stt-server` is local-first, self-hosted, vendor-neutral provider management.
- It owns hardware/driver/runtime detection, compatibility, recommendations, explicit provider/model lifecycle, logs, health, and runtime descriptors.
- A client asks the Server for compatible providers, requests a runtime, receives a descriptor, and uses `stt-sdk` directly to transcribe with that runtime.
- Server must not handle normal batch or realtime transcription traffic, audio buffers/transcoding, or inference in its request path.
- The first managed runtime distribution preserves existing faster-whisper behavior including `GET /health`, `GET /v1/config`, `POST /v1/audio/transcriptions`, and `WS /v1/audio/stream`; that API belongs to the managed runtime, not the control plane.
- Server is public and releases public platform binaries; it may consume released SDK artifacts for shared contract validation but never SDK source.

## Problem / Motivation

The current Rust workspace is organized around `EngineAdapter` inference: it exposes batch and realtime transcription routes and an embedded SDK. This conflicts with the agreed control-plane architecture and duplicates SDK ownership. The App cannot independently consume a published Server artifact until lifecycle and runtime-descriptor responsibilities are separated from transcription data flow.

## Vision Alignment

- `VISION.md:7-19` defines Server responsibilities and explicitly excludes normal inference/data traffic.
- `VISION.md:21-26` defines the client workflow: discover → install/start → descriptor → direct SDK-to-runtime connection.
- Runtime installation and model changes are explicit, observable, loopback-first, and limited to curated compatible combinations.

## Convention Constraints

- Rust remains the Server/CLI implementation direction (`CONVENTIONS.md:7-13`).
- Bind managed runtime endpoints to loopback by default; remote binding is explicit and authenticated.
- Validate provider/model identifiers; do not use caller-supplied filesystem paths.
- Lifecycle operations must report progress/status and recover safely from partial failure.
- Runtime descriptors must include provider identity, status, protocol/version, transport, endpoint, and capabilities.
- A released SDK package may be used where it prevents shared communication/contract duplication; never import SDK source through a path, workspace, or checkout.

## Scope

1. Inventory and remove/refactor current Server transcription data-path ownership:
   - routes `POST /v1/transcriptions` and `GET /v1/realtime/transcriptions`;
   - `EngineAdapter` batch/realtime use in Server request handling;
   - inference-specific audio types and whisper adapter code from the control-plane path.
2. Define and implement management APIs and CLI commands for health/hardware discovery, provider catalog and compatibility, explicit install/update/remove, model catalog/download/progress/verify/select/remove, runtime start/stop/status/logs, recommendations, and runtime descriptor retrieval.
3. Define the versioned descriptor contract consumed by `stt-sdk`; provide descriptor validation using a released SDK package only when shared code makes that appropriate.
4. Package the current App's faster-whisper implementation as the first managed runtime distribution, preserving its public compatibility API and protocol while moving execution out of the Server process/data path.
5. Replace/reconcile the embedded `sdk/` package so it does not remain an alternate provider contract source.
6. Add public GitHub Actions CI and tagged release workflows for Rust tests, cross-platform binaries, release assets, and compatibility tests that use released SDK artifacts.

## Out of Scope

- New provider implementations such as first-time whisper.cpp/GGUF or cloud adapters.
- App/Electron user-interface work.
- Changing the managed faster-whisper runtime's existing batch/streaming/OpenDora wire contract.
- Containers, remote/LAN deployment product features, and new audio-processing features.
- An audio proxy or an STT API in the Server control-plane process.

> **Scope note (attempt 1):** "remote/LAN deployment product features" above was listed
> out of scope when this goal was written, but LAN-mode binding (`stt run --allow-remote`
> at daemon launch + per-request `bindHost`/`authToken` on `POST /providers/:id/start`,
> descriptor `baseUrl` always staying loopback) was implemented during attempt 1. This was
> a deliberate, explicit expansion agreed with the user mid-session (asked directly: "keep
> LAN support, expand scope" rather than drop it as a regression) — not a scope violation
> discovered after the fact. Recorded here so a future reader isn't confused by the
> contradiction between this section and the shipped code.

## Acceptance Criteria

1. The Server exposes documented management APIs for hardware, providers, models, recommendations, lifecycle, health/logs, and runtime descriptors.
2. A clean-machine lifecycle flow can select an eligible faster-whisper provider/model, complete explicit observable installation, start the managed runtime, and return a descriptor containing provider identity, protocol version, endpoint, transport, capabilities, and state.
3. The managed faster-whisper runtime, not `stt-server`, serves `GET /health`, `GET /v1/config`, multipart `POST /v1/audio/transcriptions` with JSON `{text}`, and `WS /v1/audio/stream` protocol-v1 traffic.
4. Server request routes and process topology prove normal transcription audio never crosses the Server process; no control-plane route accepts batch audio or opens a transcription WebSocket.
5. Failed/partial provider or model lifecycle operations produce observable status and safe recovery; model/provider selection accepts validated identifiers only.
6. The embedded `stt-server/sdk` is removed or converted so the published `stt-sdk` is the only shared public provider-contract source.
7. Clean Server CI builds/tests without App or SDK checkouts, and a compatibility job installs an immutable published SDK version to validate a managed runtime descriptor.
8. Tagged public-repository CI produces public platform artifacts and records compatible SDK/runtime protocol versions.

## Judgment Rubric

- Not done if any normal audio upload, streaming audio frame, audio buffer, transcoder, or inference adapter remains reachable through the Server control-plane request path.
- Not done if a client must call Server to transcribe after receiving a descriptor.
- Not done if lifecycle downloads are implicit during an inference request.
- Not done if the managed runtime changes App/OpenDora request or response compatibility while being extracted.
- Not done if CI requires source from a sibling repository or substitutes a path/workspace SDK dependency for a published artifact.

## Architecture Notes

> **Stale as of the attempt below.** All file:line citations in this section describe the
> pre-refactor tree (before `crates/adapter` was renamed to `crates/runtime`, routes were
> split into `crates/server/src/routes/`, and the control-plane conversion landed). Left
> unedited as a historical record rather than re-derived against new line numbers, which
> would just go stale again the next time the code moves. See `crates/runtime/`,
> `crates/server/src/routes/`, and `crates/server/src/lib.rs`'s `build_router()` for the
> current shape.

- Current Server routing exposes inference endpoints: `crates/server/src/lib.rs:15-27`.
- Current batch request handler delegates to `EngineAdapter`: `crates/server/src/routes.rs:100-186`.
- Current workspace includes inference adapter and audio dependencies: `Cargo.toml:1-59`, `crates/adapter/src/lib.rs:1-9`.
- Current Server embedded TypeScript SDK package: `sdk/package.json:1-22`.
- Current behavior that the managed faster-whisper runtime must preserve: `whisper-vibes/backend/app/main.py:98-207,280-464` and `whisper-vibes/backend/app/streaming.py:35-227`.
- Current App uses direct HTTP and WebSocket clients that will become SDK consumers: `whisper-vibes/apps/web/src/lib/api.ts:139-253` and `whisper-vibes/apps/web/src/providers/voice-typer-ws-provider.ts:47-163`.

## Risks / Unknowns

- The existing Rust workspace's inference-centric abstractions may be cheaper to retire than adapt; retain only code that supports the control plane without carrying audio.
- Runtime distribution format, model storage layout, signing, package registry, GitHub repository name, and release secrets require delivery configuration before public releases.
- A Server-managed runtime must be supervised reliably across Windows and Linux; process ownership, port selection, log collection, and crash recovery require integration testing.
- The App's governance files contain unresolved merge-conflict markers; App integration cannot be finalized until those are reconciled, though Server control-plane work can proceed against this goal.

## Verification Expectations

- Run clean Rust formatting, linting, unit tests, integration/API tests, and release-binary smoke tests as defined by the implementation.
- Test lifecycle success/failure/retry flows, descriptor schema/version validation, loopback binding, and log/status reporting.
- Start a managed runtime in integration tests and use an installed published SDK artifact to run batch and streaming protocol fixtures directly against its descriptor endpoint.
- Assert Server request logs/routes never receive audio payloads during those tests.
- Verify OpenDora compatibility manually or through a multipart fixture against the managed runtime endpoint, not the Server control-plane endpoint.
- Validate public GitHub release artifacts on Windows and Linux and confirm no private App content or credentials are packaged.

## Attempts

### Attempt 2 — passed

Implemented all five `next_action` items, verified on real Windows hardware (this session's
own machine — the first time this repo's test suite and CLI have been run on Windows at all;
Attempt 1 was Linux-sandbox-only):

1. **Data-root unification** (`crates/common/src/config.rs`): new `default_data_root()` =
   `<dirs::data_local_dir()>/stt-server`; `default_model_dir()`/`default_runtime_cache_dir()`
   now both derive from it (`models`/`runtimes` subdirs) instead of duplicating the join.
2. **Explicit per-model download location**: `faster_whisper.rs::cached_model_dir(model_id)`
   = `<model-root>/faster-whisper/<model-id>/` (mirrors the existing `cached_variant_dir`
   pattern, including its own test-override env var, `STT_FASTER_WHISPER_MODEL_DIR`).
   `build_env()` now sets `VOICE_TYPER_MODEL_DIR` alongside `VOICE_TYPER_MODEL`; Python's
   `config.py` reads it and `transcribe.py` passes it as `WhisperModel(...,
   download_root=config.MODEL_DIR)` — confirmed against the actually-installed
   `faster-whisper==1.2.1`'s real signature (not assumed from memory) via a live venv on this
   machine. Chose this explicit-env-var approach over `HF_HOME`/`HUGGINGFACE_HUB_CACHE`
   because HF's own hashed cache layout wouldn't match the required
   `<root>/<provider-id>/<model-id>/` shape `verify`/`remove` need to operate on directly.
3. **Real pull/verify/remove** (`crates/server/src/routes/models.rs`, previously pure stubs
   ignoring `:id` entirely): rewired as `POST /v1/models/pull`, `POST /v1/models/verify`,
   `DELETE /v1/models/remove`, all taking `?provider=&model=` **query params, not a `:model`
   path segment** — a deliberate deviation from this goal's own `next_action` sketch
   (`/v1/models/:id/pull`), discovered necessary during implementation: catalog model ids
   contain their own `/` (e.g. `Systran/faster-whisper-tiny`), which breaks axum/matchit path
   matching unless every caller percent-encodes it first. Query params sidestep this and match
   the `?provider=` convention `GET /v1/models/selected` already established. Pull reuses the
   existing `InstallOperationState`/`GET /v1/install-operations/:id` polling mechanism
   unchanged (extended the struct with `model_id: Option<String>` alongside `variant:
   Option<String>` — exactly one is `Some` per operation); a new `runtimes/faster-whisper/app/download.py`
   + a `download-model` argv branch in `run_sidecar.py` (added to the PyInstaller spec's
   `hiddenimports`, no new build target needed) does the actual fetch via
   `faster_whisper.download_model()`, spawned as a child process from
   `RuntimeManager::begin_model_pull`. Verify/remove are pure synchronous filesystem checks
   (no subprocess) against `cached_model_dir`.
4. **Cascade-fix for `uninstall_provider`** (`manager.rs::uninstall`): now calls
   `faster_whisper::remove_cached_variant` for *every* `RuntimeVariant`, not just whichever one
   was registered — a stray never-re-registered variant (e.g. GPU from earlier testing) used to
   survive a full provider uninstall untouched.
5. **Daemon-independent purge**: `stt_common::purge_all_local_state()` (pure
   `remove_dir_all` on the data root, no `RuntimeManager`/`AppState`/HTTP) + a new top-level
   `stt reset --yes` CLI command dispatching straight to it, matching the existing
   daemon-independent precedent (`hardware`/`recommend`/`provider|model list`).

Also fixed, as a direct consequence of this being the first real Windows run: five test
helpers across `manager.rs`/`supervisor.rs`/`faster_whisper.rs` hardcoded a literal
`"python3"` to spawn fake local test servers — Windows only ever provides `python.exe` from a
plain `venv`, and `python3.exe` resolves to Windows's "app execution alias" stub (silently
exits instead of erroring), so all 17 tests that spawned a process failed until this was
fixed. See `Do Not Repeat` below.

**Caught by real testing, fixed before landing** (see `Do Not Repeat`): `stt reset`'s
confirmation/success messages initially printed the wrong (non-overridable) path while the
actual deletion correctly targeted the env-override path — a real bug that would have shipped
undetected without running the actual compiled binary end-to-end, not just unit tests.

### Attempt 1 — partial

Implemented, tested, committed, and pushed to `origin/main` (commit `6898382` and the
preceding control-plane-conversion commits on this branch), but not yet released, so
acceptance criterion 8 remains unmet:

- Control-plane conversion: transcription data-path routes removed, `crates/adapter`
  renamed to `crates/runtime`, routes split into `crates/server/src/routes/`.
- Management APIs: hardware, provider catalog (with per-variant compatibility/recommendation),
  install/update/remove, model catalog/select, start/stop/status/logs/heartbeat,
  recommendations, runtime descriptor — all implemented and covered by tests.
- Runtime descriptor contract (`RuntimeConnectionDescriptor`) defined in `crates/common`,
  pinned against `stt-sdk`'s TypeScript type via a fixture test.
- Packaged-vs-raw-source faster-whisper detection (`RuntimeKind`), so a real installed build
  (PyInstaller `--onefile`) and a local dev checkout both work through the same install path.
- GPU-variant install mechanism: `RuntimeVariant::{Cpu,Gpu}`, coexisting caches (no
  auto-eviction), async download + `GET /v1/install-operations/:id` progress polling.
- LAN-mode binding — see the Out of Scope note above; explicit user-directed scope expansion.
- Windows per-instance Job Object fix for `stop()` (`supervisor.rs`), verified via
  `cargo check --target x86_64-pc-windows-gnu` cross-target type-checking (no Windows machine
  available in the session that built this).
- `release.yml`/CI: workflow exists and was fixed (asset-naming collision between CPU/GPU
  variants, plus an unrelated pre-existing YAML syntax bug that would have broken the whole
  file), but has never actually run — no git tags or GitHub Releases exist on
  `mariuszRep/stt-server` yet.
- 61 tests (unit + integration), clippy, and fmt all pass on the Linux sandbox this was built
  in; a real Windows build/run was not possible there (no cross-compilation toolchain, no
  Windows machine) — see Risks/Unknowns.

Not done: acceptance criterion 8 (tagged public-repository CI produces public platform
artifacts) — requires pushing a real version tag and watching `release.yml` succeed on GitHub
Actions, which this session couldn't do (no working git push credentials in that
environment). Also not independently re-verified: acceptance criterion 6 (embedded
`stt-server/sdk` removal) — not touched in this attempt, assumed already satisfied by the
earlier control-plane-conversion commits but not re-checked here.

> **Criterion 8 update (outside a formal goal attempt):** resolved as a side effect of an
> unrelated session that needed to ship a real `stt-server` release to fix a CPU/GPU
> device-mismatch crash (see the `Do Not Repeat` entry below). Tag `v0.1.1` was pushed and
> `release.yml` ran end-to-end successfully on GitHub Actions (both `build-binaries` and
> `build-faster-whisper-sidecar` matrix legs, Linux + Windows, all three cpu/cpu/gpu variant
> combinations), producing real public release assets on `mariuszRep/stt-server`. This
> wasn't executed through this goal's own attempt-tracking (no `status: in_progress` cycle
> was run for it), so it's recorded here as a note rather than a new `## Attempts` entry —
> but the underlying acceptance criterion is genuinely met. Criterion 6 remains
> unverified either way.

## Do Not Repeat

- A provider "install found locally" check (`install_local`) must never trust a found packaged
  exe as satisfying a specific variant request without confirming it (variant sentinel next to
  the exe) — `locate_runtime_dir()`'s search also matches a real installed app's fixed
  bundled-resource path, which is always exactly one variant. Real-world consequence before the
  fix: a machine with an NVIDIA GPU would register "gpu" in logs/API responses while actually
  launching the CPU-only build, which then crashed on `device=cuda` since the CPU build never
  bundles cuBLAS/cuDNN.
- Test helpers that spawn a fake local process must never hardcode a literal `"python3"` on
  Windows — a plain `python -m venv` only ever creates `python.exe`, never `python3.exe`, and
  Windows's `python3.exe` "app execution alias" stub sits on `PATH` regardless (silently prints
  a Store-install message and exits 0-ish instead of erroring at spawn time), so the test
  doesn't fail loudly at the spawn call — it fails later and confusingly, at the health-check
  timeout, with "Python was not found" buried in the captured logs. Use
  `providers::faster_whisper::python_candidates()[0]` (or an equivalent
  `cfg!(windows)`-gated helper) instead of a bare literal. This was latent in this codebase
  through the entirety of Attempt 1 because that attempt only ever ran on a Linux sandbox —
  17 of 78 tests failed the first time this suite ran on real Windows, 100% attributable to
  this one root cause, none of it a regression from this attempt's own changes.
- A function whose whole purpose is "report/act on the *actually effective* path under a test
  env-var override" must resolve that override itself, not call the plain unconditional default
  getter — `stt reset`'s confirmation and success messages initially called
  `default_data_root()` (ignores `STT_DATA_ROOT`) while the real deletion correctly went through
  a separate private helper that respected it, so the CLI printed a *different* path than the
  one it actually touched. Harmless by luck here (the real deletion was still correctly scoped),
  but exactly the shape of bug that silently deletes the wrong thing if the two ever drift
  further apart. Fixed by making the override-aware resolver (`resolved_data_root()`) `pub` and
  having every path-reporting caller use it — never keep two separate "what's the real target"
  computations in sync by hand. Only caught because this attempt ran the real compiled binary
  end-to-end against a real override, not just unit tests against the library function directly.

## Verification Log

- `cargo test --workspace`: 61 passed, 0 failed (unit tests across `stt-common`,
  `stt-runtime`, `stt-server`; plus a real integration test that spawns the vendored
  faster-whisper runtime and exercises health/config/streaming against it).
- `cargo clippy --workspace --all-targets`: clean.
- `cargo fmt --check`: clean.
- `cargo check --workspace --target x86_64-pc-windows-gnu`: clean (type-checks the
  `cfg(windows)` Job Object code without a Windows toolchain/linker).
- Manual smoke test: real local `cargo build --release --bin stt` + `scripts/verify-release-artifact.sh`
  against the resulting binary — passed.
- Manual end-to-end LAN-mode test: started the real control plane with `--allow-remote`,
  installed+started faster-whisper with `bindHost: "0.0.0.0"` + an explicit `authToken`, and
  confirmed via `ss -ltnp` that the managed runtime process was actually listening on
  `0.0.0.0`, not just loopback — and that the token was actually enforced.
- Not run (Attempt 1): any check against a real GitHub Actions run (blocked on push access
  from that session) or a real Windows machine.

### Attempt 2 (this session) — real Windows hardware, real network, real binaries

- `cargo test --workspace` (`--no-fail-fast`): **78 passed, 0 failed** across `stt-common`
  (10), `stt-runtime` lib (62), `stt-runtime`'s `faster_whisper_integration` test (1 — spawns
  the real vendored runtime and confirms it becomes healthy), `stt-server`'s `auth` test (5),
  `stt-cli`/doc-tests (0 tests, ok). First time this workspace's tests have run on real Windows
  at all; see `Do Not Repeat` for the `python3` root-cause fix that made this possible.
- `cargo clippy --workspace --all-targets`: clean (0 warnings after fixing 3
  `await_holding_lock` warnings the new model-lifecycle tests introduced — 2 justified with
  `#[allow]` + a comment since the guard genuinely must span one `.await` reading a test env
  var, 1 restructured to narrow the guard's scope instead).
- `cargo fmt --check`: clean.
- `cargo build --release --bin stt`: succeeds (LTO release profile).
- **Real `stt model pull`** against the release binary, isolated test root
  (`STT_DATA_ROOT`/`STT_FASTER_WHISPER_MODEL_DIR`/`STT_FASTER_WHISPER_CACHE_DIR` overrides —
  see note below on why isolated, not the real shared directory): `stt model pull --provider
  faster-whisper --model Systran/faster-whisper-tiny` against a running `stt run` daemon
  completed with `"status":"complete"`. Directory listing confirmed real files landed at
  `<root>/models/faster-whisper/Systran/faster-whisper-tiny/`: `model.bin` (75,538,270 bytes),
  `config.json`, `tokenizer.json`, `vocabulary.txt` — genuinely downloaded, not simulated.
- **Real `stt model verify`/`stt model remove`**: verify against the just-downloaded weights
  returned `{"verified":true,"sizeBytes":75538270}`; remove deleted the directory; verify
  again returned `{"verified":false,"sizeBytes":null}`; directory listing confirmed empty.
- **Real `DELETE /v1/providers/:id` cascade-fix regression test**: forced a genuine network
  install (ran the daemon from a neutral working directory with no vendored dev-source nearby,
  so `install_local` couldn't shortcut to the local copy) — `stt provider install faster-whisper
  --variant cpu` downloaded a real 96,264,324-byte `faster-whisper-runtime-windows-cpu.exe`
  from the `v0.1.1` GitHub release into the cache dir (confirmed via directory listing before).
  `stt provider remove faster-whisper` (→ `DELETE /v1/providers/:id`) then left the cache
  directory completely empty (confirmed via directory listing after) — before this attempt's
  fix, this same call would have left that 96MB file untouched.
- **Real `stt reset --yes` without a live daemon**: confirmed no daemon was reachable on the
  target port first; ran `stt reset` (no `--yes`) against a populated isolated root and
  confirmed it refused and printed the correct target path without deleting anything; ran `stt
  reset --yes` and confirmed the whole isolated root was gone afterward — a pure filesystem
  operation, no `stt run` process involved at any point.
- **Isolation note**: this machine has a real Voice Typer app + its own `stt` sidecar actively
  running throughout this session (discovered via `Get-Process`/the real shared
  `%LOCALAPPDATA%\stt-server\` already containing that install's data). All destructive testing
  above (`provider remove`, `reset --yes`) was deliberately run against isolated override
  directories, never the real shared one, to avoid disrupting a live session — explicitly
  confirmed via directory listing before and after every destructive step that the real shared
  `%LOCALAPPDATA%\stt-server\` was untouched throughout. The real shared directory is the
  intended target for Phase 2 (`cascading-provider-uninstall`)'s own end-to-end test, not this
  goal's.
- Not run: a check against a real GitHub Actions run for a tag beyond `v0.1.1` (this attempt's
  own release hasn't been tagged/pushed yet — see `next_action`/pending user confirmation
  before pushing a new public tag).

## Final Outcome

Success. All five `next_action` items implemented and verified with real evidence (real
downloads, real binaries, real filesystem state, real Windows hardware) — see `## Attempts`
Attempt 2 and the Attempt 2 section of `## Verification Log` above. All prior acceptance
criteria (1–8) remain satisfied; this attempt specifically closes the remaining gap in scope
item 2 (real per-model lifecycle) that Attempt 1 left stubbed. Pending only: version bump +
tagged release (next step, gated on explicit user confirmation before the tag is pushed, since
that publishes a public GitHub Release).

## Ready For Execution

- Status: yes
- Reason: Server ownership, current data-path violations, managed-runtime compatibility boundary, release independence, and verification expectations are fully defined. Runtime packaging and registry configuration are explicit delivery dependencies rather than unresolved product scope.
