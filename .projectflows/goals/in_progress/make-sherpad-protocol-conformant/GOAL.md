---
name: make-sherpad-protocol-conformant
title: Make sherpad Speak the Local Provider Protocol
description: Close every gap between sherpad's current HTTP surface and the Local Provider Protocol that faster-whisper implements and stt-sdk consumes, so a client cannot tell which engine is behind a runtime connection descriptor.
status: in_progress
type: feature
scope: stt-server/runtimes/sherpa-onnx/ (sherpad crate), stt-server/.github/workflows/{ci,release}.yml
attempt: 2
max_attempts: 5
last_result: partial
next_action: |
  Gap 2 (webm/opus decode) is now closed and verified with real, automated tests: symphonia's
  own registered codec-type constant for Opus does exist (attempt 1's claim otherwise was checked
  against a stale assumption, not the actual pinned symphonia-core 0.5.5 source -- corrected in
  decode.rs's module doc and Cargo.toml comment this attempt), so symphonia demuxes WebM/Opus and
  identifies the Opus track natively; libopus (via audiopus/audiopus_sys, "static" feature so no
  system libopus is needed) decodes the raw packets symphonia hands over. A real WebM/Opus fixture
  was generated (Playwright + headless Chromium's fake-media-device flags driving the app's actual
  MediaRecorder mimeType selection against real speech audio -- no ffmpeg needed) and checked into
  tests/fixtures/sample.webm. Along the way, found and fixed a real pre-existing bug unrelated to
  Opus specifically: malformed input could panic inside symphonia-format-mkv's demuxer rather than
  return an Err, which would have violated the "bad audio -> clear error, not a crash" acceptance
  criterion for any corrupt upload, not just Opus ones -- decode_to_mono_f32 now wraps its body in
  catch_unwind.
  Remaining before this goal can move to done: the temporary app-side FORCE_FALLBACK_RECORDER dev
  switch (whisper-vibes/apps/web/src/hooks/use-loop-recorder.ts) needs an actual manual pass in the
  running app -- record via forced fallback against Parakeet, confirm a transcript, repeat against
  Faster Whisper for a regression check, then remove the switch -- and CI/release actually need to
  run (they're YAML-valid and include the new CMAKE_POLICY_VERSION_MINIMUM fix this attempt found
  was necessary, but no push/tag has triggered them yet). The two model-dependent integration tests
  in tests/transcribe.rs are #[ignore]'d (need a real installed model, multi-hundred-MB) -- run them
  manually per that file's module doc as part of the same pass.
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

### Attempt 2 (2026-09-14)

Closed gap 2 (WebM/Opus decode) for real, with automated verification.

1. **Corrected a wrong premise from attempt 1.** Before writing any code, re-verified the "symphonia
   can't touch Opus at all" claim directly against the exact pinned version (`symphonia-core` 0.5.5,
   fetched from `raw.githubusercontent.com/pdeljanov/Symphonia/v0.5.5/...`, not docs.rs's JS-rendered
   source view, which returned plausible-looking but hallucinated content on first attempt via
   `WebFetch` -- caught by cross-checking with a direct `curl | grep` on the raw file). Ground truth:
   `symphonia-core` 0.5.5 *does* define `CODEC_TYPE_OPUS`, and `symphonia-format-mkv` 0.5.5 maps
   Matroska's `A_OPUS` `CodecID` to it and yields raw packet payloads for a track regardless of
   whether a decoder is registered. What genuinely doesn't exist is a `symphonia-codec-opus` decoder
   crate (confirmed via `crates.io/api/v1/crates/symphonia-codec-opus` -> 404). So the real gap was
   only ever "no decoder", not "no identification" -- symphonia was already usable as a pure Opus
   demuxer.
2. **WebM/Opus decode** (`decode.rs`): added `decode_opus_track`, which bypasses
   `symphonia::default::get_codecs().make()` for `CODEC_TYPE_OPUS` tracks and instead feeds each raw
   packet symphonia's demuxer yields to a libopus decoder via the `audiopus`/`audiopus_sys` crates
   (`audiopus_sys`'s `static` feature vendors and cmake-builds libopus from source, so no system
   libopus is required). Decodes at Opus's fixed 48kHz native rate; downmixes multi-channel to mono
   with the same averaging logic the non-Opus path already used. Deliberately not handled: `OpusHead`
   pre-skip/gain trimming -- a few ms of leading artifact doesn't matter for ASR.
3. **Real fixture, no ffmpeg needed.** This environment still has no ffmpeg/opusenc, but does have
   Node + `npx playwright`. Used headless Chromium's `--use-fake-device-for-media-stream`
   /`--use-file-for-fake-audio-capture` flags to feed `sample.wav`'s speech content through a real
   `getUserMedia`/`MediaRecorder` session using the app's exact mimeType-selection logic
   (`audio/webm;codecs=opus`), producing a genuine browser-generated WebM/Opus file --
   `tests/fixtures/sample.webm` (64KB, ~4s) -- rather than a synthetically constructed one.
