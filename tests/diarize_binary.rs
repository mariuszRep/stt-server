//! Diarization against the real compiled binary: no model is installed or loaded, yet
//! `POST /v1/audio/diarize` tells the two voices of the two-speaker fixture apart, names them
//! from known embeddings or reference clips, and refuses bad requests.

mod common;

use std::collections::{BTreeSet, HashMap};

use base64::Engine as _;
use reqwest::multipart::{Form, Part};
use serde_json::{json, Value};

const SPEECH_WAV: &[u8] = include_bytes!("fixtures/speech-16k.wav");
const TWO_SPEAKERS_WAV: &[u8] = include_bytes!("fixtures/two-speakers-16k.wav");
const TWO_SPEAKERS_SCRIPT: &str = include_str!("fixtures/two-speakers-16k.json");

/// A multipart form: the audio, then the text fields (a name may repeat).
fn form(wav: &[u8], fields: &[(&str, &str)]) -> Form {
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
    route: &str,
    token: Option<&str>,
    form: Form,
) -> (reqwest::StatusCode, Value) {
    let mut request = reqwest::Client::new()
        .post(format!("{}{route}", server.base))
        .multipart(form);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let response = request.send().await.unwrap();
    (response.status(), response.json().await.unwrap())
}

async fn diarize(server: &common::Server, fields: &[(&str, &str)]) -> Value {
    let (status, body) = post(
        server,
        "/v1/audio/diarize",
        Some(&server.token),
        form(TWO_SPEAKERS_WAV, fields),
    )
    .await;
    assert_eq!(status, 200, "{fields:?}: {body}");
    body
}

