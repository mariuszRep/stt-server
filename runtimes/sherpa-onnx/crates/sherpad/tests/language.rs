//! `POST /v1/models/:id/language` and `POST /v1/admin/model` -- the two
//! endpoints `8f69eb2` added to give sherpa-onnx a real reload-based
//! language mechanism and the hot-swap contract `stt-server`'s
//! `RuntimeManager` already assumed every provider had. Only the paths that
//! don't require building a real recognizer are covered here (no model
//! files are available in this environment); the reload path itself is
//! covered by the goal's manual end-to-end verification against a real
//! downloaded model -- see the transcription-language-selection goal.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use sherpad::state::{AppState, ModelState};
use tower::ServiceExt;

fn test_state(default_model: Option<&str>) -> Arc<AppState> {
    let base = std::env::temp_dir().join(format!("sherpad-language-test-{}", uuid::Uuid::new_v4()));
    Arc::new(AppState::new(
        base.join("models"),
        base.join("tmp"),
        default_model.map(str::to_string),
        None,
    ))
}

async fn mark_loaded(state: &AppState, id: &str, language: &str) {
    let (jobs, _receiver) = tokio::sync::mpsc::channel(1);
    state.registry.write().await.insert(
        id.to_string(),
        ModelState::Loaded {
            dir: state.models_dir.join(id),
            jobs,
            language: language.to_string(),
        },
    );
}

async fn post_json(state: Arc<AppState>, uri: &str, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = sherpad::build_router(state).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let parsed = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, parsed)
}

#[tokio::test]
async fn set_model_language_reports_unchanged_without_rebuilding() {
    let state = test_state(None);
    mark_loaded(&state, "sense-voice-multi", "auto").await;

    let (status, body) = post_json(
        state,
        "/v1/models/sense-voice-multi/language",
        serde_json::json!({ "language": "auto" }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "unchanged");
}

#[tokio::test]
async fn set_model_language_rejects_a_language_the_model_does_not_support() {
    let state = test_state(None);
    mark_loaded(&state, "sense-voice-multi", "auto").await;

    let (status, _) = post_json(
        state,
        "/v1/models/sense-voice-multi/language",
        serde_json::json!({ "language": "fr" }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "sense-voice-multi's languages are auto/zh/en/ja/ko/yue -- 'fr' is not one of them"
    );
}

#[tokio::test]
async fn set_model_language_rejects_a_model_that_was_never_installed() {
    let state = test_state(None);
    // Registry has no entry at all for this id -- distinct from "installed
    // but not loaded" (`ModelState::Installed`).

    let (status, _) = post_json(
        state,
        "/v1/models/sense-voice-multi/language",
        serde_json::json!({ "language": "en" }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn set_model_language_rejects_an_unknown_model_id() {
    let state = test_state(None);

    let (status, _) = post_json(
        state,
        "/v1/models/not-a-real-model/language",
        serde_json::json!({ "language": "en" }),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn admin_switch_model_swaps_the_default_without_reloading_an_already_loaded_model() {
    let state = test_state(Some("parakeet-tdt-0.6b-v2"));
    mark_loaded(&state, "parakeet-tdt-0.6b-v2", "en").await;
    mark_loaded(&state, "sense-voice-multi", "auto").await;

    let (status, body) = post_json(
        state.clone(),
        "/v1/admin/model",
        serde_json::json!({ "model": "sense-voice-multi" }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
    assert_eq!(body["model"], "sense-voice-multi");
    assert_eq!(
        body["loadSeconds"],
        serde_json::Value::Null,
        "already loaded -- swapping the default must not report a reload"
    );
    assert_eq!(
        state.default_model.read().await.as_deref(),
        Some("sense-voice-multi")
    );
}
