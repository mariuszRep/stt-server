---
name: remove-voice-typer-ws-streaming-engine
title: Remove the voice-typer-ws Local Streaming Engine (Backend + SDK)
description: Delete the local WebSocket/LocalAgreement2 streaming transcription engine and its SDK client now that the app no longer surfaces it.
status: done
scope: stt-server faster-whisper runtime, stt-sdk faster-whisper provider
attempt: 1
max_attempts: 3
last_result: success
next_action: None — all success criteria met and verified.
success_criteria:
  - "`stt-server/runtimes/faster-whisper/app/streaming.py` and the `/v1/audio/stream` WebSocket endpoint in `main.py` are removed (or a replacement is in place)."
  - "`stt-sdk`'s `FasterWhisperProvider.createStream`/`FasterWhisperStreamSession` are removed and `capability.supportsStreaming` reflects reality."
  - No remaining references to the local WS streaming protocol in either repo's tests or docs, other than historical/changelog mentions.
source: user
---

# Remove the voice-typer-ws Local Streaming Engine (Backend + SDK)

## Goal

Fully remove the local Whisper WebSocket/LocalAgreement2 streaming engine (`voice-typer-ws`) from `stt-server` and `stt-sdk`, completing the removal whose UI half landed earlier in the same session (`whisper-vibes` — see git history / VISION.md's 2026-09-05 Key Decisions entry).

## Why This Exists

Investigating a hotkey-release-to-text latency complaint (~3.5s) showed the local WS streaming engine (`stt-server/runtimes/faster-whisper/app/streaming.py`, beam_size=1 LocalAgreement2 incremental transcription) was never actually the source of pasted/committed text — it was only a live preview. The real output always came from a separate full batch re-transcription (`POST /v1/audio/transcriptions`) of the final chunk. Given that, and that the streaming engine's transcription quality hasn't been good enough to justify its complexity, the decision was made to remove it entirely rather than keep maintaining dead weight.

This goal was originally drafted with execution deferred (see git history for the prior `draft/` version) — the user later confirmed in the same session that the full removal should proceed immediately rather than wait.

## Scope

- Removed `streaming.py` and the WS endpoint/`StreamingCapability` config field from `stt-server`'s faster-whisper runtime.
- Removed `createStream`/`FasterWhisperStreamSession` from `stt-sdk`'s `FasterWhisperProvider`; updated/removed the streaming-specific tests; `capability.supportsStreaming` now `false`.
- Left the SDK's generic `SttProvider`/`StreamSession` interface untouched — Deepgram, Groq, OpenAI, and whisper-cpp all still implement/reference it independently; `FasterWhisperProvider.createStream()` now throws `UnsupportedCapabilityError`, matching the existing OpenAI/Groq/whisper-cpp seam pattern exactly.
- Left `stt-server`'s Rust-side `StreamingCapability` type, `RuntimeConnectionDescriptor.streaming` field, and `fetch_streaming_capability()` fully intact — that plumbing is generic (any future streaming-capable runtime could still populate it) and already gracefully degrades to `streaming: None` when a runtime doesn't advertise it (a pre-existing, tested code path). Only faster-whisper's own advertisement of that block was removed.

## Out of Scope

- Rebuilding a better live-streaming engine — a future decision, not part of this removal.
- Any further change to `whisper-vibes` — its UI-side removal landed earlier in this session.
- The `web-speech` live-preview provider — unrelated, untouched.
- `whisper-cpp`'s own (still-unimplemented) streaming seam and `streamingEndpoint` option — a different, still-planned provider; not this goal's concern.
- Publishing the uncommitted SDK `0.3.0` source changes, pinning that package in bootstrap/compatibility checks, and updating downstream consumers — later SDK/bootstrap/consumer work, not part of removing this engine.

## Acceptance Criteria

1. `stt-server`'s faster-whisper runtime no longer exposes `/v1/audio/stream` or `streaming.py`. ✅
2. `stt-sdk`'s faster-whisper provider no longer implements `createStream`; its capability flags reflect batch-only support. ✅
3. Both repos' test suites pass with the streaming-specific tests removed/updated. ✅
4. No dangling imports/dead code referencing the removed streaming path in either repo. ✅

## Verification Expectations

- Run each repo's existing test suite after removal.
- Grep both repos for `audio/stream`, `StreamingSession`, `LocalAgreement`, `createStream` to confirm nothing's left dangling.

## Attempts

### Attempt 1 — 2026-09-05 — Success

**stt-server (Python runtime):**
- Deleted `runtimes/faster-whisper/app/streaming.py`.
- `main.py`: removed the `/v1/audio/stream` WebSocket route, the `StreamingCapability` model and its `streaming` field on `ConfigResponse`/`GET /v1/config`, and the now-unused `hmac`/`json`/`WebSocket`/`WebSocketDisconnect` imports and `app.streaming` import.

**stt-server (Rust control plane):**
- No source changes — the generic `StreamingCapability`/`descriptor.streaming`/`fetch_streaming_capability()` plumbing in `crates/common`/`crates/runtime` already gracefully degrades to `streaming: None` when a runtime's `/v1/config` omits the block (a pre-existing, already-tested path).
- Updated `crates/runtime/tests/faster_whisper_integration.rs`'s real end-to-end test: it previously asserted the real runtime *does* advertise streaming; now asserts `descriptor.streaming.is_none()`, matching the new reality. Verified by actually running this test against the real vendored Python runtime (not just the fake test double) — passed.
- Full `cargo test --workspace`: 78 tests passed (72 unit + 1 real integration + 5 auth), 0 failed.

**stt-sdk (`0.3.0` source changes in the `@open-vibe-ai/stt-sdk` package tree; not published):**
- `src/providers/faster-whisper.ts`: removed `createStream()`'s real implementation and the entire `FasterWhisperStreamSession` class; `createStream()` now throws `UnsupportedCapabilityError`, matching `OpenAIProvider`/`GroqProvider`/`WhisperCppProvider`'s existing seam pattern exactly. `capability.supportsStreaming` → `false`. Removed the now-unused `streamingEndpoint`/`auth`/`stopTimeoutMs` options and `WebSocketImpl`/`resolveWebSocket`/`toArrayBuffer` usage from this provider specifically (not the shared `TransportDeps` type, which other real-streaming providers still use).
- `src/factory.ts`: removed the `streamingEndpoint`/`auth` pass-through when constructing `FasterWhisperProvider` from a descriptor (kept for `WhisperCppProvider`, a different, still-unimplemented seam that isn't this goal's concern).
- Deleted `test/faster-whisper.stream.test.ts` and its fixture `test/fixtures/faster-whisper-stream-events.json`.
- `test/faster-whisper.e2e.test.ts`: removed the WebSocket-server test double and the one streaming-lifecycle test; kept the two genuine batch/`listModels` e2e tests intact.
- `test/faster-whisper.batch.test.ts`: `supportsStreaming` assertion → `false`; added a regression test asserting `createStream()` rejects with `UnsupportedCapabilityError`.
- `test/seams.test.ts`: replaced the obsolete "honors descriptor streaming endpoint and auth" test (which drove a real fake-WebSocket handshake) with one asserting that a descriptor still advertising a `streaming` block does *not* resurrect WS behavior — `FasterWhisperProvider` stays batch-only and `createStream()` still rejects. Removed the now-unused `createFakeWebSocketFactory` import.
- `package.json`: removed now-fully-unused `ws`/`@types/ws` devDependencies (ran `npm install` to sync `package-lock.json`); bumped version `0.2.0` → `0.3.0` (breaking change: `FasterWhisperProvider.createStream()` behavior changed from working to throwing).
- Verified: `npm run typecheck` clean, `npm test` — 25/25 passed across 4 files, `npm run build` (tsup) succeeded (ESM/CJS/.d.ts all built).

**Docs updated alongside the code:**
- `whisper-vibes/VISION.md`'s Key Decisions log: added the 2026-09-05 entry recording the pause/removal (this was already added when the UI-only removal landed earlier in the session; left as-is, accurately describes the eventual full outcome too since it already said backend/SDK removal was pending).
- `whisper-vibes/protocol.md`: replaced with a short removal notice (was entirely about the now-removed WS protocol).
- `stt-sdk/README.md`: removed the "Local streaming (protocol v1)" quick-start example, corrected the adapters table row, corrected "Protocol preservation" section, fixed the stale `voice-typer-ws-provider.ts` path reference in the "Reconcile note" (file no longer exists).
- `stt-server/README.md`: removed `WS /v1/audio/stream` from the wire-contract list (with a note it existed and was removed); updated the runtime-connection-descriptor JSON example to omit `streaming` (documented as optional, populated only when a runtime advertises it — faster-whisper currently doesn't).

## Do Not Repeat

None.

## Verification Log

- `cd stt-server && PATH="<venv>/Scripts:$PATH" cargo test --workspace` → 78 passed, 0 failed (includes the real faster-whisper integration test against the actual vendored Python runtime, not a fake double).
- `cd stt-sdk && npm run typecheck && npm test && npm run build` → typecheck clean, 25/25 tests passed, build succeeded.
- `py_compile` on all edited Python files → clean.
- Workspace-wide grep for `voice-typer-ws`/`StreamingSession`/`LocalAgreement2` → only remaining hit is the intentional backward-compat migration in `whisper-vibes/apps/web/src/hooks/use-capture-settings.ts` (collapses any old persisted `"voice-typer-ws"` setting to `"off"`).

## Final Outcome

Done. `stt-server` and the uncommitted `stt-sdk` `0.3.0` source/tests no longer implement or advertise the local WS streaming engine; the source changes and regression coverage are complete. Batch transcription — the only path that ever produced committed/pasted text — is fully unaffected. Publishing and pinning SDK `0.3.0`, updating bootstrap compatibility checks, and adopting it in consumers remain later SDK/bootstrap/consumer work outside this removal goal, so they do not reopen it. `whisper-vibes` needs no further changes for the engine removal; its UI-side removal landed earlier in this session.

## Ready For Execution

- Status: done
- Reason: Executed and verified in Attempt 1. Note: this goal (both its draft and this done record) was authored by hand across the session, since Project Flows MCP wasn't authenticated/available — flag for reconciliation through the normal flow if the format or placement (`stt-server` vs. root workspace) doesn't match convention.
