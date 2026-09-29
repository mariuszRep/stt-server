use axum::http::{HeaderMap, StatusCode};

use crate::app::App;
use crate::errors::{ApiError, ApiResult};

/// The two access levels a route can require. See
/// `.projectflows/goals/in_progress/install-scope-and-shared-access/GOAL.md`
/// ("Access levels on a shared server") and `docs/client-contract.md`'s
/// route table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessLevel {
    /// Transcribe/translate, read-only catalog/model/status/hardware routes.
    /// The admin token also satisfies this.
    User,
    /// Anything that installs, imports, verifies, changes, or removes a
    /// model or setting, or stops the server. Only the admin token
    /// satisfies this.
    Admin,
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
}

/// Checks the caller's bearer token against `app.token` (admin) and
/// `app.user_token` (user), and enforces `required`.
///
/// - No token, or a token matching neither: `401 unauthorized`.
/// - The user token on an admin-only route: `403 admin_required`.
/// - The admin token always passes, for either level.
pub fn authorize(headers: &HeaderMap, app: &App, required: AccessLevel) -> ApiResult<()> {
    match bearer_token(headers) {
        Some(token) if token == app.token => Ok(()),
        Some(token) if token == app.user_token => match required {
            AccessLevel::User => Ok(()),
            AccessLevel::Admin => Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "admin_required",
                "This route requires the admin token",
            )),
        },
        _ => Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "A valid local bearer token is required",
        )),
    }
}

/// Convenience for the (still common) admin-required case.
pub fn authorized(headers: &HeaderMap, app: &App) -> ApiResult<()> {
    authorize(headers, app, AccessLevel::Admin)
}
