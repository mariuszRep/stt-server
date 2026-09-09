---
name: fix-faster-whisper-auth-enforcement
title: Make the faster-whisper Sidecar Actually Enforce VOICE_TYPER_AUTH_TOKEN
description: The Python sidecar parses VOICE_TYPER_AUTH_TOKEN but never compares it against incoming requests -- every route accepts unauthenticated traffic regardless of a configured token. Add real bearer-token enforcement, matching sherpad's already-conformant require_auth middleware.
status: draft
type: bug
scope: stt-server/runtimes/faster-whisper/app/main.py
attempt: 0
max_attempts: 3
last_result: none
next_action: |
  Add a FastAPI dependency/middleware that compares the Authorization header against
  config.AUTH_TOKEN when it is set, returning 401 on missing/mismatched tokens, applied to every
  route (matching sherpad's api::require_auth -- no exemptions, including /health). Verify with
  provider-conformance-test-suite's protocol_conformance.rs: remove that test's faster-whisper
  soft-fail exception once this lands, so the assertion becomes a hard 401 check for both engines.
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

No attempts yet.

## Ready For Execution

- Status: yes
- Reason: Self-contained Python change with a clear, already-verified reference implementation
  (`sherpad`'s `require_auth`) and an existing automated test to confirm it.
