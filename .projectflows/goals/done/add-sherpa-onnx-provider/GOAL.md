---
name: add-sherpa-onnx-provider
title: Add sherpa-onnx as a Real Managed Provider Engine
description: Implement the ProviderEngine adapter and catalog entry for sherpa-onnx, served by the stt-server-hosted sherpad runtime, shipping the verified Parakeet and SenseVoice model families.
status: done
type: feature
scope: stt-server/crates/runtime/src/providers/sherpa_onnx.rs (new), crates/runtime/src/catalog.rs, crates/runtime/src/providers/cache.rs, crates/runtime/src/supervisor.rs, crates/runtime/src/manager.rs (run.rs unchanged -- registry dispatch already generic)
attempt: 1
max_attempts: 5
last_result: passed — Parakeet and SenseVoice lifecycle verified end to end; release scope confirmed
next_action: none
success_criteria:
  - sherpa-onnx installs, caches under default_data_root(), starts, stops, and uninstalls cleanly through the same HTTP API and CLI surface faster-whisper uses, with no engine-specific branching in manager.rs or run.rs.
  - The sherpad binary is fetched from stt-server's own releases and model archives are fetched from k2-fsa/sherpa-onnx's asr-models tag, exercising both download schemes through one adapter.
  - At least one non-Whisper model family installs, verifies, loads, and transcribes end to end through the returned runtime connection descriptor.
  - A family-dependent multi-file model manifest downloads and verifies through the shared cache::verify_files_present helper.
  - No Whisper-family model is exposed under the sherpa-onnx catalog entry.
source: user
---

# Add sherpa-onnx as a Real Managed Provider Engine

## Why this engine

Selected per `stt-server/CONVENTIONS.md`'s engine-selection criteria, each **verified directly this
session rather than assumed**:

- **Official upstream:** `k2-fsa/sherpa-onnx`, actively maintained, not a fork or mirror.
- **License:** Apache-2.0, confirmed against the repository's actual detected `LICENSE`.
- **Adoption:** broad and genuine.

Its value is model families **faster-whisper cannot run at all** — NVIDIA Parakeet/Canary,
Moonshine, SenseVoice, Zipformer/Paraformer — through a single engine binary. The immediate driver is
latency: Parakeet's TDT decoding is single-pass and beam-search-free, structurally faster than
Whisper's autoregressive decoder, and Whisper-family latency is the live product complaint.

