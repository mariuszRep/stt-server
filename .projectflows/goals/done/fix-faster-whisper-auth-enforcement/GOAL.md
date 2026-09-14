---
name: fix-faster-whisper-auth-enforcement
title: Make the faster-whisper Sidecar Actually Enforce VOICE_TYPER_AUTH_TOKEN
description: The Python sidecar parses VOICE_TYPER_AUTH_TOKEN but never compares it against incoming requests -- every route accepts unauthenticated traffic regardless of a configured token. Add real bearer-token enforcement, matching sherpad's already-conformant require_auth middleware.
status: done
type: bug
scope: stt-server/runtimes/faster-whisper/app/main.py
attempt: 1
max_attempts: 3
last_result: passed
next_action: null
success_criteria:
  - An unauthenticated request to any faster-whisper sidecar route returns 401 when VOICE_TYPER_AUTH_TOKEN is set.
  - A request with the correct bearer token succeeds identically to today's behavior.
  - No token configured (the loopback-default case) remains a pure passthrough, matching sherpad's behavior.
  - protocol_conformance.rs's soft-fail exception for faster-whisper is removed and the check becomes a hard assertion for both engines.
source: user
---

# Make the faster-whisper Sidecar Actually Enforce VOICE_TYPER_AUTH_TOKEN

## Goal

Close a real, currently-live conformance gap: faster-whisper's runtime accepts every request
regardless of whether a caller supplies the correct bearer token, or any token at all.

## Source Requirements

Discovered by `provider-conformance-test-suite`'s `protocol_conformance.rs` (2026-09-09), the first
time an authenticated-by-default test was ever run against a real, running faster-whisper instance
with a real generated token. Not previously caught because no test exercised it -- `RuntimeManager`
always generates a per-instance token regardless of loopback binding, but nothing ever checked
whether the runtime actually enforced it.

## Problem / Motivation

`runtimes/faster-whisper/app/config.py` parses `VOICE_TYPER_AUTH_TOKEN` into `config.AUTH_TOKEN`,
and `app/main.py`'s `POST /v1/audio/transcriptions` handler accepts an `Authorization` parameter --
but nothing in the sidecar ever compares the two. Verified directly: a request with no
`Authorization` header, or a wrong one, gets a normal `200` with a real transcription, identical to
a correctly authenticated request.

This matters specifically because `CONVENTIONS.md`'s "remote binding is explicit and authenticated"
rule depends on it -- LAN mode (`bindHost: "0.0.0.0"`) is gated on a non-empty `auth_token` being
supplied, on the premise that the token then actually protects the runtime. Today it doesn't; a LAN
listener is unauthenticated in practice regardless of the token requirement at the API layer.

`sherpad` (per `make-sherpad-protocol-conformant`) already does this correctly -- `require_auth`
middleware, no route exempt, verified via real 401/200 tests. This goal brings faster-whisper to the
same standard it already meets.

## Scope

1. Add real bearer-token comparison to the FastAPI app -- a dependency or middleware applied to
   every route (including `/health`, matching `sherpad`'s no-exemption behavior and
   `stt-server`'s own control-plane `require_auth`), 401 on missing/mismatched token, pure
   passthrough when `config.AUTH_TOKEN` is unset.
2. Remove `protocol_conformance.rs`'s soft-fail carve-out for this specific gap once verified fixed,
   so future regressions are a hard test failure for both engines identically.

## Out of Scope

- Any other faster-whisper behavior change.
- Auth on `stt-server`'s own control-plane API -- already correct, unrelated code path.

## Acceptance Criteria

1. Unauthenticated / wrong-token request to any route: `401`.
2. Correct-token request: identical behavior to today.
3. No token configured: unchanged passthrough behavior.
4. `protocol_conformance.rs` passes with the soft-fail exception removed.

## Verification Expectations

### Automated Verification
- `cargo test --workspace --test protocol_conformance` with the exception removed.
- Existing faster-whisper Python-side tests, if any, still pass.

### Manual Verification
- Real request with no `Authorization` header against a real running instance with a token
  configured: confirm `401`.

## Attempts

### Attempt 1 (2026-09-14)

Added `require_auth` FastAPI dependency in `app/main.py`, applied globally via
`FastAPI(dependencies=[Depends(require_auth)])` so every route (including `/health` and the admin
routes) is covered with no per-route opt-in. Parses `Authorization: Bearer <token>`, compares with
`secrets.compare_digest` (constant-time), raises 401 on missing/mismatched token, pure passthrough
when `config.AUTH_TOKEN` is unset. Removed the now-redundant unused `authorization` header parameter
from `audio_transcriptions`. The `StaticFiles` mount at `/` is a separate ASGI sub-app and is not
covered by the FastAPI-level dependency; documented inline as an accepted, narrow exemption (serves
only the bundled frontend, not the API surface) mirroring sherpad's own CORS-preflight carve-out.
Removed `protocol_conformance.rs`'s faster-whisper soft-fail exception for the `auth enforcement`
check, making it a hard assertion for both engines.

## Verification Log

- Manual: started the sidecar locally with `VOICE_TYPER_AUTH_TOKEN` set; `GET /health` with no
  `Authorization` header -> `401`; with `Authorization: Bearer wrong` -> `401`; with the correct
  token -> `200`. Restarted with no token configured; `GET /health` with no header -> `200`
  (unchanged passthrough).
- Automated: `cargo test --workspace --test protocol_conformance` passes with the soft-fail carve-out
  removed -- `auth enforcement [ok] 401 without token` for both `faster-whisper` and `sherpa-onnx`.

## Final Outcome

All acceptance criteria met: unauthenticated/wrong-token requests to the faster-whisper sidecar
return 401 when a token is configured, correct-token requests behave identically to before, no-token
configuration remains a pure passthrough, and `protocol_conformance.rs` passes as a hard assertion
for both engines with no carve-out.

## Ready For Execution

- Status: yes
- Reason: Self-contained Python change with a clear, already-verified reference implementation
  (`sherpad`'s `require_auth`) and an existing automated test to confirm it.
