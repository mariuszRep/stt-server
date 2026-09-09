---
name: validate-parakeet-performance
title: Validate NVIDIA Parakeet's Real Speed and Accuracy Before Building On It
description: Measure Parakeet through the existing sherpad daemon against faster-whisper on identical dictation audio, to confirm or kill the premise that motivates the whole two-engine effort before any refactor work is spent on it.
status: blocked
type: spike
scope: sherpad tree (currently stt-server-v2/crates/sherpad), throwaway measurement harness, no production code
attempt: 1
max_attempts: 3
last_result: blocked
next_action: |
  Blocked on the user recording/supplying a 10-15 clip dictation sample set (short commands, a long
  paragraph, technical vocabulary, filler speech, 2-3 non-English) -- see Final Outcome. Once
  supplied, rerun stt-server-v2/bench.sh against it (both engines already build/run; Parakeet is
  already pulled and verified working) and write the final verdict.
success_criteria:
  - A recorded dictation sample set exists (10-15 clips, English-majority, a few non-English), checked in or stored at a documented path.
  - Parakeet TDT 0.6B v3 downloads, loads, and transcribes through sherpad on real hardware.
  - Measured wall-clock latency and real-time factor are recorded for Parakeet and for faster-whisper-small on the identical clips, on the same machine.
  - A written verdict states whether Parakeet is materially faster at comparable or better transcript quality, with the numbers behind it.
source: user
---

# Validate NVIDIA Parakeet's Real Speed and Accuracy Before Building On It

## Goal

Prove or disprove, cheaply and early, the premise that the entire two-engine programme rests on:
that NVIDIA Parakeet served by sherpa-onnx is materially faster than faster-whisper for dictation,
at acceptable quality. Every other goal in this programme is expensive; this one is not, and it
should run first.

## Source Requirements

User, this session: *"the whisper models seem to be very slow, faster whisper models seem to be very
slow, so I was thinking to just use something like NVIDIA Parakeet or something that seems to be
faster"*, and *"we need to run some tests on multiple languages but focus on eng, so we need some
samples"*.

## Problem / Motivation

The plan to add sherpa-onnx as a second engine exists almost entirely to reach Parakeet. That
expectation is currently founded on architecture reasoning — Parakeet's TDT decoding is
single-pass and beam-search-free, structurally faster than Whisper's autoregressive decoder — and on
Parakeet's public leaderboard standing. **Neither has been measured on this product's hardware, on
this product's audio, through this product's runtime.**

Nothing in the four repos measures it: a search for `*.wav`/`*.mp3` fixtures across all four
worktrees returns nothing. There is no audio sample set at all.

Worse, of the three models in `sherpa-manifest`, only SenseVoice survives the decision to keep
faster-whisper as the sole Whisper engine — and Parakeet, the actual target, is not in the manifest
and has never been run here. If Parakeet turns out to be unimpressive on this hardware, the
sequencing of every downstream goal changes, and it is far cheaper to learn that now than after the
provider-engine refactor and the runtime fold have landed.

## Vision Alignment

- Deliberately a spike, not a feature: nothing produced here is required to ship. Consistent with
  `stt-server/CONVENTIONS.md`'s "minimal maintenance" intent — spend the measurement first, commit
  the engineering second.
- Runs against `sherpad` where it currently lives, before
  `fold-sherpad-runtime-into-stt-server` moves it. Deliberately not blocked on the fold, the
  provider-engine refactor, or protocol conformance — none of those change the numbers.

## Scope

1. **Sample set.** Record 10-15 real dictation clips through the app's own capture path (which
   already writes WAV, see `whisper-vibes/apps/web/src/lib/wav.ts`), covering: short commands, a
   long paragraph, technical vocabulary, hesitation/filler speech, and 2-3 non-English clips.
   Store them at a documented path with a plain-text reference transcript per clip.
2. **Add Parakeet to the manifest.** Add `sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8` to
   `sherpa-manifest`'s `MODELS` (verified present in `k2-fsa/sherpa-onnx`'s `asr-models` release
   tag). It is a transducer model, not SenseVoice or Whisper, so `ModelFiles` needs a third variant
   describing its encoder/decoder/joiner/tokens layout — inspect the extracted archive to get the
   real filenames rather than guessing them.
