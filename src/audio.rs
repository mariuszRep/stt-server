use std::io::Cursor;

use axum::http::StatusCode;
use rubato::{FftFixedIn, Resampler};

use crate::errors::{internal, ApiError, ApiResult};

/// One-line description of an upload's size and RIFF header, for the log line written when
/// the upload is rejected. The client only ever sees a generic message; this is what makes a
/// truncated body, an empty body or an odd data length diagnosable afterwards.
pub fn describe_wav_header(bytes: &[u8]) -> String {
    let mut out = format!("len={}", bytes.len());
    let tag = |range: std::ops::Range<usize>| {
        bytes
            .get(range)
            .map(|b| String::from_utf8_lossy(b).into_owned())
    };
    let u32_at = |at: usize| {
        bytes
            .get(at..at + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    let u16_at = |at: usize| {
        bytes
            .get(at..at + 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
    };
    out.push_str(&format!(
        " riff={:?} riff_size={:?} wave={:?} fmt={:?} format={:?} channels={:?} rate={:?} bits={:?} data_tag={:?}",
        tag(0..4),
        u32_at(4),
        tag(8..12),
        tag(12..16),
        u16_at(20),
        u16_at(22),
        u32_at(24),
        u16_at(34),
        tag(36..40),
    ));
    if let Some(data_len) = u32_at(40) {
        let block = u16_at(32).unwrap_or(0) as u64;
        let actual = bytes.len().saturating_sub(44);
        out.push_str(&format!(
            " data_len={data_len} actual_data_bytes={actual} block_align={block}"
        ));
        if block > 0 && u64::from(data_len) % block != 0 {
            out.push_str(" data_len_not_whole_samples");
        }
        if (data_len as usize) > actual {
            out.push_str(" truncated_body");
        }
    }
    out
}

pub fn decode_wav(bytes: &[u8]) -> ApiResult<Vec<f32>> {
    let mut reader = hound::WavReader::new(Cursor::new(bytes)).map_err(|error| {
        eprintln!(
            "rejected audio: invalid WAV ({error}); {}",
            describe_wav_header(bytes)
        );
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_audio",
            "A valid WAV file is required",
        )
    })?;
    let spec = reader.spec();
    if !(1..=2).contains(&spec.channels) || !(8_000..=192_000).contains(&spec.sample_rate) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "unsupported_audio",
            "WAV must have one or two channels and a sample rate from 8 to 192 kHz",
        ));
    }
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int if spec.bits_per_sample == 16 => reader
            .samples::<i16>()
            .map(|sample| sample.map(|value| f32::from(value) / 32768.0))
            .collect::<Result<Vec<_>, _>>(),
        hound::SampleFormat::Int if spec.bits_per_sample == 24 => reader
            .samples::<i32>()
            .map(|sample| sample.map(|value| value as f32 / 8_388_608.0))
            .collect::<Result<Vec<_>, _>>(),
        hound::SampleFormat::Float if spec.bits_per_sample == 32 => {
            reader.samples::<f32>().collect::<Result<Vec<_>, _>>()
        }
        _ => {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "unsupported_audio",
                "Only 16/24-bit PCM and 32-bit float WAV are supported",
            ));
        }
    }
    .map_err(|error| {
        eprintln!(
            "rejected audio: malformed samples ({error}); {}",
            describe_wav_header(bytes)
        );
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_audio",
            "Malformed WAV samples",
        )
    })?;
    if !samples.len().is_multiple_of(spec.channels as usize)
        || !samples.iter().all(|s| s.is_finite())
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_audio",
            "WAV has incomplete frames or non-finite samples",
        ));
    }
    let frame_count = samples.len() / spec.channels as usize;
    if frame_count < (spec.sample_rate / 10) as usize {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "audio_too_short",
            "At least 100 ms of audio is required",
        ));
    }
    if frame_count > spec.sample_rate as usize * 10 * 60 {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "audio_too_long",
            "Audio exceeds the ten-minute limit",
        ));
    }
    let mono: Vec<f32> = samples
        .chunks_exact(spec.channels as usize)
        .map(|frame| frame.iter().sum::<f32>() / spec.channels as f32)
        .collect();
    if spec.sample_rate == 16_000 {
        return Ok(mono);
    }
    let output_len = (mono.len() as u64 * 16_000 / u64::from(spec.sample_rate)) as usize;
    let mut resampler =
        FftFixedIn::<f32>::new(spec.sample_rate as usize, 16_000, 1024, 1, 1).map_err(internal)?;
    let delay = resampler.output_delay();
    let mut output = Vec::with_capacity(output_len + delay + 1024);
    for chunk in mono.chunks(1024) {
        let converted = if chunk.len() == 1024 {
            resampler.process(&[chunk], None)
        } else {
            resampler.process_partial(Some(&[chunk]), None)
        }
        .map_err(internal)?;
        output.extend_from_slice(&converted[0]);
    }
    while output.len() < output_len + delay {
        let converted = resampler
            .process_partial::<&[f32]>(None, None)
            .map_err(internal)?;
        output.extend_from_slice(&converted[0]);
    }
    Ok(output[delay..delay + output_len].to_vec())
}

