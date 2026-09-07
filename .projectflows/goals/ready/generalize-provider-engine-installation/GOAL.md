---
name: generalize-provider-engine-installation
title: Generalize Provider/Engine Installation Into a Pluggable Architecture
description: Replace faster-whisper-hardcoded lifecycle dispatch and cache mechanics with a ProviderEngine trait, provider registry, and shared engine-agnostic cache machinery, then migrate faster-whisper without behavior changes.
status: ready
type: refactor
scope: stt-server/crates/runtime/src/manager.rs, stt-server/crates/runtime/src/providers/, stt-server/crates/cli/src/run.rs
attempt: 0
max_attempts: 5
last_result: none
next_action: |
  Design/reading is complete (see Design Validation). Start the first implementation attempt by:
  1. Add `crates/runtime/src/providers/cache.rs` with provider-parameterized variant/model paths,
     download-with-progress and atomic completion, cache removal, and multi-file verification.
  2. Define `ProviderEngine` and `providers::registry()` in `providers/mod.rs`.
  3. Replace RuntimeManager and `run.rs` faster-whisper-specific dispatch with registry dispatch,
     including catalog-driven local registration and provider-specific uninstall variant cleanup.
  4. Migrate `faster_whisper.rs` onto the trait with no binary, URL, cache-layout, lifecycle, or
     model behavior changes, then rerun the bootstrap goal's automated and real-hardware
     regression checks.
success_criteria:
  - RuntimeManager's install/uninstall/model-pull/verify/remove methods and daemon startup registration dispatch through a provider registry, with no faster-whisper-specific dispatch in manager.rs or run.rs.
  - Download-with-progress, atomic rename-on-completion, per-variant cache directories, per-model directories, verification, and uninstall cleanup use shared provider-parameterized cache machinery rather than faster-whisper-specific copies.
  - faster-whisper implements ProviderEngine while preserving its existing install, model pull/verify/remove, cascade-uninstall, CPU/GPU cache, launch, and release behavior.
  - Existing automated tests and the bootstrap-local-stt-server real-hardware lifecycle checks pass after the refactor with no faster-whisper regression.
source: user
---

# Generalize Provider/Engine Installation Into a Pluggable Architecture

## Goal

Refactor the existing faster-whisper lifecycle into a provider-neutral `ProviderEngine` trait,
registry, and shared cache layer. Remove hardcoded provider dispatch from `RuntimeManager` and
daemon startup, then prove the abstraction preserves faster-whisper behavior. This goal creates
the seam for later providers; it does not implement whisper.cpp or sherpa-onnx adapters.

## Source Requirements

User, this session, after `bootstrap-local-stt-server` landed and a GitHub Actions storage
quota incident prompted a broader architecture review: *"with stt-server we need to find an
efficient way of installing and uninstalling... we need in the future support for multiple
providers faster-whisper whisper.cpp and others from nvidia + other popular local engines"* —
explicitly asked for a solution that is "easy flexibility but also min effort so as little
maintenance from our side." Later in the same session, after research into candidate engines and
a Plan-agent design-validation pass, the user explicitly confirmed the target roster: *"lets
design with mind that all 3 will be implemented and eny other but we keep faster-whisper as
priotrity"*. This goal supplies only the shared seam and faster-whisper migration. The separate
`add-whisper-cpp-provider` draft owns the first new adapter after this refactor; the separate
`add-sherpa-onnx-provider` draft follows it and owns multi-file/multi-family validation.

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

whisper.cpp remains the first planned adapter after this abstraction lands, followed by
sherpa-onnx. Their separate draft goals own real adapter implementation; the engine research here
is retained only as design evidence that the shared trait/cache boundary must not assume one
binary or one model-file shape.

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

1. Add `crates/runtime/src/providers/cache.rs` with shared provider-parameterized cache paths,
   cache removal, download streaming/atomic completion, and multi-file verification.
2. Define the engine-specific `ProviderEngine` trait and construct the provider registry in
   `crates/runtime/src/providers/mod.rs`.
3. Replace hardcoded faster-whisper dispatch in `RuntimeManager` lifecycle/model methods and
   `crates/cli/src/run.rs` startup registration with registry dispatch.
4. Make uninstall cleanup use the selected catalog entry's variants rather than a global
   faster-whisper-shaped CPU/GPU list.
5. Migrate faster-whisper onto the trait without changing its install, cache, model, launch,
   packaged/raw-source, lifecycle, or release behavior.
6. Run the existing automated suite and bootstrap-local-stt-server real-hardware lifecycle checks
   as regression verification.

## Out of Scope

- Real whisper.cpp or sherpa-onnx adapters, catalog entries, upstream asset integration, model
  manifests, and provider-specific runtime verification. Those are owned by the separate
  `add-whisper-cpp-provider` and `add-sherpa-onnx-provider` drafts, in that order.
