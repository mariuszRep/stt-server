//! Integration tests for `POST /v1/audio/transcriptions`, driven straight
//! through the real router via `tower::ServiceExt::oneshot` -- no listener,
//! no real network socket.
//!
//! Two of the goal's four acceptance checks need a real installed ASR model
//! (multi-hundred-MB download) and are gated behind `#[ignore]`, driven by
//! `SHERPAD_TEST_MODEL_DIR`/`SHERPAD_TEST_MODEL_ID` env vars pointing at an
//! already-installed model directory. To run them locally:
//!
//! ```sh
//! # once, to install a model into a scratch dir:
//! cargo run --release --bin sherpad &
//! curl -X POST http://127.0.0.1:7891/v1/models/parakeet-tdt-0.6b-v2/pull
//! # then:
//! SHERPAD_TEST_MODEL_DIR=<data-dir>/onnx-sherpa/models/parakeet-tdt-0.6b-v2 \
//!   cargo test -p sherpad --release -- --ignored
//! ```
//!
//! The corrupt-audio and missing-field cases need no model at all and always
//! run as part of the normal test suite.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use sherpad::state::{AppState, ModelState};
use tower::ServiceExt;

const SAMPLE_WEBM: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/sample.webm"
));

fn test_state(default_model: Option<&str>) -> Arc<AppState> {
    let unique = uuid::Uuid::new_v4();
    let base = std::env::temp_dir().join(format!("sherpad-test-{unique}"));
    Arc::new(AppState::new(
        base.join("models"),
        base.join("tmp"),
        default_model.map(str::to_string),
        None,
    ))
}

/// Builds a `multipart/form-data` body by hand -- no HTTP client crate is a
/// project dependency, and this is simple enough not to warrant adding one.
fn multipart_body(fields: &[(&str, Option<&str>, &[u8])]) -> (String, Vec<u8>) {
    let boundary = format!("sherpad-test-boundary-{}", uuid::Uuid::new_v4());
    let mut body = Vec::new();
    for (name, filename, bytes) in fields {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        match filename {
            Some(f) => body.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{name}\"; filename=\"{f}\"\r\n")
                    .as_bytes(),
            ),
            None => body.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{name}\"\r\n").as_bytes(),
            ),
        }
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (boundary, body)
}

async fn post_transcribe(
    state: Arc<AppState>,
    fields: &[(&str, Option<&str>, &[u8])],
) -> (StatusCode, serde_json::Value) {
    let (boundary, body) = multipart_body(fields);
    let request = Request::builder()
        .method("POST")
        .uri("/v1/audio/transcriptions")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(body))
        .unwrap();

    let response = sherpad::build_router(state).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, json)
}

#[tokio::test]
async fn corrupt_audio_returns_400_with_json_error_not_a_crash() {
    // A default model is configured so the request gets past the "which
    // model" check and actually reaches audio decode -- that's the path
    // this test is exercising. The model need not be installed: decode
    // happens before any model lookup.
    let state = test_state(Some("parakeet-tdt-0.6b-v2"));
    let garbage: &[u8] = &[0u8; 256];

    let (status, json) = post_transcribe(state, &[("file", Some("bad.webm"), garbage)]).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        json.get("error").is_some(),
        "expected a JSON error body, got {json:?}"
    );
}

#[tokio::test]
async fn truncated_real_webm_opus_returns_400_not_a_crash() {
    let state = test_state(Some("parakeet-tdt-0.6b-v2"));
    let truncated = &SAMPLE_WEBM[..SAMPLE_WEBM.len() / 4];

    let (status, _json) =
        post_transcribe(state, &[("file", Some("truncated.webm"), truncated)]).await;

    // Truncated input may or may not yield decodable samples depending on
    // where the cut lands, but it must never surface as anything other than
    // a clean HTTP response (i.e. the request task must not have panicked).
    assert!(
        status == StatusCode::BAD_REQUEST || status == StatusCode::OK,
        "unexpected status {status}"
    );
}

#[tokio::test]
async fn missing_file_field_returns_400() {
    let state = test_state(Some("parakeet-tdt-0.6b-v2"));
    let (status, json) = post_transcribe(state, &[("prompt", None, b"hello".as_slice())]).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(json.get("error").is_some());
}

#[tokio::test]
async fn no_default_model_and_no_model_field_returns_400() {
    let state = test_state(None);
    let (status, json) = post_transcribe(state, &[("file", Some("clip.webm"), SAMPLE_WEBM)]).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(json.get("error").is_some());
}

fn model_env() -> Option<(String, String)> {
    let dir = std::env::var("SHERPAD_TEST_MODEL_DIR").ok()?;
    let id = std::env::var("SHERPAD_TEST_MODEL_ID")
        .unwrap_or_else(|_| "parakeet-tdt-0.6b-v2".to_string());
    Some((dir, id))
}

/// Real end-to-end check for acceptance criterion 3: a webm/opus recording
/// transcribes successfully through the exact SDK request shape (file +
/// optional prompt, no model field). Needs a real installed model -- see
/// module doc for how to run this locally.
#[tokio::test]
#[ignore = "needs a real installed ASR model; see module doc"]
async fn webm_opus_transcribes_via_installed_model() {
    let (model_dir, model_id) = model_env().expect("SHERPAD_TEST_MODEL_DIR must be set");
    let state = test_state(Some(&model_id));
    state.registry.write().await.insert(
        model_id,
        ModelState::Installed {
            dir: model_dir.into(),
        },
    );

    let (status, json) = post_transcribe(
        state,
        &[
            ("file", Some("clip.webm"), SAMPLE_WEBM),
            ("prompt", None, b"".as_slice()),
        ],
    )
    .await;

    assert_eq!(status, StatusCode::OK, "response: {json:?}");
    let text = json
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(!text.trim().is_empty(), "expected non-empty transcript");
}

/// Regression companion to the Opus test above: the same installed model
/// must still transcribe a plain WAV recording (the app's primary path)
/// identically in shape.
#[tokio::test]
#[ignore = "needs a real installed ASR model; see module doc"]
async fn wav_still_transcribes_via_installed_model() {
    const SAMPLE_WAV: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../../crates/runtime/tests/fixtures/sample.wav"
    ));

    let (model_dir, model_id) = model_env().expect("SHERPAD_TEST_MODEL_DIR must be set");
    let state = test_state(Some(&model_id));
    state.registry.write().await.insert(
        model_id,
        ModelState::Installed {
            dir: model_dir.into(),
        },
    );

    let (status, json) = post_transcribe(state, &[("file", Some("clip.wav"), SAMPLE_WAV)]).await;

    assert_eq!(status, StatusCode::OK, "response: {json:?}");
    let text = json
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(!text.trim().is_empty(), "expected non-empty transcript");
}
