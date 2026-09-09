---
name: make-sherpad-protocol-conformant
title: Make sherpad Speak the Local Provider Protocol
description: Close every gap between sherpad's current HTTP surface and the Local Provider Protocol that faster-whisper implements and stt-sdk consumes, so a client cannot tell which engine is behind a runtime connection descriptor.
status: in_progress
type: feature
scope: stt-server/runtimes/sherpa-onnx/ (sherpad crate), stt-server/.github/workflows/{ci,release}.yml
attempt: 1
max_attempts: 5
last_result: partial
next_action: |
  Gaps 1, 3-6 closed and verified end to end on real hardware. Gap 2 (webm/opus decode) is
  partially closed: symphonia covers WebM/Vorbis and Ogg/PCM/Vorbis, but NOT Opus -- symphonia has
  no Opus decoder or codec-type constant at all (verified against upstream source). The app's actual
  MediaRecorder fallback produces webm/opus specifically. Follow-up options, neither attempted here:
  (a) add an FFI libopus binding (opus/opusic-sys crates) glued to symphonia's raw MKV packet
  stream -- real and buildable but needs a real webm/opus fixture (no ffmpeg in this environment) to
  verify against, or (b) revisit normalizing to WAV client-side in the app instead. CI/release jobs
  are added and YAML-valid but unrun (no push/tag event triggered them).
success_criteria:
  - sherpad implements GET /health and GET /v1/config in the shapes the protocol spec requires.
  - POST /v1/audio/transcriptions accepts the SDK's exact request (multipart file, optional prompt, no model field) and returns the full snake_case response shape including language, duration, and segments with avg_logprob/no_speech_prob/compression_ratio.
  - sherpad reads host, port, auth token, model, and model directory from the VOICE_TYPER_* environment contract, binding and caching where the control plane tells it to.
  - Bearer token authentication is enforced on every route when a token is configured.
  - Non-WAV audio the app can produce (webm/opus) transcribes successfully.
  - CI builds the sherpad binary per OS and the release workflow publishes it as a named asset.
source: user
---

# Make sherpad Speak the Local Provider Protocol

## Goal

Make `sherpad` a conformant managed runtime. After this goal, `stt-sdk` talking to `sherpad` and
`stt-sdk` talking to faster-whisper should be indistinguishable at the wire level — that
indistinguishability is the entire point of the two-engine programme.

## Problem / Motivation

`sherpad` was written as a standalone daemon with its own conventions, not as a managed runtime. Its
HTTP surface diverges from the protocol in six ways, each verified by reading both
`runtimes/sherpa-onnx/crates/sherpad/src/{main.rs,api.rs}` and
`stt-sdk/src/providers/faster-whisper.ts`. Every one is a hard blocker:

| # | Gap | Where | Required |
|---|---|---|---|
| 1 | Requires a `model` multipart field and 400s without it | `api.rs::transcribe` | The SDK never sends `model`. A managed runtime serves the single model it was launched with; treat an incoming `model` as an optional override only. |
| 2 | Decodes WAV only (`sherpa_onnx::Wave::read`) | `api.rs::transcribe` | The app's primary capture path emits WAV, but its `MediaRecorder` fallback (`whisper-vibes/apps/web/src/hooks/use-loop-recorder.ts`) emits webm/opus, which faster-whisper handles. Decode via `symphonia`, resampling to the 16 kHz mono f32 the recognizer expects. |
| 3 | Response omits `language`, `duration`, `segments` in `json` mode; `verbose_json` segments lack `avg_logprob`, `no_speech_prob`, `compression_ratio` | `api.rs::build_response` | Emit the full protocol shape. The app builds its transcript `quality` object from exactly those three fields (`whisper-vibes/apps/web/src/lib/api.ts`), so their absence degrades app behaviour silently. |
| 4 | No `GET /health`, no `GET /v1/config` — only `GET /v1/status` | `main.rs` router | Both are required. `supervisor::spawn` polls the catalog entry's `health_path` to decide the runtime came up, and `RuntimeManager::start` fetches `/v1/config` to populate the descriptor's `streaming` block. |
| 5 | No authentication of any kind | — | Enforce `Authorization: Bearer <token>` on every route when a token is configured, mirroring `stt-server`'s own `require_auth` middleware. Without this the descriptor's `auth` field is unenforceable and non-loopback binding can never be sanctioned. |
| 6 | Hardcoded `127.0.0.1` bind, `SHERPAD_PORT` as the only env var, hardcoded `dirs::data_dir()/onnx-sherpa` model directory | `main.rs` | Read the `VOICE_TYPER_*` contract that faster-whisper already implements: `HOST`, `PORT`, `AUTH_TOKEN`, `MODEL`, `MODEL_DIR`. The model directory matters beyond tidiness — if sherpa's models do not land under `stt_common::default_data_root()`, `whisper-vibes`' cascading-uninstall NSIS hook silently stops covering them, reproducing the exact bug that goal was built to fix. |

