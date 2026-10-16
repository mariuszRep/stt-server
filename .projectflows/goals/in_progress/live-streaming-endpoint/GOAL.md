---
name: live-streaming-endpoint
title: Live Streaming Transcription, OpenAI Realtime-Style, with Committed/Tentative Text
description: A WebSocket endpoint that transcribes microphone audio while it is spoken, shaped like OpenAI's Realtime API in transcription mode, and exposes Handy's committed/tentative split as an extension.
status: in_progress
type: feature
scope: stt-server
attempt: 1
max_attempts: 4
last_result: implemented and gated with unit and socket tests; not yet verified with a real streaming model
next_action: run the ignored worker test with a real streaming model (parakeet-unified-en-0.6b or nemotron-*-streaming) and a WAV, record the evidence, then mark done; then the SDK and Stanzo adopt it.
success_criteria:
  - GET /v1/realtime?intent=transcription upgrades to a WebSocket with the same bearer-token auth as the other routes.
  - The events follow OpenAI's transcription mode (session created/update, input_audio_buffer append/commit/clear, transcription delta/completed, error); every deviation is documented in docs/client-contract.md section 5a.
  - Handy's committed/tentative text is delivered through the documented partial extension event.
  - A model without streaming gets an OpenAI-style error with code streaming_unsupported, so clients fall back to batch.
  - A live stream never holds the batch queue for its whole duration (see src/realtime.rs for the design and its one limit).
  - Per-model streaming support is reported by the capability matrix (catalog claim, then the loaded model).
source: user
---

# Live Streaming Transcription

## Decisions

- Wire format: OpenAI Realtime transcription mode, because clients already know it. Both the beta names
  (`transcription_session.update`) and the GA `session.update` shape are accepted.
- Extension: `conversation.item.input_audio_transcription.partial` carries `committed`, `tentative`, `revision`.
  The standard `delta` event carries only newly committed text.
- Concurrency: the transcribe-cpp library holds a per-model compute lease for the life of a stream and refuses
  overlapping runs on that model with `Busy`, so a stream and a batch run cannot share a model. At most one stream
  runs server-wide (own slot); it takes the batch permit only while it begins, then releases it. Batch requests for
  other models proceed (and swap the model; the stream keeps its own handle); a batch request for the streaming
  model gets 503 `engine_busy` until the item ends. A second, independent model copy for streaming was rejected: it
  doubles model memory silently.

## Open decisions for the owner

- VISION.md still lists "Live streaming transcripts" under Not in scope; this goal was requested by the owner and
  VISION.md was not edited (it needs an explicit instruction).
- Translation in live mode: see the draft goal `live-translation`.
