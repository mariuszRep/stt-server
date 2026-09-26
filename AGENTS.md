# AGENTS.md — STT Server Next

This repository owns `stt-server-next`: a single Windows executable that manages GGUF speech
models and serves batch transcription through an OpenAI-compatible API.

## Read Order

1. `VISION.md` — approved product intent; do not edit without explicit human instruction
2. `CONVENTIONS.md` — architecture and behaviour rules
3. This file
4. The relevant goal: `.projectflows/goals/<status>/<slug>/GOAL.md` in this repository, and
   for cross-repo context the workspace goals in
   `D:\Users\mariu\Projects\voice-typer\.projectflows\goals\in_progress\`
   (`build-stt-server-next`, `migrate-voice-typer-to-stt-server-next`)
5. `README.md`, `docs/client-contract.md`, then the relevant source

## Repository

- Standalone repo at `D:\Users\mariu\Projects\stt-server-next`, remote
  `mariuszRep/stt-server-next` (private now, to become public open source).
- Default branch `main`; `codex/prototype` is kept identical by fast-forward. Do not create
  other branches, force-push, rebase, or amend.
- Not a gitlink of the `voice-typer` workspace. Changes here never alter the shipping
  `stt-server`, `stt-sdk`, or `whisper-vibes` repositories or their pins.

## Boundaries

| Owns | Must not own |
|---|---|
| Model catalog, download, import, drop-in refresh, verification, selection, loading, removal; batch transcription and translation; capability matrix; queue; CLI and process modes; auth, CORS, LAN guard; system information; SQLite state | Microphone capture, chunking, VAD, the dictation session, prompt or vocabulary construction, transcript editing (all client-side); provider processes or descriptors; hardware-based model ranking |

## Layout

- `src/bin/server.rs` — thin entry point; `src/cli.rs` commands and flags
- `src/api.rs` router and handlers; `src/app.rs` state and startup reconciliation
- `src/catalog.rs`, `src/capabilities.rs`, `src/run_plan.rs`, `src/format.rs` — models,
  capabilities, request planning, responses
- `src/download.rs`, `src/import.rs`, `src/verify.rs`, `src/dropin.rs`, `src/gguf_probe.rs`
- `src/queue.rs`, `src/engine.rs` — inference queue and model load/swap
- `src/store.rs` migrations; `src/discovery.rs`, `src/autostart.rs`, `src/service.rs`,
  `src/sysinfo.rs`
- `catalog/` — Handy catalog copy (byte-identical, with `SOURCE.md`)
- `scripts/build-local.ps1` (static release build), `scripts/bench_corpus.py` (corpus bench)

## Build and Verify

PowerShell environment (as `scripts/build-local.ps1` sets it):

```powershell
$env:PATH="C:\Program Files\CMake\bin;$env:PATH"
$env:VULKAN_SDK='C:\VulkanSDK\1.4.357.0'; $env:LIB="$env:VULKAN_SDK\Lib;$env:LIB"
$env:TRANSCRIBE_CMAKE_ARGS='-DGGML_NATIVE=OFF -DCMAKE_MSVC_RUNTIME_LIBRARY=MultiThreaded'
$env:RUSTFLAGS='-C target-feature=+crt-static'
$env:CARGO_TARGET_DIR='D:\Users\mariu\Projects\stt-server-next\s'
```

Required before any commit:

```powershell
cargo fmt --check
cargo clippy --release --all-targets --offline -- -D warnings
cargo test --release --offline
scripts\build-local.ps1 -Offline   # prints binary size and SHA-256
```

- Real-model checks use a temporary `--data-dir` and a spare port (54400+), never the user's
  real data folders. Stop every server you start.
- On Windows, tests must drop the app and router before deleting temp dirs (file locks).
- Run long commands with explicit timeouts and log output to a file; read the tail.
- Audit native imports after dependency changes with `dumpbin /DEPENDENTS` (Visual Studio
  Build Tools): only Windows system DLLs and `vulkan-1.dll` are allowed.

## Rules

- Commit messages are conventional; record evidence (tests, real checks, binary size and
  SHA-256) in `docs/feasibility-2026-09-25.md` and `docs/parity-ledger.md`.
- Never commit models, audio, transcripts, databases, tokens, or anything under `s/` or
  `test-data*`.
- Do not implement destructive data purge without explicit approval of that exact scope.