## Vision Alignment

- Root `CONVENTIONS.md`'s Local Provider Protocol is the extensibility seam; this goal is what makes
  a second engine actually sit on it. Depends on `specify-local-provider-protocol-v1` having made
  that section normative first, so there is one target rather than an inferred one.
- `stt-server/CONVENTIONS.md`: "Loopback is default; remote binding is explicit and authenticated" —
  gap 5 is what makes the second half of that sentence true for this runtime.
- `stt-server/VISION.md`: every engine's cached artifacts live under the server's unified data root —
  gap 6.

## Scope

1. Close gaps 1-6 above in the `sherpad` crate.
2. Preserve what already works and is good: `spawn_worker`'s batching (up to 8 jobs per 25 ms
   window), lazy model loading in `get_worker`, and the `sherpa-manifest` crate.
3. Add a CI job building `sherpad` per OS, and a release job publishing it under a stable asset name,
   mirroring `release.yml`'s existing `build-faster-whisper-sidecar` job shape. The asset name is the
   contract `add-sherpa-onnx-provider`'s `download_variant` will resolve against — fix it here.
4. Keep `GET /v1/status` as an additive, non-protocol diagnostic route if it is useful; it is not
   part of the protocol either way.

## Out of Scope

- Streaming. `sherpad` should report no streaming capability from `/v1/config`, exactly as
  faster-whisper does today. Real streaming is a separate, later goal.
- The `stt-server`-side adapter, catalog entry, and registry wiring — owned by
  `add-sherpa-onnx-provider`.
- Model catalogue decisions (which models ship) — owned by `add-sherpa-onnx-provider`.
- GPU/CUDA execution providers — `sherpad` stays CPU-only here; opening variants beyond `Cpu`/`Gpu`
  is its own future goal.

## Acceptance Criteria

1. The identical `stt-sdk` `transcribe()` call, unchanged, works against both faster-whisper and
   `sherpad` and returns the same response shape from both.
2. A request with no `model` field succeeds and is served by the launched model.
3. A webm/opus recording produced by the app's `MediaRecorder` fallback transcribes successfully.
4. With a token configured, every route rejects a missing or wrong bearer token with 401 and accepts
   the correct one.
5. Launched with `VOICE_TYPER_HOST`/`PORT`/`AUTH_TOKEN`/`MODEL`/`MODEL_DIR` set, `sherpad` honours all
   five; models land under the directory the control plane specified.
6. CI produces a `sherpad` binary per OS and the release workflow publishes it.

## Judgment Rubric

- Not done if any client code has to branch on which engine it is talking to.
- Not done if `text` is the only populated response field — `language`, `duration`, and `segments`
  must be real, not stubs.
- Not done if models still land in `dirs::data_dir()/onnx-sherpa` rather than the server-specified
  directory.

## Risks / Unknowns

1. **`avg_logprob`, `no_speech_prob`, `compression_ratio` are Whisper-decoder concepts.** A
   transducer model like Parakeet may not produce natural equivalents. Decide deliberately whether to
   omit them (the protocol marks them optional) or synthesise them, and record the decision — do not
   emit fabricated confidence numbers that the app will render as a quality score.
2. **Segment granularity.** `sherpad` currently returns one segment spanning the whole clip. Real
   per-segment timings depend on the model family; where unavailable, one segment is acceptable, but
   it must carry honest `start`/`end`.
3. **`symphonia` adds a real dependency and decode cost.** Measure the added latency; the whole point
   of this engine is speed.

## Verification Expectations

### Automated Verification
- The conformance suite from `provider-conformance-test-suite` if it has landed; otherwise a
  temporary equivalent asserting both runtimes answer the same request identically in shape.
