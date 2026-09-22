# AGENTS.md — STT Server

This repo owns the local control plane (`stt` binary): hardware/runtime detection, provider
and model lifecycle, runtime supervision, and connection descriptors — plus the managed
runtimes it ships (`runtimes/<engine>/`).

## Which checkout am I in?

This repo exists TWICE on disk, as two git worktrees sharing one object database:

```
Projects/stt-server              standalone clone      branch: main
Projects/voice-typer/stt-server  linked worktree       branch: voice-typer-windows   ← dev happens here
```

`voice-typer-windows` is the integration branch in ALL THREE repos — same name in
stt-sdk, stt-server and whisper-vibes. There is no per-repo variant such as
"stt-server-windows"; if you are looking for one, it does not exist.

Decide from your working path, never from which branch looks newer or has more commits:
  path contains /voice-typer/  → use this worktree, on the branch already checked out
  path does NOT                → use this clone's own `main`

The two are normally on DIFFERENT commits. Committing to the wrong one makes the work
invisible to the other and is expensive to reconcile — see the 2026-08-30 incident in
this repo's history.

Ignore these stale local branches, they are not part of the flow:
`backup/python-cli-main`
Confirm state before any cross-repo work: `../scripts/check-worktrees.sh`

## Read Order

1. This repository's `VISION.md` and `CONVENTIONS.md`
2. `AGENTS.md` (this file)
3. Relevant `.projectflows/goals/<status>/<goal-slug>/GOAL.md`
4. Relevant source files

The workspace root's `../AGENTS.md` matters only when a change spans repos (e.g. a gitlink
bump or the cross-repo train) — everything below is self-contained for this repo.

## Boundaries

| Owns | Must not own |
|---|---|
| Hardware/driver/runtime detection, provider catalog and compatibility, provider/model install-update-removal, runtime lifecycle, recommendations, health, runtime connection descriptors, managed runtime packaging (`runtimes/`) | Transcription proxying or inference on the data path — audio never crosses the control plane (CI enforces this with a WebSocket regression guard); app UX |

- Managed provider runtimes live at `runtimes/<engine>/` (`faster-whisper/`,
  `sherpa-onnx/`), each an independent build — never a member of the root cargo workspace.
- This repo may consume the published `@open-vibe-ai/stt-sdk` as a versioned library for
  shared provider contracts — never SDK source by repository-relative path.

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

## Build, test, release

```
push to voice-typer-windows ──▶ ci.yml runs on every push (fmt/clippy/build/test/sherpad);
                                a draft PR titled "vX.Y.Z" stays open (ensure-pr.yml
                                opens one if none exists)
merge PR ─────────────────────▶ candidate-server.yml fires on push:main → real binaries
                                + per-artifact SHA256SUMS (workflow_dispatch stays
                                available to re-test any SHA)
human acceptance ─────────────▶ download the run's artifacts, verify against SHA256SUMS,
                                install and smoke-test on a real machine
tag the tested SHA ───────────▶ release.yml fetches that run's artifacts, re-verifies
                                checksums, and publishes those exact files — never rebuilds
```

The candidate produces: `stt-linux-x86_64`, `stt-windows-x86_64.exe`,
`sherpad-linux-cpu`, `sherpad-windows-cpu`, `faster-whisper-runtime-linux-cpu`,
`faster-whisper-runtime-windows-cpu`, and — only when its opt-in dispatch input is set —
`faster-whisper-runtime-windows-gpu` (617MB, off by default).

- Candidates build the current commit as-is — no version-ahead check blocks them, so it's
  fine to build/test the same version repeatedly (most merges have nothing to bump anyway).
  `[workspace.package] version` in the root `Cargo.toml` only needs to be new at the moment
  you actually tag a release.
- Release (explicit instruction only): `git tag vX.Y.Z <tested-sha>` →
  `git push origin vX.Y.Z`. The tag need not sit on `main`. `release.yml` hard-fails when
  no successful candidate run exists for that SHA — re-dispatch `candidate-server.yml` on
  the SHA first if the artifacts expired. Right after a successful release, it auto-bumps
  the next patch version back onto `voice-typer-windows` — rarely something to do by hand.
- **Rollback is free**: re-tag an older already-tested SHA and let promote republish it —
  seconds, no rebuild, no new test cycle.

### CI housekeeping rules

- **GitHub Releases assets do not count against the Actions artifact-storage quota** —
  promoting is how bits get off the meter permanently, which is why candidate artifact
  retention is deliberately short (7 days).
- **Renaming a job or artifact orphans the old artifact's name** — nothing prunes it.
  When an artifact name changes, purge the old name (`gh api -X DELETE
  repos/<owner>/<repo>/actions/artifacts/<id>`); `cleanup-artifacts.yml` does this weekly.

## Documentation Rule

Durable product or technical direction belongs in `VISION.md` / `CONVENTIONS.md`.
Executable work belongs in `.projectflows/goals/<status>/<goal-slug>/GOAL.md`.
Do not maintain separate roadmap/status documents unless explicitly requested.
