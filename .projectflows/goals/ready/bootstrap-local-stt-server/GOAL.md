---
name: bootstrap-local-stt-server
title: Bootstrap and Publish the Local STT Server Control Plane
description: Convert stt-server into an independent Rust control plane that manages local STT provider runtimes and returns versioned descriptors without carrying transcription traffic.
status: ready
type: refactor
scope: stt-server/crates, stt-server/sdk, stt-server release and CI configuration
attempt: 1
max_attempts: 5
last_result: partial
next_action: Push a version tag, verify release.yml runs end-to-end on GitHub Actions (Linux + Windows) and produces real release assets (stt binaries, CPU/GPU faster-whisper runtimes), then re-evaluate against acceptance criterion 8 for done.
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

## Do Not Repeat

- A provider "install found locally" check (`install_local`) must never trust a found packaged
  exe as satisfying a specific variant request without confirming it (variant sentinel next to
  the exe) — `locate_runtime_dir()`'s search also matches a real installed app's fixed
  bundled-resource path, which is always exactly one variant. Real-world consequence before the
  fix: a machine with an NVIDIA GPU would register "gpu" in logs/API responses while actually
  launching the CPU-only build, which then crashed on `device=cuda` since the CPU build never
  bundles cuBLAS/cuDNN.

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
- Not run: any check against a real GitHub Actions run (blocked on push access from that
  session) or a real Windows machine.

## Final Outcome

Not yet — see `next_action`. Substantially implemented and locally verified; blocked on a
real tagged release to satisfy the last acceptance criterion.

## Ready For Execution

- Status: yes
- Reason: Server ownership, current data-path violations, managed-runtime compatibility boundary, release independence, and verification expectations are fully defined. Runtime packaging and registry configuration are explicit delivery dependencies rather than unresolved product scope.
