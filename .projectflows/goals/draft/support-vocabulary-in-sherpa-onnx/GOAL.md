---
name: support-vocabulary-in-sherpa-onnx
title: Make Vocabulary Context Actually Work on the sherpa-onnx Runtime
description: The SDK sends a per-request vocabulary/prompt to bias transcription, but sherpad (the sherpa-onnx runtime) silently discards it -- none of the currently-shipped models end up biased by it, with no error or signal to the caller. Figure out how to make vocabulary context real for this runtime, for whichever currently-shipped models can actually support it.
status: draft
type: feature
scope: stt-server/runtimes/sherpa-onnx
attempt: 0
max_attempts: 3
last_result: none
next_action: |
  Open question, not yet a defined implementation path. Needs investigation into what it takes to
  get contextual biasing genuinely working for at least one currently-shipped sherpa-onnx model,
  and a decision on whether that's worth doing per-model given the shipped lineup may not be a good
  fit as-is.
success_criteria:
  - A caller that supplies vocabulary context to a sherpa-onnx-backed session gets transcriptions
    that are demonstrably biased toward that vocabulary, for at least one shipped model -- or the
    runtime clearly signals that it can't honor the request, instead of silently ignoring it.
source: user
---

# Make Vocabulary Context Actually Work on the sherpa-onnx Runtime

## Goal

Today, vocabulary/context the SDK sends with a transcription request has no effect at all when the
active runtime is sherpa-onnx -- it's accepted by the wire protocol and then dropped without a
trace. Whatever the user configures as their vocabulary, they get zero benefit from it whenever
sherpa-onnx is serving the request. We want that to stop being true: vocabulary context supplied by
a caller should actually influence transcription output on this runtime, at least for the models we
currently ship, or the runtime should say plainly that it can't do it for a given model rather than
pretending to accept it.

## Problem / Motivation

The faster-whisper runtime already honors vocabulary context end-to-end (it's threaded through to
the underlying engine's prompt-conditioning). The sherpa-onnx runtime (`sherpad`) does not: the
field is parsed off the wire and then has nowhere to go, for every model currently offered through
it. A user who picks the sherpa-onnx runtime for its speed or footprint is silently losing a feature
they may be relying on, with no indication anything is missing -- the request succeeds normally and
returns a transcription, just one that never got a chance to be nudged toward their vocabulary.

This is a gap between what the product appears to promise (configure your vocabulary, it improves
recognition) and what actually happens depending on which runtime is active underneath, which is an
implementation detail the user shouldn't have to know about or work around.

## Scope

- Make vocabulary context have a genuine effect on transcription output for the sherpa-onnx runtime,
  for at least one of the models it currently ships.
- If a given shipped model fundamentally can't honor vocabulary context, the runtime should make
  that limitation visible rather than quietly no-op-ing.

## Out of Scope

- Changing which models the sherpa-onnx runtime ships as its curated lineup, purely to make
  vocabulary support easier -- that's a separate decision if it comes up.
- Any change to the faster-whisper runtime, which already works.
- Prescribing the mechanism here -- how this gets implemented is an open question for whoever picks
  this goal up.

## Acceptance Criteria

1. For at least one currently-shipped sherpa-onnx model, a request carrying vocabulary context
   produces transcription output that is verifiably different/better for terms in that vocabulary
   than the same request without it.
2. For any shipped model where this turns out not to be achievable, the runtime's behavior when
   vocabulary context is supplied is explicit (a clear signal to the caller) rather than silent.

## Verification Expectations

### Automated Verification
- A test that sends the same audio with and without vocabulary context to the sherpa-onnx runtime
  and asserts the outputs differ in the expected direction for at least one supported model.

### Manual Verification
- Real end-to-end check with a vocabulary term absent from a model's default output, confirming it
  appears once supplied as context.

## Attempts

No attempts yet.

## Ready For Execution

- Status: no
- Reason: Still a draft -- the feasible approach (which model(s), what's required to get there) is
  unresolved and needs investigation before this can be scoped into concrete next actions.
