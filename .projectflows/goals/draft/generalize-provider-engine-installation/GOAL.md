---
name: generalize-provider-engine-installation
title: Generalize Provider/Engine Installation Into a Pluggable, Catalog-Driven Architecture
description: Replace the faster-whisper-hardcoded install/cache/variant/model logic in RuntimeManager with a small ProviderEngine trait and shared, engine-agnostic machinery, so adding whisper.cpp or another local engine later is a catalog entry + a thin adapter, not a parallel reimplementation.
status: draft
type: refactor
scope: stt-server/crates/runtime (catalog.rs, manager.rs, providers/); no new engine implementation in this goal
attempt: 0
max_attempts: 5
last_result: none
next_action: |
  Draft, not yet ready — needs a dedicated design/reading pass before implementation
  starts (see Risks/Unknowns). Concretely: read crates/runtime/src/manager.rs and
  crates/runtime/src/providers/faster_whisper.rs in full against this goal's proposed
  ProviderEngine trait shape (see Architecture Notes) and confirm it actually
  generalizes cleanly against real code, not just in the abstract; decide whether to
  prototype genericity by sketching (not necessarily fully implementing) a second
  engine adapter (whisper.cpp is the natural candidate, per its existing SDK-level
  seam) to prove the trait boundary is right before committing; then move to ready.
success_criteria:
  - RuntimeManager's install/uninstall/model-pull/verify/remove methods dispatch through a provider registry, not a hardcoded `if id.as_str() != "faster-whisper"` check.
  - Download-with-progress, atomic rename-on-completion, per-variant cache directories, per-model directories, and uninstall cascade-delete are shared, engine-agnostic code, parameterized by provider id — not duplicated per engine.
  - Adding a second engine (prototyped, even if not fully shipped in this goal) requires only a catalog entry plus one new file implementing the ProviderEngine trait — no changes to manager.rs's dispatch logic.
  - faster-whisper's existing behavior (install, model pull/verify/remove, cascade-uninstall, CPU/GPU variant caching) is fully preserved with zero regressions after the refactor — verified against the same real end-to-end tests bootstrap-local-stt-server already established.
source: user
---

# Generalize Provider/Engine Installation Into a Pluggable, Catalog-Driven Architecture

## Goal

Make `stt-server` genuinely capable of managing multiple local STT engines with minimal
per-engine maintenance burden — today it manages exactly one (faster-whisper), and every
install/cache/variant/model operation is hardcoded to that one engine's id and shape. This goal
generalizes the Rust-side machinery so a future engine (whisper.cpp first, others — NVIDIA's
own engines, other popular local STT engines — later) is a small, additive change, not a
parallel reimplementation of everything `bootstrap-local-stt-server` already built.

## Source Requirements

User, this session, after `bootstrap-local-stt-server` landed and a GitHub Actions storage
quota incident prompted a broader architecture review: *"with stt-server we need to find an
efficient way of installing and uninstalling... we need in the future support for multiple
providers faster-whisper whisper.cpp and others from nvidia + other popular local engines"* —
explicitly asked for a solution that is "easy flexibility but also min effort so as little
maintenance from our side," reviewed against all four repos' `VISION.md`/`CONVENTIONS.md` and
every existing goal file touching this area first (see Vision Alignment and the Research Notes
below) before this goal was drafted.

## Problem / Motivation

Today, `crates/runtime/src/manager.rs::RuntimeManager::begin_install()` (and its siblings
`uninstall()`, `begin_model_pull()`, `verify_model()`, `remove_model()`) all contain an
explicit `if id.as_str() != "faster-whisper" { return Err(RuntimeError::ProviderNotFound) }`
gate before doing anything — meaning even though `catalog.rs`'s `CatalogEntry` type is already
data-driven and *could* list other providers, the actual install/download/cache logic only
ever works for the literal string `"faster-whisper"`. All of that logic — variant caching
(`cached_variant_dir`), model caching (`cached_model_dir`), local/dev-source detection
(`install_local`, `locate_runtime_dir`, `detect_runtime_kind`), download-with-progress
(`download_variant`, `download_model`), and env/launch-arg construction (`build_env`,
`packaged_launch_builder`, `raw_source_launch_builder`) — lives in one file,
`crates/runtime/src/providers/faster_whisper.rs`, written directly against that one engine's
shape. Adding whisper.cpp today would mean copying that whole file and threading new
`if provider == "whisper-cpp"` branches through `manager.rs` everywhere — the exact
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
  never invisible during inference (`stt-server/CONVENTIONS.md:25-26,35`) — the generalized
  machinery must preserve this, not just faster-whisper's current implementation of it.
