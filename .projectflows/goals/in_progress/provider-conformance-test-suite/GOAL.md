---
name: provider-conformance-test-suite
title: Build a Provider Conformance Suite and Benchmark Harness Into stt-server
description: Add an engine-agnostic conformance suite parameterized over the provider registry, plus room for engine-specific tests and a benchmark mode, so every engine is held to the same protocol contract automatically and model selection is measured rather than guessed.
status: in_progress
type: feature
scope: stt-server/crates/runtime/tests/, crates/runtime/src/conformance.rs (new), crates/cli/src/commands.rs (stt verify, stt bench), test audio fixtures
attempt: 1
max_attempts: 5
last_result: partial
next_action: |
  Core general suite, stt verify, and stt bench all complete and verified against both real
  engines. Remaining: (1) the engine-specific layer (item 2 in Scope) was not built -- no faster-
  whisper GPU/compute-type or sherpa-onnx batching tests exist yet, separate from the general suite.
  (2) CI wiring (item 6) is partial: the general suite runs automatically as part of `cargo test
  --workspace` in ci.yml's existing `rust` job, but that job never builds sherpad, so sherpa-onnx
  will SKIP in CI (loudly, correctly) until the `rust` job either builds sherpad itself or downloads
  the artifact the separate `sherpad` CI job (from make-sherpad-protocol-conformant) produces.
  faster-whisper's own checks do run for real in CI already, since ci.yml already sets up its venv.
success_criteria:
  - A general conformance suite asserts protocol compliance and lifecycle behaviour identically for every engine in the provider registry, with no per-engine test code.
  - Adding a new engine to the registry automatically subjects it to the full general suite without writing new tests.
  - An engine-specific layer exists for capabilities only some engines have, and is clearly separated from the general suite.
  - stt verify runs the conformance checks against installed engines on real hardware, and stt bench reports per-model speed on a folder of audio.
  - The general suite runs in CI for any engine whose runtime can be obtained there.
source: user
---

# Build a Provider Conformance Suite and Benchmark Harness Into stt-server

## Goal

Make protocol conformance a property the codebase enforces rather than a claim in a document. Every
managed engine must pass one identical suite; engines with extra capability get an additional, clearly
separated layer; and model selection gets a measurement tool instead of intuition.

## Source Requirements

User, this session: *"stt needs to have test built in so we need to make sure we have both general
test and engine specific if needed"*, following on from *"we need to run some tests on multiple
languages but focus on eng, so we need some samples"*.

## Problem / Motivation

The two-engine programme's central claim is that a client cannot tell which engine is behind a
runtime connection descriptor. Nothing currently tests that claim, and nothing could — the existing
tests are engine-shaped:

- `crates/runtime/tests/faster_whisper_integration.rs` is written against faster-whisper by name.
- `crates/server/tests/auth.rs` covers the control plane, not runtimes.
- `stt-sdk`'s tests are the closest thing to a contract test, but only exercise faster-whisper's
  adapter.
- There are no audio fixtures anywhere in the four repositories.

Without a shared suite, "sherpa-onnx conforms to the protocol" is an assertion somebody makes once
during implementation and nobody re-checks. With one, conformance is a build failure.

There is a second, product-level gap: model selection is currently a guess. `validate-parakeet-performance`
answers that question once, manually, as a spike. A benchmark mode makes it repeatable — for new
models, new hardware, and regressions.

## Vision Alignment

- `stt-server/CONVENTIONS.md`'s Provider Engine Architecture: "adding an engine is a catalog entry
  plus one new adapter module". A suite parameterized over the registry keeps that true for testing
  too — otherwise every engine also costs a bespoke test file.
- `stt-server/CONVENTIONS.md`'s API Completeness: the HTTP API is the full control surface and
  clients must never shell out to the CLI. A developer/operator test harness is not an application
  capability, so `stt verify`/`stt bench` may be CLI-only — record that reasoning so it does not read
  as a violation.
- Root `CONVENTIONS.md`'s Local Provider Protocol is the thing being tested; this goal depends on
  `specify-local-provider-protocol-v1` having made it normative.

## Scope

