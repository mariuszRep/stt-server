//! `GET /health` and `GET /v1/config` must tell a caller whether the launched
//! model can actually serve requests, not just that the process is up.
//! Driven through the real router; no model files are needed because a
//! "loaded" model is simulated with a bare job channel.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use sherpad::state::{AppState, ModelState};
use tower::ServiceExt;

fn test_state(default_model: Option<&str>) -> Arc<AppState> {
    let base = std::env::temp_dir().join(format!("sherpad-health-test-{}", uuid::Uuid::new_v4()));
    Arc::new(AppState::new(
        base.join("models"),
        base.join("tmp"),
        default_model.map(str::to_string),
        None,
    ))
}

async fn get_json(state: Arc<AppState>, uri: &str) -> (StatusCode, serde_json::Value) {
    let request = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let response = sherpad::build_router(state).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

async fn mark_loaded(state: &AppState, id: &str) {
    let (jobs, _receiver) = tokio::sync::mpsc::channel(1);
    state.registry.write().await.insert(
        id.to_string(),
        ModelState::Loaded {
            dir: state.models_dir.join(id),
            jobs,
        },
    );
}

#[tokio::test]
async fn health_is_unavailable_while_the_default_model_is_not_installed() {
    let state = test_state(Some("parakeet-tdt-0.6b-v2"));

    let (status, health) = get_json(state.clone(), "/health").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(health["status"], "model_not_loaded");
    assert_eq!(health["model"], "parakeet-tdt-0.6b-v2");

    let (status, config) = get_json(state, "/v1/config").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(config["model_loaded"], false);
}

#[tokio::test]
async fn health_is_unavailable_while_the_default_model_is_installed_but_not_loaded() {
    let state = test_state(Some("parakeet-tdt-0.6b-v2"));
    state.registry.write().await.insert(
        "parakeet-tdt-0.6b-v2".to_string(),
        ModelState::Installed {
            dir: state.models_dir.join("parakeet-tdt-0.6b-v2"),
        },
    );

    let (status, _) = get_json(state, "/health").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn health_is_ok_once_the_default_model_is_loaded() {
    let state = test_state(Some("parakeet-tdt-0.6b-v2"));
    mark_loaded(&state, "parakeet-tdt-0.6b-v2").await;

    let (status, health) = get_json(state.clone(), "/health").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(health["status"], "ok");

    let (_, config) = get_json(state, "/v1/config").await;
    assert_eq!(config["model_loaded"], true);
}

#[tokio::test]
async fn health_is_ok_without_a_default_model() {
    let state = test_state(None);

    let (status, health) = get_json(state.clone(), "/health").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(health["status"], "ok");

    let (_, config) = get_json(state, "/v1/config").await;
    assert_eq!(config["model_loaded"], false);
}
