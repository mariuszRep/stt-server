//! Offline speech detection on finished audio: `POST /v1/audio/vad`.
//!
//! A pure-Rust forward pass of Silero VAD v5 (16 kHz), so no ONNX runtime is linked. The
//! weights were exported once from the MIT-licensed `silero_vad.onnx` by
//! `scripts/export_silero_weights.py` into `assets/silero_v5_16k.f32`; the blob is raw
//! little-endian `f32` in the order [`Model::load`] reads it, and its SHA-256 is pinned by a
//! test and recorded in `THIRD_PARTY_NOTICES.md`.
//!
//! The pipeline is the Silero wrapper's: 512-sample frames (32 ms), each preceded by the last
//! 64 samples of the previous one, STFT as a convolution, four encoder convolutions, one
//! LSTM-cell step carried across frames, a 1x1 decoder convolution and a sigmoid. The frame
//! probabilities are turned into segments with the rules of Silero's `get_speech_timestamps`
//! (without its optional maximum-speech-length splitting). `tests/vad_golden.json` holds
//! onnxruntime's probabilities for the speech fixture; a test keeps this code within 1e-3 of
//! them. Live VAD stays client-side: this is only for audio the server already has.

use std::sync::OnceLock;

/// Identifies the model in `/v1/local/system` and the registry.
pub const MODEL_ID: &str = "silero-v5";
pub const SAMPLE_RATE: u32 = 16_000;
/// Samples scored per step: the only frame size the 16 kHz Silero model accepts.
pub const FRAME: usize = 512;
const CONTEXT: usize = 64;
const HIDDEN: usize = 128;
/// STFT window and hop of the model's first convolution, and its number of bins.
const STFT_WINDOW: usize = 256;
const STFT_HOP: usize = 128;
const BINS: usize = 129;
const KERNEL: usize = 3;

const WEIGHTS: &[u8] = include_bytes!("../assets/silero_v5_16k.f32");
/// What the registry reports about the compiled-in weights; a test pins the size and hash to the blob.
pub const WEIGHTS_FILE: &str = "silero_v5_16k.f32";
pub const WEIGHTS_BYTES: usize = WEIGHTS.len();
pub const WEIGHTS_SHA256: &str = "b8df2e6e32753b7aa47ab59571b0d9d0b490a223f8dc9118bb388efeaec6f8e3";

/// One encoder convolution (kernel 3, padding 1, ReLU after it). Weights are laid out
/// `[out][tap][in]` so the inner product of every tap is contiguous.
struct Conv {
    weight: Vec<f32>,
    bias: Vec<f32>,
    inputs: usize,
    stride: usize,
}

struct Model {
    /// `[2 * BINS][STFT_WINDOW]`: real rows first, then imaginary rows.
    stft_basis: Vec<f32>,
    encoder: [Conv; 4],
    /// `[4 * HIDDEN][HIDDEN]`, gate order input, forget, cell, output (PyTorch `LSTMCell`).
    weight_ih: Vec<f32>,
    weight_hh: Vec<f32>,
    bias_ih: Vec<f32>,
    bias_hh: Vec<f32>,
    decoder_weight: Vec<f32>,
    decoder_bias: f32,
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take(&mut self, count: usize) -> Vec<f32> {
        let (head, rest) = self.0.split_at(count * 4);
        self.0 = rest;
        head.as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect()
    }

    /// A convolution stored `[out][in][tap]` in the blob, re-laid out `[out][tap][in]`.
    fn conv(&mut self, outputs: usize, inputs: usize, stride: usize) -> Conv {
        let stored = self.take(outputs * inputs * KERNEL);
        let mut weight = vec![0.0; stored.len()];
        for o in 0..outputs {
            for i in 0..inputs {
                for k in 0..KERNEL {
                    weight[(o * KERNEL + k) * inputs + i] = stored[(o * inputs + i) * KERNEL + k];
                }
            }
        }
        Conv {
            weight,
            bias: self.take(outputs),
            inputs,
            stride,
        }
    }
}

