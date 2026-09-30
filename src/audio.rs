use std::io::Cursor;

use axum::http::StatusCode;
use rubato::{FftFixedIn, Resampler};

use crate::errors::{internal, ApiError, ApiResult};

pub fn decode_wav(bytes: &[u8]) -> ApiResult<Vec<f32>> {
    let mut reader = hound::WavReader::new(Cursor::new(bytes)).map_err(|_| {
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
    .map_err(|_| {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