- CTranslate2 (faster-whisper) and GGUF (whisper.cpp) artifacts must never mix in one runtime
  context — each engine keeps its own curated, isolated install
  (`voice-typer/CONVENTIONS.md:40`, repeated in 3+ other docs).
- CPU/GPU (and by extension future hardware) variants must cache independently, never
  auto-evicting each other (`stt-server/CONVENTIONS.md:26`) — already implemented for
  faster-whisper; the generalization must preserve this per-engine, not just for the first one.
- Any engine's cached binaries/models must stay under `stt-server`'s unified
  `default_data_root()` — `whisper-vibes`' `cascading-provider-uninstall` NSIS hook targets
  that exact root; a new engine storing its cache elsewhere would silently escape that cleanup.

### Research Notes — filling a real, previously-flagged gap

Despite whisper.cpp being named as the "next planned adapter" in 4+ docs across 3 repos
(`whisper-vibes/VISION.md:30`, `stt-sdk/VISION.md:10`, `stt-sdk/CONVENTIONS.md:5`, `stt-sdk`'s
own `bootstrap-sdk-and-provider-contract` goal), **no goal file anywhere actually plans it as
an `stt-server`-managed provider runtime** before this one. The one `whisper-vibes` goal with
that name (`cancelled/whisper-cpp-gguf-adapter`) was an explicit stub redirecting future work
here; `stt-server`'s own goals tree had exactly one goal
(`bootstrap-local-stt-server`, `done`) before this, which explicitly excluded new engines from
its own scope. This goal is filling that gap, not duplicating existing planning.

## Convention Constraints

- Rust remains the implementation direction (matches `CONVENTIONS.md:9`).
- Do not introduce a new packaging *technology* (embeddable Python, pip-based install, etc.)
  as part of this goal — explicitly considered and deferred (see Out of Scope). Each engine
  keeps whatever self-contained packaged-binary shape it naturally has; this goal only
  generalizes the Rust-side install/cache/uninstall machinery *around* whatever that shape is.
- Provider/model identifiers remain validated, curated catalog entries — never caller-supplied
  filesystem paths (matches `bootstrap-local-stt-server`'s own established constraint).

## Scope

1. Define a `ProviderEngine` trait in `crates/runtime/src/providers/mod.rs` (or similar)
   capturing the genuinely engine-specific operations: launch args/env construction, where an
   engine's release assets/models come from (URL template or equivalent), and whatever
   engine-specific detection is needed (e.g. faster-whisper's packaged-vs-raw-source /
   variant-sentinel check).
2. Generalize the currently faster-whisper-specific shared machinery — `cached_variant_dir`,
   `cached_model_dir`, download-with-progress, atomic rename-on-completion, uninstall
   cascade-delete, the `InstallOperationState` progress-polling table — to take a
   `provider_id: &str` (or equivalent) parameter instead of being hardcoded, so it's reusable
   across engines without duplication.
3. Replace `RuntimeManager`'s `if id.as_str() != "faster-whisper"` gates in `begin_install`,
   `uninstall`, `begin_model_pull`, `verify_model`, `remove_model` with dispatch through a
   provider registry (e.g. `HashMap<ProviderId, Box<dyn ProviderEngine>>`, populated from the
   catalog at startup).
4. Migrate the existing faster-whisper implementation onto the new trait, preserving 100% of
   its current behavior (variant caching, model caching, packaged-vs-raw-source detection,
   cascade-uninstall) — this is the proof the abstraction is real, not just theoretical.
5. Prove genericity: sketch (design-level, and as much real code as is proportionate — full
   judgment call during implementation) a second engine adapter, most naturally whisper.cpp
   given its existing named seam at the SDK layer, to confirm the trait boundary actually holds
   for a structurally different engine (single native binary, no Python/PyInstaller, GGUF
   models as plain downloadable files rather than a HuggingFace-hub Python download) — without
   necessarily shipping a fully production-ready whisper.cpp integration in this goal (see Out
   of Scope).

## Out of Scope

- A production-ready whisper.cpp (or any other new engine) integration — this goal generalizes
  the *architecture*; a real second engine implementation is a separate follow-up goal once
  this lands, unless Scope item 5's prototype work naturally becomes real (judgment call at
  implementation time, not decided here).
