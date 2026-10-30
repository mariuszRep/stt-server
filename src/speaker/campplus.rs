//! The CAM++ (VoxCeleb) forward pass over mean-normalised log-mel features, from weights
//! exported by `scripts/export_campplus_weights.py`.

use std::sync::OnceLock;

use super::fbank::MEL_BINS;
use super::{dot, normalise, DIM};

const CHANNELS: usize = 32;
const GROWTH: usize = 32;
const BOTTLENECK: usize = 4 * GROWTH;
const INIT_CHANNELS: usize = 128;
/// Dense layers and dilation of the three context-aware-mask blocks.
const BLOCKS: [(usize, usize); 3] = [(12, 1), (24, 2), (16, 2)];
/// The context-aware mask pools its context over segments of this many frames.
const SEGMENT: usize = 100;

const WEIGHTS: &[u8] = include_bytes!("../../assets/campplus_vox_16k.f16");
/// What the registry reports about the compiled-in weights; a test pins the size and hash to the blob.
pub const WEIGHTS_FILE: &str = "campplus_vox_16k.f16";
pub const WEIGHTS_BYTES: usize = WEIGHTS.len();
pub const WEIGHTS_SHA256: &str = "11122726706b2cfe164966cdafb93cc7727176e1db9d45952ef999d90b53e65b";

fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

fn relu(values: &mut [f32]) {
    for value in values {
        *value = value.max(0.0);
    }
}

/// Time-major activations of the 2-D front end: `[time][freq][channel]`.
struct Map {
    data: Vec<f32>,
    time: usize,
    freq: usize,
    channels: usize,
}

/// A square 2-D convolution with "same" padding that strides over frequency only; batch norm
/// is folded into weight and bias. Weights are laid out `[out][time tap][freq tap][in]`, so the
/// frequency taps of one time tap are one contiguous run, matching the activations.
struct Conv2d {
    weight: Vec<f32>,
    bias: Vec<f32>,
    inputs: usize,
    kernel: usize,
    stride: usize,
}

impl Conv2d {
    fn forward(&self, input: &Map) -> Map {
        let (kernel, pad, outputs) = (self.kernel, (self.kernel - 1) / 2, self.bias.len());
        let freq = (input.freq + 2 * pad - kernel) / self.stride + 1;
        let mut data = vec![0.0; input.time * freq * outputs];
        for t in 0..input.time {
            for fo in 0..freq {
                // Frequency taps that fall inside the input (the rest is zero padding).
                let left = fo * self.stride;
                let first = pad.saturating_sub(left);
                let last = (input.freq + pad - 1 - left).min(kernel - 1);
                let run = (last - first + 1) * self.inputs;
                // Per time tap inside the recording: (tap, offset of its run in the input).
                let mut taps = [(0, 0); 3];
                let mut count = 0;
                for tap in 0..kernel {
                    let Some(at) = (t + tap).checked_sub(pad).filter(|&at| at < input.time) else {
                        continue;
                    };
                    taps[count] = (tap, (at * input.freq + left + first - pad) * self.inputs);
                    count += 1;
                }
                let out = &mut data[(t * freq + fo) * outputs..][..outputs];
                for (o, slot) in out.iter_mut().enumerate() {
                    *slot = self.bias[o]
                        + taps[..count]
                            .iter()
                            .map(|&(tap, offset)| {
                                let weights = &self.weight
                                    [((o * kernel + tap) * kernel + first) * self.inputs..][..run];
                                dot(weights, &input.data[offset..][..run])
                            })
                            .sum::<f32>();
                }
            }
        }
        Map {
            data,
            time: input.time,
            freq,
            channels: outputs,
        }
    }
}

/// A 1-D convolution over time-major activations (`[time][channel]`) with "same" padding.
/// Weights are laid out `[out][tap][in]`; a zero bias stands in where the model has none.
struct Conv1d {
    weight: Vec<f32>,
    bias: Vec<f32>,
    inputs: usize,
    taps: usize,
    stride: usize,
    dilation: usize,
}