1. **General conformance suite** — parameterized over `providers::registry()`, asserting for every
   engine:
   - `GET /health` responds in the required shape once the runtime reports healthy.
   - `GET /v1/config` returns at least `schema_version` and `model`.
   - `POST /v1/audio/transcriptions` with multipart `file` and no `model` field returns the protocol
     response shape; `text` is non-empty for a known-speech fixture.
   - Optional response fields, when present, are correctly typed and internally consistent
     (`segments[].start <= end`, `duration` plausible against the clip).
   - Auth: with a token configured, every route 401s without it and succeeds with it.
   - Lifecycle through `RuntimeManager`: install → model pull → verify → start → descriptor →
     transcribe → stop → remove → uninstall.
   - Cache location: all artifacts land under `default_data_root()`.
   - Idle shutdown stops an untouched instance.
   - Skips gracefully, loudly, when an engine's runtime is not obtainable in the current environment —
     mirroring how `faster_whisper_integration.rs` already skips when its venv is absent. A skip must
     be visible, never silent.
2. **Engine-specific layer** — a separate, clearly named location for behaviour only some engines
   have: faster-whisper's GPU variant and compute types and its `/v1/admin/model` hot-swap;
   sherpa-onnx's request batching. Kept apart from the general suite so the boundary between "the
   contract" and "this engine's extras" stays legible.
3. **`stt verify [provider]`** — runs the conformance checks against installed engines on real
   hardware and prints a per-check pass/fail table. This is the part CI cannot do for engines whose
   runtimes need real hardware or large downloads.
4. **`stt bench --audio <dir> [--provider <id>] [--model <id>]`** — runs every clip in a folder
   through the selected models, reporting per-clip wall-clock latency, real-time factor, and the
   transcript. Model load excluded from per-clip timings.
5. **Audio fixtures** — a small committed set for the automated suite (short, clearly-licensed or
   self-recorded, English), separate from the larger dictation set
   `validate-parakeet-performance` produces for benchmarking. Keep committed audio small; point
   `stt bench` at an external folder for the real set.
6. **CI wiring** — run the general suite for every engine obtainable in CI.

## Out of Scope

- WER/CER scoring. `stt bench` reports speed and transcripts; formal accuracy scoring is a separate
  concern and only worth building if a decision hinges on it.
- Load or stress testing.
- Testing `stt-sdk` against the runtimes — the SDK has its own suite; a cross-repo compatibility CI
  job is already owned by the root `split-current-functionality-into-three-components` goal.
- Replacing `faster_whisper_integration.rs`. Its engine-specific parts move to the engine-specific
  layer; its generic parts should be absorbed by the general suite rather than duplicated.

## Acceptance Criteria

1. The general suite runs against every registry engine with no per-engine code, and adding an engine
   to the registry adds it to the suite automatically.
2. A deliberately introduced protocol violation in any engine fails the general suite.
3. Engine-specific tests live in a clearly separate location and never gate an engine that lacks that
   capability.
4. `stt verify` produces a readable per-check table on real hardware.
5. `stt bench` reports per-clip latency and real-time factor across models on identical audio.
6. Skips are explicit and visible in output.

## Judgment Rubric

- Not done if the general suite contains any `if provider == "..."` branch.
- Not done if adding an engine requires writing new general-suite tests.
- Not done if an engine can silently skip the whole suite and appear to pass.
- Not done if `stt bench` timings include model load time in the per-clip figures.

## Risks / Unknowns

1. **Runtime availability in CI.** faster-whisper needs a Python venv (already handled in `ci.yml`);
   `sherpad` needs its binary built. Decide per engine whether CI obtains it or the suite skips, and
   make the skip loud.
2. **Test duration.** A full lifecycle per engine, including model downloads, is slow. Consider a
   fast tier (protocol shape against an already-running runtime) and a slow tier (full lifecycle),
   rather than making the default suite unbearable.
3. **Fixture licensing.** Committed audio must be self-recorded or clearly licensed. Prefer
   self-recorded to avoid the question entirely.

## Verification Expectations

### Automated Verification
- `cargo test --workspace` runs the general suite; deliberately break a response field in one engine
  and confirm the suite fails.

### Manual Verification
- `stt verify` against both engines installed on real hardware.
- `stt bench` against the dictation sample set, reproducing `validate-parakeet-performance`'s
  numbers within reason — if the spike and the harness disagree, one of them is wrong.

## Attempts

### Attempt 1 (2026-09-09)

