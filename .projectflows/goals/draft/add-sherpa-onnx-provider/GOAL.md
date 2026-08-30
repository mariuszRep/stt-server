---
name: add-sherpa-onnx-provider
title: Add sherpa-onnx as a Real Managed Provider Engine
description: Implement a real ProviderEngine adapter for sherpa-onnx (k2-fsa/sherpa-onnx), covering model families no other managed engine can run (NVIDIA Parakeet/Canary, Moonshine, SenseVoice, Zipformer/Paraformer), fetching release assets directly from its own upstream releases.
status: draft
type: feature
scope: stt-server/crates/runtime/src/providers/sherpa_onnx.rs (new), catalog.rs entries
attempt: 0
max_attempts: 5
last_result: none
next_action: |
  Blocked on generalize-provider-engine-installation landing first (the ProviderEngine trait and
  shared cache machinery this adapter implements against don't exist yet). Once that goal is
  done: verify sherpa-onnx's actual current per-model-family file manifests and real release/
  model URLs against the real k2-fsa/sherpa-onnx repo and releases page — not assumed from this
  goal's own notes, which were written during a design pass without checking real current
  releases or model repos.
success_criteria:
  - sherpa-onnx installs, caches under default_data_root(), and uninstalls cleanly through the same API/CLI surface every other engine uses.
  - Release assets are fetched from k2-fsa/sherpa-onnx's own releases, never rebuilt or re-hosted by stt-server.
  - At least one non-Whisper model family (e.g. an NVIDIA Parakeet/Canary ONNX export) is a real, installable catalog entry, proving the "one engine, many model families" shape works end-to-end.
source: user
---

# Add sherpa-onnx as a Real Managed Provider Engine

## Why this engine

Selected per `CONVENTIONS.md`'s engine-selection criteria: `k2-fsa/sherpa-onnx` is an actively
maintained official upstream repository with genuine broad adoption, Apache-2.0-licensed
(re-verify against the current `LICENSE` file before implementation, not assumed), and it
publishes real prebuilt release binaries for Windows (x86/x64/ARM64), Linux
(x64/ARM64/ARM32/RISC-V), and macOS (Universal) — meaning this adapter can fetch directly from
upstream rather than `stt-server` building and hosting its own duplicate copy.

Not a faster-whisper replacement: sherpa-onnx's own ONNX-exported Whisper path has a documented
accuracy regression versus faster-whisper on identical audio
([k2-fsa/sherpa-onnx#2900](https://github.com/k2-fsa/sherpa-onnx/issues/2900)). Its real value is
model families **neither faster-whisper nor whisper.cpp can run at all** — NVIDIA Parakeet/Canary,
Moonshine, SenseVoice, Zipformer/Paraformer — through one engine binary, matching the user's
explicit "min effort, minimal maintenance" requirement better than a bespoke adapter per family.

## Blocker

Hard-blocked on `generalize-provider-engine-installation` (currently `ready`, not yet attempted)
— this goal implements a `ProviderEngine` trait impl that doesn't exist as a trait yet. Also
depends on that goal's `cache::verify_files_present` shared helper, needed here specifically for
model families with multiple weight files (encoder/decoder/joiner/tokens).

## Scope, Acceptance Criteria, Verification

Not yet fleshed out — this is a placeholder to keep the roadmap trackable. Flesh out once the
blocker goal lands and its real trait shape (and any changes made during that implementation) are
known, and once sherpa-onnx's actual current per-model-family manifests have been checked
directly. Decide which specific model(s) to launch with (likely one Parakeet/Canary ONNX export as
the proof case, per this goal's own success criteria) rather than the full catalog at once.

## Attempts

No attempts yet.

## Ready For Execution

- Status: no
- Reason: Blocked on `generalize-provider-engine-installation`.
