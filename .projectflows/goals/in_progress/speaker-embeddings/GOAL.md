---
name: speaker-embeddings
title: Speaker Embeddings (CAM++) as a Server Capability, Groundwork for Speaker Separation
description: POST /v1/audio/embeddings returns a 512-d voice fingerprint per speech segment of a finished recording with CAM++ (VoxCeleb) running in pure Rust inside stt-server, and stt-sdk exposes it as embedSpeakers() with cosine, match and clustering helpers.
status: in_progress
type: feature
scope: stt-server, stt-sdk
attempt: 1
max_attempts: 4
last_result: partial
next_action: "Slice 4+: a POST /v1/audio/diarize that clusters the embeddings itself and answers in OpenAI's diarized_json shape (speaker, start, end, text); wire it into whisper-vibes only after the owner approves."
success_criteria:
  - A pure-Rust CAM++ (VoxCeleb) forward pass with a Kaldi-style 80-bin fbank, weights exported once from the Apache-2.0 official checkpoint and embedded as f16, matches PyTorch (fbank within 1e-3, embeddings at cosine >= 0.9999, normal and attenuated input).
  - POST /v1/audio/embeddings takes file, optional segments and encoding_format (float or base64), needs no loaded model, never enters the inference queue or touches the GPU, and returns {model, dim, duration, segments:[{start, end, embedding}]} with null for segments under 0.5 s.
  - GET /v1/local/system advertises speaker_embedding {engine, dim, sample_rate}.
  - On a generated two-voice recording the same voice is clearly closer than another and clustering recovers the two speakers.
  - stt-sdk SttServerProvider.embedSpeakers() plus cosineSimilarity, matchSpeaker and clusterSpeakers; no npm publish.
source: user
---

# Speaker Embeddings (CAM++)

Owner direction (2026-10-18, "carry on with all of it"): a speaker-embedding capability as groundwork for speaker
separation, built the way offline-vad was (public API, pure Rust, no ONNX runtime, `+crt-static`).

## Decisions

- Model: CAM++ trained on VoxCeleb, the 3D-Speaker checkpoint `iic/speech_campplus_sv_en_voxceleb_16k`
  (`campplus_voxceleb.bin`, 29,357,703 bytes, SHA-256 `5b1a88b6f8d85826fabef804779c3372b42f3af21457fa48bd5c097c0686b2de`).
  Licence checked first on the official ModelScope model card (front matter `license: Apache License 2.0`; the
  3D-Speaker repository is Apache-2.0): commercial use is allowed. The model was trained on VoxCeleb2; the weights are
  the licensed artefact (notice in `THIRD_PARTY_NOTICES.md`).
- **The checkpoint's embedding is 512-d, not the 192-d the plan assumed** (3D-Speaker's own `infer_sv.py` builds this
  model with `embedding_size: 512`; the 192-d CAM++ is the separate `campplus_cn_en_common` checkpoint). The server
  reports `dim: 512` and the response `model` is `campplus-voxceleb-v1`. Choosing the 192-d common model instead is an
  open decision for the owner.
- Weights: `assets/campplus_vox_16k.f16` (14,339,136 bytes, 7,169,568 half floats, SHA-256
  `11122726706b2cfe164966cdafb93cc7727176e1db9d45952ef999d90b53e65b`, pinned by a test). Inference-mode batch norm is
  folded into the preceding convolution at export; the BNs that precede a ReLU after a concatenation stay a
  per-channel scale and shift. f16 passed the parity gates, so no f32 fallback is needed.
- Pipeline (3D-Speaker's): Kaldi fbank on the [-1, 1] waveform (25 ms / 10 ms, Povey window, pre-emphasis 0.97, 20 Hz to
  Nyquist, no dither, `realfft` already in the lock file), per-recording mean subtraction, CAM++, L2 normalisation.
- Segments: under 0.5 s -> `embedding: null`; up to 3 s whole; longer: 3 s windows every 1.5 s (last aligned to the end),
  averaged and re-normalised, on up to four threads. Without `segments` the server runs its own VAD (default
  parameters). At most 1000 segments per request.
- The route has its own `Semaphore(2)` (`speaker_slots`) and runs in `spawn_blocking`; CPU only.
- Two-voice fixture: `scripts/make_two_speaker_fixture.ps1` (System.Speech, A/B/A/B, 400 ms gaps, 11.7 s). **David is not
  installed on this machine**, so voice A is Microsoft George (male, en-GB OneCore) and voice B Microsoft Zira Desktop.
- Future diarization should follow OpenAI's `gpt-4o-transcribe-diarize` / `diarized_json` (verified against the OpenAI
  speech-to-text guide 2026-10-18: segments with `speaker`, `start`, `end`, `text`; `known_speaker_names[]` and
  `known_speaker_references[]`; `chunking_strategy` for audio over 30 s). Documented as the future `/v1/audio/diarize`
  model in client-contract section 5c.

## Attempts
- Attempt 1 (2026-10-18): slices 1-3. `src/speaker/`, `POST /v1/audio/embeddings` (`api.rs::audio_embeddings`, with the
  multipart reading shared with the VAD route in `read_audio_form`), `speaker_embedding` in `/v1/local/system`, docs
  (client-contract section 5c, README, AGENTS, THIRD_PARTY_NOTICES) and stt-sdk `embedSpeakers()`, `cosineSimilarity`,
  `matchSpeaker`, `clusterSpeakers`.

## Verification Log
- Parity (`speaker::tests::matches_pytorch_on_the_speech_fixture`, golden from `scripts/campplus_golden.py`, PyTorch
  2.14.1 + torchaudio 2.11.0 in a throwaway venv): worst fbank error 6.3e-4 over the first 300 frames (limit 1e-3);
  embedding cosine 0.999995 at gain 1.0 and 0.999995 at gain 0.03 (limit 0.9999).
- Two voices (`speaker::tests::the_same_voice_is_clearly_closer_than_another` and `tests/speaker_binary.rs`): same-voice
  cosine 0.70 and 0.88, cross-voice 0.26 to 0.30. The SDK clusters the real embeddings of that recording
  (`stt-sdk/test/fixtures/two-speaker-embeddings.json`) into exactly the two scripted speakers.
- Speed (release, 12 threads): 60 s of audio in 2.9 s (RTF 0.05); 9.7 s on one thread (RTF 0.16). The release-only
  assertion in `tests/speaker_binary.rs` allows RTF 0.5.
- Binary test `tests/speaker_binary.rs` (real executable, port 54422, temp data dir): 401 without or with a wrong token,
  `missing_file`, 400 `invalid_request_error` for a bad `encoding_format` / `segments` / unknown field, embeddings for
  the scripted segments, base64 equals float, null for a segment under 0.5 s, own VAD without `segments`, and the
  `speaker_embedding` system field.
- `cargo fmt --check`, `cargo clippy --release --all-targets --offline -- -D warnings`, `cargo test --release --offline`
  (separate target dir `t`): all pass. Executable: see the commit message and the report (about 14.5 MB over 0.3.5's
  68,993,024 bytes, the weights).
- stt-sdk: `npm run typecheck`, `npm test`, `npm run build`, `npm run verify:consumer` pass.

## Final Outcome
Slices 1-3 delivered 2026-10-18; the goal stays in progress for the diarization endpoint. Nothing is published or
wired into whisper-vibes.
