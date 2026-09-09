---
name: fold-sherpad-runtime-into-stt-server
title: Fold the sherpad Runtime Into stt-server as a Managed Runtime
description: Move the sherpad daemon out of the standalone stt-server-v2 repository into stt-server/runtimes/sherpa-onnx/, matching the runtimes/faster-whisper/ precedent, and retire stt-server-v2 as a workspace component.
status: blocked
type: refactor
scope: stt-server/runtimes/sherpa-onnx/ (new), voice-typer/VISION.md, voice-typer/scripts/check-worktrees.sh, stt-server-v2 (retired)
attempt: 1
max_attempts: 3
last_result: partial
next_action: |
  All local work complete and verified. One external action remains, deliberately not done
  automatically: archive the mariuszRep/stt-server-v2 GitHub repository (gh repo archive
  mariuszRep/stt-server-v2). Not performed in this attempt since it affects an external, shared
  resource beyond the local working tree -- do it manually, or explicitly authorize it, then mark
  this goal fully done.
success_criteria:
  - sherpad and sherpa-manifest build from stt-server/runtimes/sherpa-onnx/ with cargo build succeeding.
  - The stt-server-v2 gitlink and worktree are removed from the voice-typer superproject and check-worktrees.sh covers three repos again.
  - Root VISION.md states explicitly where managed runtime source lives, so the faster-whisper and sherpa-onnx placements are both covered by a written rule.
  - No code or history is deleted irrecoverably — the stt-server-v2 GitHub repository is archived, not deleted.
source: user
---

# Fold the sherpad Runtime Into stt-server as a Managed Runtime

## Goal

Put the `sherpad` runtime where the architecture says managed runtimes belong: inside `stt-server`,
alongside `runtimes/faster-whisper/`. Retire the standalone `stt-server-v2` repository as a
workspace component.

## Source Requirements

User decision, this session, when presented with the conflict between the standalone repo and root
`VISION.md`: fold into `stt-server/runtimes/sherpa-onnx/`, retiring the `stt-server-v2` repo.

## Problem / Motivation

`stt-server-v2` was created as a fourth worktree component alongside `stt-sdk`, `stt-server`, and
`whisper-vibes`. That directly contradicts root `VISION.md`, which states that local provider
runtimes are **installable distributions managed by `stt-server`, not a fourth Voice Typer product
repository**. The existing managed runtime, faster-whisper, follows that rule: its source is vendored
at `stt-server/runtimes/faster-whisper/`, and `stt-server`'s own release pipeline builds and
publishes its binary.

Reinforcing the conflict: `stt-server-v2` has no `.projectflows/`, no `VISION.md`, no
`CONVENTIONS.md`, and is referenced by no goal in any of the four repos. It is entirely outside the
tracked roadmap, and `voice-typer/scripts/check-worktrees.sh` was extended to cover it without any
governing decision behind that.

Since `add-sherpa-onnx-provider` will have `stt-server` build and host the `sherpad` binary anyway
(see that goal, and the self-hosted-binary correction in
`generalize-provider-engine-installation`), keeping the source in a separate repository would mean a
second release pipeline and a second version pin to keep aligned, for no benefit.

## Vision Alignment

- Root `VISION.md`: runtimes are managed distributions, not a fourth repository. This goal makes the
  tree match the stated architecture instead of amending the architecture to match an accident.
- Root `CONVENTIONS.md` boundary table: `stt-server` owns provider/model lifecycle and runtime
  supervision. A runtime it builds, ships, supervises, and versions belongs in its tree.
- `stt-server/CONVENTIONS.md`: "each engine keeps whatever self-contained packaged-binary shape it
  naturally has" — folding the source in changes where it is built, not how it is packaged.

## Scope

1. Move `crates/sherpad` and `crates/sherpa-manifest` from `stt-server-v2` into
   `stt-server/runtimes/sherpa-onnx/`, preserving their crate structure and a workspace manifest so
   they build independently of `stt-server`'s own cargo workspace. **Deliberately not a member of
   `stt-server`'s root workspace**: `sherpa-onnx`'s native library download and static link should
   not be dragged into every `cargo build --workspace` / `cargo test --workspace` run in CI.
