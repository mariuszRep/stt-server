//! `POST /v1/models/:id/unload` -- specifically the guard added alongside
//! faster-whisper's equivalent (same day, see concurrent-multi-provider-
//! serving) that refuses to unload the model a model-omitted request
//! currently resolves to. Only the paths that don't require building a real
//! recognizer are covered here (no model files are available in this
//! environment) -- mirrors `tests/language.rs`'s fixture pattern exactly.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use sherpad::state::{AppState, ModelState};
use tower::ServiceExt;

fn test_state(default_model: Option<&str>) -> Arc<AppState> {
    let base = std::env::temp_dir().join(format!("sherpad-lifecycle-test-{}", uuid::Uuid::new_v4()));
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

async fn post(state: Arc<AppState>, uri: &str) -> (StatusCode, serde_json::Value) {
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .body(Body::empty())
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
async fn unload_refuses_the_current_default_model() {
    let state = test_state(Some("sense-voice-multi"));
    mark_loaded(&state, "sense-voice-multi", "auto").await;

    let (status, _) = post(state, "/v1/models/sense-voice-multi/unload").await;

    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "unloading the model a bare transcribe request resolves to must be refused"
    );
}

#[tokio::test]
async fn unload_succeeds_for_a_non_default_loaded_model() {
    let state = test_state(Some("sense-voice-multi"));
    mark_loaded(&state, "sense-voice-multi", "auto").await;
    mark_loaded(&state, "parakeet-tdt-0.6b-v2", "auto").await;

    let (status, body) = post(state, "/v1/models/parakeet-tdt-0.6b-v2/unload").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "installed");
}

#[tokio::test]
async fn unload_of_unknown_model_is_not_found() {
    let state = test_state(Some("sense-voice-multi"));
    mark_loaded(&state, "sense-voice-multi", "auto").await;

    let (status, _) = post(state, "/v1/models/not-a-real-model/unload").await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}