- Any pip/embeddable-Python/package-manager-based distribution mechanism — considered and
  explicitly deferred. It's a substantial, separate engineering lift (bundling a redistributable
  Python runtime, managing venvs, CUDA wheel selection for GPU, losing the "single
  self-contained exe, zero prerequisites" property today's PyInstaller approach gives
  non-technical Windows users for free) with no existing pressure or stated intent behind it.
  If pursued later, it's its own goal with its own dedicated design pass.
- Any change to `whisper-vibes` or `stt-sdk` — this is a pure `stt-server`-internal refactor;
  the HTTP/CLI contract surface (routes, request/response shapes) should not need to change,
  since this goal only reorganizes what's *behind* those routes.
- Model catalog UI/onboarding changes — tracked in `whisper-vibes`' `live-onboarding-model-catalog`.

## Acceptance Criteria

1. `RuntimeManager`'s provider-lifecycle methods dispatch through a registry/trait, not a
   hardcoded string check.
2. All of `bootstrap-local-stt-server`'s existing real-hardware verification (real model pull,
   real verify/remove, real cascade-uninstall, real daemon-independent reset) still passes
   identically after the refactor — zero behavior regression for faster-whisper.
3. `cargo test --workspace`, `cargo clippy --workspace --all-targets`, `cargo fmt --check` all
   clean.
4. A second engine's adapter (even if only prototyped, per Scope item 5) demonstrates the
   `ProviderEngine` trait requires no changes to `manager.rs`'s dispatch logic to add.

## Judgment Rubric

- Not done if any provider-lifecycle method still has engine-specific `if`/`match` branches in
  `manager.rs` after the refactor.
- Not done if faster-whisper's real, currently-verified behavior regresses in any way.
- Not done if the "prove genericity" step (Scope item 5) is skipped entirely — an abstraction
  designed against only one real implementation is unproven; some real evidence of a second
  shape fitting is required, even if that second engine isn't fully shipped.

## Architecture Notes

Current faster-whisper-specific code to generalize (file:line references from this session's
own implementation, current as of `bootstrap-local-stt-server`'s `done` state — re-verify
against the actual tree at implementation time since line numbers drift):

- `crates/runtime/src/manager.rs::begin_install` — hardcoded provider-id gate.
- `crates/runtime/src/manager.rs::uninstall` — cascade-deletes via
  `faster_whisper::remove_cached_variant` directly, not through any abstraction.
- `crates/runtime/src/manager.rs::begin_model_pull`/`verify_model`/`remove_model` — same
  pattern, direct calls into `faster_whisper::*` functions.
- `crates/runtime/src/providers/faster_whisper.rs` — the entire file is the concrete
  implementation to extract a trait boundary from: `cached_variant_dir`, `cached_model_dir`,
  `install_local`, `locate_runtime_dir`, `detect_runtime_kind`, `download_variant`,
  `download_model`, `build_env`, `packaged_launch_builder`, `raw_source_launch_builder`,
  `remove_cached_variant`, `remove_cached_model`, `verify_cached_model`.
- `crates/runtime/src/catalog.rs` — already data-driven (`CatalogEntry`, `ModelEntry`,
  `VariantInfo`); should need little to no change, it's the *dispatch* logic elsewhere that's
  hardcoded, not the catalog structure itself.

## Risks / Unknowns

1. **The trait boundary might not be right on the first attempt.** faster-whisper's install
   logic conflates several concerns (packaged-vs-raw-source detection, variant sentinel
   verification, HuggingFace-hub-based Python model download) that a structurally different
   engine (whisper.cpp: single native binary, plain-file GGUF models) may not need at all, or
   may need differently. This is exactly why Scope item 5 (sketch a second engine) exists —
   don't skip it in favor of shipping an abstraction proven against only one real
   implementation.
2. **Not yet `ready`** — deliberately left in `draft` pending the design/reading pass described
   in `next_action`. Move to `ready` once that pass confirms the proposed trait shape holds.
3. **Scope creep risk**: it would be easy for "sketch a second engine to prove genericity" to
   balloon into "fully implement whisper.cpp support," which is explicitly Out of Scope here.
   Keep the prototype honestly scoped to proving the abstraction, not shipping a feature.

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

## Attempts

No attempts yet.

## Do Not Repeat

None yet.

## Verification Log

No verification yet.

## Final Outcome

Pending.

## Ready For Execution

- Status: no
- Reason: Real design work remains before this is safely executable — the proposed
  `ProviderEngine` trait shape needs to be checked against the actual current code (not just
  reasoned about abstractly) and ideally validated against a second engine's real shape before
  committing, per `next_action`. The problem, motivation, constraints, and success criteria are
  otherwise fully defined.