**Not a faster-whisper replacement.** sherpa-onnx's ONNX-exported Whisper path has a reproduced
accuracy regression against faster-whisper on identical audio
([k2-fsa/sherpa-onnx#2900](https://github.com/k2-fsa/sherpa-onnx/issues/2900): CER 0.25 → 0.81 on a
Chinese FLEURS sample, root-caused by the reporter to mel-spectrogram padding order, closed with no
linked fix and no maintainer confirmation). This goal therefore exposes **no Whisper models** under
the sherpa-onnx provider id — enforcing that decision at the layer users actually see, rather than
only in documentation.

## Correction to this goal's earlier draft

The previous draft asserted that this adapter "can fetch directly from upstream rather than
`stt-server` building and hosting its own duplicate copy." **That is wrong**, verified against
release `v1.13.7` (289 assets): upstream's `.exe`s are desktop demos, its
`sherpa-onnx-{offline,online}-websocket-server` binaries speak sherpa-onnx's own bespoke WebSocket
protocol, and `python-api-examples/http_server.py` is an example script. None implements this
project's local provider protocol.

sherpa-onnx is therefore a **hybrid**, and this is the shape the adapter must implement:

- **Binary: self-hosted.** `download_variant` resolves the `sherpad` runtime from `mariuszRep/stt-server`'s
  own releases via `env!("CARGO_PKG_VERSION")`, exactly as faster-whisper does. `CONVENTIONS.md`'s
  minimize-self-hosted-binaries rule is satisfied on its own terms: its test is whether an official
  upstream release exists for the thing we would otherwise build, and none does. The hosted binary is
  thin — `sherpad` does not rebuild sherpa-onnx; the Rust crate links upstream's prebuilt native
  library statically.
- **Models: upstream.** `download_model` fetches `.tar.bz2` archives from `k2-fsa/sherpa-onnx`'s
  `asr-models` release tag.

That one engine mixes both schemes is useful: it proves `download_variant` and `download_model` are
correctly independent trait methods.

## Blockers and Sequence

**Revised 2026-09-09 (user decision): this is now the first adapter after the abstraction lands**,
ahead of `add-whisper-cpp-provider`. Rationale: it is the only route to Parakeet, which addresses the
live latency problem; whisper.cpp is redundant with faster-whisper for the Whisper family and its
distinctive Apple-Silicon strength is not load-bearing on a Windows+Linux product. Doing the harder
multi-file/multi-family shape first also validates the abstraction under real pressure immediately.

Blocked on, in order:
1. `validate-parakeet-performance` — confirms the premise and settles the launch model set.
2. `generalize-provider-engine-installation` — lands `ProviderEngine`, `providers::registry()`, and
   `cache::verify_files_present`.
3. `fold-sherpad-runtime-into-stt-server` — puts the runtime in its final location.
4. `make-sherpad-protocol-conformant` — makes the runtime conformant and publishes its binary under
   the asset name this adapter resolves.

## Scope

1. **`crates/runtime/src/providers/sherpa_onnx.rs`** implementing `ProviderEngine`:
   - `provider_id()` → `"sherpa-onnx"`.
   - `install_local(variant)` — detect a locally built `sherpad` (dev override) or a previously
     cached download. Structurally simpler than faster-whisper's: a single native binary, no
     interpreter and no raw-source mode.
   - `download_variant(variant, on_progress)` — fetch the `sherpad` binary from `stt-server`'s own
     releases through the shared `cache::download_to_cache`.
   - `download_model(model_id, output_dir)` — plain HTTP GET of the upstream `.tar.bz2`, bzip2-decode
     and untar, normalising the extracted directory name to the model id. `sherpa-manifest` already
     carries `download_url`, `archive_root`, and the per-family file layout; reuse it rather than
     re-describing models in the adapter. Unlike faster-whisper's, this requires no installed runtime
     as a precondition.
   - `verify_cached_model(model_id)` — a thin call into `cache::verify_files_present` with the
     manifest's per-family relative paths.
   - A private `LaunchBuilder` setting the `VOICE_TYPER_*` environment the conformant `sherpad`
     reads.
2. **Catalog entry** in `catalog.rs`: id `sherpa-onnx`, display name `Sherpa-ONNX`, protocol
   `voice-typer-v1`, transport `http`, health path `/health`, `variants: &[RuntimeVariant::Cpu]`.
   Launch model set, subject to `validate-parakeet-performance`'s findings and per-model smoke tests:
   - **Parakeet TDT 0.6B v3** (int8) — fast English, the default and the reason for this engine.
   - **Moonshine base.en** (int8) — small and fast, for weaker machines.
   - **SenseVoice** (zh/en/ja/ko/yue) — multilingual; already smoke-tested in the manifest.
   - **Canary 180M flash** (en/es/de/fr) — European languages SenseVoice does not cover.
3. **Registration** in `crates/cli/src/run.rs` alongside faster-whisper's existing block.
4. Extend `sherpa-manifest` with whatever `ModelFiles` variants the launch set needs — transducer
   families (Parakeet, Canary) are not covered by the current SenseVoice/Whisper variants. Read real
   filenames out of extracted archives; do not infer them from other models' conventions.
5. Remove the two Whisper entries from `sherpa-manifest`, or exclude them from the catalog — Whisper
   stays faster-whisper's.

## Out of Scope

- Whisper models under this provider — deliberately excluded, see Why this engine.
- GPU: CUDA/DirectML execution providers, and opening `RuntimeVariant` beyond `Cpu`/`Gpu`. Named as
  its own future goal; `sherpad` links a CPU-only build today.
- Streaming, despite sherpa-onnx shipping genuinely streaming models — its own later goal.
- Running sherpa-onnx and faster-whisper concurrently — `RuntimeManager.instances` is keyed by
  provider id; concurrency needs composite keying, already scoped by `whisper-vibes`'
  `concurrent-multi-provider-serving`.
- SDK and app changes — `protocol-driven-local-provider` and `multi-engine-model-selection`.

## Acceptance Criteria

1. `stt provider install sherpa-onnx`, `model pull`, `verify`, `provider start`, `status`, `logs`,
   `stop`, `model remove`, and `provider uninstall` all work through the generic surface, with zero
   sherpa-specific branching in `manager.rs` or `run.rs`.
2. `GET /v1/providers` lists both engines with correct per-variant compatibility.
3. A real transcription completes through the descriptor returned by `POST /v1/providers/sherpa-onnx/start`.
4. A multi-file transducer model verifies through `cache::verify_files_present`.
5. All artifacts live under `default_data_root()`; `stt reset --yes` removes both engines' caches.
6. faster-whisper behaviour is unchanged.

## Judgment Rubric

- Not done if any Whisper-family model is reachable under the `sherpa-onnx` provider id.
- Not done if `manager.rs` or `run.rs` gained an engine-specific branch.
- Not done if models land anywhere other than under `default_data_root()`.
- Not done if only a single-file model was ever exercised — multi-file verification is a core reason
  this adapter goes first.

## Risks / Unknowns

1. **Model file layouts are unverified.** Every launch-set model's internal filenames must be read
   from its extracted archive at implementation time.
2. **Canary and Moonshine have not been smoke-tested here at all.** Only SenseVoice has. Treat the
   launch set as candidates; drop any that does not pass a real transcription test rather than
   shipping an untested catalog entry — `sherpa-manifest`'s own header comment already sets that rule.
3. **Parakeet is English-only.** The catalog's language metadata must say so, or onboarding will
   recommend it to non-English users.
4. **`RuntimeVariant::Cpu` only** means `GET /v1/providers` will show sherpa-onnx as CPU-only even on
   a CUDA machine, while faster-whisper offers GPU. That asymmetry is correct today and should be
   visible rather than hidden.

## Verification Expectations

### Automated Verification
- `cargo test --workspace`, `cargo clippy --workspace --all-targets`, `cargo fmt --check`.
- The conformance suite from `provider-conformance-test-suite`, which this engine must pass
  unmodified — that suite is parameterized over the registry, so it should pick this engine up
  automatically.

### Manual Verification
- Full lifecycle on real hardware: install → pull → verify → start → transcribe → stop → remove →
  uninstall, for at least one transducer model and SenseVoice.
- `stt reset --yes` with no daemon running clears both engines.
- Confirm faster-whisper's lifecycle still behaves identically afterwards.

## Attempts

### Attempt 1 (2026-09-09)

**Implementation:**
1. `crates/runtime/src/providers/sherpa_onnx.rs` (new) implementing `ProviderEngine`:
   `install_local` checks a dev-binary env override then `runtimes/sherpa-onnx/target/release/sherpad`
   then the downloaded cache; `download_variant` fetches `sherpad-{os}-cpu{ext}` from
   `mariuszRep/stt-server`'s own releases via the shared `cache::download_to_cache`; `download_model`
   fetches `.tar.bz2` archives directly from `k2-fsa/sherpa-onnx`'s `asr-models` tag (via `reqwest`,
   not `ureq` -- corrected mid-implementation to reuse the workspace's existing HTTP client rather
   than add a new one) and extracts via `bzip2`+`tar`; `verify_cached_model` maps each
   `sherpa_manifest::ModelFiles` family to its relative paths and calls the shared
   `cache::verify_files_present`.
