pub mod routes;
pub mod ws;
pub mod state;
pub mod error;

pub use state::AppState;

use axum::{routing::get, routing::post, Router};
use tower_http::trace::TraceLayer;
use tracing::info;

use stt_adapter::EngineAdapter;
use stt_common::ServerConfig;

/// Build the application router.
pub fn build_router<A: EngineAdapter + 'static>(state: state::AppState<A>) -> Router {
    Router::new()
        .route("/v1/health", get(routes::health))
        .route("/v1/readiness", get(routes::readiness))
        .route("/v1/models", get(routes::list_models))
        .route("/v1/models/selected", get(routes::get_selected_model))
        .route("/v1/models/select", post(routes::select_model))
        .route("/v1/transcriptions", post(routes::transcribe_batch))
        .route("/v1/realtime/transcriptions", get(ws::ws_handler))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Start the server.
pub async fn run_server<A: EngineAdapter + 'static>(
    config: ServerConfig,
    adapter: A,
) -> anyhow::Result<()> {
    config.validate()?;

    let state = state::AppState::new(adapter, config.clone());
    let app = build_router(state);

    let addr = config.listen_addr();
    info!("Starting stt-server on {addr}");

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