/// The base64 embedding of the fixture's `start`-`end` seconds, from `/v1/audio/embeddings`.
async fn embedding_of(server: &common::Server, turn: &Value) -> String {
    let segments = json!([{"start": turn["start"], "end": turn["end"]}]).to_string();
    let (status, body) = post(
        server,
        "/v1/audio/embeddings",
        Some(&server.token),
        form(
            TWO_SPEAKERS_WAV,
            &[("segments", &segments), ("encoding_format", "base64")],
        ),
    )
    .await;
    assert_eq!(status, 200);
    body["segments"][0]["embedding"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// A WAV file (as a data URL) of the fixture between `start` and `end` seconds.
fn clip_of(start: f64, end: f64) -> String {
    let samples: Vec<i16> = hound::WavReader::new(TWO_SPEAKERS_WAV)
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
    for sample in &samples[(start * 16_000.0) as usize..(end * 16_000.0) as usize] {
        writer.write_sample(*sample).unwrap();
    }
    writer.finalize().unwrap();
    format!(
        "data:audio/wav;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(wav.into_inner())
    )
}

fn clip_of_turn(turn: &Value) -> String {
    clip_of(
        turn["start"].as_f64().unwrap(),
        turn["end"].as_f64().unwrap(),
    )
}

/// Base64 of a unit vector along one axis, which no voice of the fixture resembles.
fn stranger_embedding() -> String {
    let mut vector = vec![0.0f32; 512];
    vector[511] = 1.0;
    let bytes: Vec<u8> = vector.iter().flat_map(|v| v.to_le_bytes()).collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The speaker label of each returned segment.
fn labels(body: &Value) -> Vec<String> {
    body["segments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|segment| {
            assert_eq!(segment["type"], "transcript.text.segment");
            segment["speaker"].as_str().unwrap().to_owned()
        })
        .collect()
}

/// The one label each scripted speaker received; panics when a speaker got two or the answer
/// does not cover the script turn by turn.
fn label_per_script_speaker(body: &Value, turns: &[Value]) -> HashMap<String, String> {
    let labels = labels(body);
    assert_eq!(labels.len(), turns.len(), "{body}");
    let mut by_script: HashMap<String, String> = HashMap::new();
    for (label, turn) in labels.iter().zip(turns) {
        let script = turn["speaker"].as_str().unwrap().to_owned();
        assert_eq!(
            by_script.entry(script).or_insert_with(|| label.clone()),
            label,
            "{body}"
        );
    }
    by_script
}

fn names_of_a_and_b(body: &Value, turns: &[Value]) -> (String, String) {
    let by_script = label_per_script_speaker(body, turns);
    (by_script["A"].clone(), by_script["B"].clone())
}

#[test]
fn the_binary_tells_two_voices_apart_names_them_and_refuses_bad_requests() {
    let server = common::Server::start("stt-diarize-test", 54424);
    let token = server.token.as_str();
    let script: Value = serde_json::from_str(TWO_SPEAKERS_SCRIPT).unwrap();
    let turns = script["segments"].as_array().unwrap().clone();
    let segments = serde_json::to_string(
        &turns
            .iter()
            .map(|t| json!({"start": t["start"], "end": t["end"]}))
            .collect::<Vec<_>>(),
    )
    .unwrap();
    let (turn_a, turn_b) = (&turns[0], &turns[1]);

    tokio::runtime::Runtime::new().unwrap().block_on(async {
        // The route needs a token.
        for token in [None, Some("wrong")] {
            let (status, _) =
                post(&server, "/v1/audio/diarize", token, form(SPEECH_WAV, &[])).await;
            assert_eq!(status, 401, "{token:?}");
        }

        // Given segments: exactly the two scripted speakers, in the scripted places, no text.
        let body = diarize(&server, &[("segments", &segments)]).await;
        assert_eq!(body["model"], "campplus-voxceleb-v1-average-linkage");
        assert_eq!(body["duration"], script["duration"]);
        let entries = body["segments"].as_array().unwrap();
        for (index, (entry, turn)) in entries.iter().zip(&turns).enumerate() {
            assert_eq!(entry["id"], format!("seg_{index}"));
            assert_eq!(entry["start"], turn["start"]);
            assert_eq!(entry["end"], turn["end"]);
            assert!(entry.get("text").is_none());
        }
        assert_eq!(names_of_a_and_b(&body, &turns), ("A".into(), "B".into()));

        // Without segments the server's own speech detection finds the speech (it may split a
        // turn at a pause): every segment lies in a turn, and each speaker keeps one label.
        let found = diarize(&server, &[]).await;
        let mut by_script: HashMap<&str, String> = HashMap::new();
        for segment in found["segments"].as_array().unwrap() {
            let (start, end) = (
                segment["start"].as_f64().unwrap(),
                segment["end"].as_f64().unwrap(),
            );
            let turn = turns
                .iter()
                .find(|t| {
                    t["start"].as_f64().unwrap() - 0.3 <= start
                        && end <= t["end"].as_f64().unwrap() + 0.3
                })
                .unwrap_or_else(|| panic!("{segment} lies in no turn"));
            let label = segment["speaker"].as_str().unwrap().to_owned();
            let script = turn["speaker"].as_str().unwrap();
            assert_eq!(
                by_script.entry(script).or_insert_with(|| label.clone()),
                &label,
                "{found}"
            );
        }
        assert_eq!(by_script.len(), 2, "{found}");
        assert_ne!(by_script["A"], by_script["B"], "{found}");

        // max_speakers merges past the threshold; a strict threshold keeps every turn apart.
        let one = diarize(&server, &[("segments", &segments), ("max_speakers", "1")]).await;
        assert!(labels(&one).iter().all(|label| label == "A"));
        let apart = diarize(&server, &[("segments", &segments), ("threshold", "0.95")]).await;
        assert_eq!(labels(&apart).iter().collect::<BTreeSet<_>>().len(), 4);

        // Known speakers by embedding, taken from the fixture's own turns and listed in the
        // opposite order of appearance.
        let embedding_a = embedding_of(&server, turn_a).await;
        let embedding_b = embedding_of(&server, turn_b).await;
        let named = diarize(
            &server,
            &[
                ("segments", &segments),
                ("known_speaker_names[]", "Bea"),
                ("known_speaker_names[]", "Alan"),
                ("known_speaker_embeddings[]", &embedding_b),
                ("known_speaker_embeddings[]", &embedding_a),
            ],
        )
        .await;
        assert_eq!(
            names_of_a_and_b(&named, &turns),
            ("Alan".into(), "Bea".into())
        );

        // Only voice B known: voice A keeps the first letter.
        let partly = diarize(
            &server,
            &[
                ("segments", &segments),
                ("known_speaker_names[]", "Bea"),
                ("known_speaker_embeddings[]", &embedding_b),
            ],
        )
        .await;
        assert_eq!(
            names_of_a_and_b(&partly, &turns),
            ("A".into(), "Bea".into())
        );

        // A known voice that is nobody here names nobody.
        let stranger = diarize(
            &server,
            &[
                ("segments", &segments),
                ("known_speaker_names[]", "Zed"),
                ("known_speaker_embeddings[]", &stranger_embedding()),
            ],
        )
        .await;
        assert_eq!(
            names_of_a_and_b(&stranger, &turns),
            ("A".into(), "B".into())
        );

        // Known speakers by reference clip (data URLs of the fixture's own turns).
        let (clip_a, clip_b) = (clip_of_turn(turn_a), clip_of_turn(turn_b));
        let by_clip = diarize(
            &server,
            &[
                ("segments", &segments),
                ("known_speaker_names[]", "Alan"),
                ("known_speaker_names[]", "Bea"),
                ("known_speaker_references[]", &clip_a),
                ("known_speaker_references[]", &clip_b),
            ],
        )
        .await;
        assert_eq!(
            names_of_a_and_b(&by_clip, &turns),
            ("Alan".into(), "Bea".into())
        );

        // Errors.
        let short_clip = clip_of(0.0, 1.0);
        let cases: Vec<(&str, Vec<(&str, &str)>)> = vec![
            ("threshold below range", vec![("threshold", "0.01")]),
            ("threshold above range", vec![("threshold", "1")]),
            ("threshold not a number", vec![("threshold", "high")]),
            ("max_speakers zero", vec![("max_speakers", "0")]),
            ("max_speakers fractional", vec![("max_speakers", "1.5")]),
            ("max_speakers too many", vec![("max_speakers", "27")]),
            ("bad segments", vec![("segments", "nonsense")]),
            ("unknown field", vec![("response_format", "diarized_json")]),
            (
                "more names than embeddings",
                vec![
                    ("known_speaker_names[]", "Ann"),
                    ("known_speaker_names[]", "Bob"),
                    ("known_speaker_embeddings[]", &embedding_a),
                ],
            ),
            (
                "more embeddings than names",
                vec![
                    ("known_speaker_names[]", "Ann"),
                    ("known_speaker_embeddings[]", &embedding_a),
                    ("known_speaker_embeddings[]", &embedding_b),
                ],
            ),
            ("names alone", vec![("known_speaker_names[]", "Ann")]),
            (
                "embeddings without names",
                vec![("known_speaker_embeddings[]", &embedding_a)],
            ),
            (
                "embeddings and references",
                vec![
                    ("known_speaker_names[]", "Ann"),
                    ("known_speaker_embeddings[]", &embedding_a),
                    ("known_speaker_references[]", &clip_a),
                ],
            ),
            (
                "duplicate names",
                vec![
                    ("known_speaker_names[]", "Ann"),
                    ("known_speaker_names[]", "Ann"),
                    ("known_speaker_embeddings[]", &embedding_a),
                    ("known_speaker_embeddings[]", &embedding_b),
                ],
            ),
            (
                "wrong-sized embedding",
                vec![
                    ("known_speaker_names[]", "Ann"),
                    ("known_speaker_embeddings[]", "AAAA"),
                ],
            ),
            (
                "reference that is no audio",
                vec![
                    ("known_speaker_names[]", "Ann"),
                    ("known_speaker_references[]", "AAAA"),
                ],
            ),
            (
                "reference under two seconds",
                vec![
                    ("known_speaker_names[]", "Ann"),
                    ("known_speaker_references[]", &short_clip),
                ],
            ),
        ];
        for (what, fields) in cases {
            let (status, body) = post(
                &server,
                "/v1/audio/diarize",
                Some(token),
                form(TWO_SPEAKERS_WAV, &fields),
            )
            .await;
            assert_eq!(status, 400, "{what}: {body}");
            assert_eq!(
                body["error"]["code"], "invalid_request_error",
                "{what}: {body}"
            );
        }
        let (status, body) = post(
            &server,
            "/v1/audio/diarize",
            Some(token),
            form(
                TWO_SPEAKERS_WAV,
                &[("threshold", "0.4"), ("threshold", "0.5")],
            ),
        )
        .await;
        assert_eq!(
            (status, &body["error"]["code"]),
            (reqwest::StatusCode::BAD_REQUEST, &json!("duplicate_field"))
        );
        let (status, body) = post(
            &server,
            "/v1/audio/diarize",
            Some(token),
            Form::new().text("threshold", "0.4"),
        )
        .await;
        assert_eq!(
            (status, &body["error"]["code"]),
            (reqwest::StatusCode::BAD_REQUEST, &json!("missing_file"))
        );

        // The capability.
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
            system["diarization"],
            json!({"engine": "campplus-voxceleb-v1-average-linkage", "max_speakers": 26})
        );
    });
}
