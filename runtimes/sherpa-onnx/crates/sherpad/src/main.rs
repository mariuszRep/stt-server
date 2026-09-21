use std::sync::Arc;

use sherpad::{api, build_router, state};

use state::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    // VOICE_TYPER_* is the Local Provider Protocol's env contract, the same
    // one faster-whisper's runtime already implements -- this is what lets
    // stt-server's RuntimeManager control bind address, port, auth, and
    // model directory identically regardless of which engine it's talking
    // to. SHERPAD_* / a hardcoded data dir remain as fallbacks so `sherpad`
    // still runs standalone for local dev (matching README.md's documented
    // faster-whisper venv convention of "works with sensible defaults when
    // not supervised").
    let host = std::env::var("VOICE_TYPER_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let port: u16 = std::env::var("VOICE_TYPER_PORT")
        .ok()
        .or_else(|| std::env::var("SHERPAD_PORT").ok())
        .and_then(|p| p.parse().ok())
        .unwrap_or(7891);
    let auth_token = std::env::var("VOICE_TYPER_AUTH_TOKEN")
        .ok()
        .filter(|t| !t.is_empty());
    let default_model = std::env::var("VOICE_TYPER_MODEL")
        .ok()
        .filter(|m| !m.is_empty());

    // VOICE_TYPER_MODEL_DIR, when set, replaces the whole models root (the
    // directory holding every model this instance can serve as
    // subdirectories) -- not a single model's directory the way
    // faster-whisper's own VOICE_TYPER_MODEL_DIR is, since sherpad's
    // multi-model registry is an additive capability the single-model
    // protocol baseline doesn't have a slot for. The stt-server-side
    // adapter (add-sherpa-onnx-provider) is responsible for pointing this
    // at its own provider-scoped cache root.
    let data_dir = dirs::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("onnx-sherpa");
    let models_dir = std::env::var("VOICE_TYPER_MODEL_DIR")
        .ok()
        .filter(|d| !d.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| data_dir.join("models"));
    let tmp_dir = data_dir.join("tmp");
    std::fs::create_dir_all(&models_dir)?;
    std::fs::create_dir_all(&tmp_dir)?;

    let state = Arc::new(AppState::new(
        models_dir,
        tmp_dir,
        default_model.clone(),
        auth_token,
    ));
    {
        let mut registry = state.registry.write().await;
        for entry in sherpa_manifest::MODELS {
            let dir = state.models_dir.join(entry.id);
            if dir.exists() {
                registry.insert(entry.id.to_string(), state::ModelState::Installed { dir });
            }
        }
    }

    // Eager-load the launched model before serving traffic, so `GET /health`
    // reporting "ok" actually means "ready to transcribe" -- not just "the
    // process is up" -- and the first real request doesn't pay a cold-load
    // cost the control plane's health poll already hid. If it isn't
    // installed yet (control plane's pull contract wasn't honored, or this
    // is a bare standalone run), log and continue rather than failing to
    // start; `GET /health` then answers 503 until the model is loaded, so
    // supervisor::spawn never reports this instance as ready.
    if let Some(model_id) = &default_model {
        match api::load_model_by_id(&state, model_id).await {
            Ok(()) => tracing::info!(model = %model_id, "default model loaded"),
            Err(e) => {
                tracing::warn!(model = %model_id, error = ?e, "default model not ready at startup")
            }
        }
    }

    let app = build_router(state);

    let addr = format!("{host}:{port}");
    tracing::info!(%addr, "sherpad listening");

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