impl Model {
    fn load() -> Self {
        let mut reader = Reader(WEIGHTS);
        let stft_basis = reader.take(2 * BINS * STFT_WINDOW);
        let encoder = [
            reader.conv(128, BINS, 1),
            reader.conv(64, 128, 2),
            reader.conv(64, 64, 2),
            reader.conv(128, 64, 1),
        ];
        let model = Self {
            stft_basis,
            encoder,
            weight_ih: reader.take(4 * HIDDEN * HIDDEN),
            weight_hh: reader.take(4 * HIDDEN * HIDDEN),
            bias_ih: reader.take(4 * HIDDEN),
            bias_hh: reader.take(4 * HIDDEN),
            decoder_weight: reader.take(HIDDEN),
            decoder_bias: reader.take(1)[0],
        };
        assert!(
            reader.0.is_empty(),
            "embedded VAD weights have trailing data"
        );
        model
    }
}

fn model() -> &'static Model {
    static MODEL: OnceLock<Model> = OnceLock::new();
    MODEL.get_or_init(Model::load)
}

/// Inner product with independent accumulators, so the compiler can vectorise it.
fn dot(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    let mut lanes = [0.0f32; 8];
    let ((a_chunks, a_tail), (b_chunks, b_tail)) = (a.as_chunks::<8>(), b.as_chunks::<8>());
    let tail: f32 = a_tail.iter().zip(b_tail).map(|(x, y)| x * y).sum();
    for (x, y) in a_chunks.iter().zip(b_chunks) {
        for lane in 0..8 {
            lanes[lane] += x[lane] * y[lane];
        }
    }
    lanes.iter().sum::<f32>() + tail
}

fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

impl Conv {
    /// `input` is time-major, `[time][inputs]`; the result is `[time'][outputs]` after ReLU.
    fn forward(&self, input: &[f32]) -> Vec<f32> {
        let steps = input.len() / self.inputs;
        let outputs = self.bias.len();
        let out_steps = (steps + 2 - KERNEL) / self.stride + 1;
        let mut out = vec![0.0; out_steps * outputs];
        for t in 0..out_steps {
            for (o, slot) in out[t * outputs..(t + 1) * outputs].iter_mut().enumerate() {
                let mut acc = self.bias[o];
                for k in 0..KERNEL {
                    // Position in the input; the padding row at either edge is zero.
                    let Some(at) = (t * self.stride + k).checked_sub(1).filter(|&p| p < steps)
                    else {
                        continue;
                    };
                    let taps = &self.weight[(o * KERNEL + k) * self.inputs..][..self.inputs];
                    acc += dot(taps, &input[at * self.inputs..][..self.inputs]);
                }
                *slot = acc.max(0.0);
            }
        }
        out
    }
}

/// Scores consecutive [`FRAME`]-sample frames of one recording, carrying the LSTM state and
/// the 64-sample context from frame to frame.
struct Scorer {
    model: &'static Model,
    h: [f32; HIDDEN],
    c: [f32; HIDDEN],
    context: [f32; CONTEXT],
}

impl Scorer {
    fn new() -> Self {
        Self {
            model: model(),
            h: [0.0; HIDDEN],
            c: [0.0; HIDDEN],
            context: [0.0; CONTEXT],
        }
    }

    /// Speech probability of one frame (zero-pad a short last frame to [`FRAME`] first).
    fn score(&mut self, frame: &[f32; FRAME]) -> f32 {
        let model = self.model;
        // Context plus frame, then 64 samples of right-side reflection padding.
        let mut padded = [0.0f32; CONTEXT + FRAME + CONTEXT];
        padded[..CONTEXT].copy_from_slice(&self.context);
        padded[CONTEXT..CONTEXT + FRAME].copy_from_slice(frame);
        for i in 0..CONTEXT {
            padded[CONTEXT + FRAME + i] = padded[CONTEXT + FRAME - 2 - i];
        }
        self.context.copy_from_slice(&frame[FRAME - CONTEXT..]);

        // STFT magnitude, time-major `[time][BINS]`.
        let steps = (padded.len() - STFT_WINDOW) / STFT_HOP + 1;
        let mut spectrum = vec![0.0f32; steps * BINS];
        for t in 0..steps {
            let window = &padded[t * STFT_HOP..][..STFT_WINDOW];
            for bin in 0..BINS {
                let real = dot(
                    &model.stft_basis[bin * STFT_WINDOW..][..STFT_WINDOW],
                    window,
                );
                let imag = dot(
                    &model.stft_basis[(BINS + bin) * STFT_WINDOW..][..STFT_WINDOW],
                    window,
                );
                spectrum[t * BINS + bin] = (real * real + imag * imag).sqrt();
            }
        }

        let encoded = model
            .encoder
            .iter()
            .fold(spectrum, |input, conv| conv.forward(&input));
        // The frame and context sizes fixed above leave exactly one time step here.
        debug_assert_eq!(encoded.len(), HIDDEN);

        let mut gates = [0.0f32; 4 * HIDDEN];
        for (g, gate) in gates.iter_mut().enumerate() {
            let row = g * HIDDEN..(g + 1) * HIDDEN;
            *gate = model.bias_ih[g]
                + model.bias_hh[g]
                + dot(&model.weight_ih[row.clone()], &encoded)
                + dot(&model.weight_hh[row], &self.h);
        }
        for j in 0..HIDDEN {
            let input = sigmoid(gates[j]);
            let forget = sigmoid(gates[HIDDEN + j]);
            let cell = gates[2 * HIDDEN + j].tanh();
            let output = sigmoid(gates[3 * HIDDEN + j]);
            self.c[j] = forget * self.c[j] + input * cell;
            self.h[j] = output * self.c[j].tanh();
        }
        let relu: Vec<f32> = self.h.iter().map(|v| v.max(0.0)).collect();
        sigmoid(model.decoder_bias + dot(&model.decoder_weight, &relu))
    }
}

