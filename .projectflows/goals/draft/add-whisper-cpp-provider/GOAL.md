---
name: add-whisper-cpp-provider
title: Add whisper.cpp as a Real Managed Provider Engine
description: Implement a real ProviderEngine adapter for whisper.cpp (ggml-org/whisper.cpp), fetching release assets directly from its own upstream releases rather than a stt-server-hosted duplicate.
status: draft
type: feature
scope: stt-server/crates/runtime/src/providers/whisper_cpp.rs (new), catalog.rs entry
attempt: 0
max_attempts: 5
last_result: none
next_action: |
  Blocked on generalize-provider-engine-installation landing first (the ProviderEngine trait and
  shared cache machinery this adapter implements against don't exist yet). Once that goal is
  done: verify whisper.cpp's actual current GH release asset layout (zip contents, whether
  CPU/CUDA/Vulkan/Metal are separate downloads or one universal build) against the real
  ggml-org/whisper.cpp releases page — not assumed from this goal's own notes, which were
  written during a design pass without checking real current releases.
success_criteria:
  - whisper.cpp installs, caches under default_data_root(), and uninstalls cleanly through the same API/CLI surface every other engine uses.
  - Release assets are fetched from ggml-org/whisper.cpp's own releases, never rebuilt or re-hosted by stt-server.
  - GGUF models download as plain files, verified via the shared cache::verify_files_present helper.
source: user
---

# Add whisper.cpp as a Real Managed Provider Engine

## Why this engine

Selected per `CONVENTIONS.md`'s engine-selection criteria: `ggml-org/whisper.cpp` is an actively
maintained official upstream repository with genuine broad adoption, MIT-licensed (re-verify
against the current `LICENSE` file before implementation, not assumed), and it publishes real
prebuilt release binaries for Windows/Linux/macOS — meaning this adapter can fetch directly from
upstream rather than `stt-server` building and hosting its own duplicate copy, per
`CONVENTIONS.md`'s "minimize self-hosted binaries" rule.

Already named as the "next planned adapter" in `whisper-vibes`' and `stt-sdk`'s own
`VISION.md`/`CONVENTIONS.md` (a `WhisperCppProvider` name exists at the SDK layer, unimplemented)
— this goal is what finally makes that real on the `stt-server` side.

Its standout strength is a purpose-built Apple Silicon path (dedicated Metal kernels + CoreML/ANE
encoder offload) — not currently load-bearing since the project ships Windows+Linux only today,
but real value once macOS becomes a live target.

## Blocker

Hard-blocked on `generalize-provider-engine-installation` (currently `ready`, not yet attempted)
— this goal implements a `ProviderEngine` trait impl that doesn't exist as a trait yet.

## Scope, Acceptance Criteria, Verification

Not yet fleshed out — this is a placeholder to keep the roadmap trackable. Flesh out once the
blocker goal lands and its real trait shape (and any changes made during that implementation) are
known, and once whisper.cpp's actual current release asset layout has been checked directly.

## Attempts

No attempts yet.

## Ready For Execution

- Status: no
- Reason: Blocked on `generalize-provider-engine-installation`.
