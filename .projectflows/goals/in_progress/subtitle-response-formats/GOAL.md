---
name: subtitle-response-formats
title: srt and vtt Response Formats for Transcriptions and Translations
description: Return subtitle files (srt, vtt) from the batch transcription and translation endpoints, as OpenAI's audio endpoints do.
status: done
type: feature
scope: stt-server, stt-sdk
attempt: 1
max_attempts: 4
last_result: passed
next_action: null
success_criteria:
  - /v1/audio/transcriptions and /v1/audio/translations accept response_format srt and vtt and return text/plain subtitle bodies built from the segment timestamps, matching OpenAI's formats.
  - A model that cannot produce segment timestamps gets a clear 422 unsupported_capability, not an empty or invented file.
  - stt-sdk exposes the formats with the same names as OpenAI.
source: user
---

# srt and vtt Response Formats

## Why

OpenAI's transcription and translation endpoints also return `srt` and `vtt`. `stt-server` accepts only `json`, `text`
and `verbose_json` today (`ResponseFormat` in `src/run_plan.rs`), so an OpenAI client asking for subtitles gets
`422 unsupported_capability`.

## Decisions (2026-10-18, owner-delegated defaults)
- A model with no segment timestamps (`timestamp_granularity` unsupported) is `422 unsupported_capability`, not one cue
  spanning the clip: an invented cue would be a lie about timing.
- `timestamp_granularities` is ignored with `srt`/`vtt`; the plan always asks for segment timestamps, explicitly (no
  silent downgrade). Token-only models (gigaam, medasr) get the same single synthesized segment as `verbose_json`.
- Bodies are `text/plain; charset=utf-8`, as OpenAI returns them. SRT text is verbatim (trimmed lines, blank lines
  dropped); WebVTT escapes `&`, `<`, `>`.

## Attempts
- Attempt 1 (2026-10-18), stt-server and stt-sdk: `ResponseFormat::{Srt,Vtt}` planned in `run_plan.rs`; `src/subtitles.rs`
  renders the cues; `format.rs` shares the timed-segment logic with `verbose_json` (and returns 422 if a model that
  should have produced timing returned none for recognised text); `capabilities.response_formats.srt|vtt` follows the
  segment-timestamp control (`unknown` for a model not loaded and claimed to timestamp). Docs: `client-contract.md`
  (Subtitle formats), README. stt-sdk: `responseFormat: "srt" | "vtt"` on `transcribe()`/`translate()` returns the text.

## Do Not Repeat
- Do not invent a single cue for a model without timestamps.

## Verification Log
- 2026-10-18: `cargo fmt --check`, `cargo clippy --release --all-targets --offline -- -D warnings`,
  `cargo test --release --offline` (target dir `t`): 424 lib tests pass, 2 ignored, all integration tests pass. stt-sdk
  `npm test`, `npm run build`, `npm run verify:consumer` pass. Not verified: a real model producing a real file (the
  formatting is covered with synthetic transcripts); no npm publish.

## Final Outcome
Done 2026-10-18. `response_format=srt|vtt` on `/v1/audio/transcriptions` and `/v1/audio/translations`, and the same
names in stt-sdk.