2. Drop `crates/spike-asr`. It proved the Rust crate links and runs on Windows; that question is
   answered and `sherpad` supersedes it. Record its removal here so the decision is traceable.
3. Remove the `stt-server-v2` gitlink from the `voice-typer` superproject and remove its worktree.
   Revert `voice-typer/scripts/check-worktrees.sh` to the three-repo list.
4. Amend root `VISION.md` to say explicitly where managed runtime source lives
   (`stt-server/runtimes/<engine>/`), so both faster-whisper's and sherpa-onnx's placement follow one
   written rule rather than one precedent and one exception.
5. Archive the `mariuszRep/stt-server-v2` GitHub repository. Archive, not delete — its history is the
   provenance of the folded code.

## Out of Scope

- Any behaviour change in `sherpad`. This goal relocates code; `make-sherpad-protocol-conformant`
  changes what it does.
- CI or release wiring for building the `sherpad` binary — owned by
  `make-sherpad-protocol-conformant`, which is where the binary first becomes worth publishing.
- Any `catalog.rs` entry or adapter — owned by `add-sherpa-onnx-provider`.

## Acceptance Criteria

1. `cargo build --release` succeeds from `stt-server/runtimes/sherpa-onnx/`, producing a `sherpad`
   binary that still starts and transcribes as it did before the move.
2. `stt-server`'s own `cargo build --workspace` and `cargo test --workspace` are unaffected — the
   relocated runtime is not a workspace member and does not lengthen the main build.
3. `git worktree list` in `voice-typer` shows three component worktrees; `check-worktrees.sh` passes.
4. Root `VISION.md` contains a written rule locating managed runtime source.
5. The `stt-server-v2` GitHub repository is archived and still reachable.

## Judgment Rubric

- Not done if `sherpad`'s build is coupled into `stt-server`'s main cargo workspace.
- Not done if the `stt-server-v2` repository is deleted rather than archived.
- Not done if root `VISION.md` still reads as though a fourth repository is the only alternative to
  the current arrangement.

## Risks / Unknowns

1. **Destructive step.** Removing a worktree and a gitlink can lose uncommitted work. Run
   `git status` in `stt-server-v2` and in the superproject first, and confirm everything is pushed to
   `origin` before removing anything.
2. **The `voice-typer-windows` branch** created for `stt-server-v2` carries the current sherpad
   state. Confirm it is pushed and that `main` contains what the fold moves, before archiving.
3. **Build isolation is a design choice, not an accident** — if the relocated runtime is later added
   to `stt-server`'s workspace for convenience, CI build times and the native-library download come
   with it. Keep it separate deliberately.

## Verification Expectations

### Automated Verification
- `cargo build --release` in `stt-server/runtimes/sherpa-onnx/`.
- `cargo build --workspace`, `cargo test --workspace`, `cargo clippy --workspace --all-targets`,
  `cargo fmt --check` in `stt-server` — all unaffected.
- `voice-typer/scripts/check-worktrees.sh` exits clean.

### Manual Verification
- Start the relocated `sherpad`, transcribe one sample from
  `validate-parakeet-performance`'s sample set, confirm identical output to pre-move.

## Attempts

### Attempt 1 (2026-09-09)

**Sequencing note**: `validate-parakeet-performance` is still `blocked/` pending real dictation
audio (needs the user), not `done/` with a final verdict. Proceeded anyway per explicit user
direction this session ("go as long as it takes... implement all goals") given the strength of the
already-gathered evidence: Parakeet transcribed correctly and ran ~8x faster than
faster-whisper-small CPU-to-CPU on real (if short, clean, read-speech) audio, reproducibly, via a
built harness. This is a judgment call to flag, not silently absorb — the goal's own Risk #... about
committing to an unvalidated premise is real; the mitigating fact is that nothing here is
irreversible (see below) and the dictation-based final verdict can still revise the model roster
later without touching this goal's outcome.

