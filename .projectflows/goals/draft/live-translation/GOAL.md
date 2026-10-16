---
name: live-translation
title: Translation, OpenAI-Style, in Batch and Live Streaming
description: Translate speech the way OpenAI and Handy do, in both the batch endpoint and the live streaming endpoint, and let Stanzo use it as a workflow option.
status: draft
type: feature
scope: stt-server, stt-sdk, whisper-vibes (Stanzo)
attempt: 0
max_attempts: 4
last_result: none
next_action: Research how OpenAI (/v1/audio/translations, Realtime transcription) and Handy (translate option, transcribe-cpp task/target language) do translation; then design. After the live streaming endpoint lands.
success_criteria:
  - Batch translation (/v1/audio/translations, already present in stt-server) is checked against OpenAI's contract and Handy's behaviour, gaps listed and fixed.
  - The live streaming endpoint can translate as well as transcribe (OpenAI-style session option), for models that support it; others give a clear OpenAI-style error.
  - stt-sdk exposes both (batch and streaming) with the same naming as OpenAI.
  - Stanzo offers translation as a workflow option (target language), batch and live.
---

# Translation, OpenAI-Style, in Batch and Live Streaming

Owner request (2026-10-16): two implementations side by side, batch and live streaming, both as close as possible to
the OpenAI API while keeping Handy's committed/tentative streaming semantics; later, translation the same way.

## Known so far
- stt-server already has `/v1/audio/translations` (src/api.rs, `transcribe_or_translate`, Endpoint::Translations) and
  reports `translation` capability status ("unknown") and `translate_target_languages` per model.
- OpenAI: `/v1/audio/translations` translates to English only (Whisper task "translate"). Check whether the Realtime API
  has a translation mode today.
- Handy: check its translate setting and how it maps onto transcribe-cpp (task / target language per model family).

## To decide
- Which models translate (Whisper-family "translate" task, Canary, Voxtral, Nemotron multilingual?) and to which targets.
- Whether non-English targets are offered (OpenAI's endpoint is English-only; an extension would be documented).
