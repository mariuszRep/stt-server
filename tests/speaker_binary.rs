//! Speaker embeddings against the real compiled binary: no model is installed or loaded, yet
//! `POST /v1/audio/embeddings` fingerprints the voices of a two-speaker recording, and
//! `/v1/local/system` advertises the engine.

mod common;

use base64::Engine as _;
use reqwest::multipart::{Form, Part};
use serde_json::{json, Value};

const SPEECH_WAV: &[u8] = include_bytes!("fixtures/speech-16k.wav");
const TWO_SPEAKERS_WAV: &[u8] = include_bytes!("fixtures/two-speakers-16k.wav");
const TWO_SPEAKERS_SCRIPT: &str = include_str!("fixtures/two-speakers-16k.json");
const DIM: usize = 512;

fn audio_form(wav: &[u8], fields: &[(&str, &str)]) -> Form {
    let mut form = Form::new().part(
        "file",
        Part::bytes(wav.to_vec())
            .file_name("audio.wav")
            .mime_str("audio/wav")
            .unwrap(),
    );
    for (name, value) in fields {
        form = form.text(name.to_string(), value.to_string());
    }
    form
}

async fn post(
    server: &common::Server,
    token: Option<&str>,
    form: Form,
) -> (reqwest::StatusCode, Value) {
    let mut request = reqwest::Client::new()
        .post(format!("{}/v1/audio/embeddings", server.base))
        .multipart(form);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let response = request.send().await.unwrap();
    (response.status(), response.json().await.unwrap())
}

fn floats(embedding: &Value) -> Vec<f64> {
    embedding
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_f64().unwrap())
        .collect()
}

fn cosine(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

#[test]
fn the_binary_fingerprints_two_voices_without_a_loaded_model() {
    let server = common::Server::start("stt-speaker-test", 54422);
    let token = server.token.as_str();
    let script: Value = serde_json::from_str(TWO_SPEAKERS_SCRIPT).unwrap();
    let turns = script["segments"].as_array().unwrap();
    let segments = serde_json::to_string(
        &turns
            .iter()
            .map(|t| json!({"start": t["start"], "end": t["end"]}))
            .collect::<Vec<_>>(),
    )
    .unwrap();

    tokio::runtime::Runtime::new().unwrap().block_on(async {
        // The route needs a token.
        for token in [None, Some("wrong")] {
            let (status, _) = post(&server, token, audio_form(SPEECH_WAV, &[])).await;
            assert_eq!(status, 401, "{token:?}");
        }

        // Errors: a missing file, then bad or unknown fields.
        let (status, body) = post(&server, Some(token), Form::new().text("segments", "[]")).await;
        assert_eq!(status, 400);
        assert_eq!(body["error"]["code"], "missing_file");
        for fields in [
            ("encoding_format", "hex"),
            ("segments", r#"[{"start": 1, "end": 0.5}]"#),
            ("segments", "nonsense"),
            ("model", "default"),
        ] {
            let (status, body) =
                post(&server, Some(token), audio_form(SPEECH_WAV, &[fields])).await;
            assert_eq!(status, 400, "{fields:?}");
            assert_eq!(body["error"]["code"], "invalid_request_error", "{fields:?}");
        }

        // Given segments: one embedding per turn of the script, and the voices separate.
        let (status, listed) = post(
            &server,
            Some(token),
            audio_form(TWO_SPEAKERS_WAV, &[("segments", &segments)]),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(listed["model"], "campplus-voxceleb-v1");
        assert_eq!(listed["dim"], DIM);
        assert_eq!(listed["duration"], script["duration"]);
        let entries = listed["segments"].as_array().unwrap();
        assert_eq!(entries.len(), turns.len());
        let vectors: Vec<Vec<f64>> = entries.iter().map(|e| floats(&e["embedding"])).collect();
        for (entry, turn) in entries.iter().zip(turns) {
            assert_eq!(
                (&entry["start"], &entry["end"]),
                (&turn["start"], &turn["end"])
            );
        }
        let (mut same, mut other) = (f64::MAX, f64::MIN);
        for i in 0..turns.len() {
            assert_eq!(vectors[i].len(), DIM);
            assert!((cosine(&vectors[i], &vectors[i]) - 1.0).abs() < 1e-4);
            for j in i + 1..turns.len() {
                let similarity = cosine(&vectors[i], &vectors[j]);
                if turns[i]["speaker"] == turns[j]["speaker"] {
                    same = same.min(similarity);
                } else {
                    other = other.max(similarity);
                }
            }
        }
        assert!(same > other + 0.3, "same voice {same}, other voice {other}");

        // base64 carries the same vectors; a segment under 0.5 s has none.
        let short = r#"[{"start": 0, "end": 0.4}, {"start": 0, "end": 2}]"#;
        let (status, encoded) = post(
            &server,
            Some(token),
            audio_form(
                TWO_SPEAKERS_WAV,
                &[("segments", short), ("encoding_format", "base64")],
            ),
        )
        .await;
        assert_eq!(status, 200);
        assert!(encoded["segments"][0]["embedding"].is_null());
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded["segments"][1]["embedding"].as_str().unwrap())
            .unwrap();
        assert_eq!(bytes.len(), DIM * 4);
        let (status, plain) = post(
            &server,
            Some(token),
            audio_form(TWO_SPEAKERS_WAV, &[("segments", short)]),
        )
        .await;
        assert_eq!(status, 200);
        let decoded: Vec<f64> = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f64::from(f32::from_le_bytes(*b)))
            .collect();
        let expected = floats(&plain["segments"][1]["embedding"]);
        assert!(decoded
            .iter()
            .zip(&expected)
            .all(|(a, b)| (a - b).abs() < 1e-6));

        // Without segments the server's own speech detection decides what to fingerprint.
        let (status, found) = post(&server, Some(token), audio_form(SPEECH_WAV, &[])).await;
        assert_eq!(status, 200);
        let found = found["segments"].as_array().unwrap();
        assert!(!found.is_empty());
        assert!(found
            .iter()
            .all(|s| s["embedding"].as_array().unwrap().len() == DIM));

        let system: Value = reqwest::Client::new()
            .get(format!("{}/v1/local/system", server.base))
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            system["speaker_embedding"],
            json!({"engine": "campplus-voxceleb-v1", "dim": 512, "sample_rate": 16000})
        );
    });
}

