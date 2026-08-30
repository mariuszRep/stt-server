---
name: generalize-provider-engine-installation
title: Generalize Provider/Engine Installation Into a Pluggable, Catalog-Driven Architecture
description: Replace the faster-whisper-hardcoded install/cache/variant/model logic in RuntimeManager with a small ProviderEngine trait and shared, engine-agnostic machinery, so adding whisper.cpp, sherpa-onnx, or any future engine is a catalog entry + a thin adapter, not a parallel reimplementation.
status: ready
type: refactor
scope: stt-server/crates/runtime (catalog.rs, manager.rs, providers/, cli/src/run.rs)
attempt: 0
max_attempts: 5
last_result: none
next_action: |
  Design/reading pass is complete (see Design Validation below) — a Plan-agent design pass read
  manager.rs, faster_whisper.rs, catalog.rs, cli/src/run.rs, routes/providers.rs, and
  routes/models.rs in full and validated the ProviderEngine trait shape against the real code,
  including sketching both a whisper.cpp and a sherpa-onnx adapter to prove genericity against
  two structurally different engines. Start the first real attempt with:
  1. Add `crates/runtime/src/providers/cache.rs`: `variant_dir(provider_id, variant)`,
     `model_dir(provider_id, model_id)`, `download_to_cache(url, dest_dir, filename,
     make_executable, on_progress)`, `verify_files_present(dir, &[relative_paths])`.
  2. Define the `ProviderEngine` trait in `providers/mod.rs` (see Architecture Notes) and
     `providers::registry() -> HashMap<String, Box<dyn ProviderEngine>>`.
  3. Migrate `faster_whisper.rs` onto the trait with zero behavior change — same binary, same
     release repo, same download URL shape, same cache layout. Re-run
     `bootstrap-local-stt-server`'s real-hardware verification steps afterward to confirm nothing
     regressed before trusting the new engines.
  4. Fix the four corrections found during design validation (see Design Validation) as part of
     this same attempt, not deferred: the `run.rs` startup-registration call site, the
     `uninstall()` cascade using the specific provider's own `entry.variants`, registry
     construction living in `providers/mod.rs`, and the explicit `RuntimeVariant` scope decision.
  5. Implement `whisper_cpp.rs` and `sherpa_onnx.rs` for real (not just the design-pass sketches),
     confirming the trait boundary holds against real upstream release asset layouts — the exact
     file manifests were not verified during design (see Risks/Unknowns).
success_criteria:
  - RuntimeManager's install/uninstall/model-pull/verify/remove methods dispatch through a provider registry, not a hardcoded `if id.as_str() != "faster-whisper"` check.
  - Download-with-progress, atomic rename-on-completion, per-variant cache directories, per-model directories, and uninstall cascade-delete are shared, engine-agnostic code, parameterized by provider id — not duplicated per engine.
  - Adding a second and third engine (whisper.cpp, sherpa-onnx) requires only a catalog entry plus one new file implementing the ProviderEngine trait — no changes to manager.rs's dispatch logic.
  - faster-whisper's existing behavior (install, model pull/verify/remove, cascade-uninstall, CPU/GPU variant caching) is fully preserved with zero regressions after the refactor — verified against the same real end-to-end tests bootstrap-local-stt-server already established.
  - New engine adapters fetch release assets from their own upstream project's official releases by default, never rebuilding/re-hosting a duplicate binary the way faster-whisper has to (see CONVENTIONS.md's "Minimize binaries the server itself builds and hosts").
source: user
---

# Generalize Provider/Engine Installation Into a Pluggable, Catalog-Driven Architecture

## Goal

Make `stt-server` genuinely capable of managing multiple local STT engines with minimal
per-engine maintenance burden — today it manages exactly one (faster-whisper), and every
install/cache/variant/model operation is hardcoded to that one engine's id and shape. This goal
generalizes the Rust-side machinery so whisper.cpp and sherpa-onnx — both real, planned engines,
not hypothetical future ones — and any engine beyond those, are a small, additive change, not a
parallel reimplementation of everything `bootstrap-local-stt-server` already built.

## Source Requirements

