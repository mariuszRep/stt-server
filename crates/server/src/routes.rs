use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;

use stt_adapter::EngineAdapter;
use stt_common::{
    ErrorResponse, HealthResponse, ModelIdentifier, ModelInfo, ReadinessResponse, SttError,
};

use super::state::AppState;

// ── Health / Readiness ───────────────────────────────────────

pub async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

pub async fn readiness<A: EngineAdapter>(
    State(state): State<AppState<A>>,
) -> Result<Json<ReadinessResponse>, (StatusCode, Json<ErrorResponse>)> {
    let models = state
        .adapter
        .list_models()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse::from(SttError::from(e)))))?;

    Ok(Json(ReadinessResponse {
        ready: true,
        reason: Some(format!("{} models registered", models.len())),
    }))
}

// ── Models ───────────────────────────────────────────────────

pub async fn list_models<A: EngineAdapter>(
    State(state): State<AppState<A>>,
) -> Result<Json<Vec<ModelInfo>>, (StatusCode, Json<ErrorResponse>)> {
    let models = state
        .adapter
        .list_models()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse::from(SttError::from(e)))))?;

    Ok(Json(models))
}

pub async fn get_selected_model<A: EngineAdapter>(
    State(state): State<AppState<A>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let selected = state
        .adapter
        .get_selected_model()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse::from(SttError::from(e)))))?;

    Ok(Json(serde_json::json!({
        "selected_model_id": selected.map(|id| id.to_string()),
    })))
}

#[derive(serde::Deserialize)]
pub struct SelectModelRequest {
    pub model_id: String,
}

pub async fn select_model<A: EngineAdapter>(
    State(state): State<AppState<A>>,
    Json(req): Json<SelectModelRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let model_id = req
        .model_id
        .parse::<uuid::Uuid>()
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    code: "INVALID_MODEL_ID".into(),
                    message: "invalid model ID format".into(),
                }),
            )
        })?;

    let handle = stt_common::ModelId(model_id);

    state
        .adapter
        .select_model(handle)
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, Json(ErrorResponse::from(SttError::from(e)))))?;

    Ok(Json(serde_json::json!({
        "status": "ok",
        "model_id": handle.to_string(),
    })))
}

// ── Batch Transcription ──────────────────────────────────────

#[derive(serde::Deserialize)]
pub struct TranscriptionQuery {
    pub model: Option<String>,
    pub language: Option<String>,
    pub prompt: Option<String>,
    pub temperature: Option<f32>,
}

pub async fn transcribe_batch<A: EngineAdapter>(
    State(state): State<AppState<A>>,
    axum::extract::Query(query): axum::extract::Query<TranscriptionQuery>,
    body: axum::body::Bytes,
) -> Result<Json<stt_common::TranscriptionResult>, (StatusCode, Json<ErrorResponse>)> {
    let audio = stt_common::AudioBuffer::from_wav_bytes(&body).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                code: e.code().to_string(),
                message: e.to_string(),
            }),
        )
    })?;

    let model_id = if let Some(model_name) = &query.model {
        let model_id = ModelIdentifier::new(model_name).map_err(|e| {
            (StatusCode::BAD_REQUEST, Json(ErrorResponse::from(e)))
        })?;

        let models = state
            .adapter
            .list_models()
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse::from(SttError::from(e)))))?;

        let model = models
            .iter()
            .find(|m| m.id == model_id && m.loaded)
            .ok_or_else(|| {
                (
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse {
                        code: "MODEL_NOT_LOADED".into(),
                        message: format!("model '{model_name}' is not loaded"),
                    }),
                )
            })?;

        model.model_id.ok_or_else(|| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    code: "INTERNAL_ERROR".into(),
                    message: "model loaded but no handle".into(),
                }),
            )
        })?
    } else {
        state
            .adapter
            .get_selected_model()
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse::from(SttError::from(e)))))?
            .ok_or_else(|| {
                (
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse {
                        code: "NO_MODEL_SELECTED".into(),
                        message: "no model specified and no default model selected".into(),
                    }),
                )
            })?
    };

    let result = state
        .adapter
        .transcribe_batch(model_id, audio, query.language.as_deref())
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::from(SttError::from(e))),
            )
        })?;

    Ok(Json(result))
}
