pub mod error;
pub mod routes;
pub mod state;

pub use state::AppState;

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::{
    routing::{delete, get, post},
    Router,
};
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing::info;

use stt_common::ServerConfig;
use stt_runtime::RuntimeManager;

/// How often the idle-shutdown sweep runs. Independent of any one
/// provider's configured idle timeout — this just controls how promptly an
/// expired instance gets noticed and stopped.
const IDLE_SWEEP_INTERVAL: Duration = Duration::from_secs(30);

/// Enforces `Authorization: Bearer <token>` on every request when the
/// config carries an auth token (only possible when `allow_remote` was set —
/// `ServerConfig::validate` rejects non-loopback binding without one). A
/// passthrough no-op in the loopback-default case, so the same router
/// wiring works for both.
async fn require_auth(
    State(state): State<state::AppState>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let Some(expected) = &state.config.auth_token else {
        return Ok(next.run(req).await);
    };

    let provided = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    match provided {
        Some(token) if token == expected => Ok(next.run(req).await),
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

/// Build the application router.
pub fn build_router(state: state::AppState) -> Router {
    Router::new()
        .route("/v1/health", get(routes::health))
        .route("/v1/readiness", get(routes::readiness))
        .route("/v1/hardware", get(routes::get_hardware))
        .route("/v1/providers", get(routes::list_providers))
        .route("/v1/providers/:id/install", post(routes::install_provider))
        .route(
            "/v1/providers/:id/install/:variant",
            delete(routes::uninstall_provider_variant),
        )
        .route("/v1/providers/:id/update", post(routes::update_provider))
        .route("/v1/providers/:id", delete(routes::uninstall_provider))
        .route(
            "/v1/install-operations/:operation_id",
            get(routes::install_operation_status),
        )
        .route("/v1/providers/:id/start", post(routes::start_provider))
        .route("/v1/providers/:id/stop", post(routes::stop_provider))
        .route("/v1/providers/:id/status", get(routes::provider_status))
        .route("/v1/providers/:id/logs", get(routes::provider_logs))
        .route(
            "/v1/providers/:id/descriptor",
            get(routes::provider_descriptor),
        )
        .route(
            "/v1/providers/:id/heartbeat",
            post(routes::provider_heartbeat),
        )
        .route("/v1/models", get(routes::list_models))
        .route("/v1/models/select", post(routes::select_model))
        .route("/v1/models/switch", post(routes::switch_model))
        .route("/v1/models/selected", get(routes::selected_model))
        .route("/v1/models/pull", post(routes::pull_model))
        .route("/v1/models/verify", post(routes::verify_model))
        .route("/v1/models/remove", delete(routes::remove_model))
        .route("/v1/recommendations", get(routes::recommendations))
        .layer(middleware::from_fn_with_state(state.clone(), require_auth))
        .layer(TraceLayer::new_for_http())
        // Outermost layer: browser clients (the App's dev/web build) call
        // this control-plane API from a different origin/port than the one
        // they're served from. Loopback-bound by default and no
        // cookie/credential auth is used (Bearer tokens are sent explicitly
        // by the caller), so an open CORS policy carries no more risk than
        // any other local dev tool listening on loopback.
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any),
        )
        .with_state(state)
}

/// Start the server: binds the HTTP router and, alongside it, an
/// idle-shutdown sweep loop so a managed runtime nobody's using doesn't sit
/// around holding memory/GPU/CPU for no reason.
pub async fn run_server(
    config: ServerConfig,
    runtime_manager: Arc<RuntimeManager>,
) -> anyhow::Result<()> {
    config.validate()?;

    spawn_idle_sweep(runtime_manager.clone());

    let state = state::AppState::new(config.clone(), runtime_manager);
    let app = build_router(state);

    let addr = config.listen_addr();
    info!("Starting stt-server on {addr}");

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

fn spawn_idle_sweep(runtime_manager: Arc<RuntimeManager>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(IDLE_SWEEP_INTERVAL);
        loop {
            interval.tick().await;
            for provider_id in runtime_manager.sweep_idle().await {
                info!(provider = %provider_id, "auto-stopped idle managed runtime");
            }
        }
    });
}
