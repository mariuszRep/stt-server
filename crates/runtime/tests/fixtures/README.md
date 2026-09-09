# Test fixtures

## `sample.wav`

7.44s, 16kHz mono WAV, real English speech (public-domain audiobook narration:
"Well, I don't wish to see it any more, observed Phoebe, turning away her eyes.
It is certainly very like the old portrait.").

**Provenance**: `test_wavs/0.wav` from `k2-fsa/sherpa-onnx`'s own
`sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8` model release archive
(`https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8.tar.bz2`).
This is LibriSpeech-derived content — LibriSpeech (OpenSLR SLR12) is built from
LibriVox public-domain audiobook recordings and is broadly redistributed by the
ML community for exactly this purpose (model/engine test fixtures); sherpa-onnx
itself ships and redistributes it as a test asset in every ASR model release.

Used by `protocol_conformance.rs` as the shared "known-speech" clip every
registered provider engine is tested against identically. Deliberately small
(clean, short, single-speaker) — this is a protocol-shape/smoke fixture, not a
benchmark corpus; `validate-parakeet-performance`'s (user-supplied) dictation
set is the real speed/quality benchmark, kept out of the repo.