impl Conv1d {
    /// Reads `steps` rows of `input` (row `input_stride` apart, first `inputs` columns) and
    /// writes the result into `out` (rows `out_stride` apart); returns the number of rows.
    fn forward(
        &self,
        input: &[f32],
        input_stride: usize,
        steps: usize,
        out: &mut [f32],
        out_stride: usize,
    ) -> usize {
        let pad = (self.taps - 1) / 2 * self.dilation;
        let span = self.dilation * (self.taps - 1) + 1;
        let out_steps = (steps + 2 * pad - span) / self.stride + 1;
        for t in 0..out_steps {
            let row = &mut out[t * out_stride..][..self.bias.len()];
            for (o, slot) in row.iter_mut().enumerate() {
                let mut acc = self.bias[o];
                for tap in 0..self.taps {
                    // Position in the input; the padding row at either edge is zero.
                    let Some(at) = (t * self.stride + tap * self.dilation)
                        .checked_sub(pad)
                        .filter(|&at| at < steps)
                    else {
                        continue;
                    };
                    let weights =
                        &self.weight[(o * self.taps + tap) * self.inputs..][..self.inputs];
                    acc += dot(weights, &input[at * input_stride..][..self.inputs]);
                }
                *slot = acc;
            }
        }
        out_steps
    }

    /// A convolution with no padding, stride or dilation, as a plain matrix product.
    fn apply(&self, input: &[f32], input_stride: usize, steps: usize) -> Vec<f32> {
        let mut out = vec![0.0; steps * self.bias.len()];
        self.forward(input, input_stride, steps, &mut out, self.bias.len());
        out
    }
}

/// A per-channel scale and shift (inference-mode batch norm) followed by ReLU.
struct Affine {
    scale: Vec<f32>,
    shift: Vec<f32>,
}

impl Affine {
    /// Applies it to the first `channels` columns of every row of `input` (rows `stride` apart).
    fn relu(&self, input: &[f32], stride: usize, steps: usize) -> Vec<f32> {
        let channels = self.scale.len();
        let mut out = Vec::with_capacity(steps * channels);
        for row in input.chunks(stride).take(steps) {
            out.extend(
                row.iter()
                    .zip(&self.scale)
                    .zip(&self.shift)
                    .map(|((x, scale), shift)| (x * scale + shift).max(0.0)),
            );
        }
        out
    }
}

struct DenseLayer {
    input_affine: Affine,
    bottleneck: Conv1d,
    local: Conv1d,
    gate_reduce: Conv1d,
    gate_expand: Conv1d,
}

impl DenseLayer {
    /// The [`GROWTH`] new channels for `steps` rows of `input` (see [`Conv1d::forward`]).
    fn forward(&self, input: &[f32], stride: usize, steps: usize) -> Vec<f32> {
        let activated = self.input_affine.relu(input, stride, steps);
        let mut bottleneck = self
            .bottleneck
            .apply(&activated, self.bottleneck.inputs, steps);
        relu(&mut bottleneck);
        let mut local = self.local.apply(&bottleneck, BOTTLENECK, steps);

        // Context-aware mask: the mean over the whole recording plus the mean over the
        // segment of SEGMENT frames the row belongs to (the last segment may be shorter).
        let segments = steps.div_ceil(SEGMENT);
        let mut context = vec![0.0f32; segments * BOTTLENECK];
        for (index, row) in bottleneck.as_chunks::<BOTTLENECK>().0.iter().enumerate() {
            let segment = &mut context[index / SEGMENT * BOTTLENECK..][..BOTTLENECK];
            segment.iter_mut().zip(row).for_each(|(sum, x)| *sum += x);
        }
        let mut whole = [0.0f32; BOTTLENECK];
        for (segment, sum) in context
            .as_chunks_mut::<BOTTLENECK>()
            .0
            .iter_mut()
            .enumerate()
        {
            whole
                .iter_mut()
                .zip(&*sum)
                .for_each(|(total, x)| *total += x);
            let frames = (steps - segment * SEGMENT).min(SEGMENT);
            sum.iter_mut().for_each(|x| *x /= frames as f32);
        }
        for segment in context.as_chunks_mut::<BOTTLENECK>().0 {
            segment
                .iter_mut()
                .zip(&whole)
                .for_each(|(x, total)| *x += total / steps as f32);
        }
        let mut hidden = self.gate_reduce.apply(&context, BOTTLENECK, segments);
        relu(&mut hidden);
        let mut mask = self.gate_expand.apply(&hidden, BOTTLENECK / 2, segments);
        mask.iter_mut().for_each(|x| *x = sigmoid(*x));

        for (index, row) in local.as_chunks_mut::<GROWTH>().0.iter_mut().enumerate() {
            let mask = &mask[index / SEGMENT * GROWTH..][..GROWTH];
            row.iter_mut().zip(mask).for_each(|(x, m)| *x *= m);
        }
        local
    }
}

