//! Fallback audio decoder for anything `sherpa_onnx::Wave::read` can't
//! handle (WAV only), via symphonia's demuxer/codec registry: WebM/Matroska
//! or Ogg containers carrying PCM or Vorbis.
//!
//! **Known gap**: this does NOT decode Opus, which is what the app's
//! `MediaRecorder` fallback (`whisper-vibes/apps/web/src/hooks/use-loop-recorder.ts`)
//! actually produces (`audio/webm;codecs=opus`). Verified directly against
//! symphonia's upstream source (not assumed): the published `symphonia`
//! crate has no Opus decoder and no registered Opus codec-type constant at
//! all, so a webm-demux-then-decode approach has nothing to hand Opus
//! packets to. Closing this fully needs either an FFI `libopus` binding
//! (e.g. the `opus`/`opusic-sys` crates) glued to symphonia's raw packet
//! stream -- real, buildable, but unverified here (no ffmpeg in this
//! environment to produce a real webm/opus fixture to decode against) -- or
//! revisiting the app-side alternative of normalizing to WAV before it ever
//! reaches a runtime. Left as a follow-up rather than shipped unverified.

use anyhow::{Context, Result};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// Decode `bytes` (any container/codec symphonia's registered demuxers
/// cover) into mono f32 PCM at the container's own native sample rate. No
/// resampling here -- `sherpa_onnx`'s `accept_waveform` resamples
/// internally, the same contract the WAV-only path already relied on.
pub fn decode_to_mono_f32(bytes: Vec<u8>) -> Result<(i32, Vec<f32>)> {
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
