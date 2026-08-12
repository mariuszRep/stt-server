use axum::extract::State;
use axum::Json;

use stt_runtime::HardwareReport;

use crate::state::AppState;

pub async fn get_hardware(State(state): State<AppState>) -> Json<HardwareReport> {
    Json(state.runtime_manager.hardware().clone())
}