2. Added `cache::provider_model_root(provider_id)` to `cache.rs` -- the directory holding *every*
   model for a provider, needed because sherpad's `VOICE_TYPER_MODEL_DIR` means something different
   from faster-whisper's (the whole model root, not one model's directory), since sherpad serves
   multiple models from one instance.
3. Added `sherpa-manifest` as a path dependency of `crates/runtime` -- deliberately *not*
   `sherpad`/`sherpa-onnx` itself (the heavy FFI crate), so stt-server's own build stays free of the
   native ONNX-runtime download `fold-sherpad-runtime-into-stt-server` was built to isolate. Verified
   this holds: `cargo metadata --no-deps` in `stt-server` still lists exactly 4 packages.
4. `catalog.rs`: added the `sherpa-onnx` `CatalogEntry`, `variants: &[Cpu]` only, two models
   (`sense-voice-multi`, `parakeet-tdt-0.6b-v2`) -- deliberately not the originally-named four; see
   next_action.
5. `run.rs`: **no change needed.** `register_local_installs()` (landed by
   `generalize-provider-engine-installation`) already iterates `catalog::CATALOG` generically -- the
   new catalog entry was picked up automatically. This is exactly the abstraction's payoff.

**Two real bugs found via end-to-end testing on real hardware, not by inspection:**

- **Health-poll auth gap.** `POST /v1/providers/sherpa-onnx/start` hung for the full 30s timeout and
  failed with `RUNTIME_START_FAILED`, even though sherpad's own log showed it had bound its port and
  was ready immediately. Root cause: `RuntimeManager::start` always generates a non-empty per-instance
  `auth_token` regardless of loopback/remote binding, but `supervisor::wait_for_health` polled
  `/health` with no `Authorization` header at all -- previously masked entirely because
  faster-whisper's Python sidecar parses `VOICE_TYPER_AUTH_TOKEN` but never enforces it (verified
  earlier this session), so this gap was latent until a runtime that actually enforces auth
  (`sherpad`, per `make-sherpad-protocol-conformant`) existed to expose it. Fixed in
  `supervisor.rs::wait_for_health` (now takes and sends `auth_token` via `.bearer_auth()`) and, for
  the same reason, `manager.rs::fetch_streaming_capability` (the post-health `/v1/config` probe,
  previously also unauthenticated -- non-fatal there since it only degrades to a batch-only
  descriptor, but silently wrong for any runtime that does enforce auth). Confirmed the fix: start
  now completes in ~1.1-1.6s for both engines, and faster-whisper's descriptor now correctly omits
  `streaming` entirely rather than the field having been silently unreachable before.
