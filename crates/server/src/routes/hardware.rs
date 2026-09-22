use axum::extract::State;
use axum::Json;

use stt_runtime::{snapshot_memory, HardwareReport, MemorySnapshot};

use crate::state::AppState;

pub async fn get_hardware(State(state): State<AppState>) -> Json<HardwareReport> {
    Json(state.runtime_manager.hardware().clone())
}

/// `GET /v1/system/memory` -- a *live* read of currently available RAM/VRAM,
/// unlike `get_hardware`'s cached-at-startup report. Meant to be polled
/// before a client decides whether to keep pinning warm models or start
/// evicting older ones -- see concurrent-multi-provider-serving's
/// capacity-aware pinning.
pub async fn get_system_memory() -> Json<MemorySnapshot> {
    Json(snapshot_memory())
}