- Any pip/embeddable-Python/package-manager-based distribution mechanism for faster-whisper —
  considered and explicitly deferred. Substantial, separate engineering lift with no existing
  pressure behind it; its own goal with its own design pass if ever pursued.
- Any change to `whisper-vibes` or `stt-sdk`; the HTTP/CLI contract surface should not need to
  change for this internal refactor.
- Opening up `RuntimeVariant` beyond the closed `Cpu`/`Gpu` enum into a fully general hardware-
  variant model (needed eventually — sherpa-onnx alone needs `cuda`/`directml`/`coreml`,
  whisper.cpp needs `metal`) — explicit, deliberate scope decision: the `ProviderEngine` trait
  speaks `variant: &str` throughout so the install/cache boundary doesn't hard-depend on the
  enum, but `RuntimeVariant` itself stays as-is at the catalog/HTTP layer for this goal. Opening
  it up requires new hardware-detection work (DirectML/CoreML capability, not just
  `has_nvidia_gpu`) — real, separate, non-trivial work, named as its own future goal.
- Model catalog UI/onboarding changes — tracked in `whisper-vibes`' `live-onboarding-model-catalog`.

## Acceptance Criteria

1. `RuntimeManager`'s lifecycle/model methods and `run.rs` startup registration dispatch through
   the `ProviderEngine` registry, with no concrete faster-whisper dispatch branch there.
2. Cache paths, download/progress/atomic completion, verification, removal, and provider uninstall
   cleanup are shared and parameterized by provider identity.
3. faster-whisper implements the trait with unchanged install, model, launch, cache, lifecycle,
   variant, and release behavior.
4. All bootstrap real-hardware checks (model pull/verify/remove, cascade-uninstall, reset) still
   pass after the refactor, and `cargo test --workspace`, `cargo clippy --workspace --all-targets`,
   and `cargo fmt --check` are clean.

## Judgment Rubric

- Not done if any provider-lifecycle/model method or `run.rs` startup path retains concrete
  faster-whisper dispatch after the refactor.
- Not done if cache/download/removal mechanics that are provider-neutral remain duplicated inside
  faster-whisper.
- Not done if faster-whisper's real, currently verified behavior regresses in any way, including
  its release-hosting mechanism.
- Real whisper.cpp and sherpa-onnx behavior is neither required nor permitted as completion
  evidence for this goal; their draft goals own that work.

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

### Follow-up adapter design evidence (not implementation scope)

The design pass used two sketches only to pressure-test the abstraction. `whisper_cpp.rs` modeled
a native binary, plain-file GGUF download, archive release, and CLI-flag launch, showing that the
trait cannot assume faster-whisper's Python/env-var shape. `sherpa_onnx.rs` modeled one binary with
family-dependent multi-file manifests, showing why shared verification must accept multiple
relative paths rather than assume one model file.

No adapter or catalog entry for either engine belongs in this goal. The sketches are non-binding
design notes; `add-whisper-cpp-provider` must re-check real upstream layouts first, and
`add-sherpa-onnx-provider` follows it to validate multi-file/multi-family support against the
landed abstraction.

## Risks / Unknowns

1. **Follow-up provider unknowns**: whisper.cpp release layout/license details and sherpa-onnx
   release/model manifests/license details remain intentionally unresolved here. Their separate
   drafts must verify current upstream facts when each adapter is implemented; they do not block
   or expand this abstraction goal.
2. **`RuntimeVariant`'s closed 2-variant enum is confirmed insufficient for the full future
   story** (see Out of Scope) — this goal's trait speaks `variant: &str` so install/cache doesn't
   hard-depend on the enum, but `evaluate_variant`'s hardware-compat logic and
   `manager.rs::start()`'s CUDA gate still only understand `Cpu`/`Gpu`. This goal does not
   deliver a `"directml"`/`"coreml"`/`"metal"` variant actually being requestable end-to-end —
   only that the string can flow through the install/cache layer. Name the full opening-up as its
   own future goal, don't let it silently balloon into this one.
3. **Scope discipline**: do not use the abstraction attempt to begin either real provider.
   Finish and regression-verify the faster-whisper migration first; then execute the whisper.cpp
   draft, followed by the sherpa-onnx draft.

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
  hardware, confirming identical faster-whisper behavior to before the refactor.

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
- Reason: The trait, registry, shared-cache boundary, hardcoded dispatch sites, faster-whisper
  migration, and regression gates are defined against the current code. Follow-up adapter sketches
  only pressure-tested the boundary; real whisper.cpp and sherpa-onnx implementation is explicitly
  owned by separate drafts and is not required to execute or complete this ready refactor.
