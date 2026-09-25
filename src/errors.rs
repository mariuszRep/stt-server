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
}
