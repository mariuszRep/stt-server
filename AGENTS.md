# AGENTS.md — STT Server

This repository owns `stt-server`: a single Windows executable that manages GGUF speech
models and serves batch transcription through an OpenAI-compatible API.

## Read Order

1. `VISION.md` — approved product intent; do not edit without explicit human instruction
2. `CONVENTIONS.md` — architecture and behaviour rules
3. This file
4. The relevant goal: `.projectflows/goals/<status>/<slug>/GOAL.md` in this repository, and
   for cross-repo context the workspace goals in
   `D:\Users\mariu\Projects\voice-typer\.projectflows\goals\in_progress\`
   (`build-stt-server-next`, `migrate-voice-typer-to-stt-server-next`, `promote-stt-server-next-to-stt-server`)
5. `README.md`, `docs/client-contract.md`, then the relevant source

## Repository

- `mariuszRep/stt-server`, checked out in the workspace as `voice-typer/stt-server` on the
  integration branch `voice-typer-windows` (see the workspace `AGENTS.md` for the sibling-clone
  rules and the cross-repo train).
- This code was developed as `stt-server-next` and took over this repository at 0.3.0. The
  earlier provider-based server is preserved at tag `legacy-provider-final` and branch
  `legacy`; the previous `main` at tag `legacy-main-final`. Its releases (up to v0.2.10) are
  untouched. Do not delete those refs.
- Do not create other branches, force-push, rebase, or amend.

## Boundaries

| Owns | Must not own |
|---|---|
| Model catalog, download, import, drop-in refresh, verification, selection, loading, removal; batch transcription and translation; capability matrix; queue; CLI and process modes; install scope and data folder; auth with admin and user tokens; network modes (local, LAN, Tailscale); CORS; self-update; system information; SQLite state | Microphone capture, chunking, VAD, the dictation session, prompt or vocabulary construction, transcript editing (all client-side); provider processes or descriptors; hardware-based model ranking |

## Layout

- `src/bin/server.rs` — thin entry point; `src/cli.rs` commands and flags
- `src/api.rs` router and handlers; `src/app.rs` state and startup reconciliation
- `src/catalog.rs`, `src/capabilities.rs`, `src/run_plan.rs`, `src/format.rs` — models,
  capabilities, request planning, responses
- `src/download.rs`, `src/import.rs`, `src/verify.rs`, `src/dropin.rs`, `src/gguf_probe.rs`
- `src/queue.rs`, `src/engine.rs` — inference queue and model load/swap
- `src/import_user.rs` — copy a user's models into a machine-wide install
- `src/store.rs` migrations; `src/discovery.rs`, `src/autostart.rs`, `src/service.rs`,
  `src/sysinfo.rs`
- `src/auth.rs` access levels; `src/network.rs` network modes; `src/selfupdate.rs` updates;
  `src/model_cli.rs` CLI model commands over the API
- `tests/` — tests that run the real compiled binary
- `catalog/` — Handy catalog copy (byte-identical, with `SOURCE.md`)
- `scripts/build-local.ps1` (static release build), `scripts/bench_corpus.py` (corpus bench),
  `scripts/catalog_sweep.py` (unattended every-model check, one model on disk at a time)

## Build and Verify

PowerShell environment (as `scripts/build-local.ps1` sets it):

```powershell
$env:PATH="C:\Program Files\CMake\bin;$env:PATH"
$env:VULKAN_SDK='C:\VulkanSDK\1.4.357.0'; $env:LIB="$env:VULKAN_SDK\Lib;$env:LIB"
$env:TRANSCRIBE_CMAKE_ARGS='-DGGML_NATIVE=OFF -DCMAKE_MSVC_RUNTIME_LIBRARY=MultiThreaded'
$env:RUSTFLAGS='-C target-feature=+crt-static'
$env:CARGO_TARGET_DIR='D:\Users\mariu\Projects\voice-typer\stt-server\s'
```

Required before any commit:

```powershell
cargo fmt --check
cargo clippy --release --all-targets --offline -- -D warnings
cargo test --release --offline
scripts\build-local.ps1 -Offline   # prints binary size and SHA-256
```

- Everyday development can use a second target folder (`CARGO_TARGET_DIR=...	`) with plain
  `cargo clippy --all-targets -- -D warnings` and `cargo test`, so a running release or sweep
  exe under `s\` is never locked or overwritten. The release commands above remain the gate
  before tagging a release.
- Real-model checks use a temporary `--data-dir` and a spare port (54400+), never the user's
  real data folders. Stop every server you start (`stt-server stop --data-dir <dir>`).
- Never install the Windows Service or change system settings yourself; give the user the
  commands to run in an admin terminal.
- On Windows, tests must drop the app and router before deleting temp dirs (file locks).
- Run long commands with explicit timeouts and log output to a file; read the tail.
- Audit native imports after dependency changes with `dumpbin /DEPENDENTS` (Visual Studio
  Build Tools): only Windows system DLLs and `vulkan-1.dll` are allowed.

## Build, test, release

Nothing builds or ships on its own: every workflow is `workflow_dispatch`-only. The stages run
from the workspace root (`voice-typer/`), never from this folder:

```text
npm run vt -- server dev                 local gate (scripts/dev-gate.ps1): fmt, clippy, tests
npm run vt -- server uat [--local]       build the candidate; store it as a private draft release
                                         candidate-<sha> with stt-server.exe, .sha256, manifest.json
                                         (--local builds with scripts/build-local.ps1: zero Actions minutes)
human acceptance                         install the candidate and test it by hand
npm run vt -- server prod <ver> [--local]  tag the tested SHA; release.yml (or --local) promotes those exact
                                         files, never rebuilds, then bumps the patch version
```

- The version in `Cargo.toml` must equal the release version when UAT runs; `prod` refuses
  to promote a candidate whose `Cargo.toml` disagrees.
- Release assets are `stt-server.exe`, `stt-server.exe.sha256` and `manifest.json`. The
  self-updater (`update check` / `update install`) reads them from this repository's releases.

## Rules

- Commit messages are conventional; record evidence (tests, real checks, binary size and
  SHA-256) in `docs/feasibility-2026-09-25.md` and `docs/parity-ledger.md`.
- Never commit models, audio, transcripts, databases, tokens, or anything under `s/` or
  `test-data*`.
- Do not implement destructive data purge without explicit approval of that exact scope.
- Architectural choices the goal does not settle are the user's to make: ask, or list them
  as open decisions; do not settle them silently.
