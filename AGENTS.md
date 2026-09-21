# AGENTS.md — STT Server

Repository instructions for agents working in this checkout of `stt-server`. This repo exists
on disk both as a standalone clone and as a git worktree nested under `voice-typer/` — if you
arrived here via `voice-typer/stt-server`, read the workspace root's `AGENTS.md` first (its
"Which checkout to use" and "Development workflow" sections) before doing anything cross-repo;
it governs whether you should be touching this checkout at all versus the sibling one.

## Read Order

Before planning or changing files, read:

1. Workspace `../AGENTS.md` (only if this checkout is nested under `voice-typer/` — see note above), then `../VISION.md` and `../CONVENTIONS.md`
2. This repository's `VISION.md` and `CONVENTIONS.md`
3. `AGENTS.md` (this file)
4. Relevant `.projectflows/goals/<status>/<goal-slug>/GOAL.md`
5. Relevant source files

## Architecture Boundary

- This repository is the local control plane: hardware/driver/runtime detection, provider
  catalog and compatibility, provider/model install-update-removal, runtime lifecycle,
  recommendations, health, and runtime connection descriptors.
- It does not proxy, inspect, buffer, or execute normal transcription traffic — audio never
  crosses the control plane (CI enforces this with a WebSocket regression guard).
- Managed provider runtimes live inside this tree at `runtimes/<engine>/`
  (`runtimes/faster-whisper/`, `runtimes/sherpa-onnx/`), each as its own independent build —
  never a member of the root cargo workspace.
- It may consume the published `@open-vibe-ai/stt-sdk` as a versioned library for shared
  provider contracts — never SDK source by repository-relative path.

## Workspace Layout

- `crates/common` (`stt-common`), `crates/runtime` (`stt-runtime`), `crates/server`
  (`stt-server`), `crates/cli` (the `stt` binary) — the root cargo workspace.
- `runtimes/sherpa-onnx/` — separate cargo workspace on purpose (its native ONNX-runtime
  link must not slow every `cargo build --workspace`); build it with
  `cargo build --release --bin sherpad` from inside that directory.
- `runtimes/faster-whisper/` — Python runtime packaged with PyInstaller; its `venv/` is
  dependency output, never edit code inside it.

## Verify Commands

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace
cargo test --workspace        # faster-whisper integration tests need runtimes/faster-whisper/venv
./smoke-test.sh target/release/stt   # after cargo build --release --bin stt
```

`scripts/verify-release-artifact.sh dist` audits staged release binaries — CI runs it on
every built artifact.

## Releasing

- Releases are tag-push only: `git tag vX.Y.Z <tested-sha>` → `git push origin vX.Y.Z`
  on an explicit release instruction only. The tag marks the commit whose
  `candidate-server.yml` artifacts passed verification — the release workflow fetches
  and checksum-verifies those artifacts instead of rebuilding.
- Version bumps (`[workspace.package] version` in the root `Cargo.toml`) are ordinary
  commits on the integration branch before the final candidate run — see the workspace
  `RELEASE_PROCESS.md` for the full PR + candidate → tag-tested-SHA → promote flow.

## Documentation Rule

Durable product or technical direction belongs in `VISION.md` / `CONVENTIONS.md`.
Executable work belongs in `.projectflows/goals/<status>/<goal-slug>/GOAL.md`.
Do not maintain separate roadmap/status documents unless explicitly requested.