/// A densely connected block followed by the transition that halves its channels.
struct Block {
    layers: Vec<DenseLayer>,
    inputs: usize,
    transit_affine: Affine,
    transit: Conv1d,
}

impl Block {
    fn channels(&self) -> usize {
        self.inputs + self.layers.len() * GROWTH
    }

    /// `input` has `steps` rows of `self.inputs` channels; the result has `self.channels() / 2`.
    fn forward(&self, input: &[f32], steps: usize) -> Vec<f32> {
        let width = self.channels();
        let mut dense = vec![0.0f32; steps * width];
        for (row, source) in dense
            .chunks_exact_mut(width)
            .zip(input.chunks_exact(self.inputs))
        {
            row[..self.inputs].copy_from_slice(source);
        }
        for (index, layer) in self.layers.iter().enumerate() {
            let grown = layer.forward(&dense, width, steps);
            let at = self.inputs + index * GROWTH;
            for (row, new) in dense
                .chunks_exact_mut(width)
                .zip(grown.as_chunks::<GROWTH>().0)
            {
                row[at..at + GROWTH].copy_from_slice(new);
            }
        }
        let activated = self.transit_affine.relu(&dense, width, steps);
        self.transit.apply(&activated, width, steps)
    }
}

struct Residual {
    conv1: Conv2d,
    conv2: Conv2d,
    shortcut: Option<Conv2d>,
}

impl Residual {
    fn forward(&self, input: &Map) -> Map {
        let mut hidden = self.conv1.forward(input);
        relu(&mut hidden.data);
        let mut out = self.conv2.forward(&hidden);
        let shortcut = self.shortcut.as_ref().map(|conv| conv.forward(input));
        let skip = shortcut.as_ref().map_or(&input.data, |map| &map.data);
        out.data.iter_mut().zip(skip).for_each(|(x, s)| *x += s);
        relu(&mut out.data);
        out
    }
}

struct Model {
    conv1: Conv2d,
    stages: [Residual; 4],
    conv2: Conv2d,
    tdnn: Conv1d,
    blocks: [Block; 3],
    output_affine: Affine,
    dense: Conv1d,
}