/// A minute of speech (the fixture repeated) is embedded in a fraction of its length. Only a
/// release build is fast enough for the CAM++ forward pass to be measured.
#[cfg(not(debug_assertions))]
#[test]
fn a_minute_of_audio_is_embedded_well_faster_than_real_time() {
    /// Seconds of work allowed per second of audio; generous for a busy machine.
    const REAL_TIME_FACTOR_LIMIT: f64 = 0.5;
    let samples: Vec<i16> = hound::WavReader::new(SPEECH_WAV)
        .unwrap()
        .into_samples::<i16>()
        .map(Result::unwrap)
        .collect();
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 16_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut wav = std::io::Cursor::new(Vec::new());
    let mut writer = hound::WavWriter::new(&mut wav, spec).unwrap();
    for sample in samples.iter().cycle().take(60 * 16_000) {
        writer.write_sample(*sample).unwrap();
    }
    writer.finalize().unwrap();
    let wav = wav.into_inner();

    let server = common::Server::start("stt-speaker-rtf", 54423);
    let (elapsed, status, body) = tokio::runtime::Runtime::new().unwrap().block_on(async {
        let started = std::time::Instant::now();
        let (status, body) = post(
            &server,
            Some(&server.token),
            audio_form(&wav, &[("segments", r#"[{"start": 0, "end": 60}]"#)]),
        )
        .await;
        (started.elapsed().as_secs_f64(), status, body)
    });
    assert_eq!(status, 200);
    assert_eq!(
        body["segments"][0]["embedding"].as_array().unwrap().len(),
        DIM
    );
    eprintln!("60 s of audio took {elapsed:.2} s");
    assert!(
        elapsed < REAL_TIME_FACTOR_LIMIT * 60.0,
        "60 s of audio took {elapsed:.1} s"
    );
}
