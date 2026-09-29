---
name: openai-model-per-request
title: Choose the Model per Request, OpenAI-Style, with a Separate Model Manager
description: Let clients pick any downloaded model on each request, as with OpenAI, while the server swaps models itself; keep the OpenAI model list for downloaded models and move download and removal into a clearly named model manager.
status: done
type: feature
scope: stt-server-next only
attempt: 2
max_attempts: 6
last_result: success
next_action: none -- both slices implemented, gated, and verified with a real model.
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

### Attempt 2 (2026-09-28) -- second slice: the `/models/manage` rename

Implemented the remaining success criteria: the model manager path rename and its CLI follow-on.

- Router (`src/api.rs`): everything formerly under `/v1/local/models*`,
  `/v1/local/recommendations`, and `/v1/local/operations*` moved to `/models/manage` with no
  aliases for the old paths (removed outright, per the user's decision -- no clients exist yet):
  `GET /models/manage` (was `GET /v1/local/models`), `POST /models/manage/{id}/download` (was
  `.../install`), `POST /models/manage/{id}/verify` (unchanged suffix), `POST
  /models/manage/{id}/default` (was `.../select` and `.../load`, both removed -- one route now),
  `DELETE /models/manage/{id}` (remove), `GET`/`DELETE /models/manage/default` (was
  `/v1/local/models/selected`, same response shapes kept), `POST /models/manage/refresh`,
  `POST /models/manage/import` / `import-user`, `GET /models/manage/operations/{id}` and
  `POST .../cancel`, `GET /models/manage/recommendations`. `/v1/local/system`, `/v1/local/config`,
  `/v1/local/shutdown`, `/health`, `/readiness`, `/v1/models*`, `/v1/audio/*` unchanged. Access
  levels unchanged (list/default-GET/recommendations/operation-status = user; every change =
  admin) -- authorization in this codebase is per-handler (`authorize(..., AccessLevel::_)` inside
  each function), not a path-prefix middleware check, so no separate gate needed updating.
- Route precedence: axum's router matches a literal path segment ahead of a `{id}` capture
  regardless of registration order, so `/models/manage/default`, `/refresh`, `/import`,
  `/import-user`, `/operations/...`, and `/recommendations` are never shadowed by
  `/models/manage/{id}` -- pinned by a new test,
  `static_models_manage_routes_take_precedence_over_id_capture`, that hits each static suffix with
  an id-shaped bogus value and checks for the *right* handler's response shape (not just a
  non-404 status, since a legitimate `operation_not_found`/`404` from the correct handler and a
  routing miss both return 404 -- distinguished by the error `code` in the body).
- Response shape: `/models/manage`'s per-model view now has explicit `downloaded: bool` (renamed
  from `installed`, which was already a bool; `installed_quant` kept as-is) and a new `default:
  bool` (missing before this slice) on both catalog and custom/drop-in entries, so a client no
  longer needs to cross-reference `GET /models/manage/default` separately to know which model is
  starred.
- `src/model_cli.rs`, `src/cli.rs`, `src/bin/server.rs`: all CLI HTTP calls use the new paths.
  Added `models download <id>` (was `models install`, kept as an alias) and `models default <id>`
  (was `models select`, kept as an alias) to the parser (`cli.rs::parse`) and `USAGE`; `models
  unload` already meant "clear the default" and needed no rename, just its underlying path
  (`DELETE /models/manage/default`). New parse tests pin both aliases.
- `scripts/catalog_sweep.py`: every API call path updated to `/models/manage/...`.
- `docs/client-contract.md` (route table in section 3, section 4's model-centric flow, the old-call
  mapping table in section 9), `README.md` (API section, CLI usage block and command
  descriptions), and `.projectflows/goals/ready/hands-on-acceptance-tests/GOAL.md` (every
  `/v1/local/models*` path and `models select`/`install` wording replaced; steps otherwise
  unchanged) updated to the new paths/names.
- Internal "selected" -> "default" naming: left the `selected_model` settings/DB key name as-is
  (with its existing comment already covering the rename-in-meaning) since renaming the column
  itself is a migration for no behavioural gain; renamed the doc-comment/route-string level
  references throughout `src/store.rs`, `src/import.rs`, `src/import_user.rs`, `src/dropin.rs`.
- Tests: every existing route test updated to the new paths (was: string literals scattered across
  ~15 call sites in `src/api.rs` plus the shared `user_routes()`/`admin_routes()` tables); added
  `old_local_models_paths_are_gone` (every old path 404s: list, selected GET/DELETE,
  recommendations, refresh, select/load/install/verify/remove-by-id, import, import-user,
  operations GET/cancel) and the precedence test above.

Gates (debug target dir `t/`): `cargo fmt --check` clean; `cargo clippy --all-targets -- -D
warnings` clean (0 warnings); `cargo test` (all four test binaries: lib, `stt-proof` bin, `server`
bin, and the `tests/` integration binary) -- 305 + 0 + 11 + 1 = 317 passed, 0 failed. (One new
precedence test failed on the first run for a test-design reason, not a product bug -- a bogus
operation id legitimately 404s from `cancel_operation` itself, indistinguishable by status alone
from a routing miss; fixed by asserting the response body's `error.code` is `operation_not_found`
instead of just checking the status code.)

Real check (debug exe, port 54401, temp data dir under `%TEMP%`):

```
models download whisper-tiny --wait   -> completed: 45981088/45981088 bytes
models default whisper-tiny           -> {"backend":{"fallback_reason":null,"observed_backend":"Vulkan0"},"model":"whisper-tiny"}
GET /models/manage                    -> catalog + drop-in list; whisper-tiny row shows "downloaded":true,"default":true
GET /v1/models                        -> {"data":[{"id":"whisper-tiny","default":true,"capabilities":{...live...},...}]}
GET /v1/local/models (old path)       -> 404
POST /v1/audio/transcriptions (WAV)   -> {"text":"you","x_diagnostics":{"model":"whisper-tiny","backend":"Vulkan0","inference_ms":298,...}}
stop                                  -> "stopped via shutdown endpoint"; temp data dir removed
```

## Verification Log

2026-09-28: Created from the user's decisions: model per request with host-side switching,
settings only download/remove/star default, selection per workflow, OpenAI list for downloaded
models, and the model manager named `/models/manage`.

2026-09-28: First slice (per-request model choice and default model) implemented and verified --
see Attempt 1 above for the gate results and real-model check. `/models/manage` rename is the
next slice.

2026-09-28: Second slice (`/models/manage` path rename, `downloaded`/`default` fields, CLI
`download`/`default` with `install`/`select` kept as aliases, route-precedence and old-path-404
tests, docs) implemented and verified -- see Attempt 2 above for the gate results and real-model
check. All success criteria met; not committed per instructions -- the user commits.

## Final Outcome

Done. Both slices of the goal are implemented: per-request model choice with host-side switching
and a default model (Attempt 1), and the `/models/manage` model manager rename with its CLI
follow-on (Attempt 2). All six success criteria are met, gates are green (fmt/clippy clean, 317
tests passing), and a real-model check exercised the new download/default/list/transcribe/stop
path end to end. Changes are staged in the working tree, not committed -- committing is the user's
call.
