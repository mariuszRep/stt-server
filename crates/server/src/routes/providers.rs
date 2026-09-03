use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

use stt_common::{ErrorResponse, RuntimeConnectionDescriptor};
use stt_runtime::{
    InstallOperationState, InstallOutcome, ProviderId, ProviderInfo, RuntimeError, RuntimeStatus,
    RuntimeVariant, StartOptions,
};

use crate::error::{runtime_error_response, ApiError};
use crate::state::AppState;

fn parse_provider_id(id: String) -> Result<ProviderId, ApiError> {
    ProviderId::new(id).map_err(runtime_error_response)
}

fn parse_variant(variant: &str) -> Result<RuntimeVariant, ApiError> {
    RuntimeVariant::parse(variant).ok_or_else(|| {
        runtime_error_response(RuntimeError::UnsupportedVariant(variant.to_string()))
    })
}

pub async fn list_providers(State(state): State<AppState>) -> Json<Vec<ProviderInfo>> {
    Json(state.runtime_manager.list_providers())
}

/// `POST /v1/providers/:id/install` with no body (or `{}`, or an explicit
/// `{"variant": null}`) sends no opinion about which variant to install:
/// `RuntimeManager::begin_install` resolves that itself — an
/// already-registered variant wins untouched if one exists, otherwise this
/// machine's own hardware preference. Never hardcode "cpu" here; that was
/// exactly the bug (a caller with no real opinion sent a literal "cpu"
/// default that silently downgraded a correct boot-time GPU registration).
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct InstallProviderRequest {
    variant: Option<String>,
}

#[derive(Serialize)]
#[serde(tag = "status")]
pub enum InstallResponse {
    #[serde(rename = "installed", rename_all = "camelCase")]
    Installed {
        provider_id: String,
        variant: String,
    },
    #[serde(rename = "downloading", rename_all = "camelCase")]
    Downloading {
        provider_id: String,
        variant: String,
        operation_id: String,
    },
}

pub async fn install_provider(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Result<(StatusCode, Json<InstallResponse>), ApiError> {
    let provider_id = parse_provider_id(id)?;
    let request: InstallProviderRequest = if body.is_empty() {
        InstallProviderRequest::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    code: "INVALID_INSTALL_REQUEST".to_string(),
                    message: format!("invalid install request body: {e}"),
                }),
            )
        })?
    };
    let variant = request.variant.as_deref().map(parse_variant).transpose()?;

    let outcome = state
        .runtime_manager
        .begin_install(&provider_id, variant)
        .await
        .map_err(runtime_error_response)?;

    Ok(match outcome {
        InstallOutcome::Installed {
            provider_id,
            variant,
        } => (
            StatusCode::OK,
            Json(InstallResponse::Installed {
                provider_id,
                variant,
            }),
        ),
        InstallOutcome::Downloading {
            operation_id,
            variant,
        } => (
            StatusCode::ACCEPTED,
            Json(InstallResponse::Downloading {
                provider_id: provider_id.to_string(),
                variant,
                operation_id,
            }),
        ),
    })
}

/// Alias to `install_provider`: with no versioned release artifact to
/// compare against yet (no registry/publish pipeline exists — see the
/// goal's CI phase), "update" and "install" both mean the same thing today:
/// re-confirm the local/cached copy (or kick off a download) and (re-)register it.
pub async fn update_provider(
    state: State<AppState>,
    path: Path<String>,
    body: axum::body::Bytes,
) -> Result<(StatusCode, Json<InstallResponse>), ApiError> {
    install_provider(state, path, body).await
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

/// Remove a downloaded variant's cached copy (never a vendored dev copy —
/// see `RuntimeManager::uninstall_variant`), reclaiming disk space without
/// forgetting the provider is installed entirely (that's `uninstall_provider`).
pub async fn uninstall_provider_variant(
    State(state): State<AppState>,
    Path((id, variant)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let provider_id = parse_provider_id(id)?;
    let variant = parse_variant(&variant)?;
    state
        .runtime_manager
        .uninstall_variant(&provider_id, variant)
        .await
        .map_err(runtime_error_response)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Poll an in-flight or finished variant download kicked off by
/// `install_provider`'s `202 Accepted` response.
pub async fn install_operation_status(
    State(state): State<AppState>,
    Path(operation_id): Path<String>,
) -> Result<Json<InstallOperationState>, ApiError> {
    state
        .runtime_manager
        .install_operation(&operation_id)
        .await
        .map(Json)
        .ok_or_else(|| runtime_error_response(RuntimeError::InstallOperationNotFound(operation_id)))
}

/// Optional device/compute_type/bind_host/auth_token hints. `POST
/// /v1/providers/:id/start` with no body (or `{}`) keeps today's
/// loopback/auto-detect behavior; existing callers (the CLI's no-flag case,
/// current tests) send no body at all and must keep working, so this is
/// parsed from raw bytes rather than a required `Json<T>` extractor, which
/// would 400 on an empty body.
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct StartProviderRequest {
    device: Option<String>,
    compute_type: Option<String>,
    bind_host: Option<String>,
    auth_token: Option<String>,
}

pub async fn start_provider(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Json<RuntimeConnectionDescriptor>, ApiError> {
    let provider_id = parse_provider_id(id)?;
    let request: StartProviderRequest = if body.is_empty() {
        StartProviderRequest::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    code: "INVALID_START_REQUEST".to_string(),
                    message: format!("invalid start request body: {e}"),
                }),
            )
        })?
    };

    // Guardrail #2 (defense in depth alongside `RuntimeManager::start`'s
    // own auth_token check): a caller of this admin API can't unilaterally
    // create network exposure the operator never sanctioned when *launching*
    // the control plane itself — even though the admin API stays
    // loopback-only regardless, a caller with access to it shouldn't be able
    // to make the *managed runtime* reachable from the network on a whim.
    if let Some(bind_host) = &request.bind_host {
        if !stt_common::is_loopback_host(bind_host) && !state.config.allow_remote {
            return Err((
                StatusCode::FORBIDDEN,
                Json(ErrorResponse {
                    code: "REMOTE_BIND_NOT_ALLOWED".to_string(),
                    message: "starting a provider on a non-loopback bind_host requires the \
                              control plane itself to have been launched with --allow-remote"
                        .to_string(),
                }),
            ));
        }
    }

    let options = StartOptions {
        device: request.device,
        compute_type: request.compute_type,
        bind_host: request.bind_host,
        auth_token: request.auth_token,
    };
    let descriptor = state
        .runtime_manager
        .start(&provider_id, &options)
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
