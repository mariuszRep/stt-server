use axum::http::StatusCode;
use axum::Json;

use stt_common::ErrorResponse;
use stt_runtime::RuntimeError;

pub type ApiError = (StatusCode, Json<ErrorResponse>);

/// Map a `RuntimeError` (provider/model/lifecycle failures from
/// `stt-runtime`) to a structured HTTP error response.
pub fn runtime_error_response(err: RuntimeError) -> ApiError {
    let status = match &err {
        RuntimeError::InvalidProviderId(_) | RuntimeError::ModelNotFound(_) => {
            StatusCode::BAD_REQUEST
        }
        RuntimeError::ProviderNotFound(_) => StatusCode::NOT_FOUND,
        RuntimeError::ProviderNotInstalled(_) | RuntimeError::RuntimeNotRunning(_) => {
            StatusCode::CONFLICT
        }
        RuntimeError::RuntimeStartFailed(_) | RuntimeError::Io(_) | RuntimeError::Internal(_) => {
            StatusCode::INTERNAL_SERVER_ERROR
        }
    };
    let code = match &err {
        RuntimeError::InvalidProviderId(_) => "INVALID_PROVIDER_ID",
        RuntimeError::ModelNotFound(_) => "MODEL_NOT_FOUND",
        RuntimeError::ProviderNotFound(_) => "PROVIDER_NOT_FOUND",
        RuntimeError::ProviderNotInstalled(_) => "PROVIDER_NOT_INSTALLED",
        RuntimeError::RuntimeNotRunning(_) => "RUNTIME_NOT_RUNNING",
        RuntimeError::RuntimeStartFailed(_) => "RUNTIME_START_FAILED",
        RuntimeError::Io(_) => "IO_ERROR",
        RuntimeError::Internal(_) => "INTERNAL_ERROR",
    };
    (
        status,
        Json(ErrorResponse {
            code: code.to_string(),
            message: err.to_string(),
        }),
    )
}
