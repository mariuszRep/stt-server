use axum::extract::State;
use axum::Json;

use stt_runtime::ModelRecommendation;

use crate::state::AppState;

pub async fn recommendations(State(state): State<AppState>) -> Json<Vec<ModelRecommendation>> {
    Json(stt_runtime::recommend(state.runtime_manager.hardware()))
}
