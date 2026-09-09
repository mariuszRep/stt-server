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
  Parked 2026-09-09 behind add-sherpa-onnx-provider (see Blocker and Sequence). Do not start this
  until sherpa-onnx has landed and the latency problem it targets is resolved or understood. When
  picked up, verify whisper.cpp's current GitHub release asset layout (zip contents and CPU/CUDA/
  Vulkan/Metal packaging) against the real ggml-org/whisper.cpp releases, then flesh out this
  draft against the landed ProviderEngine/cache API before moving it to ready.
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

## Blocker and Sequence

Hard-blocked on `generalize-provider-engine-installation` (currently `ready`, not yet attempted)
— this goal implements a `ProviderEngine` trait impl that doesn't exist as a trait yet.

**Parked behind `add-sherpa-onnx-provider` (2026-09-09, user decision).** This goal was originally
the first adapter after the refactor; it is now the second. Rationale:

- The live product problem is Whisper-family latency. whisper.cpp is another Whisper engine, so it
  does not address it; sherpa-onnx does, by way of NVIDIA Parakeet.
- whisper.cpp is largely redundant with faster-whisper for the Whisper family, which faster-whisper
  already serves as the accuracy baseline.
- Its distinctive strength — the Apple Silicon Metal/CoreML path — is not load-bearing while the
  product ships Windows and Linux only. It becomes valuable when macOS becomes a live target, and
  that is the natural trigger to unpark this goal.
- Doing sherpa-onnx first also puts the harder multi-file/multi-family model shape against the new
  abstraction immediately, rather than letting an easier single-file adapter shape it first.

This goal remains genuinely wanted, not cancelled: it stays in the roster named by
`stt-server/VISION.md`'s 2026-08-30 key decision, and `stt-sdk` still exports an unimplemented
`WhisperCppProvider` seam awaiting it.

## Scope, Acceptance Criteria, Verification

Not yet fleshed out — this is a placeholder to keep the roadmap trackable. Flesh out once the
blocker goal lands and its real trait shape (and any changes made during that implementation) are
known, and once whisper.cpp's actual current release asset layout has been checked directly.

## Attempts

No attempts yet.

## Ready For Execution

- Status: no
- Reason: Blocked on `generalize-provider-engine-installation`, and deliberately parked behind
  `add-sherpa-onnx-provider`. Natural trigger to revisit: macOS becoming a live target, or
  sherpa-onnx failing to resolve the latency problem.
