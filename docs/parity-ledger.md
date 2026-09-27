# Voice Typer parity ledger (prototype)

The references are the `voice-typer-windows` nested worktrees under
`D:\Users\mariu\Projects\voice-typer`, not the sibling `main` clones. This ledger records
required behavior and current evidence; it does not imply a replacement verdict.

| Current client need | Prototype evidence | Evidence still required |
|---|---|---|
| Provider/model discovery and progress | Fixed recommendations; offline, model-free first start; durable install progress | Failure and retry contract tests |
| Runtime connection descriptor | Stable loopback URL, protected token, LocalSystem service tested | Clean-machine client handoff |
| Download, verify, selection, load, removal | Every catalog model/quant is installable via download or import (versioned SQLite migration records the installed quant/filename/size); hash, selection, restart reload, deselection, removal tested; durable `verify` operation end-to-end on the installed copy at its recorded (possibly non-default) quant, including restart reconciliation and live corruption quarantine; download hardening (stall timeout, retries, mirror fallback, disk preflight, machine-readable `error_code`) and CORS added and E2E-tested (real cancel/resume, unit-tested resume-from-nonzero-partial); mirror URL shape now confirmed against Handy (`src-tauri/src/catalog/mod.rs:29-30`, commit `8f9cf53`); model selection loads off the inference slot (old model keeps serving) and E2E-verified non-blocking; deselection now waits (bounded) for in-flight inference instead of failing instantly | Real disk-full test (unit-tested only, hard to fake reliably); language/prompt/translation/verbose controls (separate later task) |
| Batch audio | OpenAI-style multipart, same-file transcript, and tested stereo 48 kHz downmix/resample; bounded FIFO inference queue (8 waiters, 60s wait timeout) replaces instant-429-on-busy, with `x_diagnostics` (queue wait, inference time, audio duration, model, backend) on every response, E2E-verified under 4-way concurrency (2026-09-25 re-run on Parakeet: 4/4 200, `queue_wait_ms` 0/416/794/1159, FIFO-ordered). Real E2E (2026-09-25): Parakeet, whisper-tiny transcribe correctly in json/text/verbose_json; moonshine-streaming-tiny transcribes correctly in json/text but returns a raw engine 422 (`engine_unsupported`) instead of a clean capability error for `verbose_json` despite its capability matrix claiming `timestamp_granularity: supported` (bug, see `docs/feasibility-2026-09-25.md` Bug 1) | Other containers and dictation corpus; observing `queue_full` end-to-end under real load (unit-tested); fix for moonshine-streaming-tiny `verbose_json` capability mismatch |
| Prompt and vocabulary | Parakeet matrix says unsupported; prompt rejected with `unsupported_capability`. Real E2E (2026-09-25): whisper-tiny accepts short and ~3,600-word prompts (`prompt_applied: true`, HTTP 200); Parakeet rejects `prompt` with `unsupported_capability` (200/422 confirmed live) | Admit and test a model with supported decode hints on non-English audio |
| Language and translation | Unsupported Parakeet controls rejected. Real E2E (2026-09-25) on whisper-tiny/Parakeet/moonshine-streaming-tiny with `test-data/stereo48.wav` (English only, no other-language clip available): `language=en` gives `language_evidence: "user_selected"`; no hint or `auto` gives `"model_detected"`; unsupported `language=xx`/`de` falls back to English (`applied_language: "en"`) but the response omits an explicit `language_hint_applied: false` key rather than including it (bug, see `docs/feasibility-2026-09-25.md` Bug 2); `/v1/audio/translations` on whisper-tiny with no hint returns `task: "translate"`; with `language=en` it returns `task: "transcribe"` unexpectedly (bug, see Bug 3); Parakeet and moonshine-streaming-tiny reject `translations`/unsupported language-dependent controls with `unsupported_capability` as expected | Multilingual (non-English) audio — no such fixture exists in the repo; a model with supported decode hints tested on non-English speech |
| Streaming | Native catalog metadata is distinct from server batch support | API contract test |
| Diagnostics | CPU, Vulkan0, and forced-failure CPU fallback tested | More GPU/driver combinations |
| Current app settings | App preserves global settings, omits unavailable optional fields, explains disabled controls | Separate SDK/app cutover goal |
| Drop-in models and refresh | v4 migration adds `installed.source`/custom metadata and `operations.progress_items`/`total_items`/`result`; `user_models_dir` config get/validate/patch; `POST /v1/local/models/refresh` durable operation registers catalog-hash matches and header-probed custom models, lists duplicates/unsupported, unregisters disappeared files, and re-verifies changed ones; startup reconciliation leaves `user_folder` files untouched except a `needs_verification` flag; delete/verify/list/select updated for the new source; unit and router tests cover catalog-match, custom registration, duplicate, unsupported, disappeared, needs_verification/select-refused, and delete-keeps-file. Real check: a copied real Parakeet fixture was registered by refresh, selected, and transcribed correctly, then deleted without removing the file | Real-machine drop-in test on a LocalSystem service install (folder recorded by `service::install`, not exercised here); a genuinely mixed drop folder with several simultaneous custom architectures |