fn half_to_f32(bits: u16) -> f32 {
    let sign = u32::from(bits >> 15) << 31;
    let exponent = u32::from((bits >> 10) & 0x1f);
    let mantissa = u32::from(bits & 0x3ff);
    let magnitude = match exponent {
        // Zero and subnormals.
        0 => mantissa as f32 * 2f32.powi(-24),
        31 if mantissa == 0 => f32::INFINITY,
        31 => f32::NAN,
        _ => f32::from_bits(((exponent + 112) << 23) | (mantissa << 13)),
    };
    f32::from_bits(sign | magnitude.to_bits())
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take(&mut self, count: usize) -> Vec<f32> {
        let (head, rest) = self.0.split_at(count * 2);
        self.0 = rest;
        head.as_chunks::<2>()
            .0
            .iter()
            .map(|b| half_to_f32(u16::from_le_bytes(*b)))
            .collect()
    }

    fn affine(&mut self, channels: usize) -> Affine {
        Affine {
            scale: self.take(channels),
            shift: self.take(channels),
        }
    }

    /// A 2-D convolution with folded batch norm, stored `[out][in][freq tap][time tap]` (the
    /// PyTorch layout, height being frequency), re-laid out as [`Conv2d`] wants it.
    fn conv2d(&mut self, outputs: usize, inputs: usize, kernel: usize, stride: usize) -> Conv2d {
        let stored = self.take(outputs * inputs * kernel * kernel);
        let mut weight = vec![0.0; stored.len()];
        for o in 0..outputs {
            for i in 0..inputs {
                for freq_tap in 0..kernel {
                    for time_tap in 0..kernel {
                        weight[((o * kernel + time_tap) * kernel + freq_tap) * inputs + i] =
                            stored[((o * inputs + i) * kernel + freq_tap) * kernel + time_tap];
                    }
                }
            }
        }
        Conv2d {
            weight,
            bias: self.take(outputs),
            inputs,
            kernel,
            stride,
        }
    }

    /// A 1-D convolution stored `[out][in][tap]`, re-laid out `[out][tap][in]`. `input_order`
    /// maps a stored input index to the activation column it reads (identity for most layers).
    fn conv1d(
        &mut self,
        (outputs, inputs, taps): (usize, usize, usize),
        (stride, dilation): (usize, usize),
        has_bias: bool,
        input_order: impl Fn(usize) -> usize,
    ) -> Conv1d {
        let stored = self.take(outputs * inputs * taps);
        let mut weight = vec![0.0; stored.len()];
        for o in 0..outputs {
            for i in 0..inputs {
                for tap in 0..taps {
                    weight[(o * taps + tap) * inputs + input_order(i)] =
                        stored[(o * inputs + i) * taps + tap];
                }
            }
        }
        Conv1d {
            weight,
            bias: if has_bias {
                self.take(outputs)
            } else {
                vec![0.0; outputs]
            },
            inputs,
            taps,
            stride,
            dilation,
        }
    }

    fn pointwise(&mut self, outputs: usize, inputs: usize, has_bias: bool) -> Conv1d {
        self.conv1d((outputs, inputs, 1), (1, 1), has_bias, |i| i)
    }

    fn residual(&mut self, stride: usize, shortcut: bool) -> Residual {
        Residual {
            conv1: self.conv2d(CHANNELS, CHANNELS, 3, stride),
            conv2: self.conv2d(CHANNELS, CHANNELS, 3, 1),
            shortcut: shortcut.then(|| self.conv2d(CHANNELS, CHANNELS, 1, stride)),
        }
    }

    fn block(&mut self, layers: usize, inputs: usize, dilation: usize) -> Block {
        let layers = (0..layers)
            .map(|index| {
                let channels = inputs + index * GROWTH;
                DenseLayer {
                    input_affine: self.affine(channels),
                    bottleneck: self.pointwise(BOTTLENECK, channels, true),
                    local: self.conv1d((GROWTH, BOTTLENECK, 3), (1, dilation), false, |i| i),
                    gate_reduce: self.pointwise(BOTTLENECK / 2, BOTTLENECK, true),
                    gate_expand: self.pointwise(GROWTH, BOTTLENECK / 2, true),
                }
            })
            .collect::<Vec<_>>();
        let width = inputs + layers.len() * GROWTH;
        Block {
            layers,
            inputs,
            transit_affine: self.affine(width),
            transit: self.pointwise(width / 2, width, false),
        }
    }
}