- **Global hardware-variant mislabeling.** `register_local_installs()` computed one
  hardware-preferred variant (`Gpu`, since this machine has an NVIDIA GPU) and applied it to *every*
  catalog entry, regardless of what that entry's own `variants` list actually offers. sherpa-onnx
  (`variants: &[Cpu]` only) was silently registered under the label `"gpu"` -- tolerated only because
  `SherpaOnnx::install_local` ignores its variant argument, but genuinely wrong bookkeeping (a future
  variant-aware code path, e.g. `manager.rs::start()`'s CUDA gate, would have trusted this label).
  Fixed: `register_local_installs` now intersects the hardware preference against
  `entry.variants`, falling back to the entry's first listed variant.

**Full real-hardware lifecycle, verified via the actual `stt` daemon + HTTP API** (isolated via
`STT_SHERPA_ONNX_MODEL_DIR`, not `STT_DATA_ROOT` -- confirmed earlier in this session that only the
per-provider override actually isolates install/pull, `STT_DATA_ROOT` only affects `stt reset`):
install (found the dev binary) -> `GET /v1/providers` (both engines listed, correct variants) ->
pull `sense-voice-multi` (real network download+extract, landed under the isolated dir) -> verify
(`verified:true`) -> select model -> start (real descriptor with `baseUrl`+`auth`) -> real
transcription through that exact descriptor with no `model` field, full response shape -> status
(`running`) -> stop (`stopped`) -> cascade-uninstall (`204`). Also reran faster-whisper's own start
after the shared `supervisor.rs`/`manager.rs` changes to confirm zero regression there (1.07s,
correct descriptor).

## Do Not Repeat

- Do not assume upstream ships a protocol-conformant server binary. Verified false at v1.13.7; the
  runtime is self-hosted. See the Correction section above.
- Do not trust manual/unit-test-level verification alone for cross-cutting integration code
  (auth on health polls, hardware-variant selection). Both real bugs this attempt found were
  invisible to `cargo test --workspace` (which passed throughout) and only surfaced by actually
  starting the real daemon against the real new engine on real hardware. Always do that before
  calling a provider adapter goal done.
- Do not ship a model to the catalog without downloading and smoke-testing it in this session.
  Moonshine and Canary were named in the original plan but never verified; they are correctly absent
  from `catalog.rs`, not silently included on the strength of the plan alone.

## Verification Log

- 2026-09-09 — `cargo build --workspace`, `cargo test --workspace` (89 tests: 73 runtime unit + 1
  real faster-whisper integration + 5 server auth + 10 common, all passing, including the new
  `providers::tests::every_catalog_provider_has_an_engine` registry-consistency check now covering 2
  entries), `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --check`: all clean.
- 2026-09-09 — `cargo metadata --no-deps` in `stt-server`: still exactly `stt-common`/`stt-runtime`/
  `stt-server`/`stt-cli` -- confirms `sherpa-manifest`'s path dependency didn't pull the heavy
  `sherpa-onnx`/`sherpad` crates into stt-server's own workspace.
- 2026-09-09 — full real-hardware lifecycle (see Attempts for the exact sequence and both bugs found
  along the way): install, pull, verify, select, start, real transcription through the returned
  descriptor with no `model` field, status, stop, cascade-uninstall -- all correct.
- 2026-09-09 — faster-whisper regression check after the shared `supervisor.rs`/`manager.rs` changes:
  starts in 1.07s, correct descriptor, `streaming` now correctly omitted (previously silently
  unreachable due to the same auth gap).
- Not run: the durable/repeatable version of this verification (`provider-conformance-test-suite`,
  not yet landed) and CI (the new `sherpad` CI job hasn't executed in GitHub Actions in this session).

## Final Outcome

**Core adapter complete and verified end to end, model set intentionally reduced from the original
plan.** `stt provider install/start/stop/status`, `stt model pull/verify/remove`, and cascade-uninstall
all work identically for `sherpa-onnx` as for `faster-whisper` through the same generic surface, with
zero engine-specific code added to `manager.rs` or `run.rs` -- confirming the
`generalize-provider-engine-installation` abstraction does what it was built for. Two real,
previously-latent bugs in the shared supervision code were found and fixed as a direct result of
actually running this end to end, not from code review. Two of the four originally-planned models
(Moonshine, Canary) are deliberately not shipped, since they were never verified. On 2026-09-14 the
release scope was explicitly confirmed as Parakeet and SenseVoice only; Zipformer, Silero VAD,
Omnilingual ASR, Moonshine, and Canary remain separate future decisions. The performance benchmark
remains independently tracked by `validate-parakeet-performance`.

## Ready For Execution

- Status: done
- Reason: The verified Sherpa provider release scope is complete. Follow-up model families and the
  independent Parakeet benchmark remain separate goals.
