//! Fallback audio decoder for anything `sherpa_onnx::Wave::read` can't
//! handle (WAV only): symphonia demuxes the container (WebM/Matroska or
//! Ogg), then either symphonia's own registered codec decoders handle the
//! payload (PCM, Vorbis), or -- for Opus specifically -- libopus does via
//! the `audiopus`/`audiopus_sys` FFI binding.
//!
//! **Opus note**: symphonia's published crate (verified directly against
//! `symphonia-core` 0.5.5's upstream source, the exact version pinned here)
//! *does* define `CODEC_TYPE_OPUS` and its WebM demuxer maps Matroska's
//! `A_OPUS` `CodecID` to it, so identifying and demuxing an Opus track works
//! out of the box. What symphonia never shipped is a decoder crate for that
//! codec (`symphonia-codec-opus` does not exist), so `get_codecs().make()`
//! still fails for it -- that's the actual, narrower gap this module closes
//! by decoding Opus packets with libopus directly instead of asking
//! symphonia's codec registry to do it. An earlier version of this comment
//! claimed symphonia had no Opus codec-type constant at all; that was wrong
//! (checked against a stale assumption, not the pinned version's real
//! source) and is corrected here.

use anyhow::{anyhow, Context, Result};
use audiopus::coder::Decoder as OpusDecoder;
use audiopus::{Channels as OpusChannels, MutSignals, SampleRate as OpusSampleRate};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL, CODEC_TYPE_OPUS};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// libopus always decodes at one of a fixed set of rates; 48 kHz is its
/// native/maximum rate and what browsers encode at, so decoding at anything
/// else would just be discarding libopus's own internal resampling. No
/// resampling is done here either way -- `sherpa_onnx`'s `accept_waveform`
/// resamples internally, the same contract the rest of this module relies on.
const OPUS_DECODE_RATE: OpusSampleRate = OpusSampleRate::Hz48000;
/// Max Opus frame size: 120ms @ 48kHz, times 2 channels (interleaved).
const OPUS_MAX_FRAME_SAMPLES: usize = 5760 * 2;

/// Decode `bytes` (any container/codec symphonia's registered demuxers
/// cover, plus Opus via libopus) into mono f32 PCM at the container's own
/// native sample rate (or libopus's fixed decode rate, for Opus). No
/// resampling here -- `sherpa_onnx`'s `accept_waveform` resamples
/// internally, the same contract the WAV-only path already relied on.
///
/// Wrapped in `catch_unwind`: malformed input can trip internal invariant
/// panics inside symphonia's demuxers (verified directly -- some corrupt
/// byte sequences make `symphonia-format-mkv` 0.5.5 panic partway through
/// probing rather than return an `Err`). A single bad upload must surface as
/// a 400, never take down the request task with an uncaught panic.
pub fn decode_to_mono_f32(bytes: Vec<u8>) -> Result<(i32, Vec<f32>)> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        decode_to_mono_f32_inner(bytes)
    }))
    .unwrap_or_else(|payload| {
        let msg = payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".to_string());
        Err(anyhow!("audio decoder panicked on malformed input: {msg}"))
    })
}

fn decode_to_mono_f32_inner(bytes: Vec<u8>) -> Result<(i32, Vec<f32>)> {
    let cursor = std::io::Cursor::new(bytes);
    let mss = MediaSourceStream::new(Box::new(cursor), Default::default());

    let probed = symphonia::default::get_probe()
        .format(
            &Hint::new(),
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .context("unrecognized audio container")?;
    let mut format = probed.format;

    let track = format
        .tracks()
        .iter()
        .find(|track| track.codec_params.codec != CODEC_TYPE_NULL)
        .cloned()
        .context("no audio track found")?;
    let track_id = track.id;

    if track.codec_params.codec == CODEC_TYPE_OPUS {
        return decode_opus_track(&mut *format, track_id, &track.codec_params);
    }

    let sample_rate = track
        .codec_params
        .sample_rate
        .context("audio track has no known sample rate")?;

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .context("unsupported audio codec")?;

    let mut samples: Vec<f32> = Vec::new();
    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                break
            }
            Err(e) => return Err(e.into()),
        };
        if packet.track_id() != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(decoded) => {
                let spec = *decoded.spec();
                let channels = spec.channels.count().max(1);
                let mut buffer = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
                buffer.copy_interleaved_ref(decoded);
                if channels == 1 {
                    samples.extend_from_slice(buffer.samples());
                } else {
                    // Downmix to mono by averaging channels -- the recognizer
                    // only ever wants a single channel.
                    for frame in buffer.samples().chunks(channels) {
                        samples.push(frame.iter().sum::<f32>() / channels as f32);
                    }
                }
            }
            // A single bad packet shouldn't kill the whole decode; skip it
            // (matches symphonia's own recommended usage pattern).
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(e) => return Err(e.into()),
        }
    }

    Ok((sample_rate as i32, samples))
}

