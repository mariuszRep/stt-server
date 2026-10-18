---
name: offline-vad
title: Offline Speech Detection (Silero VAD) as a Server Capability
description: POST /v1/audio/vad finds the speech segments of a finished recording with Silero VAD v5 running in pure Rust inside stt-server, and stt-sdk exposes it as detectSpeech().
status: done
type: feature
scope: stt-server, stt-sdk
attempt: 1
max_attempts: 4
last_result: passed
next_action: null
success_criteria:
  - A pure-Rust Silero v5 16 kHz forward pass, with weights exported once from the MIT silero_vad.onnx and embedded, matches onnxruntime within 1e-3 per frame on the speech fixture.
  - POST /v1/audio/vad returns {duration, speech_seconds, speech_ratio, segments} for a WAV, needs no loaded model, never enters the inference queue, and rejects bad parameters with 400 invalid_request_error.
  - GET /v1/local/system advertises vad {engine, sample_rate}; /v1/realtime is unchanged.
  - VISION.md and AGENTS.md say live VAD and chunking stay client-side and offline speech detection is a server capability.
  - stt-sdk SttServerProvider.detectSpeech() reuses the shared POST path; no npm publish.
source: user
---

# Offline Speech Detection (Silero VAD)

Owner direction (2026-10-14, 2026-10-17): audio models such as Silero become stt-server capabilities through its public
API, and stt-server stays an independent product.

## Decisions

- Engine: pure Rust, no ONNX runtime (ort/onnxruntime would conflict with the static CRT, `+crt-static`). The 16 kHz branch
  of `silero_vad.onnx` (MIT; the copy in the `voice_activity_detector` crate) was exported once by
  `scripts/export_silero_weights.py` into `assets/silero_v5_16k.f32` (1,238,532 bytes, SHA-256
  `b8df2e6e32753b7aa47ab59571b0d9d0b490a223f8dc9118bb388efeaec6f8e3`, pinned by a test, notice in
  `THIRD_PARTY_NOTICES.md`).
- Frames follow the Silero wrapper (512 samples plus a 64-sample context); segmenting follows `get_speech_timestamps`
  without its optional maximum-speech-length splitting; segments that touch after padding are merged.
- The route has its own `Semaphore(2)` and runs in `spawn_blocking`, so it never waits behind a transcription.
- Parameter ranges: `threshold` 0.05-0.95; the three durations 0-60000 ms. Errors are `400 invalid_request_error`.
- Live VAD stays client-side (VISION.md "Not in scope").

## Attempts
- Attempt 1 (2026-10-17): `src/vad.rs`, `POST /v1/audio/vad` (`api.rs::audio_vad`), `vad` in `/v1/local/system`, docs
  (client-contract section 5b, README, AGENTS, VISION), and stt-sdk `detectSpeech()`.

## Verification Log
- Parity: `vad::tests::matches_onnxruntime_on_the_speech_fixture` compares 193 frames at two input gains against
  onnxruntime 1.31 (`scripts/silero_golden.py`, `tests/vad_golden.json`): within 1e-3.
- Silence, a 440 Hz tone and faint noise give no segments (`vad::tests`); API tests cover 200 with the fixture, 200 with
  `[]` for silence, 400 bad parameters, 401 from the LAN without a token, and `vad` in the system report; the binary
  test `tests/vad_binary.rs` posts the fixture to the real executable with no model installed.
- `cargo fmt --check`, `cargo clippy --release --all-targets --offline -- -D warnings`, `cargo test --release --offline`
  (separate target dir `t`): all pass. `scripts/build-local.ps1 -Offline` (target dir `t`): 68,993,024 bytes
  (about 1.3 MB over 0.3.4's 67.7 MB, the weights).
- stt-sdk: `npm run typecheck`, `npm test`, `npm run build`, `npm run verify:consumer` pass.

## Final Outcome
Done 2026-10-17. Offline speech detection is a server capability and `detectSpeech()` is in stt-sdk (not published).
