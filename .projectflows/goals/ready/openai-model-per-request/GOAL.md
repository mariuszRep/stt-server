---
name: openai-model-per-request
title: Choose the Model per Request, OpenAI-Style, with a Separate Model Manager
description: Let clients pick any downloaded model on each request, as with OpenAI, while the server swaps models itself; keep the OpenAI model list for downloaded models and move download and removal into a clearly named model manager.
status: ready
type: feature
scope: stt-server-next only
attempt: 0
max_attempts: 6
last_result: none
next_action: Implement per-request model choice and the default model, then the /models/manage rename, then docs and the CLI.
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

None yet.

## Verification Log

2026-09-28: Created from the user's decisions: model per request with host-side switching,
settings only download/remove/star default, selection per workflow, OpenAI list for downloaded
models, and the model manager named `/models/manage`.

## Final Outcome

Not started.
