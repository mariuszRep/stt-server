---
name: capacity-arbitration-support
title: Server Support for Clients That Arbitrate Memory and Priority
description: Generic, documented additions that let a client share the server with other memory users and keep a live dictation turn ahead of background work - batch waits for a live stream instead of failing, an interactive priority class, and unload-only.
status: in_progress
type: feature
scope: stt-server (client of this work - Stanzo's capacity coordinator, tracked in the workspace)
attempt: 1
max_attempts: 3
last_result: partial - built (d16aece) and the Stanzo side built 2026-10-17; owner test pending
next_action: owner test with the Stanzo build that sends priority=interactive for dictation turns and calls POST /models/manage/unload (Stanzo 78fadab and 9040d4d in whisper-vibes); then a stt-server release carrying these additions.
success_criteria:
  - A batch request for the model a live stream holds waits for the stream's item to end instead of failing with engine_busy; when the wait outlasts queue_wait_timeout_ms it gets 503 engine_busy with details.reason "stream_active". A batch for another model is not delayed.
  - Transcription and translation requests accept an optional priority (interactive or normal, default normal); interactive requests are served before waiting normal ones, FIFO within a class; with no priority the behaviour is exactly the previous FIFO. The live stream's begin step queues as interactive.
  - A client can free the loaded model's memory without losing the saved default (POST /models/manage/unload), while DELETE /models/manage/default keeps clearing the default.
  - Everything is generic, additive and documented in docs/client-contract.md; nothing names a particular client.
---

# Server Support for Clients That Arbitrate Memory and Priority

A client that shares a PC with other memory users (a local LLM runtime) and runs live dictation alongside background
work needs three things from the server that no particular client should have to hack around. All three are small,
generic and optional.

## Findings
- `DELETE /models/manage/default` is "deselect": it waits for idle inference, **deletes the saved `selected_model`
  setting** and drops the loaded model. A client using it to free memory would silently forget the user's default.
  Hence the separate unload-only endpoint.
- Before this goal a batch request for the model a live stream held failed at once with `503 engine_busy`
  (the library refuses overlapping runs on one model); the caller had to retry. Now it waits.
- The queue was a single tokio semaphore permit (strict FIFO). It is now a small dispatcher in `src/queue.rs`:
  one mutex around two waiting lists, a permit drop hands the slot to the next waiter (interactive first).

## Decisions made (in the design, not owner-level)
- The stream lease (which model the live stream holds) lives with the stream slot in `InferenceQueue`
  (`StreamSlot::lease_model`, cleared when the slot drops), not on `App`: one owner for the stream slot and its lease.
  Buffered translation turns (models that cannot stream) hold the slot but no lease, since they hold no model.
- The wait for the stream and the queue wait share one `queue_wait_timeout_ms` budget; the lease is re-checked after
  the batch permit is taken (a stream may have begun while the request queued).
- `priority` is accepted on both transcription endpoints; an unknown value is `400 invalid_priority`. It is trusted
  (no auth distinction between user and admin tokens); the queue bounds still apply to both classes.

## Attempts
- Attempt 1 (2026-10-17), stt-server: S1 (batch waits for a stream, `engine_busy` + `stream_active` on timeout),
  S2 (interactive class, `priority` field, `start_item` interactive), `POST /models/manage/unload`.
  Docs: `docs/client-contract.md` sections 3, 4, 4.3, 5, 5a, 6; `README.md` queue section (`d16aece`).
- Stanzo side, 2026-10-17 (whisper-vibes): voice bursts send `priority: interactive` (`78fadab`); the capacity coordinator
  unloads idle speech models through `POST /models/manage/unload` (`9040d4d`).

## Verification Log
- 2026-10-17: new unit tests in `src/queue.rs` (interactive beats an earlier normal waiter; FIFO within each class;
  a dropped or timed-out waiter does not take the slot; same-model batch waits then runs; lease re-check after the
  permit; timeout gives StreamActive; another model is not delayed) and in `src/api.rs` (priority accepted / invalid /
  duplicate; unload keeps the default while deselect clears it; StreamActive maps to engine_busy + stream_active).
  The five pre-existing queue tests pass unchanged apart from the added `Priority::Normal` argument.
- 2026-10-17: Stanzo side built and tested by automation, as reported at delivery; not yet exercised by the owner under
  real memory pressure.
- 2026-10-18: the Memory status line in Stanzo that showed the capacity reading was removed by the owner's request; the owner's memory scenarios are still untested, so this goal stays in progress.
- Not verified: a real streaming model holding the lease while an HTTP batch waits (needs a local streaming model; the
  ignored `realtime_worker_streams_a_wav_through_a_streaming_model` test exercises the worker only).