User, this session, after `bootstrap-local-stt-server` landed and a GitHub Actions storage
quota incident prompted a broader architecture review: *"with stt-server we need to find an
efficient way of installing and uninstalling... we need in the future support for multiple
providers faster-whisper whisper.cpp and others from nvidia + other popular local engines"* —
explicitly asked for a solution that is "easy flexibility but also min effort so as little
maintenance from our side." Later in the same session, after research into candidate engines and
a Plan-agent design-validation pass, the user explicitly confirmed the target roster: *"lets
design with mind that all 3 will be implemented and eny other but we keep faster-whisper as
priotrity"* — faster-whisper, whisper.cpp, and sherpa-onnx are all real planned engines (not one
deferred in favor of another), with faster-whisper's existing shipped behavior never
destabilized.

## Problem / Motivation

Today, `crates/runtime/src/manager.rs::RuntimeManager::begin_install()` (and its siblings
`uninstall()`, `begin_model_pull()`, `verify_model()`, `remove_model()`) all contain an
explicit `if id.as_str() != "faster-whisper" { return Err(RuntimeError::ProviderNotFound) }`
gate before doing anything — meaning even though `catalog.rs`'s `CatalogEntry` type is already
data-driven and *could* list other providers, the actual install/download/cache logic only
ever works for the literal string `"faster-whisper"`. A fourth call site has the same problem:
`crates/cli/src/run.rs`'s daemon-startup auto-registration also hardcodes `"faster-whisper"` and
calls `faster_whisper::install_local` directly — not previously identified until this session's
design-validation pass (see Design Validation). All of the actual install/cache/uninstall logic
— variant caching (`cached_variant_dir`), model caching (`cached_model_dir`), local/dev-source
detection (`install_local`, `locate_runtime_dir`, `detect_runtime_kind`), download-with-progress
(`download_variant`, `download_model`), and env/launch-arg construction (`build_env`,
`packaged_launch_builder`, `raw_source_launch_builder`) — lives in one file,
`crates/runtime/src/providers/faster_whisper.rs`, written directly against that one engine's
shape. Adding whisper.cpp or sherpa-onnx today would mean copying that whole file and threading
new `if provider == "..."` branches through `manager.rs` and `run.rs` everywhere — the exact
high-per-engine-maintenance outcome this goal exists to avoid.

## Vision Alignment

Reviewed against all four repos' `VISION.md`/`CONVENTIONS.md` before drafting (full citations
recorded in this session's conversation; summarized here):

- **No repo commits to a specific packaging technology** (PyInstaller, pip, embeddable Python,
  etc.) for provider engines — PyInstaller is only ever `stt-server`'s current *implementation
  detail* for faster-whisper (`README.md`, `bootstrap-local-stt-server`'s own attempt log),
  never an architectural decision. This goal has real latitude on packaging *technique* per
  engine; what's binding is the *extensibility contract*, not the packaging mechanism.
- **What is consistently binding, across every doc that touches this**: new engines are meant
  to be added as separate "runtime adapters" / SDK "named adapters" / `stt-server` "typed
  seams" that all speak the same shared local provider protocol (`GET /v1/info`,
  `GET /v1/models`, `POST /v1/audio/transcriptions`, `WS /v1/audio/transcriptions/stream` —
  `voice-typer/CONVENTIONS.md:15-24`). The protocol contract is the extensibility seam, not the
  packaging mechanism — this goal's `ProviderEngine` trait boundary should sit on the Rust
  install/lifecycle side of that same seam.
- Install/update/remove must stay explicit, observable (real progress for large downloads), and
  never invisible during inference (`stt-server/CONVENTIONS.md`) — the generalized
  machinery must preserve this, not just faster-whisper's current implementation of it.
- CTranslate2 (faster-whisper), GGUF (whisper.cpp), and ONNX (sherpa-onnx) artifacts must never
  mix in one runtime context — each engine gets its own curated, isolated install
  (`voice-typer/CONVENTIONS.md`, repeated in 3+ other files).
- CPU/GPU (and by extension future hardware) variants must cache independently, never
  auto-evicting each other (`stt-server/CONVENTIONS.md`) — already implemented for
  faster-whisper (`RuntimeVariant::{Cpu,Gpu}`, `cached_variant_dir`).
- Any engine's cached binaries/models must live under `stt-server`'s unified
  `default_data_root()` — otherwise `whisper-vibes`' cascading-uninstall NSIS hook silently
  stops covering it, reproducing the exact "uninstall doesn't actually clean everything" problem
  that goal was built to fix, just for a new engine instead of preventing it.
- **New this session**: `CONVENTIONS.md`'s selection criteria (official upstream repo, genuine
  community adoption, redistribution-compatible license) and "minimize self-hosted binaries"
  principle (an engine's adapter should fetch from its own upstream releases by default;
  `stt-server` only builds and hosts its own binary when no upstream release exists —
  faster-whisper is the sole current exception, not the template).