/// Speech probability of every [`FRAME`]-sample frame of 16 kHz mono audio; a short last
/// frame is zero-padded.
pub fn frame_probabilities(samples: &[f32]) -> Vec<f32> {
    let mut scorer = Scorer::new();
    samples
        .chunks(FRAME)
        .map(|chunk| {
            let mut frame = [0.0f32; FRAME];
            frame[..chunk.len()].copy_from_slice(chunk);
            scorer.score(&frame)
        })
        .collect()
}

/// Segmenter settings (Silero's `get_speech_timestamps` names, durations in milliseconds).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Params {
    pub threshold: f32,
    pub min_speech_ms: u32,
    pub min_silence_ms: u32,
    pub speech_pad_ms: u32,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            threshold: 0.5,
            min_speech_ms: 250,
            min_silence_ms: 100,
            speech_pad_ms: 30,
        }
    }
}

impl Params {
    pub const MAX_MS: u32 = 60_000;

    /// `Err` carries the message for the 400 response.
    pub fn validate(&self) -> Result<(), String> {
        if !(0.05..=0.95).contains(&self.threshold) {
            return Err("threshold must be between 0.05 and 0.95".to_owned());
        }
        for (name, value) in [
            ("min_speech_ms", self.min_speech_ms),
            ("min_silence_ms", self.min_silence_ms),
            ("speech_pad_ms", self.speech_pad_ms),
        ] {
            if value > Self::MAX_MS {
                return Err(format!("{name} must be between 0 and {}", Self::MAX_MS));
            }
        }
        Ok(())
    }
}

/// A speech span as sample indices into the 16 kHz audio, `start` inclusive, `end` exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

/// Silero's `get_speech_timestamps` over frame probabilities. Speech starts at the first frame
/// at or above `threshold` and ends once probabilities stay below `threshold - 0.15` (not
/// below 0.01) for `min_silence_ms`; spans no longer than `min_speech_ms` are dropped; every
/// span grows by `speech_pad_ms`, neighbours closer than twice the pad meet in the middle of
/// their gap, and the result is clamped to `0..total_samples`. Spans that end up touching are
/// merged.
pub fn segments(probabilities: &[f32], total_samples: usize, params: &Params) -> Vec<Span> {
    let ms = |value: u32| value as usize * (SAMPLE_RATE as usize / 1000);
    let (min_speech, min_silence, pad) = (
        ms(params.min_speech_ms),
        ms(params.min_silence_ms),
        ms(params.speech_pad_ms),
    );
    let negative_threshold = (params.threshold - 0.15).max(0.01);

    let mut spans: Vec<Span> = Vec::new();
    let mut start = None;
    let mut silence_from: Option<usize> = None;
    for (i, &probability) in probabilities.iter().enumerate() {
        let at = i * FRAME;
        if probability >= params.threshold {
            silence_from = None;
            if start.is_none() {
                start = Some(at);
            }
        } else if probability < negative_threshold {
            if let Some(speech_start) = start {
                let silence_start = *silence_from.get_or_insert(at);
                if at - silence_start >= min_silence {
                    if silence_start - speech_start > min_speech {
                        spans.push(Span {
                            start: speech_start,
                            end: silence_start,
                        });
                    }
                    start = None;
                    silence_from = None;
                }
            }
        }
    }
    if let Some(speech_start) = start {
        if total_samples - speech_start > min_speech {
            spans.push(Span {
                start: speech_start,
                end: total_samples,
            });
        }
    }

    for i in 0..spans.len() {
        if i == 0 {
            spans[0].start = spans[0].start.saturating_sub(pad);
        }
        if let Some(next) = spans.get(i + 1).copied() {
            let gap = next.start - spans[i].end;
            if gap < 2 * pad {
                spans[i].end += gap / 2;
                spans[i + 1].start = next.start - gap / 2;
            } else {
                spans[i].end = (spans[i].end + pad).min(total_samples);
                spans[i + 1].start = next.start - pad;
            }
        } else {
            spans[i].end = (spans[i].end + pad).min(total_samples);
        }
    }

    let mut merged: Vec<Span> = Vec::with_capacity(spans.len());
    for span in spans {
        match merged.last_mut() {
            // Spans that met in the middle of a gap can be one sample apart (odd gap).
            Some(last) if span.start <= last.end + 1 => last.end = last.end.max(span.end),
            _ => merged.push(span),
        }
    }
    merged
}

