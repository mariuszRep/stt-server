---
name: handy-gguf-parity-evidence
title: Prove Handy GGUF model and speech-function parity
description: Verify the complete claimed Handy GGUF catalog and each model's real speech capabilities, then record a replacement verdict against the old server.
status: ready
type: validation
scope: stt-server-next only
attempt: 0
max_attempts: 8
last_result: none
next_action: Build the model-by-model evidence matrix from the pinned Handy GGUF catalog and existing ledger; exercise missing families and options. The accuracy verdict for the default model is already recorded.
success_criteria:
  - Every model and published quantisation in the pinned Handy GGUF catalog has a recorded install/import, verification, load, transcription and removal result, or an explicit documented exception.
  - Actual prompts, language hints, translation, temperature and timestamp behaviour agree with the advertised capabilities for each applicable model.
  - The known moonshine verbose-response mismatch and language-diagnostic gaps are resolved or the claims are narrowed truthfully.
  - Replacement accuracy is accepted by the user after listening to the largest old/new disagreements (done 2026-09-26 for Parakeet TDT v2); other families need only a working transcription check, not a written reference.
  - Results distinguish the full pinned GGUF catalog from Handy's older ONNX or bin formats, which are outside this server's approved scope.
source: user
---

# Prove Handy GGUF model and speech-function parity

## Why

The catalog is copied and every entry is exposed, but that does not prove every model/quantisation actually loads, transcribes or honours its advertised options. This goal supplies the missing functional proof for the intended Handy-derived engine.

## Business rules

- Label catalog metadata, upstream claims and measurements made by this server separately.
- Use legal, consented or public audio fixtures; never commit personal recordings or transcripts.
- Record tested hardware and backend for each result. A failure must name the model/quantisation and observed consequence.
- No replacement verdict follows from speed alone or from word disagreement without a human reference.

## Plan

1. Inventory the pinned catalog and reconcile it with the existing parity ledger.
2. Run representative families and all remaining quantisations with supported option checks, using a resource-aware sequence.
3. Fix mismatches or narrow capability claims; recheck them with real audio.
4. Compare against human-checked speech and record a pass/fail matrix for the old-server replacement decision.

## Out of scope

Handy's desktop UI, microphone capture, ONNX and legacy bin models, public release and client migration.

## Related goals

- draft/ready-for-voice-typer: overall acceptance.
- voice-typer/in_progress/build-stt-server-next: original model-coverage commitment and current evidence log.

## Attempts

None yet.

## Do Not Repeat

- Do not equate catalog presence with a successful model run.
- Do not call the 6.2 percent word disagreement an error rate.

## Verification Log

2026-09-26: Created from the approved Handy GGUF coverage promise and existing parity gaps; no new model runs performed.

## Ready For Execution

The pinned catalog and existing ledger provide an inventory; the agent can build the matrix and run available fixtures while sourcing lawful multilingual reference audio.

## Final Outcome

Not started.

2026-09-26: Human accuracy verdict (default model, Parakeet TDT v2 Q8_0 on Vulkan vs old sherpa int8 ONNX). The user listened to the 12 longer clips (6 s or more) where the servers disagreed most, out of 483 real Voice Typer chunks. Verdict: both are good. Most differences came from poor audio or number formatting (digits vs words, which both models choose inconsistently, so it is not a server difference), plus spelling variants and filler words. The user accepts the new server's accuracy as a replacement. No written reference transcripts were made, by the user's decision. No recordings or transcripts were committed.
