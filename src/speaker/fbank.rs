//! Kaldi-style log-mel filterbank features, as 3D-Speaker computes them with torchaudio's
//! `kaldi.fbank` (80 bins, 25 ms frames every 10 ms, no dither) followed by per-recording mean
//! subtraction.

use std::f32::consts::PI;
use std::sync::{Arc, OnceLock};

use realfft::num_complex::Complex;
use realfft::{RealFftPlanner, RealToComplex};

use super::{dot, SAMPLE_RATE};

pub(super) const MEL_BINS: usize = 80;
const FRAME_LEN: usize = 400;
const FRAME_SHIFT: usize = 160;
const FFT_LEN: usize = 512;
const SPECTRUM: usize = FFT_LEN / 2 + 1;
const PREEMPHASIS: f32 = 0.97;
const LOW_HZ: f64 = 20.0;
const HIGH_HZ: f64 = 8_000.0;
/// Kaldi floors the mel energies at the f32 epsilon before the log.
const ENERGY_FLOOR: f32 = f32::EPSILON;

/// Mel scale as Kaldi defines it.
fn mel(hz: f64) -> f64 {
    1127.0 * (1.0 + hz / 700.0).ln()
}

/// The Kaldi filterbank front end: framing, DC removal, pre-emphasis, Povey window, power
/// spectrum, triangular mel filters, log.
struct Frontend {
    fft: Arc<dyn RealToComplex<f32>>,
    window: [f32; FRAME_LEN],
    /// Per mel bin: the first spectrum bin it covers and its weights from there on.
    filters: Vec<(usize, Vec<f32>)>,
}

impl Frontend {
    fn new() -> Self {
        let window = std::array::from_fn(|n| {
            let hann = 0.5 - 0.5 * (2.0 * PI * n as f32 / (FRAME_LEN - 1) as f32).cos();
            hann.powf(0.85)
        });
        let bin_hz = f64::from(SAMPLE_RATE) / FFT_LEN as f64;
        let (low, high) = (mel(LOW_HZ), mel(HIGH_HZ));
        let delta = (high - low) / (MEL_BINS + 1) as f64;
        let filters = (0..MEL_BINS)
            .map(|bin| {
                let left = low + bin as f64 * delta;
                let centre = left + delta;
                let right = centre + delta;
                // The Nyquist bin (index FFT_LEN / 2) has weight zero in every filter.
                let weights: Vec<f64> = (0..SPECTRUM - 1)
                    .map(|k| {
                        let m = mel(bin_hz * k as f64);
                        ((m - left) / (centre - left))
                            .min((right - m) / (right - centre))
                            .max(0.0)
                    })
                    .collect();
                let first = weights.iter().position(|&w| w > 0.0).unwrap_or(0);
                let last = weights.iter().rposition(|&w| w > 0.0).unwrap_or(0);
                let slice = weights[first..=last].iter().map(|&w| w as f32).collect();
                (first, slice)
            })
            .collect();
        Self {
            fft: RealFftPlanner::<f32>::new().plan_fft_forward(FFT_LEN),
            window,
            filters,
        }
    }

    /// Log-mel features of the audio as rows of [`MEL_BINS`] (empty if under one frame), each
    /// column then mean-subtracted over the whole recording.
    fn features(&self, samples: &[f32]) -> Vec<f32> {
        let Some(frames) = samples.len().checked_sub(FRAME_LEN) else {
            return Vec::new();
        };
        let frames = frames / FRAME_SHIFT + 1;
        let mut features = vec![0.0f32; frames * MEL_BINS];
        let mut input = vec![0.0f32; FFT_LEN];
        let mut spectrum = vec![Complex::new(0.0f32, 0.0); SPECTRUM];
        let mut scratch = self.fft.make_scratch_vec();
        for (frame, row) in features
            .as_chunks_mut::<MEL_BINS>()
            .0
            .iter_mut()
            .enumerate()
        {
            let samples = &samples[frame * FRAME_SHIFT..][..FRAME_LEN];
            let mean = samples.iter().sum::<f32>() / FRAME_LEN as f32;
            for n in 0..FRAME_LEN {
                // The first sample is pre-emphasised against itself (Kaldi replicates the edge).
                let previous = samples[n.saturating_sub(1)];
                input[n] = (samples[n] - mean - PREEMPHASIS * (previous - mean)) * self.window[n];
            }
            input[FRAME_LEN..].fill(0.0);
            self.fft
                .process_with_scratch(&mut input, &mut spectrum, &mut scratch)
                .expect("FFT buffers have the planned lengths");
            let power: Vec<f32> = spectrum.iter().map(Complex::norm_sqr).collect();
            for (slot, (first, weights)) in row.iter_mut().zip(&self.filters) {
                *slot = dot(weights, &power[*first..][..weights.len()])
                    .max(ENERGY_FLOOR)
                    .ln();
            }
        }
        for bin in 0..MEL_BINS {
            let mean = features.iter().skip(bin).step_by(MEL_BINS).sum::<f32>() / frames as f32;
            features
                .iter_mut()
                .skip(bin)
                .step_by(MEL_BINS)
                .for_each(|v| *v -= mean);
        }
        features
    }
}

/// Mean-normalised log-mel features of 16 kHz mono audio: rows of [`MEL_BINS`] values, one per
/// 10 ms (none for audio under one 25 ms frame).
pub(super) fn features(samples: &[f32]) -> Vec<f32> {
    static FRONTEND: OnceLock<Frontend> = OnceLock::new();
    FRONTEND.get_or_init(Frontend::new).features(samples)
}