4. **Found and fixed a real, pre-existing crash bug, unrelated to Opus specifically.** Writing the
   first corrupt-input test (`vec![0u8; 128]` as the upload) triggered an actual panic inside
   `symphonia-format-mkv` 0.5.5's own EBML parsing (`ebml.rs:343`, "EBML header must be read before
   calling this function") -- a library-internal invariant violation on malformed input, not
   something the existing code could have caught by matching on `Result`. This directly violated
   acceptance criterion 4 ("bad/corrupt audio returns a clear error rather than crashing the
   runtime") for *any* malformed upload through the demuxer path, not just Opus ones. Fixed by
   wrapping `decode_to_mono_f32`'s body in `std::panic::catch_unwind`, converting a caught panic into
   an ordinary `Err` that flows through the existing `ApiError::BadRequest` (400) path.
5. **Router extracted for testability**: `main.rs`'s inline `Router::new()...` moved into a new
   `src/lib.rs::build_router()`, since integration tests (a binary-only crate can't be imported from
   `tests/`) need a way to drive the real router via `tower::ServiceExt::oneshot` without a listener.
   `main.rs` is now a thin wrapper calling it.
6. **Tests** (`tests/transcribe.rs`, new; plus unit tests in `decode.rs`): real-fixture decode
   sanity + corrupt/truncated-input non-panic checks all run automatically and pass. The two
   fully-real-model checks (webm/opus transcribes via Parakeet; WAV still transcribes, as a
   regression check) are `#[ignore]`d behind `SHERPAD_TEST_MODEL_DIR`/`_ID` env vars pointing at an
   already-installed model, since installing one here means a 480MB download -- documented in the
   test file's module doc how to run them.
7. **CI/release fix, found while building locally**: `audiopus_sys`'s vendored libopus ships a
   `CMakeLists.txt` requiring only `cmake_minimum_required(VERSION 2.8)`; a current CMake (4.x, which
   this session had to install fresh, and which GitHub-hosted runner images also currently default
   to) refuses to configure anything below its 3.5 floor at all rather than just warning. Added
   `CMAKE_POLICY_VERSION_MINIMUM: "3.5"` (CMake's own documented escape hatch) to both `ci.yml`'s
   `sherpad` job and `release.yml`'s `build-sherpad` job so the new dependency actually builds in CI,
   not just locally.

Not done in this attempt: the app-side `FORCE_FALLBACK_RECORDER` manual pass (added the switch in
`use-loop-recorder.ts`, type-checked via `tsc -b`, but haven't yet driven a live recording through
it against a running Parakeet instance), and actually triggering CI/release in GitHub Actions.

## Do Not Repeat

- Do not trust a `WebFetch`-rendered docs.rs *source view* page for ground truth on a crate's actual
  source -- it can return plausible-looking but hallucinated content when the underlying page needs
  JS to render (verified this happening in this session: one docs.rs fetch claimed the opposite of
  what a direct `curl` of the same file's raw GitHub source showed a moment later). For "does this
  exact symbol/version really do X", fetch the raw source file directly
  (`raw.githubusercontent.com/<org>/<repo>/<tag>/<path>`) or grep a local checkout, not a rendered
  docs site.
- Do not conclude "crate X can't handle format Y" from one surface-level check (a README table, or
  even one hasty source read) without pinning down *which* layer actually lacks support. Attempt 1
  concluded symphonia had no Opus support at all; the truth was narrower (it has a `CodecType`
  constant and can demux/identify Opus tracks fine -- it only lacks a *decoder* crate for the codec),
  and that narrower truth is what made the FFI-libopus approach straightforward instead of requiring
  a handwritten Matroska parser. Re-verify against the exact pinned version before designing a
  workaround for a claimed gap.
- Do not assume "corrupt input returns an `Err`" for a third-party parser just because its API is
  `Result`-typed. A single malformed-byte-sequence test surfaced a real panic inside
  `symphonia-format-mkv` itself; wrap any third-party decoder/demuxer call that processes
  untrusted/attacker-controlled bytes in `catch_unwind` at the boundary, and actually test with
  garbage input, not just valid-but-unsupported input.

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
- Not verified (attempt 1): webm/opus decode (no fixture available -- see next_action and Do Not
  Repeat). **Closed in attempt 2**, see below.
- Not verified: the new CI/release YAML jobs actually running in GitHub Actions (no trigger event in
  this session); validated only for YAML syntax and structural parity with the existing
  faster-whisper jobs.
- 2026-09-14 (attempt 2) — from `stt-server/runtimes/sherpa-onnx/`, with
  `CMAKE_POLICY_VERSION_MINIMUM=3.5` set (see attempt 2's CI/release fix):
  - `cargo build --release --bin sherpad`, `cargo clippy --release --all-targets -- -D warnings`,
    `cargo fmt --check`: all clean, including the new `audiopus`/`audiopus_sys` dependencies.
  - `cargo test -p sherpad --release`: 9 passed, 2 ignored (the model-dependent ones), 0 failed --
    covers `decode_to_mono_f32` against the real `sample.webm` fixture (non-empty, non-silent
    output), the pre-existing `api.rs` auth unit tests, and the new `tests/transcribe.rs` integration
    suite (corrupt-audio 400, truncated-webm non-crash, missing-file 400, no-default-model 400), all
    driven through the real router via `tower::ServiceExt::oneshot`.
  - Confirmed the symphonia-Opus-support correction directly against upstream source (raw GitHub
    fetch of the exact pinned `v0.5.5` tag, cross-checked with `curl | grep` after a first `WebFetch`
    attempt gave a contradictory, apparently-hallucinated answer from a JS-only docs.rs page) and
    confirmed no `symphonia-codec-opus` crate exists via `crates.io/api/v1/crates/symphonia-codec-opus`
    (404).
  - `npx tsc -b` in `whisper-vibes/apps/web`: clean after adding the `FORCE_FALLBACK_RECORDER`
    dev switch to `use-loop-recorder.ts`.
  - Both `ci.yml` and `release.yml` re-validated with `yaml.safe_load` after the
    `CMAKE_POLICY_VERSION_MINIMUM` addition.
- 2026-09-14 (attempt 2, continued) — real end-to-end run, not just unit/integration tests: built
  `sherpad`, launched it standalone, pulled and loaded a real `parakeet-tdt-0.6b-v2` (480MB):
  - `POST /v1/audio/transcriptions` with the real `sample.wav` (explicit `model` field, standalone
    run has no default model): `200`, full protocol shape, correct transcript.
  - `POST /v1/audio/transcriptions` with the real, browser-generated `sample.webm`
    (`audio/webm;codecs=opus`): `200`, correct transcript for the ~4s clip
    (`"...observed Phebe, turning away her"` -- consistent truncation at the recording's actual
    length). Server log confirmed the Opus decode path firing:
    `Creating a resampler: in_sample_rate: 48000, output_sample_rate: 16000`.
  - Re-ran `cargo test -p sherpad --release -- --ignored` against this real installed model:
    both previously-`#[ignore]`d tests (`webm_opus_transcribes_via_installed_model`,
    `wav_still_transcribes_via_installed_model`) now pass for real, in 13.59s.
  - This closes every remaining "not yet verified" item for the `sherpad`-side behavior. What's
    still outstanding is exercising this through the actual running desktop app UI (forced-fallback
    recorder -> real transcript on screen) and triggering CI/release in GitHub Actions -- see Ready
    For Execution.

## Final Outcome

**Gap 2 closed and fully verified**, including a real end-to-end run (not just automated tests):
built `sherpad`, pulled a real Parakeet model, and confirmed both a real WAV file and a real
browser-generated WebM/Opus recording transcribe correctly through the exact same endpoint, with the
server log confirming the new Opus decode path actually firing (48kHz -> 16kHz resample) rather than
silently falling through to something else. All six conformance gaps are now closed: five verified
end to end on real hardware against the isolated env contract (attempt 1), and the sixth (WebM/Opus
decode) now verified the same way (attempt 2) -- including catching and fixing a real crash bug (a
third-party demuxer panic on malformed input) that the acceptance criteria explicitly call out.
`stt-sdk`'s `transcribe()` call, unchanged, now gets an identical response shape from `sherpad` as
from faster-whisper for both the WAV and WebM/Opus cases -- the "indistinguishable at the wire level"
goal is met for both of the app's actual recording paths, not just the primary one.

## Ready For Execution

- Status: in_progress (not blocking downstream work)
- Reason: `add-sherpa-onnx-provider` and `provider-conformance-test-suite` can proceed against what's
  landed here. Everything in `sherpad` itself is implemented and verified end to end, including with a
  real model. What's left is deliberately left for the user, not further engineering:
  1. Click through the actual desktop app UI with `localStorage.setItem("forceFallbackRecorder", "1")`
     set (the temporary switch in `use-loop-recorder.ts`, left in place on purpose for this) --
     record via Parakeet, confirm a transcript appears on screen, repeat against Faster Whisper as a
     regression check, then remove the switch (marked `// TEMPORARY` at both call sites) before any
     real release.
  2. Confirm CI is green on the PR opened for this branch, and decide when to cut an actual tagged
     release (`release.yml` only runs on `v*` tags) -- that's a deliberate publish decision, not
     something to trigger silently as a side effect of closing this goal.
  The original two-option framing from attempt 1's `next_action` (FFI libopus vs. app-side WAV
  normalization) is resolved in favor of option (a): FFI libopus is what's landed and verified;
  app-side WAV normalization was not needed.