- `cargo build --release`, `cargo clippy`, `cargo fmt --check` in the runtime tree.

### Manual Verification
- Drive `sherpad` through `stt-sdk` with no engine-specific code and confirm a real transcription.
- Run one webm/opus clip and one WAV clip through it.
- Confirm 401 on a bad token.

## Attempts

### Attempt 1 (2026-09-09)

Closed gaps 1, 3, 4, 5, 6 in full; gap 2 partially (see next_action).

1. **Model field optional** (`api.rs::transcribe`): `params.model` now falls back to
   `state.default_model` (from `VOICE_TYPER_MODEL`) when absent, erroring only if neither is set.
2. **Audio decode** (`decode.rs`, new): added a symphonia-based fallback that runs when
   `sherpa_onnx::Wave::read` fails, covering WebM/Matroska and Ogg containers carrying PCM/Vorbis.
   Investigated Opus support directly against symphonia's upstream `Cargo.toml`
   (`pdeljanov/Symphonia`, `symphonia/Cargo.toml`): no `opus` feature exists in the published crate
   at all, and a repo-wide search found no `CODEC_TYPE_OPUS` constant anywhere -- so the originally
   assumed "symphonia decodes webm/opus" premise was wrong, not just under-scoped. Documented this
   honestly in `decode.rs`'s module doc rather than shipping unverified FFI glue (an `opus`/
   `opusic-sys` libopus binding is buildable but has no real webm/opus fixture to verify against in
   this environment -- no ffmpeg available).
3. **Full response shape**: `build_response` now returns the same full shape
   (`text`/`language`/`duration`/`segments`) for both `json` and unset `response_format`, matching
   faster-whisper's own unconditional-full-shape behavior exactly (verified faster-whisper has no
   `response_format` parameter at all -- it always returns the full shape). `avg_logprob`/
   `no_speech_prob`/`compression_ratio` are deliberately `null` (not fabricated) since sherpa-onnx's
   `OfflineRecognizerResult` doesn't expose Whisper-decoder-specific confidence metrics for any
   model family here, transducer models like Parakeet least of all -- matches Risk #1's explicit
   guidance.
4. **`/health` + `/v1/config`** added; existing `/v1/status` kept as an additive diagnostic route,
   not part of the protocol.
5. **Auth**: `require_auth` middleware (mirrors `stt-server`'s own), enforced on every route
   including `/health` when `VOICE_TYPER_AUTH_TOKEN` is set; pure passthrough otherwise.
6. **Env contract**: `VOICE_TYPER_HOST`/`PORT`/`AUTH_TOKEN`/`MODEL`/`MODEL_DIR` all read and honored.
   `VOICE_TYPER_MODEL_DIR` replaces sherpad's whole models root (not a single model's directory the
   way faster-whisper's own env var works) -- documented in `main.rs` as a deliberate difference
   `add-sherpa-onnx-provider`'s adapter needs to account for when constructing the launch env.
   `SHERPAD_PORT` and the hardcoded `dirs::data_dir()` path kept as fallbacks for standalone dev use
   only, not when the `VOICE_TYPER_*` vars are set.

Also added: eager-loads `VOICE_TYPER_MODEL` at startup (before serving traffic) so `GET /health`
reporting "ok" means "ready to transcribe," not just "process is up" -- logs and continues rather
than failing to start if the model isn't installed yet (matches faster-whisper/supervisor's own
healthcheck-driven readiness model). Fixed two pre-existing issues found along the way: `ApiError`
had no `Debug` impl (needed for the new startup-warning log line) and one pre-existing clippy
`collapsible_match` lint in the multipart field parser, unrelated to this goal's scope but blocking
a clean `clippy -D warnings`; this crate had also never had `cargo fmt` run on it before -- ran it
once, fixing a large pre-existing formatting backlog alongside this session's new code.

CI: added a `sherpad` job to `ci.yml` (fmt/clippy/build, its own matrix, `runtimes/sherpa-onnx` as
its own `Swatinem/rust-cache` workspace so it doesn't share/pollute the root workspace's cache) and
a `build-sherpad` job to `release.yml` mirroring `build-faster-whisper-sidecar`'s shape, publishing
`sherpad-{linux|windows}-cpu{ext}` -- the exact asset name `add-sherpa-onnx-provider`'s
`download_variant` must resolve against. Both YAML files validated with `yaml.safe_load`; neither
job has actually run in GitHub Actions (no push/tag triggered them in this session).