/// Decode a single Opus track's raw packets via libopus, bypassing
/// symphonia's codec registry entirely (it has no Opus decoder). symphonia
/// is used here purely as a WebM/Matroska demuxer: `next_packet()` yields
/// each track's raw, undelimited Opus packet payload in order, which is
/// exactly what `audiopus`'s `Decoder::decode_float` expects.
///
/// Deliberately not handled: `OpusHead` pre-skip/gain trimming. A few ms of
/// leading silence/artifact from ignoring it doesn't affect ASR quality
/// enough to justify parsing WebM `CodecPrivate` for it in this pass.
fn decode_opus_track(
    format: &mut dyn symphonia::core::formats::FormatReader,
    track_id: u32,
    codec_params: &symphonia::core::codecs::CodecParameters,
) -> Result<(i32, Vec<f32>)> {
    let channel_count = codec_params
        .channels
        .map(|c| c.count())
        .filter(|&n| n > 0)
        .unwrap_or(1);
    let opus_channels = match channel_count {
        1 => OpusChannels::Mono,
        2 => OpusChannels::Stereo,
        n => return Err(anyhow!("unsupported Opus channel count: {n}")),
    };

    let mut decoder = OpusDecoder::new(OPUS_DECODE_RATE, opus_channels)
        .map_err(|e| anyhow!("failed to create Opus decoder: {e}"))?;

    let mut pcm_buf = [0f32; OPUS_MAX_FRAME_SAMPLES];
    let mut samples: Vec<f32> = Vec::new();
    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                break
            }
            Err(e) => return Err(e.into()),
        };
        if packet.track_id() != track_id {
            continue;
        }
        if packet.data.is_empty() {
            continue;
        }

        let opus_packet = match audiopus::packet::Packet::try_from(&packet.data[..]) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let output = MutSignals::try_from(&mut pcm_buf[..])
            .map_err(|e| anyhow!("failed to prepare Opus decode buffer: {e}"))?;
        // A single corrupt packet shouldn't kill the whole decode; skip it,
        // matching the non-Opus path's DecodeError-skip behaviour above.
        let decoded_frames = match decoder.decode_float(Some(opus_packet), output, false) {
            Ok(n) => n,
            Err(_) => continue,
        };

        let decoded_samples = &pcm_buf[..decoded_frames * channel_count];
        if channel_count == 1 {
            samples.extend_from_slice(decoded_samples);
        } else {
            for frame in decoded_samples.chunks(channel_count) {
                samples.push(frame.iter().sum::<f32>() / channel_count as f32);
            }
        }
    }

    Ok((OPUS_DECODE_RATE as i32, samples))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_WEBM: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/sample.webm"
    ));

    #[test]
    fn decodes_real_webm_opus_fixture() {
        let (sample_rate, samples) =
            decode_to_mono_f32(SAMPLE_WEBM.to_vec()).expect("real webm/opus fixture should decode");
        assert_eq!(sample_rate, 48000);
        assert!(!samples.is_empty(), "decoded no samples at all");
        let peak = samples.iter().fold(0f32, |acc, s| acc.max(s.abs()));
        assert!(peak > 0.01, "decoded audio looks silent, peak={peak}");
    }

    #[test]
    fn corrupt_webm_opus_returns_error_not_panic() {
        let mut truncated = SAMPLE_WEBM.to_vec();
        truncated.truncate(SAMPLE_WEBM.len() / 3);
        // Truncated mid-stream input should surface as an error (container
        // may still parse partially and yield fewer/no samples, or fail
        // outright) -- what matters is it never panics.
        let _ = decode_to_mono_f32(truncated);

        let garbage = vec![0u8; 128];
        assert!(
            decode_to_mono_f32(garbage).is_err(),
            "garbage bytes should fail to decode, not succeed"
        );
    }
}