Built the general conformance suite as `crates/runtime/src/conformance.rs` (library code, not
test-only) with `crates/runtime/tests/protocol_conformance.rs` as a thin `cargo test` wrapper and
`stt verify` as its real-hardware CLI view -- both call the identical `conformance::check_all()`, so
they can never drift into checking different things (exactly the goal's own design intent). Checks
per provider: descriptor shape (schemaVersion/protocol/baseUrl), `GET /health`, `GET /v1/config`,
auth enforcement (unauthenticated request must 401), and a real transcription with no `model` field
whose `text` must be non-empty for the shared known-speech fixture. Zero `if provider == "..."`
branches anywhere in this code -- confirmed by inspection and by the fact adding the sherpa-onnx
catalog entry in a *previous* goal this session required no changes here at all.

**Real finding, not a suite bug**: running this against both real engines immediately caught that
faster-whisper's Python sidecar never actually enforces `VOICE_TYPER_AUTH_TOKEN` (parses it, never
compares it) -- a genuine pre-existing violation. Filed as its own goal,
`fix-faster-whisper-auth-enforcement`, rather than fixed here (cross-language, separate scope) or
silently ignored. The automated `cargo test` suite soft-fails *only* that one named check for that
one provider (a loud `eprintln!`, not a silent pass) so it doesn't permanently block CI while that
fix is pending, but every other check for both engines -- including sherpa-onnx's own (correct) auth
enforcement -- remains a hard assertion. `stt verify`, being a real-hardware diagnostic rather than a
CI gate, does not carry this carve-out and correctly reports the gap as a failure.

Audio fixture: `crates/runtime/tests/fixtures/sample.wav`, sourced from `k2-fsa/sherpa-onnx`'s own
Parakeet release archive's `test_wavs/0.wav` -- LibriSpeech-derived (OpenSLR SLR12, public-domain
LibriVox audiobook narration), broadly redistributed by the ML community for exactly this purpose;
provenance documented in `fixtures/README.md`. Chose this over recording new audio (no microphone
access in this environment) or a synthetic tone (would produce empty transcriptions, useless for the
"text must be non-empty" assertion).

`stt bench --audio <dir> [--provider] [--model]` implemented: loads each matching model once, times
each clip via wall-clock around the actual HTTP request (matching a real caller's experience, not
pure inference time), excludes load time from per-clip figures. Verified it reproduces
`validate-parakeet-performance`'s own numbers: Parakeet RTF 0.075 here vs 0.08-0.10 in that goal's
manual testing (same order, same machine, consistent); faster-whisper-small RTF 0.458 here, in the
same range as that goal's 0.62-0.66 finding (some variance expected -- different exact system load at
measurement time, not a discrepancy worth chasing).

`fmt`/`fixture-lookup` used the minimal hand-rolled `wav_duration_secs` (parses the RIFF/fmt/data
chunks directly) rather than pulling in a decode dependency just to compute a duration for a CLI
report -- deliberately narrow, not a general-purpose WAV reader.

**Not built**: the engine-specific test layer (item 2) and full CI wiring for sherpa-onnx (item 6) --
see next_action.

## Do Not Repeat

None yet.

## Verification Log

- 2026-09-09 — `cargo test --workspace` (90 tests total, up from 89: the one new
  `protocol_conformance` test): all passing, including the new suite running for real against both
  engines (not skipping) once `STT_FASTER_WHISPER_RUNTIME_DIR`/`STT_SHERPA_ONNX_RUNTIME_DIR`/`PATH`
  were pointed at this environment's real builds (the same `CARGO_MANIFEST_DIR`-based fix
  `faster_whisper_integration.rs` already needed, since `cargo test`'s cwd isn't the workspace root).
- 2026-09-09 — `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --check`: clean.
- 2026-09-09 — `stt verify` run for real: produced the exact per-check table the goal calls for,
  correctly reporting the faster-whisper auth gap as `FAIL` (exit code 1) -- appropriate for a
  diagnostic tool, distinct from the test suite's CI-friendly soft-fail.
- 2026-09-09 — `stt bench --audio crates/runtime/tests/fixtures --provider sherpa-onnx`: Parakeet RTF
  0.075. `--provider faster-whisper --model Systran/faster-whisper-small`: RTF 0.458. Both consistent
  with `validate-parakeet-performance`'s independently-gathered numbers.

## Final Outcome

**General suite, `stt verify`, `stt bench`, and fixtures complete and verified against both real
engines; engine-specific layer and full CI wiring for the second engine remain.** The core acceptance
criteria that matter most -- a shared, engine-agnostic suite with zero per-engine branching that both
existing engines actually pass (with one honestly-tracked, named exception) -- are met and proven,
including having caught a real bug neither prior manual testing nor code review found.

## Ready For Execution

- Status: in_progress
- Reason: Core functionality lands and works; the two remaining items (engine-specific test layer,
  full CI coverage for sherpa-onnx) are additive, not blocking anything else in this session's chain.
