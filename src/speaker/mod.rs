//! Speaker embeddings ("voice fingerprints") of finished audio: `POST /v1/audio/embeddings`.
//!
//! A pure-Rust forward pass of CAM++ trained on VoxCeleb (3D-Speaker, Apache-2.0), so no ONNX
//! runtime is linked. The weights were exported once from the official
//! `iic/speech_campplus_sv_en_voxceleb_16k` checkpoint by `scripts/export_campplus_weights.py`
//! into `assets/campplus_vox_16k.f16`: raw little-endian `f16` in the order `campplus.rs` reads
//! them, inference-mode batch normalisation already folded in, SHA-256 pinned by a test and
//! recorded in `THIRD_PARTY_NOTICES.md`. `fbank.rs` computes the features and `campplus.rs` runs
//! the network; this file windows long audio and builds the response.
//!
//! The pipeline is 3D-Speaker's: Kaldi-style 80-bin log-mel filterbank (25 ms frames every
//! 10 ms, no dither), per-recording mean subtraction, a 2-D residual front end (frequency
//! 80 -> 10), a stride-2 time-delay layer, three densely connected blocks of context-aware-mask
//! layers (12, 24 and 16 layers), statistics pooling and a dense layer to a 512-d vector, which
//! is L2-normalised. `tests/speaker_golden.json` holds PyTorch's features and embeddings for the
//! speech fixture; a test keeps this code within 1e-3 of the features and at cosine >= 0.9999
//! of the embeddings.

mod campplus;
mod cluster;
pub mod diarize;
mod fbank;

pub const SAMPLE_RATE: u32 = 16_000;
/// Identifies the embedding space in responses: vectors of different models never compare.
pub const MODEL_ID: &str = "campplus-voxceleb-v1";
pub const DIM: usize = 512;
/// Shortest audio that gets an embedding; anything shorter is too little to describe a voice.
pub const MIN_SECONDS: f64 = 0.5;
/// A longer segment is cut into windows of this many seconds ([`WINDOW_SECONDS`] apart by
/// [`HOP_SECONDS`]) whose embeddings are averaged.
pub const WINDOW_SECONDS: f64 = 3.0;
pub const HOP_SECONDS: f64 = 1.5;
/// Threads one embedding request may use for its windows.
const MAX_THREADS: usize = 4;

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

fn normalise(vector: &mut [f32]) {
    let norm = dot(vector, vector).sqrt();
    if norm > 0.0 {
        vector.iter_mut().for_each(|v| *v /= norm);
    }
}

/// Samples in `seconds`.
fn samples_in(seconds: f64) -> usize {
    (seconds * f64::from(SAMPLE_RATE)).round() as usize
}