### Research Notes — engine selection

Considered and researched this session (full detail in conversation; summarized here):
- **whisper.cpp** (`ggml-org/whisper.cpp`): single native C/C++ binary, GGUF models as plain
  downloadable files, real official prebuilt releases for Windows/Linux/macOS. Its standout
  strength is a purpose-built Apple Silicon path (dedicated Metal kernels + CoreML/ANE encoder
  offload, ~10x realtime on large-v3) — not currently load-bearing since the project ships
  Windows+Linux only today, but real, planned work, not indefinitely deferred.
- **sherpa-onnx** (`k2-fsa/sherpa-onnx`): single ONNX-Runtime-based binary running multiple model
  families (Whisper, NVIDIA Parakeet/Canary, Moonshine, SenseVoice, Zipformer/Paraformer) through
  one adapter. Real official prebuilt releases for Windows (x86/x64/ARM64), Linux
  (x64/ARM64/ARM32/RISC-V), macOS (Universal); CUDA and DirectML GPU support on Windows (AMD/Intel
  GPUs too, not NVIDIA-only), CoreML on macOS. Its own ONNX-exported Whisper path has a
  documented accuracy regression versus faster-whisper on identical audio
  ([k2-fsa/sherpa-onnx#2900](https://github.com/k2-fsa/sherpa-onnx/issues/2900)) — it is additive
  for model families neither faster-whisper nor whisper.cpp can run, not a Whisper replacement.
- **Considered and explicitly not pursued**: Coqui STT and Mozilla DeepSpeech (both effectively
  unmaintained ecosystems); NVIDIA NeMo directly (pulls in a full PyTorch stack — the same
  heavy-dependency problem embeddable-Python packaging would create; sherpa-onnx's ONNX export of
  the same Parakeet/Canary models achieves the same model access without that dependency weight).

Despite whisper.cpp being named as the "next planned adapter" in `whisper-vibes`' and `stt-sdk`'s
own `VISION.md`/`CONVENTIONS.md`, **no goal file anywhere planned it (or sherpa-onnx) as an
`stt-server`-managed provider runtime before this one.** This goal fills that gap for both.

## Convention Constraints

- Rust remains the implementation direction (matches `CONVENTIONS.md`).
- Do not introduce a new packaging *technology* (embeddable Python, pip-based install, etc.) for
  faster-whisper as part of this goal — considered and deferred (see Out of Scope). Each engine
  keeps whatever self-contained packaged-binary shape it naturally has; this goal only
  generalizes the Rust-side install/cache/uninstall machinery *around* whatever that shape is.
- Provider/model identifiers remain validated, curated catalog entries — never caller-supplied
  filesystem paths (matches `bootstrap-local-stt-server`'s own established constraint).
- New engine adapters fetch from their own upstream official releases by default
  (`CONVENTIONS.md`'s "minimize self-hosted binaries") — faster-whisper's existing self-hosted
  release pipeline is unaffected and stays exactly as it is today.
- The HTTP API must remain the complete control surface (`CONVENTIONS.md`'s "API Completeness")
  — this goal must not introduce any capability that's only reachable via the CLI.

## Scope

1. Add `crates/runtime/src/providers/cache.rs` with the shared, engine-agnostic machinery
   (see Architecture Notes: Design Validation for the exact function list) — cache path
   computation, cache removal, the download streaming primitive, and multi-file verification.
2. Define a `ProviderEngine` trait in `crates/runtime/src/providers/mod.rs` capturing the
   genuinely engine-specific operations (see Architecture Notes for the validated signature).
3. Replace `RuntimeManager`'s `if id.as_str() != "faster-whisper"` gates in `begin_install`,
   `uninstall`, `begin_model_pull`, `verify_model`, `remove_model` with dispatch through
   `providers::registry()`.
4. Fix `crates/cli/src/run.rs`'s independent `"faster-whisper"` hardcoding (the 4th call site
   found during design validation, not in the original scope sketch) — add
   `RuntimeManager::register_all_available_locally()` looping over `catalog::CATALOG` instead of
   naming one provider.
5. Fix `manager.rs::uninstall()`'s cascade-delete loop to read the specific provider's own
   `entry.variants` from the catalog rather than a hardcoded `[RuntimeVariant::Cpu,
   RuntimeVariant::Gpu]` global list — correct today only because exactly one provider exists.
6. Migrate the existing faster-whisper implementation onto the new trait, preserving 100% of its
   current behavior (variant caching, model caching, packaged-vs-raw-source detection,
   cascade-uninstall, self-hosted release pipeline) — this is the proof the abstraction is real,
   not just theoretical, and the regression gate before trusting new engines.
7. Implement `whisper_cpp.rs` and `sherpa_onnx.rs` for real, fetching from their own upstream
   releases (`ggml-org/whisper.cpp`, `k2-fsa/sherpa-onnx`) per the minimize-self-hosted-binaries
   principle — this is the "prove genericity against two structurally different engines" step,
   now scoped as real shipped work rather than a design-only sketch.

## Out of Scope

- Any pip/embeddable-Python/package-manager-based distribution mechanism for faster-whisper —
  considered and explicitly deferred. Substantial, separate engineering lift with no existing
  pressure behind it; its own goal with its own design pass if ever pursued.
- Any change to `whisper-vibes` or `stt-sdk` — this is a pure `stt-server`-internal refactor plus
  two new engine adapters; the HTTP/CLI contract surface (routes, request/response shapes)
  should not need to change.
- Opening up `RuntimeVariant` beyond the closed `Cpu`/`Gpu` enum into a fully general hardware-
  variant model (needed eventually — sherpa-onnx alone needs `cuda`/`directml`/`coreml`,
  whisper.cpp needs `metal`) — explicit, deliberate scope decision: the `ProviderEngine` trait
  speaks `variant: &str` throughout so the install/cache boundary doesn't hard-depend on the
  enum, but `RuntimeVariant` itself stays as-is at the catalog/HTTP layer for this goal. Opening
  it up requires new hardware-detection work (DirectML/CoreML capability, not just
  `has_nvidia_gpu`) — real, separate, non-trivial work, named as its own future goal.
- Model catalog UI/onboarding changes — tracked in `whisper-vibes`' `live-onboarding-model-catalog`.

## Acceptance Criteria

1. `RuntimeManager`'s provider-lifecycle methods, and `run.rs`'s startup registration, dispatch
   through a registry/trait, not a hardcoded string check.
2. All of `bootstrap-local-stt-server`'s existing real-hardware verification (real model pull,
   real verify/remove, real cascade-uninstall, real daemon-independent reset) still passes
   identically after the refactor — zero behavior regression for faster-whisper, including its
   self-hosted release pipeline staying unchanged.
3. `cargo test --workspace`, `cargo clippy --workspace --all-targets`, `cargo fmt --check` all
   clean.
4. whisper.cpp and sherpa-onnx are real, working catalog entries — installable, cache correctly
   under `default_data_root()`, uninstall cleanly, fetch their release assets from their own
   upstream repos (not a `mariuszRep/stt-server`-hosted duplicate) — demonstrating the
   `ProviderEngine` trait requires no changes to `manager.rs`'s dispatch logic to add an engine.

## Judgment Rubric

- Not done if any provider-lifecycle method or `run.rs`'s startup path still has engine-specific
  `if`/`match` branches after the refactor.
- Not done if faster-whisper's real, currently-verified behavior regresses in any way, including
  its release-hosting mechanism.
- Not done if whisper.cpp or sherpa-onnx's adapter builds/hosts its own duplicate binary instead
  of fetching from its own upstream release.
- Not done if any capability added for the new engines is only reachable via the CLI, not the API.

## Architecture Notes

### Design Validation (this session, before the first implementation attempt)

A Plan-agent design pass read `manager.rs`, `faster_whisper.rs`, `catalog.rs`, `run.rs`,
`routes/providers.rs`, and `routes/models.rs` in full and validated the trait shape below against
the real code — re-verify against the actual tree at implementation time since line numbers
drift, but the shape itself is checked, not just reasoned about abstractly.

**Trait** (`crates/runtime/src/providers/mod.rs`), engine-owned methods only:

```rust
#[async_trait]
pub trait ProviderEngine: Send + Sync {
    fn provider_id(&self) -> &'static str;
    fn install_local(&self, variant: &str) -> Option<LaunchBuilder>;
    async fn download_variant(&self, variant: &str, on_progress: ProgressCallback) -> Result<LaunchBuilder, RuntimeError>;
    async fn download_model(&self, model_id: &str, output_dir: &Path) -> Result<(), RuntimeError>;
    fn verify_cached_model(&self, model_id: &str) -> Result<Option<u64>, RuntimeError>;
}
```

`async-trait` is already in the workspace root `Cargo.toml`, unused today — add it to
`crates/runtime/Cargo.toml`, no new dependency decision needed.

**Shared vs. trait boundary** (corrects this goal's own earlier draft, which overstated what
generalizes):

| Function | Shared or trait? | Why |
|---|---|---|
| Cache path computation (`variant_dir`/`model_dir`) | **Shared** (`cache::variant_dir(provider_id, variant)` / `cache::model_dir(...)`) | Zero engine-specific logic — `root.join(id).join(...)`. Making this fixed and non-overridable *enforces* the "every engine's cache lives under `default_data_root()`" constraint rather than trusting each adapter author to remember it. |
| Cache removal | **Shared** | Pure `rm -rf` of the shared-layout path. |
| Download streaming primitive (`.part` file, progress, atomic rename, unix chmod) | **Shared** (`cache::download_to_cache(url, dest_dir, filename, make_executable, on_progress)`) | Bytes-agnostic mechanics generalize cleanly. |
| Multi-file verification | **Shared, new** (`cache::verify_files_present(dir, &[relative_paths]) -> Result<Option<u64>, RuntimeError>`) | Handles both "one file" engines (faster-whisper/whisper.cpp) and "several files per model family" engines (sherpa-onnx: encoder/decoder/joiner/tokens, family-dependent) with the same helper — no catalog/trait schema change needed for that difference. |
| Download **orchestration** (URL/repo/tag construction, archive extraction, launch-spec assembly) | **Trait method** (`download_variant`) | Does not generalize — whisper.cpp/sherpa-onnx ship zip archives from their own upstream repo+tag scheme (`ggml-org/whisper.cpp`, `k2-fsa/sherpa-onnx`), never `env!("CARGO_PKG_VERSION")` against `mariuszRep/stt-server`'s own releases the way faster-whisper does. |
| `install_local` (dev-copy detection) | **Trait method** | whisper.cpp/sherpa-onnx have no "raw Python source + interpreter" concept; a dev override is a locally-built binary, structurally different detection. |
| `download_model` | **Trait method** | faster-whisper spawns its own installed runtime as a subprocess; whisper.cpp/sherpa-onnx do plain HTTP GET(s) with no runtime-installed precondition — proves the trait doesn't presuppose faster-whisper's shape. |
| `verify_cached_model` | **Trait method**, built on `cache::verify_files_present` | "What proves completion" varies by shape; each engine's impl becomes a 2-line call into the shared helper. |
| `build_env`/`packaged_launch_builder`/`raw_source_launch_builder` | **Engine-private, not on the trait** | `LaunchBuilder`'s existing `Box<dyn Fn(u16, &str, Option<&str>, &StartOptions) -> Launch + Send + Sync>` shape is already the generalized launch seam — confirmed it cleanly covers faster-whisper's `VOICE_TYPER_*` env-var contract and whisper.cpp/sherpa-onnx's CLI-flag-based contracts with no trait changes needed. |

**Registry**: `providers::registry() -> HashMap<String, Box<dyn ProviderEngine>>`, built once in
`providers/mod.rs` — `manager.rs` never imports a concrete engine module, keeping "adding an
engine requires zero `manager.rs` changes" literally true. Keyed by plain `String` (matches how
`manager.rs` already keys `installed`/`instances`/`selected_models`); no `ProviderId` lifetime
issue either way since `ProviderId` is fully owned despite `CatalogEntry.id` being `&'static`.
Recommend a startup invariant check (`debug_assert!` or a test) that
`catalog::CATALOG.iter().all(|e| providers::registry().contains_key(e.id))`.

**Async/ownership note**: `begin_install`'s background work runs inside `tokio::spawn(async move
{ ... })`. The spawned task must re-look-up the engine from `manager.engines` by id string inside
the task body, not capture a borrowed `&dyn ProviderEngine` from the outer scope (wouldn't satisfy
`tokio::spawn`'s `'static` bound). Since `engines: HashMap<String, Box<dyn ProviderEngine>>` is
populated once in `RuntimeManager::new()` and never mutated afterward, it needs no `Mutex` —
a plain field, read via `&manager.engines` inside the spawned block.

### Sketch adapters (design-pass output, not yet real implementations)

**`whisper_cpp.rs`** — single native binary, plain-file GGUF downloads, zip-archive releases from
`ggml-org/whisper.cpp` (own repo+tag, not `mariuszRep/stt-server`'s), CLI-flag launch (not
`VOICE_TYPER_*` env vars) — proves the trait doesn't presuppose faster-whisper's env-var contract.

**`sherpa_onnx.rs`** — one binary serving multiple model families, each with its own file
manifest (`ModelSpec { files: &[(filename, url)] }`, 1-4 files depending on family) fed into
`cache::verify_files_present` — proves `CatalogEntry.models: &'static [ModelEntry]` already
supports "many families under one provider id" with zero schema change, and that the
multi-file-verification shared helper (not a single-file assumption) is load-bearing.

Full sketch code for both (signatures + illustrative bodies) is preserved in this session's
conversation history; re-derive against the real tree at implementation time rather than
copy-pasting stale sketches — upstream release asset layouts for both engines were not verified
against real current releases during design (see Risks/Unknowns).

## Risks / Unknowns

1. **Real open questions not resolved by design-pass reading alone**: whisper.cpp's actual GH
   release asset layout (zip contents, whether CPU/CUDA/Vulkan are separate downloads or one
   universal build with runtime backend selection); sherpa-onnx's exact per-model-family file
   manifests and real HF/GH URLs; both engines' current license text (MIT for whisper.cpp,
   Apache-2.0 for sherpa-onnx per public knowledge — re-confirm against each project's actual
   `LICENSE` file before implementation, not assumed). None of these are verifiable without
   checking each project's real releases page at implementation time.