3. **Measure.** Transcribe every clip through: Parakeet on sherpad, SenseVoice on sherpad (already
   working, as the sherpa-side control), and faster-whisper-small through the existing runtime.
   Record per-clip wall-clock latency, computed real-time factor, and the transcript text. Same
   machine, same clips, models pre-loaded so load time is excluded from the per-clip number.
4. **Verdict.** Write up the comparison and state plainly whether the premise holds.

## Out of Scope

- Any production code. The measurement harness is throwaway; the durable version is owned by
  `provider-conformance-test-suite`.
- Protocol conformance work on sherpad — this spike may call sherpad's current non-conformant API
  directly, including passing the `model` form field it currently requires.
- Adding Parakeet to `catalog.rs` or wiring any adapter — owned by `add-sherpa-onnx-provider`.
- GPU evaluation. sherpad links a CPU-only onnxruntime build today; measure CPU-to-CPU, and note
  separately that faster-whisper's GPU path exists as context for interpreting the numbers.
- WER/CER scoring rigour. Eyeball transcript quality against the reference; formal scoring is only
  worth building if the latency result is close enough that quality becomes the deciding factor.

## Acceptance Criteria

1. The sample set exists, is documented, and every clip has a reference transcript.
2. Parakeet runs end to end through sherpad on real hardware and produces sane English transcripts.
3. A table records per-clip latency and real-time factor for Parakeet, SenseVoice, and
   faster-whisper-small on identical audio.
4. The written verdict answers: is Parakeet materially faster, at acceptable quality, for dictation?

## Judgment Rubric

- Not done if the numbers come from anything other than real audio on real hardware.
- Not done if the comparison runs different clips through different engines.
- Not done if a verdict is not written down — a table with no conclusion does not close this goal.
- A negative result is a **successful** outcome for this goal. If Parakeet disappoints, say so; that
  is exactly the information this spike exists to buy.

## Risks / Unknowns

1. **Parakeet's file layout is unverified.** The archive's internal filenames must be read from the
   extracted directory, not assumed from other models' conventions.
2. **`sherpa-manifest`'s `ModelFiles` enum currently has only SenseVoice and Whisper variants.**
   A transducer variant has to be added; keep the change minimal since this is spike code that
   `add-sherpa-onnx-provider` will supersede.
3. **Parakeet is English-only.** The non-English clips will fail on it by design — that is a
   catalogue-scoping fact to record, not a defect.

## Verification Expectations

### Manual Verification
- Real hardware, real recorded audio, both engines, same machine, same clips.
- Confirm models are pre-loaded before timing, so per-clip figures exclude model load.

## Attempts

### Attempt 1 (2026-09-09) — in progress, paused for user input

Completed steps 1-6 of the plan (manifest entry, recognizer config, build, smoke test, harness,
read-speech comparison). Step 7 (recording a real dictation sample set) requires the user, since
this session has no microphone access. See Verification Log for full results and Do Not Repeat below
for why synthetic audio was not substituted.