/// The embedding of one stretch of 16 kHz mono audio, or `None` when it is shorter than
/// [`MIN_SECONDS`]. Up to [`WINDOW_SECONDS`] it is embedded whole; longer audio is cut into
/// windows of that length every [`HOP_SECONDS`] (the last one aligned to the end), and the
/// window embeddings (computed on up to [`MAX_THREADS`] threads) are averaged and normalised
/// again.
pub fn embed(samples: &[f32]) -> Option<Vec<f32>> {
    if samples.len() < samples_in(MIN_SECONDS) {
        return None;
    }
    let window = samples_in(WINDOW_SECONDS);
    let whole = |audio: &[f32]| campplus::embed(fbank::features(audio));
    if samples.len() <= window {
        return Some(whole(samples));
    }
    let hop = samples_in(HOP_SECONDS);
    let last = samples.len() - window;
    let mut starts: Vec<usize> = (0..last).step_by(hop).collect();
    starts.push(last);
    // Windows are independent, so a few threads share them (the request holds one slot).
    let workers = std::thread::available_parallelism()
        .map_or(1, usize::from)
        .min(MAX_THREADS);
    let embeddings: Vec<Vec<f32>> = std::thread::scope(|scope| {
        let handles: Vec<_> = starts
            .chunks(starts.len().div_ceil(workers))
            .map(|chunk| {
                scope.spawn(|| {
                    chunk
                        .iter()
                        .map(|&start| whole(&samples[start..start + window]))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().expect("embedding thread panicked"))
            .collect()
    });
    let mut mean = vec![0.0f32; DIM];
    for embedding in &embeddings {
        mean.iter_mut().zip(embedding).for_each(|(m, e)| *m += e);
    }
    normalise(&mut mean);
    Some(mean)
}

/// Most segments one request may ask for.
pub const MAX_SEGMENTS: usize = 1_000;

/// A stretch of the audio in seconds from its start, as a client sends it.
#[derive(Debug, Clone, Copy, PartialEq, serde::Deserialize)]
pub struct Segment {
    pub start: f64,
    pub end: f64,
}

/// Parses and checks the `segments` field: a JSON array of `{start, end}` in seconds, at most
/// [`MAX_SEGMENTS`], each starting at or after zero and ending after it starts. `Err` carries
/// the message for the 400 response.
pub fn parse_segments(raw: &str) -> Result<Vec<Segment>, String> {
    let segments: Vec<Segment> = serde_json::from_str(raw)
        .map_err(|e| format!("segments must be a JSON array of {{start, end}} in seconds: {e}"))?;
    if segments.len() > MAX_SEGMENTS {
        return Err(format!("segments may hold at most {MAX_SEGMENTS} entries"));
    }
    for (index, segment) in segments.iter().enumerate() {
        let valid = segment.start.is_finite()
            && segment.end.is_finite()
            && segment.start >= 0.0
            && segment.end > segment.start;
        if !valid {
            return Err(format!(
                "segments[{index}] needs finite start >= 0 and end > start"
            ));
        }
    }
    Ok(segments)
}

/// How embeddings are written into the response (`encoding_format`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// A JSON array of numbers.
    Float,
    /// Base64 of the little-endian `f32` bytes, as OpenAI's embeddings endpoint does it.
    Base64,
}

fn encode(embedding: &[f32], encoding: Encoding) -> serde_json::Value {
    match encoding {
        Encoding::Float => serde_json::json!(embedding),
        Encoding::Base64 => {
            use base64::Engine as _;
            let bytes: Vec<u8> = embedding.iter().flat_map(|v| v.to_le_bytes()).collect();
            serde_json::json!(base64::engine::general_purpose::STANDARD.encode(bytes))
        }
    }
}

/// The `POST /v1/audio/embeddings` response body for 16 kHz mono audio: one entry per
/// requested segment (or, without any, per speech segment the server's own VAD finds), echoing
/// its times in seconds rounded to milliseconds. A segment too short for [`embed`] (or lying
/// beyond the end of the audio) has a `null` embedding.
pub fn report(
    samples: &[f32],
    requested: Option<&[Segment]>,
    encoding: Encoding,
) -> serde_json::Value {
    let seconds = |count: usize| count as f64 / f64::from(SAMPLE_RATE);
    let found: Vec<Segment>;
    let segments = match requested {
        Some(segments) => segments,
        None => {
            found = crate::vad::detect(samples, &crate::vad::Params::default())
                .into_iter()
                .map(|span| Segment {
                    start: seconds(span.start),
                    end: seconds(span.end),
                })
                .collect();
            &found
        }
    };
    let entries: Vec<_> = segments
        .iter()
        .map(|segment| {
            let start = samples_in(segment.start).min(samples.len());
            let end = samples_in(segment.end).min(samples.len());
            serde_json::json!({
                "start": crate::vad::rounded(segment.start, 3),
                "end": crate::vad::rounded(segment.end, 3),
                "embedding": embed(&samples[start..end]).map(|e| encode(&e, encoding)),
            })
        })
        .collect();
    serde_json::json!({
        "model": MODEL_ID,
        "dim": DIM,
        "duration": crate::vad::rounded(seconds(samples.len()), 3),
        "segments": entries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Vec<f32> {
        crate::audio::decode_wav(include_bytes!("../../tests/fixtures/speech-16k.wav")).unwrap()
    }

    fn cosine(a: &[f32], b: &[f32]) -> f64 {
        let dot: f64 = a
            .iter()
            .zip(b)
            .map(|(x, y)| f64::from(*x) * f64::from(*y))
            .sum();
        let norm = |v: &[f32]| v.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>().sqrt();
        dot / (norm(a) * norm(b))
    }

    #[test]
    fn matches_pytorch_on_the_speech_fixture() {
        let golden: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/speaker_golden.json")).unwrap();
        assert_eq!(golden["dim"], DIM);
        let speech = fixture();
        for case in golden["cases"].as_array().unwrap() {
            let gain = case["gain"].as_f64().unwrap() as f32;
            let scaled: Vec<f32> = speech.iter().map(|s| s * gain).collect();
            if let Some(expected) = case.get("fbank") {
                let actual = fbank::features(&scaled);
                let rows = golden["fbank_frames"].as_u64().unwrap() as usize;
                let worst = (0..rows * fbank::MEL_BINS)
                    .map(|i| {
                        let want = expected[i / fbank::MEL_BINS][i % fbank::MEL_BINS]
                            .as_f64()
                            .unwrap();
                        (f64::from(actual[i]) - want).abs()
                    })
                    .fold(0.0, f64::max);
                assert!(worst < 1e-3, "gain {gain}: worst feature error {worst}");
            }
            let expected: Vec<f32> = case["embedding"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_f64().unwrap() as f32)
                .collect();
            // The golden is the whole-recording embedding; `embed` would window 6 s of audio.
            let actual = campplus::embed(fbank::features(&scaled));
            let similarity = cosine(&actual, &expected);
            assert!(similarity >= 0.9999, "gain {gain}: cosine {similarity}");
        }
    }

    /// The two-voice fixture and the script of who speaks when (`make_two_speaker_fixture.ps1`).
    fn two_speakers() -> (Vec<f32>, Vec<(char, Segment)>) {
        let audio =
            crate::audio::decode_wav(include_bytes!("../../tests/fixtures/two-speakers-16k.wav"))
                .unwrap();
        let script: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/two-speakers-16k.json"))
                .unwrap();
        let turns = script["segments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|turn| {
                (
                    turn["speaker"].as_str().unwrap().chars().next().unwrap(),
                    Segment {
                        start: turn["start"].as_f64().unwrap(),
                        end: turn["end"].as_f64().unwrap(),
                    },
                )
            })
            .collect();
        (audio, turns)
    }

    #[test]
    fn the_same_voice_is_clearly_closer_than_another() {
        let (audio, turns) = two_speakers();
        let embeddings: Vec<Vec<f32>> = turns
            .iter()
            .map(|(_, t)| embed(&audio[samples_in(t.start)..samples_in(t.end)]).unwrap())
            .collect();
        let (mut same, mut other) = (Vec::new(), Vec::new());
        for i in 0..turns.len() {
            for j in i + 1..turns.len() {
                let similarity = cosine(&embeddings[i], &embeddings[j]);
                if turns[i].0 == turns[j].0 {
                    same.push(similarity);
                } else {
                    other.push(similarity);
                }
            }
        }
        let lowest_same = same.iter().cloned().fold(f64::MAX, f64::min);
        let highest_other = other.iter().cloned().fold(f64::MIN, f64::max);
        assert!(
            lowest_same > highest_other + 0.3,
            "same {same:?}, other {other:?}"
        );
    }

    #[test]
    fn audio_under_half_a_second_has_no_embedding() {
        let speech = fixture();
        assert_eq!(embed(&speech[..samples_in(MIN_SECONDS) - 1]), None);
        let embedding = embed(&speech[..samples_in(MIN_SECONDS)]).unwrap();
        assert_eq!(embedding.len(), DIM);
        assert!((dot(&embedding, &embedding) - 1.0).abs() < 1e-4);
    }

    #[test]
    fn long_audio_is_windowed_and_stays_close_to_the_whole_recording() {
        let speech = fixture();
        let windowed = embed(&speech).unwrap();
        assert_eq!(windowed.len(), DIM);
        assert!((dot(&windowed, &windowed) - 1.0).abs() < 1e-4);
        let whole = campplus::embed(fbank::features(&speech));
        let similarity = cosine(&windowed, &whole);
        assert!(similarity > 0.85, "cosine {similarity}");
    }

    #[test]
    fn segments_are_parsed_and_checked() {
        let parsed =
            parse_segments(r#"[{"start": 0, "end": 1.5}, {"start": 2, "end": 3, "speaker": "x"}]"#);
        assert_eq!(
            parsed.unwrap(),
            vec![
                Segment {
                    start: 0.0,
                    end: 1.5
                },
                Segment {
                    start: 2.0,
                    end: 3.0
                }
            ]
        );
        assert_eq!(parse_segments("[]").unwrap(), vec![]);
        for bad in [
            "",
            "{}",
            r#"[{"start": 1}]"#,
            r#"[{"start": "a", "end": 2}]"#,
            r#"[{"start": -0.1, "end": 2}]"#,
            r#"[{"start": 2, "end": 2}]"#,
            r#"[{"start": 3, "end": 2}]"#,
            r#"[{"start": 0, "end": 1e999}]"#,
        ] {
            assert!(parse_segments(bad).is_err(), "{bad}");
        }
        let many = format!(
            "[{}]",
            vec![r#"{"start": 0, "end": 1}"#; MAX_SEGMENTS + 1].join(",")
        );
        assert!(parse_segments(&many).is_err());
    }

    #[test]
    fn the_report_embeds_each_segment_in_the_requested_encoding() {
        let speech = fixture();
        let segments = [
            Segment {
                start: 0.2,
                end: 1.2,
            },
            // Too short, and one that starts beyond the end of the audio.
            Segment {
                start: 2.0,
                end: 2.3,
            },
            Segment {
                start: 7.0,
                end: 9.0,
            },
        ];
        let floats = report(&speech, Some(&segments), Encoding::Float);
        assert_eq!(floats["model"], MODEL_ID);
        assert_eq!(floats["dim"], DIM);
        assert_eq!(floats["duration"], 6.165);
        let entries = floats["segments"].as_array().unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0]["start"], 0.2);
        assert_eq!(entries[0]["end"], 1.2);
        assert_eq!(entries[0]["embedding"].as_array().unwrap().len(), DIM);
        assert!(entries[1]["embedding"].is_null());
        assert!(entries[2]["embedding"].is_null());

        use base64::Engine as _;
        let encoded = report(&speech, Some(&segments), Encoding::Base64);
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded["segments"][0]["embedding"].as_str().unwrap())
            .unwrap();
        assert_eq!(bytes.len(), DIM * 4);
        for (chunk, value) in bytes
            .as_chunks::<4>()
            .0
            .iter()
            .zip(entries[0]["embedding"].as_array().unwrap())
        {
            let decoded = f32::from_le_bytes(*chunk);
            assert!((f64::from(decoded) - value.as_f64().unwrap()).abs() < 1e-6);
        }
    }

    #[test]
    fn without_segments_the_report_follows_the_speech_detection() {
        let speech = fixture();
        let report = report(&speech, None, Encoding::Float);
        let detected = crate::vad::report(&speech, &crate::vad::Params::default());
        let entries = report["segments"].as_array().unwrap();
        assert!(!entries.is_empty());
        for (entry, span) in entries.iter().zip(detected["segments"].as_array().unwrap()) {
            assert_eq!(
                (&entry["start"], &entry["end"]),
                (&span["start"], &span["end"])
            );
            assert_eq!(entry["embedding"].as_array().unwrap().len(), DIM);
        }
    }
}