/// Speech spans of 16 kHz mono audio.
pub fn detect(samples: &[f32], params: &Params) -> Vec<Span> {
    segments(&frame_probabilities(samples), samples.len(), params)
}

pub(crate) fn rounded(value: f64, decimals: i32) -> f64 {
    let scale = 10f64.powi(decimals);
    (value * scale).round() / scale
}

/// The `POST /v1/audio/vad` response body for 16 kHz mono audio: times in seconds, rounded to
/// milliseconds; `speech_ratio` is the share of the audio inside segments.
pub fn report(samples: &[f32], params: &Params) -> serde_json::Value {
    let seconds = |count: usize| count as f64 / f64::from(SAMPLE_RATE);
    let spans = detect(samples, params);
    let speech_samples: usize = spans.iter().map(|s| s.end - s.start).sum();
    serde_json::json!({
        "duration": rounded(seconds(samples.len()), 3),
        "speech_seconds": rounded(seconds(speech_samples), 3),
        "speech_ratio": rounded(speech_samples as f64 / samples.len().max(1) as f64, 4),
        "segments": spans
            .iter()
            .map(|s| serde_json::json!({
                "start": rounded(seconds(s.start), 3),
                "end": rounded(seconds(s.end), 3),
            }))
            .collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    fn fixture() -> Vec<f32> {
        crate::audio::decode_wav(include_bytes!("../tests/fixtures/speech-16k.wav")).unwrap()
    }

    #[test]
    fn embedded_weights_are_the_exported_blob() {
        assert_eq!(WEIGHTS_BYTES, 309_633 * 4);
        assert_eq!(format!("{:x}", Sha256::digest(WEIGHTS)), WEIGHTS_SHA256);
    }

    #[test]
    fn matches_onnxruntime_on_the_speech_fixture() {
        let golden: serde_json::Value =
            serde_json::from_str(include_str!("../tests/vad_golden.json")).unwrap();
        assert_eq!(golden["frame_samples"], FRAME);
        let speech = fixture();
        for case in golden["cases"].as_array().unwrap() {
            let gain = case["gain"].as_f64().unwrap() as f32;
            let scaled: Vec<f32> = speech.iter().map(|s| s * gain).collect();
            let expected = case["probabilities"].as_array().unwrap();
            let actual = frame_probabilities(&scaled);
            assert_eq!(actual.len(), expected.len());
            let worst = actual
                .iter()
                .zip(expected)
                .map(|(a, e)| (f64::from(*a) - e.as_f64().unwrap()).abs())
                .fold(0.0, f64::max);
            assert!(worst < 1e-3, "gain {gain}: worst frame error {worst}");
        }
    }

    #[test]
    fn finds_speech_in_the_fixture() {
        let speech = fixture();
        let spans = detect(&speech, &Params::default());
        assert!(!spans.is_empty());
        let covered: usize = spans.iter().map(|s| s.end - s.start).sum();
        assert!(covered * 2 > speech.len(), "{covered} of {}", speech.len());
        assert!(spans
            .iter()
            .all(|s| s.start < s.end && s.end <= speech.len()));
        assert!(spans.windows(2).all(|w| w[0].end < w[1].start));
    }

    #[test]
    fn silence_a_tone_and_faint_noise_have_no_speech() {
        let seconds = 3 * SAMPLE_RATE as usize;
        let silence = vec![0.0f32; seconds];
        let tone: Vec<f32> = (0..seconds)
            .map(|n| 0.5 * (2.0 * std::f32::consts::PI * 440.0 * n as f32 / 16_000.0).sin())
            .collect();
        let mut seed = 12_345u32;
        let noise: Vec<f32> = (0..seconds)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                ((seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.01
            })
            .collect();
        for (name, audio) in [("silence", silence), ("tone", tone), ("noise", noise)] {
            assert_eq!(detect(&audio, &Params::default()), vec![], "{name}");
        }
    }

    /// Probabilities from a pattern of `(probability, frames)` runs.
    fn runs(pattern: &[(f32, usize)]) -> Vec<f32> {
        pattern
            .iter()
            .flat_map(|&(p, n)| std::iter::repeat_n(p, n))
            .collect()
    }

    fn span(start: usize, end: usize) -> Span {
        Span { start, end }
    }

    #[test]
    fn a_segment_is_padded_and_clamped_to_the_audio() {
        // 10 silent frames, 20 speech frames, 10 silent frames.
        let probabilities = runs(&[(0.0, 10), (0.9, 20), (0.0, 10)]);
        let total = 40 * FRAME;
        let spans = segments(&probabilities, total, &Params::default());
        assert_eq!(spans, vec![span(10 * FRAME - 480, 30 * FRAME + 480)]);
        // Speech running into both edges is clamped.
        let spans = segments(&runs(&[(0.9, 40)]), total, &Params::default());
        assert_eq!(spans, vec![span(0, total)]);
    }

    #[test]
    fn short_speech_is_dropped_and_short_pauses_do_not_split() {
        let total = 60 * FRAME;
        // 7 frames = 224 ms of speech is not longer than 250 ms.
        let short = runs(&[(0.0, 10), (0.9, 7), (0.0, 43)]);
        assert_eq!(segments(&short, total, &Params::default()), vec![]);
        // A 2-frame (64 ms) dip is shorter than min_silence_ms: one segment.
        let dip = runs(&[(0.0, 5), (0.9, 10), (0.0, 2), (0.9, 10), (0.0, 33)]);
        assert_eq!(segments(&dip, total, &Params::default()).len(), 1);
        // A 5-frame (160 ms) pause splits.
        let pause = runs(&[(0.0, 5), (0.9, 10), (0.0, 5), (0.9, 10), (0.0, 30)]);
        assert_eq!(segments(&pause, total, &Params::default()).len(), 2);
    }

    #[test]
    fn the_hysteresis_band_keeps_a_segment_open() {
        // 0.4 is below the 0.5 threshold but above 0.35, so it is neither speech nor silence.
        let probabilities = runs(&[(0.9, 10), (0.4, 10), (0.9, 10), (0.0, 10)]);
        let spans = segments(&probabilities, 40 * FRAME, &Params::default());
        assert_eq!(spans, vec![span(0, 30 * FRAME + 480)]);
    }

    #[test]
    fn neighbours_closer_than_twice_the_pad_meet_in_the_middle_and_merge() {
        // Two 12-frame segments 5 frames (2560 samples) apart; a 100 ms pad is 1600 samples,
        // and 2560 < 3200, so they meet and merge into one.
        let params = Params {
            speech_pad_ms: 100,
            ..Params::default()
        };
        let probabilities = runs(&[(0.0, 5), (0.9, 12), (0.0, 5), (0.9, 12), (0.0, 6)]);
        let spans = segments(&probabilities, 40 * FRAME, &params);
        assert_eq!(spans, vec![span(5 * FRAME - 1600, 34 * FRAME + 1600)]);
    }

    #[test]
    fn distant_neighbours_stay_apart() {
        let probabilities = runs(&[(0.0, 5), (0.9, 12), (0.0, 20), (0.9, 12), (0.0, 5)]);
        let spans = segments(&probabilities, 54 * FRAME, &Params::default());
        assert_eq!(
            spans,
            vec![
                span(5 * FRAME - 480, 17 * FRAME + 480),
                span(37 * FRAME - 480, 49 * FRAME + 480)
            ]
        );
    }

    #[test]
    fn params_are_validated() {
        assert!(Params::default().validate().is_ok());
        for threshold in [0.04, 0.96, f32::NAN] {
            let params = Params {
                threshold,
                ..Params::default()
            };
            assert!(params.validate().is_err(), "{threshold}");
        }
        let params = Params {
            speech_pad_ms: Params::MAX_MS + 1,
            ..Params::default()
        };
        assert!(params.validate().is_err());
    }
}