2. **`RuntimeVariant`'s closed 2-variant enum is confirmed insufficient for the full future
   story** (see Out of Scope) — this goal's trait speaks `variant: &str` so install/cache doesn't
   hard-depend on the enum, but `evaluate_variant`'s hardware-compat logic and
   `manager.rs::start()`'s CUDA gate still only understand `Cpu`/`Gpu`. This goal does not
   deliver a `"directml"`/`"coreml"`/`"metal"` variant actually being requestable end-to-end —
   only that the string can flow through the install/cache layer. Name the full opening-up as its
   own future goal, don't let it silently balloon into this one.
3. **Scope discipline**: it would be easy for "implement two real engines" to balloon well beyond
   a single attempt. If whisper.cpp and sherpa-onnx can't both land in one attempt, land
   faster-whisper's migration + one new engine first (whichever has fewer open questions once
   real upstream release layouts are checked), and split the second into a following attempt
   rather than leaving the whole goal half-migrated.

## Verification Expectations

### Automated Verification
- `cargo test --workspace`, `cargo clippy --workspace --all-targets`, `cargo fmt --check`.
- All of `bootstrap-local-stt-server`'s existing tests continue passing unmodified in behavior
  (test *code* may need updating for the new trait-based structure, but assertions/outcomes
  should be identical).

