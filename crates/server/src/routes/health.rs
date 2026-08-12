use axum::extract::State;
use axum::Json;

use stt_common::{HealthResponse, ReadinessResponse};

use crate::state::AppState;

pub async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

pub async fn readiness(State(_state): State<AppState>) -> Json<ReadinessResponse> {
    Json(ReadinessResponse {
        ready: true,
        reason: None,
    })
}