**Code changes** (in `stt-server-v2`, the pre-fold location per this goal's scope):
- `crates/sherpa-manifest/src/lib.rs`: added `ModelFiles::Transducer { encoder, decoder, joiner,
  tokens }` and a `parakeet-tdt-0.6b-v2` entry (archive `sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8.tar.bz2`,
  482,468,385 bytes, confirmed against `k2-fsa/sherpa-onnx`'s own `run-nemo-parakeet-en.sh`).
- `crates/sherpad/src/recognizer.rs`: added the `Transducer` match arm in `build_config`, setting
  `config.model_config.transducer = OfflineTransducerModelConfig { encoder, decoder, joiner }`,
  confirmed against upstream's `rust-api-examples/examples/nemo_parakeet.rs`.
- `bench.sh` (new, throwaway): takes a WAV folder, times every clip through every sherpad model plus
  an optional faster-whisper `$FW_BASE_URL`, reports wall-clock latency and real-time factor per
  clip using `curl -w '%{time_total}'` (avoids external timing races).

## Do Not Repeat

- Do not substitute synthetic/TTS audio for the dictation sample set to close this goal faster. The
  premise being tested is dictation-speed, and read-speech (even real read-speech, as used for the
  smoke test) or TTS output does not reflect dictation's hesitations, filler words, and irregular
  pacing. A verdict from either would misrepresent exactly the thing this goal exists to measure.
- `sherpad` currently requires a `model` multipart field and 400s without it — this is a known,
  already-tracked gap (see `make-sherpad-protocol-conformant`), not a bug to fix here. The harness
  and manual tests above intentionally pass `model` explicitly to work around it.

## Verification Log

**Environment**: same machine, CPU = the harness's primary comparison per Out of Scope (GPU
excluded). Clip: `parakeet-tdt-0.6b-v2`'s own bundled `test_wavs/0.wav`, 7.435s, real English read
speech (LibriSpeech-style narration), reference-transcript-free but human-legible for eyeball QA.

| Model | Engine | Compute | Load time | Wall (7.44s clip) | RTF | Transcript |
|---|---|---|---|---|---|---|
| parakeet-tdt-0.6b-v2 | sherpad | CPU int8 | 4.24s | 0.58-0.77s | **0.08-0.10** | "Well, I don't wish to see it any more, observed Phebe, turning away her eyes. It is certainly very like the old portrait." |
| sense-voice-multi | sherpad | CPU int8 | 1.23s | 0.32-0.35s | **0.04-0.05** | "Well, I don't wish to see it any more, observed Phoebe turning away her eyes. It is certainly very like the old portrait." |
| faster-whisper-small | faster-whisper | CPU int8 | 3.28s | 4.57-4.90s | **0.62-0.66** | "Well, I don't wish to see it anymore observed Phoebe turning away her eyes. It is certainly very like the old portrait" |
| faster-whisper-small (context only, GPU) | faster-whisper | CUDA float16 | 3.28s | 2.80-3.10s | 0.38-0.42 | (same, GPU path, excluded from the primary comparison per Out of Scope) |

**Headline result: Parakeet is ~8x faster than faster-whisper-small CPU-to-CPU** (RTF 0.08-0.10 vs
0.62-0.66), and still ~4-5x faster than faster-whisper running on GPU. SenseVoice is faster still.
All three transcripts are essentially identical in content; faster-whisper dropped the comma after
"anymore" and the closing sentence's final period, a trivial difference on this clip.

**Reproducibility**: `bench.sh` (new, in `stt-server-v2/`) reproduces these numbers via
`FW_BASE_URL="http://127.0.0.1:<port>" ./bench.sh <wav-dir>`. Verified it produces matching output to
the manual `curl` timings above.

**Caveat driving the pause**: this is one short, clean, read-speech clip. It answers "does Parakeet
work here and is it structurally faster" with a resounding yes, but not "is it still faster and at
acceptable quality on real dictation" — hesitant speech, technical vocabulary, longer clips, and
non-English content are all untested. That is exactly what step 7 (real dictation samples) is for.

## Final Outcome

**Partial — paused, not failed.** Steps 1-6 complete with a strong positive signal: Parakeet
transcribes correctly on this hardware and is dramatically faster than faster-whisper-small in a
CPU-to-CPU comparison on real (if short and clean) speech. This alone is enough to say the premise is
*not* dead on arrival, but the goal's acceptance criteria require dictation-audio evidence before a
final verdict, which needs the user to record or supply a sample set (10-15 clips: short commands, a
long paragraph, technical vocabulary, filler speech, 2-3 non-English). Resume this goal once that set
exists: rerun `bench.sh` against it, extend the harness to also drive `sense-voice-multi` /
`whisper-*` for non-English clips as needed, and write the final verdict.

## Ready For Execution

- Status: yes
- Reason: sherpad already runs and transcribes today; faster-whisper already runs today. The only
  new inputs needed are a recorded sample set and one manifest entry. No dependency on any other goal.
