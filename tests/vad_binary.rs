//! Offline speech detection against the real compiled binary: no model is installed or loaded,
//! yet `POST /v1/audio/vad` finds the speech in the fixture and `/v1/local/system` advertises
//! the engine.

mod common;

use serde_json::{json, Value};

const SPEECH_WAV: &[u8] = include_bytes!("fixtures/speech-16k.wav");

#[test]
fn the_binary_detects_speech_without_a_loaded_model() {
    let server = common::Server::start("stt-vad-test", 54421);
    let base = &server.base;

    let (system, vad): (Value, Value) = tokio::runtime::Runtime::new().unwrap().block_on(async {
        let client = reqwest::Client::new();
        let system = client
            .get(format!("{base}/v1/local/system"))
            .bearer_auth(&server.token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let form = reqwest::multipart::Form::new().part(
            "file",
            reqwest::multipart::Part::bytes(SPEECH_WAV)
                .file_name("speech.wav")
                .mime_str("audio/wav")
                .unwrap(),
        );
        let response = client
            .post(format!("{base}/v1/audio/vad"))
            .bearer_auth(&server.token)
            .multipart(form)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        (system, response.json().await.unwrap())
    });

    assert_eq!(
        system["vad"],
        json!({"engine": "silero-v5", "sample_rate": 16000})
    );
    assert!(!vad["segments"].as_array().unwrap().is_empty());
    assert!(vad["speech_ratio"].as_f64().unwrap() > 0.5);
}
