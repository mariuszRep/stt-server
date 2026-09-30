use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    /// Additive, optional error detail (e.g. an in-progress operation id);
    /// only serialized into the response when present.
    pub details: Option<Value>,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            details: None,
        }
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut error = json!({"code": self.code, "message": self.message});
        if let Some(details) = self.details {
            error["details"] = details;
        }
        (self.status, Json(json!({"error": error}))).into_response()
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

pub fn internal(error: impl std::fmt::Display) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal_error",
        error.to_string(),
    )
}

/// The outcome of a `Session::run` call, decided purely from the
/// `transcribe_cpp::Result` it returned. Separated from `map_engine_error` so
/// `OutputTruncated`'s "200 with the partial transcript" behaviour (not an
/// error response at all) stays out of the error-mapping table.
pub enum RunOutcome {
    /// The run completed; format and return its transcript normally.
    Success(transcribe_cpp::Transcript),
    /// The decode hit the generation budget before end-of-stream. Per the
    /// decided design this is NOT an error response: format the partial
    /// transcript as a normal 200 with `x_diagnostics.truncated: true`.
    Truncated(transcribe_cpp::Transcript),
    /// Every other outcome, already mapped to the response it should produce.
    Failed(ApiError),
}

pub fn classify_run_result(
    result: transcribe_cpp::Result<transcribe_cpp::Transcript>,
) -> RunOutcome {
    match result {
        Ok(transcript) => RunOutcome::Success(transcript),
        Err(transcribe_cpp::Error::OutputTruncated { partial, .. }) => {
            RunOutcome::Truncated(partial.map(|boxed| *boxed).unwrap_or_default())
        }
        Err(other) => RunOutcome::Failed(map_engine_error(other)),
    }
}

/// Map a `transcribe_cpp::Error` (everything except `OutputTruncated`, which
/// `classify_run_result` handles separately) to the `ApiError` it produces.
/// See `parity-design.md` "Errors":
/// - `InvalidArgument` -> 422 `engine_rejected_option` (engine message).
/// - `Unsupported` -> 422 `engine_unsupported`.
/// - `InputTooLong` -> 413 `audio_too_long`.
/// - `Aborted` -> current cancel behaviour (no special-casing beyond mapping
///   to the same internal error this code already produced before this
///   change; a client-disconnect abort never actually reaches the caller
///   since the connection is already gone by the time this would be sent).
/// - `Busy` -> 503 `engine_busy`.
/// - `OutOfMemory` -> 507 `insufficient_memory`.
/// - anything else -> 500 `inference_failed`.
pub fn map_engine_error(error: transcribe_cpp::Error) -> ApiError {
    use transcribe_cpp::Error;
    match error {
        Error::InvalidArgument(message) => ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "engine_rejected_option",
            message,
        ),
        Error::Unsupported(message) => ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "engine_unsupported",
            message,
        ),
        Error::InputTooLong(message) => {
            ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "audio_too_long", message)
        }
        Error::Aborted { message, .. } => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "inference_failed",
            message,
        ),
        Error::Busy(message) => {
            ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "engine_busy", message)
        }
        Error::OutOfMemory(message) => ApiError::new(
            StatusCode::INSUFFICIENT_STORAGE,
            "insufficient_memory",
            message,
        ),
        other => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "inference_failed",
            other.to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    #[tokio::test]
    async fn details_are_omitted_when_absent() {
        let error = ApiError::new(StatusCode::BAD_REQUEST, "bad", "nope");
        let response = error.into_response();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(body["error"].get("details").is_none());
        assert_eq!(body["error"]["code"], "bad");
    }

    #[tokio::test]
    async fn details_are_present_when_set() {
        let error = ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "not_ready", "no model")
            .with_details(json!({"operation_id": "abc-123"}));
        let response = error.into_response();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["details"]["operation_id"], "abc-123");
    }

    #[test]
    fn engine_error_mapping_table() {
        use transcribe_cpp::Error;

        let cases: Vec<(Error, StatusCode, &str)> = vec![
            (
                Error::InvalidArgument("bad arg".to_string()),
                StatusCode::UNPROCESSABLE_ENTITY,
                "engine_rejected_option",
            ),
            (
                Error::Unsupported("nope".to_string()),
                StatusCode::UNPROCESSABLE_ENTITY,
                "engine_unsupported",
            ),
            (
                Error::InputTooLong("too long".to_string()),
                StatusCode::PAYLOAD_TOO_LARGE,
                "audio_too_long",
            ),
            (
                Error::Aborted {
                    message: "aborted".to_string(),
                    partial: None,
                },
                StatusCode::INTERNAL_SERVER_ERROR,
                "inference_failed",
            ),
            (
                Error::Busy("busy".to_string()),
                StatusCode::SERVICE_UNAVAILABLE,
                "engine_busy",
            ),
            (
                Error::OutOfMemory("oom".to_string()),
                StatusCode::INSUFFICIENT_STORAGE,
                "insufficient_memory",
            ),
            (
                Error::Other("weird".to_string()),
                StatusCode::INTERNAL_SERVER_ERROR,
                "inference_failed",
            ),
        ];
        for (error, status, code) in cases {
            let mapped = map_engine_error(error);
            assert_eq!(mapped.status, status);
            assert_eq!(mapped.code, code);
        }
    }

    #[test]
    fn output_truncated_classifies_as_truncated_not_failed() {
        let partial = transcribe_cpp::Transcript {
            text: "partial text".to_string(),
            ..Default::default()
        };
        let result: transcribe_cpp::Result<transcribe_cpp::Transcript> =
            Err(transcribe_cpp::Error::OutputTruncated {
                message: "truncated".to_string(),
                partial: Some(Box::new(partial)),
            });
        match classify_run_result(result) {
            RunOutcome::Truncated(transcript) => assert_eq!(transcript.text, "partial text"),
            _ => panic!("expected Truncated"),
        }
    }

    #[test]
    fn success_classifies_as_success() {
        let result: transcribe_cpp::Result<transcribe_cpp::Transcript> =
            Ok(transcribe_cpp::Transcript::default());
        assert!(matches!(
            classify_run_result(result),
            RunOutcome::Success(_)
        ));
    }

    #[test]
    fn other_errors_classify_as_failed() {
        let result: transcribe_cpp::Result<transcribe_cpp::Transcript> =
            Err(transcribe_cpp::Error::Busy("busy".to_string()));
        match classify_run_result(result) {
            RunOutcome::Failed(err) => assert_eq!(err.code, "engine_busy"),
            _ => panic!("expected Failed"),
        }
    }
}
