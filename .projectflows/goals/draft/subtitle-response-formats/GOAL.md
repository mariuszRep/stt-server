---
name: subtitle-response-formats
title: srt and vtt Response Formats for Transcriptions and Translations
description: Return subtitle files (srt, vtt) from the batch transcription and translation endpoints, as OpenAI's audio endpoints do.
status: draft
type: feature
scope: stt-server, stt-sdk
attempt: 0
max_attempts: 4
last_result: none
next_action: Not started. Split out of live-translation by owner decision D2 (2026-10-17). Design first - which models can produce usable segment timestamps, and how a model with no timestamps behaves.
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

## Known so far
- `verbose_json` already carries segments with start/end; srt/vtt are a rendering of those.
- Models without a working segment-timestamp path (see `EffectiveCaps::supports_segment_timestamps`) cannot honestly
  produce subtitles.

## To decide
- Behaviour for models that report no segment timestamps (error vs. one segment spanning the clip).
- Whether `timestamp_granularities` interacts with the subtitle formats.

## Attempts

## Do Not Repeat

## Verification Log

## Final Outcome

## Ready For Execution
Not ready: draft only.
