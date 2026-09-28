---
name: openai-model-per-request
title: Choose the Model per Request, OpenAI-Style, with a Separate Model Manager
description: Let clients pick any downloaded model on each request, as with OpenAI, while the server swaps models itself; keep the OpenAI model list for downloaded models and move download and removal into a clearly named model manager.
status: in_progress
type: feature
scope: stt-server-next only
attempt: 1
max_attempts: 6
last_result: none
next_action: First slice (per-request model choice and default model) implemented and gates green; next slice is the /models/manage path rename, then CLI (`models select` -> `models default`).
success_criteria:
  - A request naming any downloaded model is served by that model without a separate select call; the server loads it itself.
  - A request naming a model that is not downloaded gets a clear "model not installed" error, and nothing is ever downloaded by a request.
  - A request with no model, or "default", uses the starred default model, which is also what loads at startup.
  - Requests already waiting keep the model they asked for when another request switches models.
  - GET /v1/models lists only downloaded, usable models, in OpenAI shape, each with its capabilities and a default marker.
  - Everything available (catalog plus drop-ins) and all download, verify, default, remove, refresh and import actions live under /models/manage, outside the OpenAI paths.
source: user
---

# Choose the Model per Request, OpenAI-Style, with a Separate Model Manager

## Why

Voice Typer workflows will each use their own model, picked from the downloaded ones. OpenAI and
other providers let a client name the model on every request and handle loading themselves, with
no "select" step. The SDK treats this server like any cloud provider, so the server must behave
the same way. Separately, "available to download" and "downloaded and usable" are different lists
and need names that aren't confused with each other or with OpenAI's.

## Business rules

- **Model per request.** Every transcription or translation may name a model. A downloaded model
  is loaded by the server if it isn't already. One model is kept loaded at a time.
- **Never download on request.** A model that isn't downloaded is refused with a clear error.
- **Default model.** The user stars one downloaded model as the default. It is used when a request
  names no model or "default", and it is loaded at startup. Health and readiness say which it is.
- **Fair queue.** Requests are served in arrival order, each with the model it asked for. Switching
  happens between requests, never in the middle of one, and a failed switch keeps the previous
  model usable.
- **Two lists, two names.**
  - OpenAI part: `GET /v1/models` (downloaded, usable) and `GET /v1/models/{id}`. Each entry
    carries its capabilities and whether it is the default.
  - Model manager: `/models/manage`, deliberately outside `/v1`. It lists everything available
    with its downloaded, verified and default state. Downloading, verifying, setting the default,
    removing, refreshing the drop-in folder, importing, tracking and cancelling operations, and
    recommendations all live here.
- **Replaces the old paths.** The old `/v1/local/models`, recommendations and operations paths go
  away (no clients yet). System, config and shutdown stay where they are.
- **Access levels unchanged.** Listing and operation status are open to user access; every
  change needs admin access.
- **CLI follows.** `models` commands use the new paths; `models select` becomes
  `models default` (keep `select` as an alias).

## Out of scope

Locking the model on a shared server (possible later), several models loaded at once, streaming.

## Related goals

- Workspace: `migrate-voice-typer-to-stt-server-next`.
- `stt-sdk`: `draft/stt-server-next-adapter`; `whisper-vibes`: `draft/switch-to-stt-server-next`,
  `draft/workflow-stt-model-binding`.

## Attempts

### Attempt 1 (2026-09-28) -- first slice: per-request model choice and default model

Implemented, deliberately excluding the `/models/manage` rename (next slice):

- `transcribe_or_translate` (`src/api.rs`) now resolves `model` (default or a named id) before
  joining the queue, refusing an unknown/not-downloaded id with `404 model_not_installed` and an
  unverified one with `409 needs_verification` -- never a download. This replaces the old `409
  model_not_active` at the former line ~993.
- Once a request holds the sole inference permit (`bind_or_swap_model`), it makes the model it
  asked for the resident one, loading/swapping only then -- never mid-request, never two loads at
  once. A same-model background load in progress is waited out (bounded by
  `queue_wait_timeout_ms`) instead of an immediate 503; a swap load failure leaves the previous
  model untouched and fails only that request (`409 model_load_failed`).
- `select_model` keeps its route/name (rename is next slice) but is now documented as "set the
  default and load it" -- unchanged behaviour, since it already did both.
- `GET /v1/models` lists only downloaded+verified callable models in OpenAI shape plus `default`
  and `capabilities` (live `EffectiveCaps` if ever loaded, cached per model id in the new
  `App::live_caps`, else the catalog's static view). Added `GET /v1/models/{id}`.
- `/health` and `/readiness` gained `default_model`/`loaded_model`; existing fields kept for
  compatibility.
- Updated `CONVENTIONS.md` and `docs/client-contract.md` (model semantics, queue/swap behaviour,
  `/v1/models` shape, error table, health-card fields).

Choices not settled by the goal (picked as simplest-safe, listed here per instructions):
- Kept the `selected_model` DB/settings key name as-is (only its *meaning* -- "default" -- changed)
  rather than renaming the column; a storage-key rename is cosmetic and safer to fold into the
  `/models/manage` slice.
- `model_load_failed` on a per-request swap uses `409`, matching `select_model`'s existing code for
  the same failure.
- The same-model-load wait timeout reuses `queue_wait_timeout_ms` (the goal said "follow existing
  queue timeout settings") rather than introducing a second setting.

Real check (debug build, temp data dir, port 54499): installed `whisper-tiny` and
`moonshine-tiny` via `models install ... --wait`; `models select whisper-tiny` set it as default
and loaded it. Sent transcriptions of the same WAV alternating `model=moonshine-tiny`,
`whisper-tiny`, `default`, `moonshine-tiny`:

| request | `x_diagnostics.model` | inference_ms |
|---|---|---|
| moonshine-tiny | moonshine-tiny | 138 |
| whisper-tiny | whisper-tiny | 247 |
| default | whisper-tiny | 229 |
| moonshine-tiny | moonshine-tiny | 264 |

`GET /v1/models` correctly showed both as callable, `whisper-tiny` with `"default":true`, both
with live (loaded-at-least-once) capability views. `GET /health` after the sequence reported
`"default_model":"whisper-tiny","loaded_model":"moonshine-tiny"` (last one actually resident),
correctly distinct from the default. `model=not-a-model` returned `404 model_not_installed`.
Server stopped via `stt-server-next stop`; temp data dir removed.

Gates (debug target dir `t/`, `cargo fmt --check` / `cargo clippy --all-targets -- -D warnings` /
`cargo test`, all green): 285 lib tests + 11 + 1 integration tests = 297 passed, 0 failed.

Not committed (per instructions).

## Verification Log

2026-09-28: Created from the user's decisions: model per request with host-side switching,
settings only download/remove/star default, selection per workflow, OpenAI list for downloaded
models, and the model manager named `/models/manage`.

2026-09-28: First slice (per-request model choice and default model) implemented and verified --
see Attempt 1 above for the gate results and real-model check. `/models/manage` rename is the
next slice.

## Final Outcome

Not done. First slice (per-request model choice, default model, `GET /v1/models`/`/v1/models/{id}`
restricted to callable models, health/readiness `default_model`/`loaded_model`) is implemented,
gated (fmt/clippy clean, 297 tests passed at the time), and documented in `README.md` and
`docs/client-contract.md`. Remaining success criteria not yet done: the `/models/manage` path
rename (everything currently under `/v1/local/models*`, recommendations, and operations), and the
CLI rename `models select` -> `models default` (with `select` kept as an alias).