/// Incremental decoder for live audio: little-endian signed 16-bit mono PCM
/// bytes in arbitrary chunk sizes (a sample may straddle two chunks), out as
/// 16 kHz `f32`. Used by the live endpoint (`crate::realtime`); the batch path
/// decodes whole WAV files with [`decode_wav`].
pub struct PcmStream {
    odd_byte: Option<u8>,
    resampler: Option<LiveResampler>,
}

struct LiveResampler {
    resampler: FftFixedIn<f32>,
    pending: Vec<f32>,
    /// Leading output samples still to discard (the resampler's filter delay).
    skip: usize,
}

impl LiveResampler {
    fn emit(&mut self, converted: &[f32], out: &mut Vec<f32>) {
        let drop = self.skip.min(converted.len());
        self.skip -= drop;
        out.extend_from_slice(&converted[drop..]);
    }
}

/// 20 ms of input per resampler chunk.
fn live_chunk_frames(sample_rate: u32) -> usize {
    sample_rate as usize / 50
}

impl PcmStream {
    /// `sample_rate` is the rate of the incoming PCM; anything but 16 kHz is
    /// resampled to 16 kHz.
    pub fn new(sample_rate: u32) -> ApiResult<Self> {
        let resampler = if sample_rate == 16_000 {
            None
        } else {
            let resampler = FftFixedIn::<f32>::new(
                sample_rate as usize,
                16_000,
                live_chunk_frames(sample_rate),
                1,
                1,
            )
            .map_err(internal)?;
            let skip = resampler.output_delay();
            Some(LiveResampler {
                resampler,
                pending: Vec::new(),
                skip,
            })
        };
        Ok(Self {
            odd_byte: None,
            resampler,
        })
    }

    /// Decode the next chunk of PCM bytes. A trailing odd byte is held back
    /// until the next call.
    pub fn push(&mut self, bytes: &[u8]) -> ApiResult<Vec<f32>> {
        let mut samples = Vec::with_capacity(bytes.len() / 2 + 1);
        let mut rest = bytes;
        if let (Some(low), Some(&high)) = (self.odd_byte, rest.first()) {
            samples.push(f32::from(i16::from_le_bytes([low, high])) / 32768.0);
            self.odd_byte = None;
            rest = &rest[1..];
        }
        let (pairs, remainder) = rest.as_chunks::<2>();
        if let [byte] = remainder {
            self.odd_byte = Some(*byte);
        }
        samples.extend(
            pairs
                .iter()
                .map(|pair| f32::from(i16::from_le_bytes(*pair)) / 32768.0),
        );

        let Some(live) = self.resampler.as_mut() else {
            return Ok(samples);
        };
        live.pending.extend_from_slice(&samples);
        let mut out = Vec::new();
        loop {
            let needed = live.resampler.input_frames_next();
            if live.pending.len() < needed {
                break;
            }
            let chunk: Vec<f32> = live.pending.drain(..needed).collect();
            let converted = live.resampler.process(&[chunk], None).map_err(internal)?;
            live.emit(&converted[0], &mut out);
        }
        Ok(out)
    }

