//! Structural + auth-enforcement tests against the real router (not a live
//! bound socket) via `tower::ServiceExt::oneshot`.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use stt_common::ServerConfig;
use stt_runtime::RuntimeManager;

fn test_state(auth_token: Option<&str>) -> stt_server::AppState {
    let config = ServerConfig {
        auth_token: auth_token.map(|s| s.to_string()),
        ..ServerConfig::default()
    };
    stt_server::AppState::new(config, Arc::new(RuntimeManager::new(None)))
}

async fn get(app: axum::Router, path: &str, bearer: Option<&str>) -> StatusCode {
    let mut req = Request::builder().uri(path);
    if let Some(token) = bearer {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    let response = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
    response.status()
}

#[tokio::test]
async fn requests_pass_through_without_auth_configured() {
    let app = stt_server::build_router(test_state(None));
    assert_eq!(get(app, "/v1/health", None).await, StatusCode::OK);
}

#[tokio::test]
async fn requests_without_token_are_rejected_when_auth_configured() {
    let app = stt_server::build_router(test_state(Some("secret")));
    assert_eq!(get(app, "/v1/health", None).await, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn requests_with_wrong_token_are_rejected_when_auth_configured() {
    let app = stt_server::build_router(test_state(Some("secret")));
    assert_eq!(
        get(app, "/v1/health", Some("wrong")).await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn requests_with_correct_token_are_allowed_when_auth_configured() {
    let app = stt_server::build_router(test_state(Some("secret")));
    assert_eq!(get(app, "/v1/health", Some("secret")).await, StatusCode::OK);
}

/// Belt-and-suspenders regression guard: the goal this router serves
/// explicitly forbids any transcription data-path route (batch upload or a
/// WebSocket upgrade) ever existing on the control plane again. Assert the
/// old paths are gone rather than just trusting no one re-adds them.
#[tokio::test]
async fn no_transcription_data_path_routes_exist() {
    let app = stt_server::build_router(test_state(None));
    assert_eq!(
        get(app.clone(), "/v1/transcriptions", None).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(app.clone(), "/v1/audio/transcriptions", None).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(app, "/v1/realtime/transcriptions", None).await,
        StatusCode::NOT_FOUND
    );
}