### Manual Verification
- Re-run `bootstrap-local-stt-server`'s real-hardware verification steps (real `stt model
  pull`/`verify`/`remove`, real cascade-uninstall via `DELETE /v1/providers/:id`, real
  `stt reset --yes` with no daemon running) against the refactored code, on real Windows
  hardware, confirming byte-for-byte identical behavior to before the refactor.
- Real install/verify/remove/uninstall for whisper.cpp and sherpa-onnx on real hardware, with
  directory listings before/after confirming their assets land under `default_data_root()` and
  are fully removed on uninstall.
- Confirm both new engines' downloads hit their own upstream release URLs
  (`ggml-org/whisper.cpp`, `k2-fsa/sherpa-onnx`), not a `mariuszRep/stt-server`-hosted asset.

## Attempts

No attempts yet.

## Do Not Repeat

None yet.

## Verification Log

No verification yet — design validation (not execution) is recorded in Architecture Notes above.

## Final Outcome

Pending.

## Ready For Execution

- Status: yes
- Reason: The design/reading pass this goal previously required before `ready` is complete — the
  `ProviderEngine` trait shape was checked against the real current code (not reasoned about
  abstractly), the shared-vs-trait boundary was corrected and justified per function, a 4th
  hardcoded call site was found and added to scope, and both target engines (whisper.cpp,
  sherpa-onnx) were sketched to prove the boundary holds against structurally different shapes.
  Problem, motivation, constraints, scope, and acceptance criteria are fully defined. Remaining
  unknowns (exact upstream release layouts, license re-verification) are real but are
  implementation-time lookups, not open design questions blocking readiness.
