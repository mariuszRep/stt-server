use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

use stt_runtime::{ProviderId, CATALOG};

use crate::error::{runtime_error_response, ApiError};
use crate::state::AppState;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    pub id: String,
    pub display_name: String,
    pub provider_id: String,
}

/// Flat curated model list across all providers (there's one today).
pub async fn list_models() -> Json<Vec<ModelInfo>> {
    let models = CATALOG
        .iter()
        .flat_map(|entry| {
            entry.models.iter().map(move |m| ModelInfo {
                id: m.id.to_string(),
                display_name: m.display_name.to_string(),
                provider_id: entry.id.to_string(),
            })
        })
        .collect();
    Json(models)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectModelRequest {
    provider_id: String,
    model_id: String,
}

pub async fn select_model(
    State(state): State<AppState>,
    Json(req): Json<SelectModelRequest>,
) -> Result<StatusCode, ApiError> {
    let provider_id = ProviderId::new(req.provider_id).map_err(runtime_error_response)?;
    state
        .runtime_manager
        .select_model(&provider_id, &req.model_id)
        .await
        .map_err(runtime_error_response)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct SelectedModelQuery {
    provider: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectedModelResponse {
    model_id: Option<String>,
}

pub async fn selected_model(
    State(state): State<AppState>,
    Query(query): Query<SelectedModelQuery>,
) -> Result<Json<SelectedModelResponse>, ApiError> {
    let provider_id = ProviderId::new(query.provider).map_err(runtime_error_response)?;
    Ok(Json(SelectedModelResponse {
        model_id: state.runtime_manager.selected_model(&provider_id).await,
    }))
}

#[derive(Serialize)]
pub struct AutomaticResponse {
    automatic: bool,
    message: &'static str,
}

/// faster-whisper models are downloaded and cached by `ctranslate2`'s
/// HuggingFace integration inside the managed runtime on first use — there
/// is no separate file for the control plane to fetch, so these three
/// endpoints exist (rather than 404ing) but say so explicitly instead of
/// pretending to manage files that don't exist at this layer.
pub async fn pull_model() -> Json<AutomaticResponse> {
    Json(AutomaticResponse {
        automatic: true,
        message: "faster-whisper models download automatically inside the managed runtime on first use; explicit pre-download isn't implemented.",
    })
}

pub async fn verify_model() -> Json<AutomaticResponse> {
    Json(AutomaticResponse {
        automatic: true,
        message: "faster-whisper models are verified by the managed runtime's own load path; no separate control-plane verification exists.",
    })
}

pub async fn remove_model() -> Json<AutomaticResponse> {
    Json(AutomaticResponse {
        automatic: true,
        message: "no separate model file is managed by the control plane for faster-whisper; remove the runtime's HuggingFace cache directly to reclaim space.",
    })
}
