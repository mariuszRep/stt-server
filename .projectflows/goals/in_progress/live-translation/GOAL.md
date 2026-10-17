---
name: live-translation
title: Translation, OpenAI-Style, in Batch and Live Streaming
description: Translate speech the way OpenAI and Handy do, in both the batch endpoint and the live streaming endpoint, and let Stanzo use it as a workflow option.
status: in_progress
type: feature
scope: stt-server, stt-sdk, whisper-vibes (Stanzo)
attempt: 1
max_attempts: 4
last_result: partial
next_action: stt-server batch and live are done and gated (S1, S2). Remaining here - stt-sdk exposure (S3) is gated separately; Stanzo workflow option (whisper-vibes) is not started; the srt/vtt output formats are a separate draft goal (owner decision D2). Real-model check of a multi-target model (Canary) is still outstanding.
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

## Owner decisions (2026-10-17)
- D1: buffered per-turn live translation for models that translate but cannot stream.
- D2: srt/vtt response formats are a separate draft goal (`subtitle-response-formats`), not built here.
- D3: targets are every language the model offers (a documented extension beyond OpenAI); the default is `en`.

## Attempts
- Attempt 1 (2026-10-17), stt-server:
  - Batch: `target_language` on `/v1/audio/translations` only (unknown field on transcriptions), planned in `run_plan.rs`
    (a target the model does not offer is `422 unsupported_target_language`; source equal to target transcribes);
    `x_diagnostics.target_language`; `supports_translation` no longer requires an advertised `en` target (an empty
    list means "not advertised", English only); the catalog view reports the targets as unknown (`null`).
  - Live: session `target_language` (`input_audio_transcription.target_language` or `audio.output.language`), echoed
    in the session object; streaming-capable models translate on the stream, others use the buffered turn path
    (audio collected, one batch translate on commit holding the batch permit only for that run, one full-turn delta
    then `completed`). Errors are `error` events on the first append.
  - Docs: `docs/client-contract.md` sections 4.1, 5, 5a and 6.

## Do Not Repeat
- Do not require an advertised `en` target to call a model translating; engines that translate to English only
  often advertise no list.

## Verification Log
- 2026-10-17: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` (separate target dir `t`):
  364 lib tests pass, 2 ignored (opt-in real-model tests), all integration tests pass.
- Real model, turn path: the ignored test `realtime_translation_turn_runs_one_batch_translate_on_commit` ran against a
  local Whisper medium (read-only, CPU, `STT_TEST_TRANSLATE_MODEL`/`STT_TEST_TRANSLATE_WAV`): events
  `committed, delta, partial, completed`, delta equal to the full transcript.
- Not verified: a real multi-target translating model (Canary) and the `verbose_json` `language` of an
  auto-detected source on a translate run; documented as the source language from the plan/engine report.