    /// Flush what the resampler still holds at the end of an utterance.
    pub fn finish(&mut self) -> ApiResult<Vec<f32>> {
        let Some(live) = self.resampler.as_mut() else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        let tail = std::mem::take(&mut live.pending);
        if !tail.is_empty() {
            let converted = live
                .resampler
                .process_partial(Some(&[tail]), None)
                .map_err(internal)?;
            live.emit(&converted[0], &mut out);
        }
        let converted = live
            .resampler
            .process_partial::<&[f32]>(None, None)
            .map_err(internal)?;
        live.emit(&converted[0], &mut out);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm16_bytes(samples: usize, value: i16) -> Vec<u8> {
        (0..samples).flat_map(|_| value.to_le_bytes()).collect()
    }

    #[test]
    fn pcm_stream_passes_16k_through_and_holds_an_odd_byte() {
        let mut decoder = PcmStream::new(16_000).unwrap();
        let bytes = pcm16_bytes(4, 16384);
        let first = decoder.push(&bytes[..3]).unwrap();
        assert_eq!(first, vec![0.5]);
        let second = decoder.push(&bytes[3..]).unwrap();
        assert_eq!(second, vec![0.5, 0.5, 0.5]);
        assert!(decoder.finish().unwrap().is_empty());
    }

    #[test]
    fn pcm_stream_resamples_24k_to_16k_in_arbitrary_chunks() {
        let mut decoder = PcmStream::new(24_000).unwrap();
        let bytes = pcm16_bytes(24_000, 8000);
        let mut total = 0;
        for chunk in bytes.chunks(1001) {
            total += decoder.push(chunk).unwrap().len();
        }
        total += decoder.finish().unwrap().len();
        // One second in, one second out, plus the flush's trailing silence
        // (at most one 20 ms resampler chunk).
        assert!((15_900..=16_400).contains(&total), "got {total}");
    }

    #[test]
    fn audio_downmixes_and_resamples_before_inference() {
        let mut cursor = Cursor::new(Vec::new());
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        {
            let mut writer = hound::WavWriter::new(&mut cursor, spec).unwrap();
            for _ in 0..48_000 {
                writer.write_sample(16_384_i16).unwrap();
                writer.write_sample(-16_384_i16).unwrap();
            }
            writer.finalize().unwrap();
        }
        let decoded = decode_wav(&cursor.into_inner()).unwrap();
        assert_eq!(decoded.len(), 16_000);
        assert!(decoded.iter().all(|sample| sample.abs() < 0.0001));
    }

    fn wav_bytes(samples: usize) -> Vec<u8> {
        let mut cursor = Cursor::new(Vec::new());
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        {
            let mut writer = hound::WavWriter::new(&mut cursor, spec).unwrap();
            for _ in 0..samples {
                writer.write_sample(0_i16).unwrap();
            }
            writer.finalize().unwrap();
        }
        cursor.into_inner()
    }

    #[test]
    fn header_description_reports_valid_header() {
        let text = describe_wav_header(&wav_bytes(3200));
        assert!(text.contains("len=6444"), "{text}");
        assert!(text.contains("riff=Some(\"RIFF\")"), "{text}");
        assert!(
            text.contains("data_len=6400 actual_data_bytes=6400"),
            "{text}"
        );
        assert!(!text.contains("truncated_body"), "{text}");
        assert!(!text.contains("not_whole_samples"), "{text}");
    }

    #[test]
    fn header_description_flags_truncated_body_and_odd_length() {
        let mut bytes = wav_bytes(3200);
        bytes.truncate(1000);
        assert!(describe_wav_header(&bytes).contains("truncated_body"));

        let mut odd = wav_bytes(3200);
        odd[40..44].copy_from_slice(&6401u32.to_le_bytes());
        assert!(describe_wav_header(&odd).contains("data_len_not_whole_samples"));
    }

    #[test]
    fn header_description_survives_empty_and_tiny_bodies() {
        assert_eq!(describe_wav_header(&[]).split(' ').next(), Some("len=0"));
        assert!(describe_wav_header(&[1, 2, 3]).contains("len=3"));
    }

    #[test]
    fn empty_and_truncated_bodies_are_rejected_not_panicked() {
        assert!(decode_wav(&[]).is_err());
        let mut bytes = wav_bytes(3200);
        bytes.truncate(60);
        assert!(decode_wav(&bytes).is_err());
    }
}