## Do Not Repeat

- Do not assume a crate's README feature table reflects its currently published version. symphonia's
  own README lists Opus with a real-looking support table; the actual published `symphonia` crate
  (and even its unreleased dev branch's `Cargo.toml`) has no Opus feature or codec-type constant at
  all. Check the dependency's actual `Cargo.toml`/source, not its marketing table, before designing
  around a specific format/codec's support.
- Do not ship unverified format-decode code for a codec with no available test fixture. WebM/Opus
  decode was left honestly incomplete rather than writing FFI glue that compiles but was never run
  against real Opus audio.

## Verification Log

- 2026-09-09 — `cargo build --release --bin sherpad`, `cargo clippy --release --all-targets -- -D
  warnings`, `cargo fmt --check`: all clean from `stt-server/runtimes/sherpa-onnx/`.
- 2026-09-09 — `cargo build --workspace` in `stt-server` itself: unaffected, `cargo metadata --no-deps`
  still lists exactly `stt-common`/`stt-runtime`/`stt-server`/`stt-cli`.
- 2026-09-09 — full manual conformance pass, isolated env
  (`VOICE_TYPER_HOST=127.0.0.1 VOICE_TYPER_PORT=7899 VOICE_TYPER_AUTH_TOKEN=test-token-123
  VOICE_TYPER_MODEL=sense-voice-multi VOICE_TYPER_MODEL_DIR=/tmp/sherpad-models-test`, model dir
  starting empty):
  - `GET /health` with no `Authorization` header: `401 {"error":"unauthorized"}`.
  - `GET /health` with a wrong bearer token: `401`.
  - `GET /health` with the correct token: `200 {"status":"ok","model":"sense-voice-multi"}`.
  - `GET /v1/config`: `{"model":"sense-voice-multi","schema_version":1}`.
  - Startup log confirmed the eager-load-at-startup warning fired correctly when the model wasn't
    yet installed: `"default model not ready at startup" model=sense-voice-multi
    error=BadRequest("model 'sense-voice-multi' is not installed; ...")`.
  - `POST /v1/models/sense-voice-multi/pull` and `.../load`: both succeeded, and the model landed
    under the isolated `VOICE_TYPER_MODEL_DIR` (confirmed on disk), not the hardcoded default.
  - `POST /v1/audio/transcriptions` with **no `model` field**, real WAV audio: succeeded, served by
    the default model, full shape returned:
    `{"text": "...", "language": null, "duration": 7.435, "segments": [{"text": "...", "start": 0.0,
    "end": 7.435, "avg_logprob": null, "no_speech_prob": null, "compression_ratio": null, "words":
    null}]}`.
  - The identical unauthenticated request: `401`.
- Not verified: webm/opus decode (no fixture available -- see next_action and Do Not Repeat).
- Not verified: the new CI/release YAML jobs actually running in GitHub Actions (no trigger event in
  this session); validated only for YAML syntax and structural parity with the existing
  faster-whisper jobs.

## Final Outcome

**Substantially complete, gap 2 partial.** Five of six conformance gaps fully closed and verified
end to end on real hardware against the isolated env contract; the sixth (non-WAV decode) closes the
WebM/Vorbis and Ogg paths but not Opus specifically, for a real, verified technical reason (symphonia
doesn't support it) rather than an oversight. `stt-sdk`'s `transcribe()` call, unchanged, now gets an
identical response shape from `sherpad` as from faster-whisper for the WAV case, which is the app's
actual primary capture path -- so the core "indistinguishable at the wire level" goal is met for the
path that matters most; the MediaRecorder-fallback-specific Opus gap is a named, scoped follow-up,
not a silent omission.

## Ready For Execution

- Status: in_progress (not blocking downstream work)
- Reason: `add-sherpa-onnx-provider` and `provider-conformance-test-suite` can proceed against what's
  landed here -- neither depends on the Opus decode path specifically. Revisit gap 2 per the two
  options in `next_action` when a way to produce/obtain a real webm/opus test fixture exists, or when
  the app-side WAV-normalization alternative is decided instead.