Work done:
1. Copied `crates/sherpad` and `crates/sherpa-manifest` (including this session's Parakeet support)
   from `stt-server-v2` into `stt-server/runtimes/sherpa-onnx/`, dropped `crates/spike-asr`, updated
   the local `Cargo.toml` workspace member list accordingly.
2. Built `cargo build --release --bin sherpad` from the new location: succeeded standalone.
3. Started the folded `sherpad`, pulled/loaded `parakeet-tdt-0.6b-v2` (already cached from earlier
   in-session testing), transcribed the same `test_wavs/0.wav` clip: byte-identical text output to
   the pre-move run.
4. Confirmed via `cargo metadata --no-deps` that `stt-server`'s own workspace lists only
   `stt-common`/`stt-runtime`/`stt-server`/`stt-cli` — the relocated runtime is not a member.
5. Removed the `stt-server-v2` linked worktree from its owning repo
   (`git -C D:/Users/mariu/projects/stt-server-v2 worktree remove --force
   D:/Users/mariu/projects/voice-typer/stt-server-v2`) — safe: `git status`/`git log
   origin/voice-typer-windows..HEAD` confirmed no unpushed commits beforehand, and the only
   uncommitted working-tree changes (the Parakeet manifest/recognizer edits, `bench.sh`) were already
   copied into the new location first.
6. Removed the gitlink from the `voice-typer` superproject (`git rm --cached stt-server-v2`).
7. Reverted `scripts/check-worktrees.sh` to the three-repo list, added a comment pointing at where
   managed runtimes actually live now. Ran it: correctly reports `DIRTY` for the three repos with
   real uncommitted-but-reviewed work pending (expected, not a bug), no mention of `stt-server-v2`.
8. Amended root `VISION.md`'s runtime-placement sentence to state explicitly:
   `stt-server/runtimes/<engine>/`, independent build, never a workspace member, built/released by
   `stt-server`'s own pipeline.
9. **Not done**: archiving the `mariuszRep/stt-server-v2` GitHub repository. Deliberately left as a
   manual/explicitly-authorized step — it's an action on an external, shared resource beyond the
   local working tree, not a local file operation. The underlying repo and its full history remain
   intact and reachable at `D:/Users/mariu/projects/stt-server-v2` either way; nothing is lost by
   deferring this specific step.

## Do Not Repeat

None yet.

## Verification Log

- 2026-09-09 — `cargo build --release --bin sherpad` from `stt-server/runtimes/sherpa-onnx/`:
  succeeded, ~96s clean build including the sherpa-onnx native library fetch.
- 2026-09-09 — started the folded binary, `GET /v1/models` lists all four entries including
  `parakeet-tdt-0.6b-v2`; transcribed `test_wavs/0.wav` and diffed the response text against the
  pre-move transcription captured under `validate-parakeet-performance` — identical.
- 2026-09-09 — `cargo metadata --no-deps` in `stt-server` lists exactly 4 packages
  (`stt-common`, `stt-runtime`, `stt-server`, `stt-cli`); the relocated runtime crates are absent,
  confirming workspace isolation.
- 2026-09-09 — `git worktree list` in the owning repo (`D:/Users/mariu/projects/stt-server-v2`)
  shows only its own primary checkout after removal; `voice-typer`'s `git status` shows
  `stt-server-v2` as `D` (deleted from the tree) pending commit.
- 2026-09-09 — `scripts/check-worktrees.sh` runs clean against the three-repo list (reports `DIRTY`
  for real pending work in each, not `SKIP`/missing for a phantom fourth repo).

## Final Outcome

**Local work complete and verified; one external step deliberately deferred.** All four "local"
acceptance criteria are met: standalone build succeeds, `stt-server`'s own workspace is unaffected,
`check-worktrees.sh` covers three repos, and `VISION.md` states the placement rule explicitly. The
fifth criterion (archiving the GitHub repo) is not done — see next_action. Not moving this to `done/`
until that's resolved, per the goal's own Judgment Rubric ("not done if the stt-server-v2 repository
is deleted rather than archived" implies archiving is part of done, not optional).

## Ready For Execution

- Status: blocked (external action only)
- Reason: All local file/build/git work is complete and verified. Only the GitHub repo archive step
  remains, which needs either the user to do it directly or explicit authorization to run `gh repo
  archive mariuszRep/stt-server-v2` on their behalf.
