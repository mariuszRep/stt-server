use axum::http::{HeaderMap, StatusCode};

use crate::app::App;
use crate::errors::{ApiError, ApiResult};

pub fn authorized(headers: &HeaderMap, app: &App) -> ApiResult<()> {
    let expected = format!("Bearer {}", app.token);
    if headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        == Some(expected.as_str())
    {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "A valid local bearer token is required",
        ))
    }
}
