use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

use stt_common::RuntimeConnectionDescriptor;
use stt_runtime::{ProviderId, ProviderInfo, RuntimeError, RuntimeStatus};

use crate::error::{runtime_error_response, ApiError};
use crate::state::AppState;

fn parse_provider_id(id: String) -> Result<ProviderId, ApiError> {
    ProviderId::new(id).map_err(runtime_error_response)
}

/// Resolve how to launch a provider once its artifact is confirmed present
/// locally. Only one provider exists in the catalog today; this dispatch
/// grows a match arm per provider rather than a plugin trait, since a
/// single-entry catalog doesn't justify that abstraction yet.
fn installer_for(id: &ProviderId) -> Result<stt_runtime::LaunchBuilder, ApiError> {
    match id.as_str() {
        "faster-whisper" => {
            stt_runtime::providers::faster_whisper::install().map_err(runtime_error_response)
        }
        _ => Err(runtime_error_response(RuntimeError::ProviderNotFound(
            id.to_string(),
        ))),
    }
}

pub async fn list_providers(State(state): State<AppState>) -> Json<Vec<ProviderInfo>> {
    Json(state.runtime_manager.list_providers())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallResponse {
    status: &'static str,
    provider_id: String,
}

pub async fn install_provider(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<InstallResponse>, ApiError> {
    let provider_id = parse_provider_id(id)?;
    let launch = installer_for(&provider_id)?;
    state
        .runtime_manager
        .register_install(&provider_id, launch)
        .await;
    Ok(Json(InstallResponse {
        status: "installed",
        provider_id: provider_id.as_str().to_string(),
    }))
}

/// Alias to `install_provider`: with no versioned release artifact to
/// compare against yet (no registry/publish pipeline exists — see the
/// goal's CI phase), "update" and "install" both mean the same thing today:
/// re-confirm the local vendored copy and (re-)register it.
pub async fn update_provider(
    state: State<AppState>,
    path: Path<String>,
) -> Result<Json<InstallResponse>, ApiError> {
    install_provider(state, path).await
}

pub async fn uninstall_provider(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let provider_id = parse_provider_id(id)?;
    state
        .runtime_manager
        .uninstall(&provider_id)
        .await
        .map_err(runtime_error_response)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn start_provider(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<RuntimeConnectionDescriptor>, ApiError> {
    let provider_id = parse_provider_id(id)?;
    let descriptor = state
        .runtime_manager
        .start(&provider_id)
        .await
        .map_err(runtime_error_response)?;
    Ok(Json(descriptor))
}

pub async fn stop_provider(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let provider_id = parse_provider_id(id)?;
    state
        .runtime_manager
        .stop(&provider_id)
        .await
        .map_err(runtime_error_response)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
pub struct StatusResponse {
    status: RuntimeStatus,
}

pub async fn provider_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<StatusResponse>, ApiError> {
    let provider_id = parse_provider_id(id)?;
    let status = state.runtime_manager.status(&provider_id).await;
    Ok(Json(StatusResponse { status }))
}

#[derive(Deserialize)]
pub struct LogsQuery {
    tail: Option<usize>,
}

pub async fn provider_logs(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<LogsQuery>,
) -> Result<Json<Vec<String>>, ApiError> {
    let provider_id = parse_provider_id(id)?;
    let tail = query.tail.unwrap_or(100);
    Ok(Json(state.runtime_manager.logs(&provider_id, tail).await))
}

pub async fn provider_descriptor(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<RuntimeConnectionDescriptor>, ApiError> {
    let provider_id = parse_provider_id(id)?;
    let descriptor = state
        .runtime_manager
        .descriptor(&provider_id)
        .await
        .map_err(runtime_error_response)?;
    Ok(Json(descriptor))
}

/// Lightweight "still in use" signal a client can send while a session is
/// ongoing, resetting the idle-shutdown clock. `start`/`descriptor` already
/// count as activity on their own; this covers long sessions in between.
pub async fn provider_heartbeat(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let provider_id = parse_provider_id(id)?;
    state
        .runtime_manager
        .touch(&provider_id)
        .await
        .map_err(runtime_error_response)?;
    Ok(StatusCode::NO_CONTENT)
}
