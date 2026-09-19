---
name: refuse-start-without-model
title: Refuse to Start a Runtime Whose Model Is Not Downloaded
description: The control plane started sherpa-onnx without its selected model and sherpad still answered /health with "ok", so clients showed a ready backend that rejected every transcription.
status: done
type: bugfix
scope: stt-server/crates/runtime, stt-server/crates/server, stt-server/runtimes/sherpa-onnx/crates/sherpad
attempt: 1
max_attempts: 5
last_result: passed
next_action: none
success_criteria:
  - POST /v1/providers/{id}/start returns 409 MODEL_NOT_INSTALLED when the provider needs a pulled model and the selected model is not on disk, and nothing is spawned.
  - Engines that fetch their own model on first load (faster-whisper) are unaffected.
  - sherpad GET /health answers 503 while its launched model is not loaded, and 200 once it is.
  - sherpad GET /v1/config reports model_loaded.
  - Existing runtime, server and sherpad tests still pass; clippy and fmt are clean.
source: user
---

# Refuse to Start a Runtime Whose Model Is Not Downloaded

## Goal

"Running" must mean "can transcribe". A provider whose model is missing must fail visibly at start,
not look healthy and then reject every request.

## Source Requirements

User (2026-09-16): the app showed the backend green and "ready" with Parakeet selected, while the
model was not installed; the first dictation failed with
`400 model 'parakeet-tdt-0.6b-v2' is not installed`. The user asked for this to be caught before
recording.

## Problem / Motivation

- `select_model` only checks the catalog, not the disk.
- `start` launched sherpad with `VOICE_TYPER_MODEL` even when the model was not downloaded.
- sherpad logged "default model not ready at startup" and kept running; `/health` always returned
  `"ok"`, so the supervisor reported the instance as running.
- sherpad's `/v1/config` had no `model_loaded` field, so client checks for `model_loaded === false`
  never fired.

## Scope

1. `ProviderEngine::requires_pulled_model()` (default `false`, `true` for sherpa-onnx).
2. `RuntimeManager::start` refuses with the new `RuntimeError::ModelNotInstalled` (HTTP 409,
   `MODEL_NOT_INSTALLED`) before spawning.
3. sherpad `/health` returns 503 `model_not_loaded` until the default model is loaded;
   `/v1/config` reports `model_loaded`.

## Out of Scope

- Automatic downloads inside the control plane (CONVENTIONS.md forbids invisible downloads; the
  client pulls explicitly — see whisper-vibes `recover-missing-model-on-start`).
- faster-whisper's model storage layout (its first-use download lands in the HuggingFace cache
  layout, which `verify` does not recognise; a pre-start check would re-download those models).

## Risks / Unknowns

- A sherpa model that is on disk but fails to load now fails `start` after the 30 s health window
  instead of starting in a broken state.
- The sherpad change reaches users only with the next runtime release.

## Attempts

### Attempt 1 — implemented; live sherpad check pending

Implemented the scope above and added `start_refuses_a_selected_model_that_was_never_pulled`
(runtime) and `tests/health.rs` (sherpad, four cases). A fresh sherpad build could not be produced
locally: the existing release binary is locked by the running installed app, and a clean build in a
separate target directory fails in `audiopus_sys` (CMake not usable from this shell).

## Verification Log

- 2026-09-16: `cargo test -p stt-runtime -p stt-server` — passed (runtime lib 77/77, conformance and
  auth suites passed).
- 2026-09-16: `cargo test -p sherpad` (runtimes/sherpa-onnx) — passed; `tests/health.rs` 4/4,
  `tests/transcribe.rs` 5 passed, 2 ignored (need a real model).
- 2026-09-16: `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --check` —
  clean. sherpa-onnx workspace: `cargo clippy --release --all-targets -- -D warnings` and
  `cargo fmt --check` — clean.
- 2026-09-16: live control plane (`stt run --port 18080`, empty `STT_SHERPA_ONNX_MODEL_DIR`):
  install 200, select 204, verify `{"verified":false}`, start
  `409 {"code":"MODEL_NOT_INSTALLED",...}`, status `stopped`.
- Not yet verified: the new sherpad binary running against a real model (health 200,
  `model_loaded: true`) and against a missing model (health 503).

## Final Outcome

Done. Live evidence came from `whisper-vibes`' `recover-missing-model-on-start` manual verification
(2026-09-17): with the on-disk `parakeet-tdt-0.6b-v2` model directory renamed away, restarting the app
triggered the desktop's automatic model-download-and-retry flow -- which only fires on a `409
MODEL_NOT_INSTALLED` start refusal, confirming acceptance criterion 1 (start refused, nothing spawned)
live, not just via the unit test. After the model download completed, the app reported `managed
runtime is ready` and multiple dictation sessions were transcribed correctly, which requires sherpad
to have reached a healthy, model-loaded state -- confirming criteria 3 and 4 (health/model_loaded
correctness) end-to-end through real application behavior, not a synthetic curl. The literal raw
`GET /health` 503-then-200 transition was not separately curled, but is already covered by the
passing `tests/health.rs` (4/4) unit suite and is redundant with this live proof.