impl Model {
    fn load() -> Self {
        let mut reader = Reader(WEIGHTS);
        let conv1 = reader.conv2d(CHANNELS, 1, 3, 1);
        let stages = [
            reader.residual(2, true),
            reader.residual(1, false),
            reader.residual(2, true),
            reader.residual(1, false),
        ];
        let conv2 = reader.conv2d(CHANNELS, CHANNELS, 3, 2);
        // The front end hands over frequency 10 x 32 channels, flattened channel-major in
        // PyTorch (`channel * 10 + freq`) and frequency-major here (`freq * 32 + channel`).
        let freq = MEL_BINS / 8;
        let tdnn = reader.conv1d((INIT_CHANNELS, CHANNELS * freq, 5), (2, 1), true, |i| {
            (i % freq) * CHANNELS + i / freq
        });
        let mut inputs = INIT_CHANNELS;
        let blocks = BLOCKS.map(|(layers, dilation)| {
            let block = reader.block(layers, inputs, dilation);
            inputs = block.channels() / 2;
            block
        });
        let model = Self {
            conv1,
            stages,
            conv2,
            tdnn,
            blocks,
            output_affine: reader.affine(inputs),
            dense: reader.pointwise(DIM, 2 * inputs, true),
        };
        assert!(
            reader.0.is_empty(),
            "embedded speaker weights have trailing data"
        );
        model
    }

    /// The L2-normalised embedding of mean-normalised features (rows of [`MEL_BINS`]).
    fn embed(&self, features: Vec<f32>) -> Vec<f32> {
        let steps = features.len() / MEL_BINS;
        let input = Map {
            data: features,
            time: steps,
            freq: MEL_BINS,
            channels: 1,
        };
        let mut map = self.conv1.forward(&input);
        relu(&mut map.data);
        for stage in &self.stages {
            map = stage.forward(&map);
        }
        map = self.conv2.forward(&map);
        relu(&mut map.data);
        debug_assert_eq!(map.freq * map.channels, CHANNELS * MEL_BINS / 8);

        let width = map.freq * map.channels;
        let mut x = vec![0.0; (steps / 2 + 1) * INIT_CHANNELS];
        let steps = self
            .tdnn
            .forward(&map.data, width, steps, &mut x, INIT_CHANNELS);
        x.truncate(steps * INIT_CHANNELS);
        relu(&mut x);
        for block in &self.blocks {
            x = block.forward(&x, steps);
        }
        x = self
            .output_affine
            .relu(&x, self.output_affine.scale.len(), steps);

        // Statistics pooling: per-channel mean and unbiased standard deviation over time.
        let channels = self.output_affine.scale.len();
        let mut stats = vec![0.0f32; 2 * channels];
        for row in x.chunks_exact(channels) {
            stats.iter_mut().zip(row).for_each(|(s, v)| *s += v);
        }
        stats[..channels]
            .iter_mut()
            .for_each(|s| *s /= steps as f32);
        for row in x.chunks_exact(channels) {
            for c in 0..channels {
                stats[channels + c] += (row[c] - stats[c]).powi(2);
            }
        }
        stats[channels..]
            .iter_mut()
            .for_each(|s| *s = (*s / (steps - 1) as f32).sqrt());
        let mut embedding = self.dense.apply(&stats, 2 * channels, 1);
        normalise(&mut embedding);
        embedding
    }
}

fn model() -> &'static Model {
    static MODEL: OnceLock<Model> = OnceLock::new();
    MODEL.get_or_init(Model::load)
}

/// The L2-normalised embedding of mean-normalised features (rows of [`MEL_BINS`] values); at
/// least a few frames are needed for the standard deviation, so callers pass half a second or more.
pub(super) fn embed(features: Vec<f32>) -> Vec<f32> {
    model().embed(features)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    #[test]
    fn half_floats_convert_exactly() {
        for (bits, value) in [
            (0x0000, 0.0),
            (0x3c00, 1.0),
            (0xc000, -2.0),
            (0x3555, 0.333_251_95),
            (0x0001, 5.960_464_5e-8),
            (0x7bff, 65_504.0),
        ] {
            assert_eq!(half_to_f32(bits), value, "{bits:#06x}");
        }
        assert!(half_to_f32(0x7c00).is_infinite());
        assert!(half_to_f32(0x7e00).is_nan());
    }

    #[test]
    fn embedded_weights_are_the_exported_blob() {
        assert_eq!(WEIGHTS_BYTES, 7_169_568 * 2);
        assert_eq!(format!("{:x}", Sha256::digest(WEIGHTS)), WEIGHTS_SHA256);
    }
}
