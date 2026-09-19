use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

use stt_runtime::{ModelPullOutcome, ProviderId, SetLanguageOutcome, SwitchModelOutcome, CATALOG};

use crate::error::{runtime_error_response, ApiError};
use crate::state::AppState;

fn parse_provider_id(id: String) -> Result<ProviderId, ApiError> {
    ProviderId::new(id).map_err(runtime_error_response)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    pub id: String,
    pub display_name: String,
    pub provider_id: String,
    /// BCP-47-ish language tags this model covers, or `["auto"]` for a
    /// language-agnostic/auto-detecting model -- mirrors
    /// `runtime::catalog::ModelEntry::languages` verbatim. Lets callers (the
    /// model picker UI) tell a single-language model from a multilingual one
    /// without parsing `display_name`.
    pub languages: Vec<String>,
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
                languages: m.languages.iter().map(|lang| lang.to_string()).collect(),
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
#[serde(rename_all = "camelCase")]
pub struct SwitchModelRequest {
    provider_id: String,
    model_id: String,
}

#[derive(Serialize)]
#[serde(tag = "status")]
pub enum SwitchModelResponse {
    /// The provider wasn't running; behaves exactly like `select_model` --
    /// persisted only, applied on the next start.
    #[serde(rename = "selected")]
    Selected,
    /// An already-running instance was swapped in-process; no subprocess
    /// restart happened.
    #[serde(rename = "swapped", rename_all = "camelCase")]
    Swapped { load_seconds: Option<f64> },
}

/// `POST /v1/models/switch` -- a new, separate, explicit operation from
/// `select_model` above; that route's persist-only contract and callers
/// are unchanged. See `RuntimeManager::switch_model`'s doc comment for the
/// running-vs-not-running behavior split.
pub async fn switch_model(
    State(state): State<AppState>,
    Json(req): Json<SwitchModelRequest>,
) -> Result<Json<SwitchModelResponse>, ApiError> {
    let provider_id = ProviderId::new(req.provider_id).map_err(runtime_error_response)?;
    let outcome = state
        .runtime_manager
        .switch_model(&provider_id, &req.model_id)
        .await
        .map_err(runtime_error_response)?;
    Ok(Json(match outcome {
        SwitchModelOutcome::Selected => SwitchModelResponse::Selected,
        SwitchModelOutcome::Swapped { load_seconds } => {
            SwitchModelResponse::Swapped { load_seconds }
        }
    }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetLanguageRequest {
    provider_id: String,
    model_id: String,
    language: String,
}

#[derive(Serialize)]
#[serde(tag = "status")]
pub enum SetLanguageResponse {
    /// Already serving the requested language; nothing rebuilt.
    #[serde(rename = "unchanged")]
    Unchanged,
    /// The runtime rebuilt its recognizer/model for the new language.
    #[serde(rename = "reloaded", rename_all = "camelCase")]
    Reloaded { load_seconds: Option<f64> },
}

/// `POST /v1/models/language` -- for a runtime whose language is baked into
/// the loaded model rather than a per-request field (sherpa-onnx's
/// `sense-voice-multi` today). Faster-whisper has no reason to call this:
/// its language is a per-request form field on the transcription request
/// itself (see the transcription-language-selection goal). Requires the
/// provider to already be running -- see
/// `RuntimeManager::set_model_language`'s doc comment.
pub async fn set_model_language(
    State(state): State<AppState>,
    Json(req): Json<SetLanguageRequest>,
) -> Result<Json<SetLanguageResponse>, ApiError> {
    let provider_id = ProviderId::new(req.provider_id).map_err(runtime_error_response)?;
    let outcome = state
        .runtime_manager
        .set_model_language(&provider_id, &req.model_id, &req.language)
        .await
        .map_err(runtime_error_response)?;
    Ok(Json(match outcome {
        SetLanguageOutcome::Unchanged => SetLanguageResponse::Unchanged,
        SetLanguageOutcome::Reloaded { load_seconds } => {
            SetLanguageResponse::Reloaded { load_seconds }
        }
    }))
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

/// Shared by pull/verify/remove: which provider's copy of which model.
/// Query-param based, not a `:model` path segment, because curated model
/// ids (e.g. `"Systran/faster-whisper-small"`) contain their own `/` —
/// axum/matchit route matching splits on literal `/` bytes in the *raw*
/// request path, so a slash-containing id can't safely be a single path
/// segment without every caller correctly percent-encoding it first. A
/// query param sidesteps that entirely and matches the `?provider=`
/// convention `selected_model` (above) already established.
#[derive(Deserialize)]
pub struct ModelIdentityQuery {
    provider: String,
    model: String,
}

#[derive(Serialize)]
#[serde(tag = "status")]
pub enum PullModelResponse {
    #[serde(rename = "cached")]
    Cached,
    #[serde(rename = "downloading", rename_all = "camelCase")]
    Downloading { operation_id: String },
}

/// `POST /v1/models/pull?provider=<id>&model=<id>` — downloads `model`'s
/// weights into stt-server's own structured model directory
/// (`cached_model_dir`), reusing the install-operations progress-polling
/// mechanism `POST /v1/providers/:id/install` already established. Requires
/// a provider variant to already be installed (see
/// `RuntimeManager::begin_model_pull`'s doc comment for why).
pub async fn pull_model(
    State(state): State<AppState>,
    Query(query): Query<ModelIdentityQuery>,
) -> Result<(StatusCode, Json<PullModelResponse>), ApiError> {
    let provider_id = parse_provider_id(query.provider)?;
    let outcome = state
        .runtime_manager
        .begin_model_pull(&provider_id, &query.model)
        .await
        .map_err(runtime_error_response)?;
    Ok(match outcome {
        ModelPullOutcome::Cached => (StatusCode::OK, Json(PullModelResponse::Cached)),
        ModelPullOutcome::Downloading { operation_id } => (
            StatusCode::ACCEPTED,
            Json(PullModelResponse::Downloading { operation_id }),
        ),
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyModelResponse {
    verified: bool,
    size_bytes: Option<u64>,
}

/// `POST /v1/models/verify?provider=<id>&model=<id>` — a pure filesystem
/// check (no subprocess, no network) confirming the model's weights are
/// actually present under `cached_model_dir`.
pub async fn verify_model(
    State(state): State<AppState>,
    Query(query): Query<ModelIdentityQuery>,
) -> Result<Json<VerifyModelResponse>, ApiError> {
    let provider_id = parse_provider_id(query.provider)?;
    let size_bytes = state
        .runtime_manager
        .verify_model(&provider_id, &query.model)
        .map_err(runtime_error_response)?;
    Ok(Json(VerifyModelResponse {
        verified: size_bytes.is_some(),
        size_bytes,
    }))
}

/// `DELETE /v1/models/remove?provider=<id>&model=<id>` — deletes the
/// model's cached weight directory. Idempotent: succeeds even if nothing
/// was downloaded yet.
pub async fn remove_model(
    State(state): State<AppState>,
    Query(query): Query<ModelIdentityQuery>,
) -> Result<StatusCode, ApiError> {
    let provider_id = parse_provider_id(query.provider)?;
    state
        .runtime_manager
        .remove_model(&provider_id, &query.model)
        .map_err(runtime_error_response)?;
    Ok(StatusCode::NO_CONTENT)
}
