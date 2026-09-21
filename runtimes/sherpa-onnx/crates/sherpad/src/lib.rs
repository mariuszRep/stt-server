pub mod api;
pub mod decode;
pub mod download;
pub mod recognizer;
pub mod state;

use std::sync::Arc;

use axum::extract::DefaultBodyLimit;
use axum::middleware;
use axum::routing::{delete, get, post};
use axum::Router;
use tower_http::cors::CorsLayer;

use state::AppState;

// axum's `Multipart` extractor defaults to a hard 2MB request-body cap when
// no `DefaultBodyLimit` layer is configured. That's well under a minute of
// real dictation audio (16kHz mono 16-bit PCM alone is ~32KB/s, and browser
// MediaRecorder webm/opus can still land well past 2MB for longer clips),
// so uploads were silently rejected mid-stream with axum's generic
// "Error parsing `multipart/form-data` request" -- the same message for
// every Multipart failure regardless of cause. 100MB comfortably covers any
// realistic single dictation.
pub const MAX_UPLOAD_BYTES: usize = 100 * 1024 * 1024;

/// Builds the full router, extracted out of `main()` so integration tests
/// can drive it directly via `tower::ServiceExt::oneshot` without spawning a
/// real listener.
pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route(
            "/",
            get(|| async { axum::response::Html(include_str!("../static/index.html")) }),
        )
        .route("/health", get(api::health))
        .route("/v1/config", get(api::config))
        .route("/v1/models", get(api::list_models))
        .route("/v1/models/{id}/pull", post(api::pull_model))
        .route("/v1/models/{id}", delete(api::delete_model))
        .route("/v1/models/{id}/load", post(api::load_model))
        .route("/v1/models/{id}/unload", post(api::unload_model))
        .route("/v1/models/{id}/language", post(api::set_model_language))
        .route("/v1/admin/model", post(api::admin_switch_model))
        .route("/v1/audio/transcriptions", post(api::transcribe))
        // Additive diagnostic route, not part of the protocol -- kept
        // alongside /health and /v1/config rather than replaced by them.
        .route("/v1/status", get(api::status))
        // This only binds to 127.0.0.1 by default, so permissive CORS just
        // means "any local page/app on this machine can call it" - fine for
        // a local daemon. Non-loopback binding requires an auth token (see
        // require_auth below), matching CONVENTIONS.md's "remote binding is
        // explicit and authenticated".
        .layer(CorsLayer::permissive())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            api::require_auth,
        ))
        .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES))
        .with_state(state)
}