The current SDK sends `file`, optional `prompt`, `language`, and `model` to
`/v1/audio/transcriptions`. The current Windows app management client uses provider lifecycle,
hardware, recommendations, model pull/verify/remove/select/load, and operation polling. Provider
processes, descriptors, and hardware-ranked recommendations intentionally have no direct
replacement in the new server API.

The completed nested server `validate-parakeet-performance` goal records four same-machine
short English dictation clips. Its recorded Parakeet RTFs are 0.230–0.255 and its
faster-whisper-small.en RTFs are 0.809–2.607. The underlying exported WAV clips could not be
found in current app data on 2026-09-25, so those results are historical baseline evidence,
not a replayable corpus yet. Add consented long dictation, technical vocabulary, and multiple
languages before a replacement verdict.

## 2026-09-26: safe stop, startup and model-file recovery

Implemented lock-aware graceful stop without PID force termination; lightweight startup
fingerprints recorded after install/verification; preservation of inaccessible model files,
registrations and selected preferences; startup load exclusion for unverified models;
per-file refresh failures with retry and changing-file detection; explicit verification
recovery while retaining confirmed-corruption rejection.

Validation: cargo fmt --check; cargo clippy --release --all-targets --offline -- -D warnings;
cargo test --release --offline: 173 tests passed (171 library + 2 CLI). New regression tests
cover stale live PID, held lock with unavailable shutdown, managed startup without hashing,
locked startup selection, mixed locked/readable refresh and retry, verification access-error
recovery, and actual corruption quarantine. Static release build passed with the repository
build environment. Real binary check passed: isolated start, health, status, restart, stop,
and repeated stop, on port 54471. Test server stopped; no user data directory was used.

Binary size: 66224128 bytes. SHA-256: 2A421EE00E9E65DDA8BD14798694C50E3D32E00228B89A24497DDB41C7D71307.

Limits: metadata checks are not continuous hash verification; an unchanged size/timestamp
is trusted after prior verification. Older installations without fingerprints require one
explicit verification (no download). Selected-model loading cost still applies. No clean-PC,
full-disk, CPU-only, LAN or transcription-quality claims are made by these checks. No release
or commit was made as part of this goal.

## 2026-09-27: capability-matrix sweep findings and Handy gap list

A full-catalog sweep (69 models, default quant, Vulkan; `scripts/catalog_sweep.py`, results
in `default_results.jsonl`) against the selected-model endpoint found:

1. **Bug 1 (timestamp over-claim) -- root cause corrected after reading the vendored engine.**
   33 models advertised `timestamp_granularity: supported` in the effective (selected-model)
   view but a `response_format=verbose_json` + `timestamp_granularities` request returned a raw
   `422 engine_unsupported`: canary-*, cohere-*, Voxtral-Mini-*, Qwen3-ASR-*, Fun-ASR-*,
   granite-*, moonshine-* (all sizes), SenseVoiceSmall. Only whisper and parakeet worked.
   - **First (withdrawn) theory:** that `transcribe_cpp::Model::capabilities().max_timestamp_kind`
     is an optimistic per-engine self-report, like `Feature::InitialPrompt` (Handy #1601), and
     that Handy's engine dispatch (only ever requesting timestamps on its `TranscribeCpp`/
     `Parakeet` branches) was the ground truth to copy. **This was wrong.** Handy has no
     granite/gigaam/medasr code paths to copy from at all, and a
     `capabilities::timestamp_capable_arch` allowlist built on this theory incorrectly reported
     `gigaam-v3-*`, `medasr`, and `granite-speech-4.1-2b-plus` as timestamp-incapable even
     though they genuinely are not.
   - **Actual root cause (verified by reading `transcribe-cpp-sys` 0.2.3's vendored C++ at
     `C:\Users\mariu\.cargo\registry\src\index.crates.io-*\transcribe-cpp-sys-0.2.3\src\arch\`,
     not Handy):** `max_timestamp_kind` IS accurate. Per architecture:
     - `arch/{canary,cohere,moonshine,moonshine_streaming,qwen3_asr,voxtral,sensevoice,
       funasr_nano}/capabilities.cpp` hard-code the family default to
       `TRANSCRIBE_TIMESTAMPS_NONE`, and `transcribe-meta.cpp`'s `read_capability_kv` explicitly
       does **not** overlay `max_timestamp_kind` from any GGUF KV for any family ("max_timestamp_kind
       is NOT read here ... the ceiling is variant-specific and applied in each family's load()"),
       so it stays `NONE` for the model's lifetime. This exactly matches this catalog's own
       per-model `capabilities.timestamps: "none"` claim for every one of those slugs.
     - `arch/granite/model.cpp` (~line 236-250) reads the GGUF's own
       `stt.capability.word_timestamps` bool and lowers the family default `WORD` to `NONE`
       per-variant **before** `capabilities()` is ever read by us -- already correct by the time
       our code sees it (`granite-speech-4.1-2b-plus` claims `"word"`; other granite variants
       claim `"none"`, matching the catalog).
     - `arch/{gigaam,medasr}/capabilities.cpp` set the family default to `TOKEN` and never lower
       it, matching the catalog's `"token"` claim for `gigaam-v3-*`/`medasr`.
     - `transcribe.cpp`'s shared `validate_run_params_common` (used by every family before
       `run()`) ranks `NONE(0) < SEGMENT(1) < WORD(2) < TOKEN(3)` and returns
       `TRANSCRIBE_ERR_UNSUPPORTED_TIMESTAMPS` (our `engine_unsupported`) whenever the request
       rank exceeds the model's ceiling. For the `NONE`-ceiling architectures above, this generic
       check is exactly what produced the sweep's 422s -- a real, correct engine rejection, not
       a bug on either side.
     - **The actual bug was entirely on our side**, in `EffectiveCaps::to_json`'s
       `timestamp_granularity` branch: it reported `Status::Supported` unconditionally whenever
       no prior run had been observed to reject it (`timestamp_granularity_rejected == false`)
       -- it never looked at `self.loaded.max_timestamp_kind` at all. A model whose own,
       accurate engine ceiling was `None` was still advertised "supported" until a live request
       failed once and got remembered.
   - **Fix (final):** removed the `timestamp_capable_arch` allowlist entirely.
     `EffectiveCaps::max_timestamp_granularity` now returns `self.loaded.max_timestamp_kind`
     directly (no gate), and the `timestamp_granularity` control's status is
     `max_timestamp_granularity() != None`. `catalog::capability_matrix` trusts the catalog's own
     `capabilities.timestamps` claim the same way it already trusted `translate`/`lang_detect` --
     no architecture restriction. `run_plan::build_plan` still checks
     `caps.supports_segment_timestamps()` before planning an *explicit*
     `timestamp_granularities=segment` request (this check was previously entirely missing --
     only `word` was gated -- and was the actual code path that reached the engine and came back
     `engine_unsupported`), and falls back to `TimestampGranularity::None` for a *defaulted*
     verbose_json request on an incapable model (`200` with empty `segments`, not a raw engine
     failure).
   - **Gap vs. Handy:** none for the genuinely `NONE`-ceiling architectures -- "report
     unsupported honestly" is correct and final. Handy itself has no code path for
     granite/gigaam/medasr/qwen3_asr/voxtral/cohere/funasr_nano at all (those architectures do
     not exist in Handy's engine set), so "parity with Handy" does not apply to them; the
     correct standard is transcribe-cpp's own, verified, ground truth.

2. **Bug 2 (gigaam-v3-*, medasr: `200` but no usable timestamps) -- also root-caused in the
   engine, not gated away.** `arch/gigaam/model.cpp` and `arch/medasr/model.cpp`'s `run()`
   ignores the requested timestamp granularity entirely (the `params` argument to `run()` is
   unused for this purpose) and always fills only `transcript.tokens` (real per-token
   `t0_ms`/`t1_ms`, computed as `frame_index * 40ms` from the CTC/RNN-T decoder's kept frames)
   -- never `transcript.segments` or `.words`, unlike parakeet/whisper which always populate
   `segments`. Because these two families' engine-reported ceiling genuinely is `TOKEN`
   (confirmed above), the request is never rejected; the transcribe-cpp Rust wrapper
   (`session.rs`) faithfully exposes `Transcript::tokens: Vec<Token>` with real timing, but
   `format::format_response` never read it -- only `segments`/`words` -- so a model that
   produced real timing data looked like it produced nothing.
   - **Fix:** `format::format_response` now synthesizes a `segments` row (start/end from the
     first/last token's `t0_ms`/`t1_ms`, text = the full transcript) whenever `transcript.segments`
     is empty but `transcript.tokens` is not and timestamps were requested; for an explicit word
     request with `transcript.words` empty but `transcript.tokens` populated, each token is
     emitted as a word row (an honest, finer-than-segment approximation -- not always a
     linguistic word, since these are CTC/RNN-T subword units, but real per-unit timing rather
     than nothing).
   - **Gap vs. Handy:** N/A -- GigaAM/MedASR do not exist in Handy at all.

3. **Bug 3 (catalog list always "unsupported") -- same correction applied.** `GET
   /v1/local/models`'s `catalog::capability_matrix` hard-coded `"status": "unsupported"` for
   every control of every model via its `unimplemented(...)` helper, regardless of the catalog's
   own claims -- a left-over placeholder, not a truthful static answer.
   - **Fix:** rewrote `capability_matrix` to trust the catalog's own per-model claim for every
     control uniformly, no architecture allowlist: `language_hint`/`language_detect`/
     `translation`/`timestamp_granularity` are `"unknown"` (unverified) when the catalog claims
     them, `"unsupported"`/`model_lacks` otherwise. `prompt`/`temperature` are `"unknown"` only
     on `architecture == "whisper"` -- this one genuinely is architecture-restricted in *our own
     code* (see the language/translate/prompt table below: only whisper's run extension carries
     a user-supplied free-text prompt), not a claim about the engine as a whole.
     `"unknown"` never counts as a `catalog_mismatch` against the live view, and the catalog view
     is documented (`docs/client-contract.md` 4.1) as non-authoritative once a model is loaded.

4. **Bug 4 (moonshine-streaming-tiny timeout) -- investigated further, still open.** See the
   dedicated section below.

5. **Bug 5 (language_hint truthfulness) -- verified, one new gap found, not yet fixed.**
   `run_plan::resolve_language` already tracks `language_hint_applied` separately from whether a
   hint was *provided* and from `language_evidence_hint`, so a hint dropped for a model with no
   language mechanism at all (empty `caps.languages`) is reported honestly. However, reading
   `arch/granite/model.cpp` found that **granite never reads `params->language` in `run()` at
   all** (grep for `params->language` across every `arch/*/model.cpp` found it in canary, cohere,
   qwen3_asr, sensevoice, funasr_nano, parakeet, voxtral -- every arch that has any language
   mechanism -- except granite). Granite's `caps.languages` is still populated informationally
   from the GGUF's `general.languages` (a list of languages the model was trained on, not a
   hint slot), so if a granite GGUF lists multiple languages, `EffectiveCaps::
   supports_language_hint()` (`!self.loaded.languages.is_empty()`) and `catalog::capability_matrix`'s
   `multi_language` check would currently advertise `language_hint` as usable even though sending
   one has zero effect on granite's output -- a genuine, not-yet-fixed instance of exactly the
   silent-drop concern Bug 5 asks about. **Not fixed in this pass** (found during the
   orchestrator-requested per-architecture mechanism review, out of the original fix's scope);
   see the table below and the "Remaining gaps" list.

### Per-architecture mechanism review: language hint / translate / prompt (engine vs. our call)

Read directly from `transcribe-cpp-sys` 0.2.3's `arch/*/model.cpp` (grep for
`params->language`, `params->task`/`target_language`, and any free-text prompt input) and
compared against `run_plan::Plan::to_run_options`, which sets `RunOptions{ language, task,
target_language, family }` **identically for every architecture** (never gated per-arch except
`family`, the whisper-only run extension).

| Arch | Language hint (engine mechanism) | Translate (engine mechanism) | Prompt (engine mechanism) | Our call | Finding |
|---|---|---|---|---|---|
| whisper | `params->language` (LID token) | `params->task==TRANSLATE` (always -> English, no target needed) | `WhisperRunOptions.initial_prompt`/`.temperature` via `TRANSCRIBE_EXT_KIND_WHISPER_RUN` ext slot | generic `language`/`task` + `family: Whisper(...)` when prompt/temperature set | Correct, reference case |
| canary | `params->language` (multitask prompt lang slot, default "en") | `params->task==TRANSLATE` + `params->target_language` (multitask prompt) | none (fixed multitask prompt; PNC/nopnc token only) | generic `language`/`task`/`target_language` | Correct -- no drop |
| cohere | `params->language` (decoder prompt lang token, default "en") | `supports_translate=false`, no mechanism | none | generic `language`; translate gated off | Correct |
| moonshine | none (`accepts_ext_kind=nullptr`; English-only, no `params->language` read) | none | none | We only offer `language`/`prompt` when `caps` says so; moonshine's mono `caps.languages` correctly gates both off | Correct |
| moonshine_streaming | none (same as moonshine) | none | none | same | Correct (see Bug 4 for the separate timeout issue) |
| qwen3_asr | `params->language` -> `encode_language_prefix` | `supports_translate=false` | none | generic `language` | Correct |
| **granite** | **none** -- `run()` never reads `params->language`; `caps.languages` is informational-only metadata from `general.languages` | `params->task==TRANSLATE` + `params->target_language` (fixed instruction template, e.g. "can you translate the speech into X?") | fixed instruction template selected by task/diarize/timestamps; no user-editable free text | We send `language` unconditionally (harmless no-op: granite ignores it) but gate `language_hint` support purely on `caps.languages` non-empty | **Gap (Bug 5 addendum, not fixed):** if a granite GGUF lists >1 language, we would advertise `language_hint: supported` even though it is silently ignored by the engine. Translate is correctly wired (generic `task`/`target_language` matches granite's own read). |
| voxtral | `params->language` (used inside a fixed instruction string) | `params->task==TRANSLATE` + `params->target_language` (fixed "Translate this to X." instruction) | `FEATURE_INITIAL_PROMPT` is set `true` in `capabilities.cpp` with a comment claiming a "Voxtral run extension" carries free text, but `accepts_ext_kind = nullptr` and `model.cpp` never reads any user-supplied prompt/instruction string anywhere -- **the flag and its comment do not match the implementation** | We correctly never expose `prompt` for voxtral (`is_whisper_arch` gate) | No drop, but validates our existing "never trust `Feature::InitialPrompt`" design (Handy #1601) as necessary *inside transcribe-cpp itself*, not just as a Handy quirk |
| sensevoice | `params->language` (mapped to zh/en/ja/ko/yue tokens) | `supports_translate=false` | none (ITN toggle only) | generic `language`; translate gated off | Correct |
| funasr_nano | `params->language` (feeds system-prompt language/ITN construction) | `supports_translate=false` | none (ITN toggle only) | generic `language`; translate gated off | Correct |
| gigaam | none (monolingual Russian) | `supports_translate=false` | none | We correctly offer neither (mono `caps.languages`) | Correct |
| medasr | none (monolingual English) | `supports_translate=false` | none | We correctly offer neither | Correct |
| parakeet | `params->language` (looked up in a prompt-conditioned dictionary; only for `has_prompt` variants) | `supports_translate=false` | none | generic `language`, gated by `caps.languages` | Correct |

**Summary:** language hint and translate are passed through **uniformly and correctly** for
every architecture via the same generic `RunOptions{ language, task, target_language }` fields,
which is exactly the mechanism every arch's `model.cpp` reads from (no whisper-only or
per-arch-specific call needed on our side -- there is no "wrong ext slot" or "wrong struct"
bug for these two controls). Prompt is correctly whisper-only, because that is genuinely the
only architecture with a user-editable free-text mechanism in the engine today (voxtral's
advertised prompt support is a stale/inaccurate flag inside transcribe-cpp itself). The one real
gap found is granite's `language_hint` being advertisable when the engine cannot honor it
(listed above, not yet fixed).

### Bug 4: moonshine-streaming-tiny timeout -- investigation (no code change)

Handy's `LoadedEngine::MoonshineStreaming` branch (`managers/transcription.rs`) calls
`.transcribe(&audio, &TranscribeOptions::default())` -- a single one-shot batch call over the
whole buffer, exactly like every other non-streaming engine variant, never `stream_feed`/
`stream_finalize`. Our server does the same: a single `session.run(&audio, &run_options)` call,
uniformly for every architecture, with no special-casing for `moonshine_streaming`. **We are not
misusing the streaming API or driving it incorrectly; our calling convention matches Handy's
exactly.**

`arch/moonshine_streaming/model.cpp`'s plain (non-stream) `run()` entry point calls a dedicated
`run_one_shot_inner()` -> `decode_from_kv_cache()` path (distinct from the incremental
`stream_feed`/`stream_finalize` hooks), so the one-shot path is a real, intentional
implementation, not a misuse of the streaming hooks. Its greedy AR decode loop
(`while (next_token != eos && n_past < gen_cap)`) is bounded by `gen_cap` (the tighter of a
duration-based budget and `hp.dec_max_position_embeddings`), so an unbounded/infinite loop there
is unlikely for correctly-loaded weights.

**Likely root cause (not confirmed -- would require running/profiling the debug build, which
was not available for this read-only pass):** something specific to the `-tiny` variant's
hyperparameters or tensor shapes in the earlier stages -- `encode_window_to_host`,
`apply_adapter_window`, `ensure_kv_cache_for_T`, or the cross-KV graph build/compute
(`ggml_backend_sched_alloc_graph`/`ggml_backend_sched_graph_compute`) -- rather than the bounded
decode loop itself. Other moonshine-streaming sizes (small/medium) working rules out a
universal bug in the one-shot path and points at a `-tiny`-specific hyperparameter (e.g. a
mis-sized `n_ctx`/window/frame-length constant for that checkpoint) causing pathologically slow
(not necessarily infinite) graph construction or compute, especially under Vulkan.

**Proposed fix (not applied):** re-run `moonshine-streaming-tiny` alone with
`transcribe::debug::enabled()` (its dump/logging path is already wired throughout
`moonshine_streaming/model.cpp`) to see which stage the time is spent in, or add a wall-clock
timeout around the FFI `run()` call using the session's existing `poll_abort()`/cancellation
hook (already used by every family) so a hang degrades to a clean timeout/cancel instead of
consuming the full HTTP request timeout with no diagnostic. This needs the debug exe and a real
run against that specific model, which is out of scope for this read-only pass.

### Remaining gaps vs. Handy (this pass)

- Granite `language_hint` may be falsely advertised as supported (see the mechanism table
  above) -- not yet fixed.
- Moonshine-streaming-tiny timeout (Bug 4) is unresolved -- root cause narrowed to the one-shot
  encode/adapter/cross-KV stages, not the bounded decode loop, but not confirmed without a real
  run.
- No `prompt_max_tokens` ceiling is exposed by transcribe-cpp 0.2.3 (documented in
  `capabilities.rs`'s field doc already); Handy does not expose one either via its own
  `WhisperRunOptions`, so this is parity, not a gap.
- Handy supports SRT/VTT export in its own UI layer (client-side), which this server
  intentionally does not own (`response_formats` correctly reports `srt`/`vtt` unsupported).
- Granite/GigaAM/MedASR/Qwen3-ASR/Voxtral/Cohere/Fun-ASR-Nano have no equivalent engine or UI
  code in Handy at all, so "parity with Handy" is not a meaningful standard for them; transcribe-
  cpp's own source is the correct ground truth and was used instead for this pass.

### Verification

See the Verification Log entry in
`.projectflows/goals/ready/handy-gguf-parity-evidence/GOAL.md` for gate results and per-model
before/after sweep evidence for this pass. The orchestrator is running the real-model sweep
verification directly against the debug build; this document records the source-level root
cause analysis and the code fix, not sweep output captured by this pass.
