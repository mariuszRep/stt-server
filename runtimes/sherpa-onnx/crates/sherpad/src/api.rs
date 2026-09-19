use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Multipart, Path as AxPath, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use serde_json::json;

use crate::recognizer::{self, Job, TranscribeRequest, TranscribeResponse};
use crate::state::{AppState, ModelState};

fn is_cors_preflight(request: &Request) -> bool {
    request.method() == axum::http::Method::OPTIONS
        && request.headers().contains_key(header::ORIGIN)
        && request
            .headers()
            .contains_key(header::ACCESS_CONTROL_REQUEST_METHOD)
}

/// Enforces `Authorization: Bearer <token>` on every real API request when
/// `state.auth_token` is configured. Browser CORS preflights are exempt: an
/// OPTIONS preflight carries no credentials by design and only negotiates
/// whether the subsequent authenticated request may be sent. A pure
/// passthrough when no token is configured (the loopback-default case).
pub async fn require_auth(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    if is_cors_preflight(&request) {
        return next.run(request).await;
    }

    let Some(expected) = &state.auth_token else {
        return next.run(request).await;
    };

    let provided = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));

    if provided == Some(expected.as_str()) {
        next.run(request).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "unauthorized" })),
        )
            .into_response()
    }
}

#[cfg(test)]
mod auth_tests {
    use axum::body::Body;
    use axum::http::{header, Method, Request};

    use super::is_cors_preflight;

    #[test]
    fn browser_cors_preflight_is_recognized_without_authorization() {
        let request = Request::builder()
            .method(Method::OPTIONS)
            .header(header::ORIGIN, "http://tauri.localhost")
            .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
            .body(Body::empty())
            .unwrap();

        assert!(is_cors_preflight(&request));
    }

    #[test]
    fn ordinary_options_request_is_not_treated_as_cors_preflight() {
        let request = Request::builder()
            .method(Method::OPTIONS)
            .body(Body::empty())
            .unwrap();

        assert!(!is_cors_preflight(&request));
    }

    #[test]
    fn authenticated_get_is_still_a_real_api_request() {
        let request = Request::builder()
            .method(Method::GET)
            .header(header::ORIGIN, "http://tauri.localhost")
            .header(header::AUTHORIZATION, "Bearer secret")
            .body(Body::empty())
            .unwrap();

        assert!(!is_cors_preflight(&request));
    }
}

/// Whether the model this instance was launched to serve is in memory. An
/// instance with no default model has nothing to load, so it counts as ready.
async fn default_model_loaded(state: &AppState) -> bool {
    match state.default_model.read().await.as_ref() {
        Some(id) => matches!(
            state.registry.read().await.get(id),
            Some(ModelState::Loaded { .. })
        ),
        None => true,
    }
}

/// `GET /health` -- required by the Local Provider Protocol; polled by
/// `stt-server`'s `supervisor::spawn` to decide the runtime has come up.
/// Answers 503 while the launched model isn't loaded, so "healthy" always
/// means "can transcribe" rather than just "the process is up".
pub async fn health(State(state): State<Arc<AppState>>) -> Response {
    let model = state.default_model.read().await.clone().unwrap_or_default();
    if default_model_loaded(&state).await {
        return Json(json!({ "status": "ok", "model": model })).into_response();
    }
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "status": "model_not_loaded", "model": model })),
    )
        .into_response()
}

/// `GET /v1/config` -- required by the Local Provider Protocol.
/// `RuntimeManager::start` fetches this to populate the descriptor's
/// `streaming` block; sherpad reports no streaming capability today
/// (matches faster-whisper -- real streaming is a separate future goal), so
/// the `streaming` key is simply omitted rather than a hardcoded `false`.
pub async fn config(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let default_model = state.default_model.read().await.clone();
    Json(json!({
        "schema_version": 1,
        "model": default_model.clone().unwrap_or_default(),
        "model_loaded": default_model.is_some() && default_model_loaded(&state).await,
    }))
}

#[derive(Debug)]
pub enum ApiError {
    NotFound(String),
    BadRequest(String),
    Internal(anyhow::Error),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ApiError::NotFound(m) => (StatusCode::NOT_FOUND, m),
            ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            ApiError::Internal(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError::Internal(e)
    }
}

#[derive(Serialize)]
pub struct ModelView {
    id: &'static str,
    description: &'static str,
    languages: &'static [&'static str],
    download_bytes: u64,
    status: &'static str,
    /// The language the loaded recognizer was actually built with, or the
    /// catalog default when not loaded -- lets a caller show the current
    /// state of `set_model_language` without a separate round trip.
    active_language: String,
}

pub async fn list_models(State(state): State<Arc<AppState>>) -> Json<Vec<ModelView>> {
    let registry = state.registry.read().await;
    let views = sherpa_manifest::MODELS
        .iter()
        .map(|m| {
            let (status, active_language) = match registry.get(m.id) {
                Some(ModelState::Loaded { language, .. }) => ("loaded", language.clone()),
                Some(ModelState::Installed { .. }) => ("installed", m.default_language.to_string()),
                None => ("available", m.default_language.to_string()),
            };
            ModelView {
                id: m.id,
                description: m.description,
                languages: m.languages,
                download_bytes: m.download_bytes,
                status,
                active_language,
            }
        })
        .collect();
    Json(views)
}

fn find_entry(id: &str) -> Result<&'static sherpa_manifest::ModelEntry, ApiError> {
    sherpa_manifest::find(id).ok_or_else(|| ApiError::NotFound(format!("unknown model id '{id}'")))
}

pub async fn pull_model(
    State(state): State<Arc<AppState>>,
    AxPath(id): AxPath<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let entry = find_entry(&id)?;

    let models_dir = state.models_dir.clone();
    let dir = tokio::task::spawn_blocking(move || {
        crate::download::download_and_extract(entry, &models_dir)
    })
    .await
    .map_err(|e| ApiError::Internal(anyhow::anyhow!(e)))??;

    state
        .registry
        .write()
        .await
        .insert(id.clone(), ModelState::Installed { dir });

    Ok(Json(json!({ "id": id, "status": "installed" })))
}

pub async fn delete_model(
    State(state): State<Arc<AppState>>,
    AxPath(id): AxPath<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    find_entry(&id)?;
    state.registry.write().await.remove(&id);

    let models_dir = state.models_dir.clone();
    let id_for_removal = id.clone();
    tokio::task::spawn_blocking(move || {
        crate::download::remove_install(&models_dir, &id_for_removal)
    })
    .await
    .map_err(|e| ApiError::Internal(anyhow::anyhow!(e)))??;

    Ok(Json(json!({ "id": id, "status": "available" })))
}

/// Core of `load_model`, factored out so `main`'s startup eager-load (of
/// `VOICE_TYPER_MODEL`, so `GET /health` doesn't report ready until the
/// runtime's actual served model is resident) and the HTTP handler share one
/// implementation rather than two copies that could drift.
pub async fn load_model_by_id(state: &Arc<AppState>, id: &str) -> Result<(), ApiError> {
    let entry = find_entry(id)?;

    let dir = {
        let registry = state.registry.read().await;
        match registry.get(id) {
            Some(ModelState::Loaded { .. }) => return Ok(()),
            Some(ModelState::Installed { dir }) => dir.clone(),
            None => {
                return Err(ApiError::BadRequest(format!(
                    "model '{id}' is not installed; POST /v1/models/{id}/pull first"
                )))
            }
        }
    };

    let num_threads = std::thread::available_parallelism()
        .map(|n| n.get() as i32)
        .unwrap_or(2)
        .min(4);

    let dir_for_build = dir.clone();
    let language = entry.default_language.to_string();
    let language_for_build = language.clone();
    let recognizer = tokio::task::spawn_blocking(move || {
        let config = recognizer::build_config(entry, &dir_for_build, num_threads, &language_for_build);
        sherpa_onnx::OfflineRecognizer::create(&config)
    })
    .await
    .map_err(|e| ApiError::Internal(anyhow::anyhow!(e)))?
    .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("failed to create recognizer for {id}")))?;

    let jobs = recognizer::spawn_worker(recognizer);
    state
        .registry
        .write()
        .await
        .insert(id.to_string(), ModelState::Loaded { dir, jobs, language });
    Ok(())
}

pub async fn load_model(
    State(state): State<Arc<AppState>>,
    AxPath(id): AxPath<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    load_model_by_id(&state, &id).await?;
    Ok(Json(json!({ "id": id, "status": "loaded" })))
}

pub async fn unload_model(
    State(state): State<Arc<AppState>>,
    AxPath(id): AxPath<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Parity with faster-whisper's equivalent guard (added same day, see
    // concurrent-multi-provider-serving): refuse to unload the model a
    // model-omitted request currently resolves to, so that path is never
    // left with nothing to serve out from under it.
    if state.default_model.read().await.as_deref() == Some(id.as_str()) {
        return Err(ApiError::BadRequest(format!(
            "cannot unload '{id}': it is this instance's current default model"
        )));
    }

    let dir = {
        let registry = state.registry.read().await;
        match registry.get(&id) {
            Some(ModelState::Loaded { dir, .. }) => dir.clone(),
            Some(ModelState::Installed { dir }) => {
                return Ok(Json(json!({ "id": id, "status": "installed", "dir": dir })))
            }
            None => return Err(ApiError::NotFound(format!("model '{id}' is not installed"))),
        }
    };
    state
        .registry
        .write()
        .await
        .insert(id.clone(), ModelState::Installed { dir });

    Ok(Json(json!({ "id": id, "status": "installed" })))
}

pub async fn status(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let registry = state.registry.read().await;
    let loaded: Vec<&String> = registry
        .iter()
        .filter(|(_, s)| matches!(s, ModelState::Loaded { .. }))
        .map(|(id, _)| id)
        .collect();
    Json(json!({
        "models_dir": state.models_dir,
        "loaded": loaded,
    }))
}

async fn get_worker(
    state: &AppState,
    id: &str,
) -> Result<tokio::sync::mpsc::Sender<Job>, ApiError> {
    let entry = find_entry(id)?;

    {
        let registry = state.registry.read().await;
        if let Some(ModelState::Loaded { jobs, .. }) = registry.get(id) {
            return Ok(jobs.clone());
        }
    }

    // lazy-load: installed but not yet loaded into memory
    let dir = {
        let registry = state.registry.read().await;
        match registry.get(id) {
            Some(ModelState::Installed { dir }) => dir.clone(),
            _ => {
                return Err(ApiError::BadRequest(format!(
                    "model '{id}' is not installed; POST /v1/models/{id}/pull first"
                )))
            }
        }
    };

    let num_threads = std::thread::available_parallelism()
        .map(|n| n.get() as i32)
        .unwrap_or(2)
        .min(4);
    let dir_for_build = dir.clone();
    let language = entry.default_language.to_string();
    let language_for_build = language.clone();
    let recognizer = tokio::task::spawn_blocking(move || {
        let config = recognizer::build_config(entry, &dir_for_build, num_threads, &language_for_build);
        sherpa_onnx::OfflineRecognizer::create(&config)
    })
    .await
    .map_err(|e| ApiError::Internal(anyhow::anyhow!(e)))?
    .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("failed to create recognizer for {id}")))?;

    let jobs = recognizer::spawn_worker(recognizer);
    state.registry.write().await.insert(
        id.to_string(),
        ModelState::Loaded {
            dir,
            jobs: jobs.clone(),
            language,
        },
    );

    Ok(jobs)
}

#[derive(serde::Deserialize)]
pub struct SetLanguageRequest {
    language: String,
}

#[derive(Serialize)]
#[serde(tag = "status")]
pub enum SetLanguageResponse {
    /// Already serving the requested language -- no rebuild needed.
    #[serde(rename = "unchanged")]
    Unchanged,
    /// Rebuilt the recognizer for the new language.
    #[serde(rename = "reloaded", rename_all = "camelCase")]
    Reloaded { load_seconds: f64 },
}

/// `POST /v1/models/:id/language` -- unlike faster-whisper's per-request
/// `language` form field, sherpa-onnx has no such request-time hook: a
/// model's language is baked into the recognizer at build time. Changing it
/// means rebuilding and replacing the loaded recognizer, which is why this
/// is its own explicit reload operation (with a measured `load_seconds`,
/// mirroring `stt-server`'s own model-switch response) rather than a field
/// on the transcribe request.
pub async fn set_model_language(
    State(state): State<Arc<AppState>>,
    AxPath(id): AxPath<String>,
    Json(req): Json<SetLanguageRequest>,
) -> Result<Json<SetLanguageResponse>, ApiError> {
    let entry = find_entry(&id)?;
    // Not `.contains(&req.language.as_str())`: that requires unifying
    // `req.language`'s borrow with `languages`' `&'static str` element type,
    // which a request-scoped `String` can never satisfy. Compare values
    // instead of trying to match reference lifetimes.
    if !entry.languages.iter().any(|&lang| lang == req.language) {
        return Err(ApiError::BadRequest(format!(
            "model '{id}' does not support language '{}': supported are {:?}",
            req.language, entry.languages
        )));
    }

    let dir = {
        let registry = state.registry.read().await;
        match registry.get(&id) {
            Some(ModelState::Loaded { language, .. }) if *language == req.language => {
                return Ok(Json(SetLanguageResponse::Unchanged));
            }
            Some(ModelState::Loaded { dir, .. }) => dir.clone(),
            Some(ModelState::Installed { dir }) => dir.clone(),
            None => {
                return Err(ApiError::BadRequest(format!(
                    "model '{id}' is not installed; POST /v1/models/{id}/pull first"
                )))
            }
        }
    };

    let num_threads = std::thread::available_parallelism()
        .map(|n| n.get() as i32)
        .unwrap_or(2)
        .min(4);
    let dir_for_build = dir.clone();
    let language_for_build = req.language.clone();
    let started = std::time::Instant::now();
    let recognizer = tokio::task::spawn_blocking(move || {
        let config = recognizer::build_config(entry, &dir_for_build, num_threads, &language_for_build);
        sherpa_onnx::OfflineRecognizer::create(&config)
    })
    .await
    .map_err(|e| ApiError::Internal(anyhow::anyhow!(e)))?
    .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("failed to create recognizer for {id}")))?;
    let load_seconds = started.elapsed().as_secs_f64();

    let jobs = recognizer::spawn_worker(recognizer);
    state.registry.write().await.insert(
        id.clone(),
        ModelState::Loaded {
            dir,
            jobs,
            language: req.language,
        },
    );

    Ok(Json(SetLanguageResponse::Reloaded { load_seconds }))
}

#[derive(serde::Deserialize)]
pub struct AdminModelBody {
    model: String,
    // `device`/`compute_type` are accepted for wire-shape parity with
    // faster-whisper's identically-named endpoint (`stt-server`'s
    // `RuntimeManager::switch_model` posts the same body to every provider
    // uniformly) but sherpad links a CPU-only onnxruntime build with no
    // per-model device/compute_type concept, so both are silently ignored.
    #[allow(dead_code)]
    device: Option<String>,
    #[allow(dead_code)]
    compute_type: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminModelResponse {
    status: &'static str,
    model: String,
    load_seconds: Option<f64>,
}

/// `POST /v1/admin/model` -- sherpad's side of the in-process model-swap
/// contract `stt-server`'s `RuntimeManager::switch_model` already calls
/// uniformly for every provider (see `manager.rs`'s doc comment on that
/// method); only faster-whisper's Python runtime implemented it until now,
/// so switching sherpa-onnx's active model on an already-running instance
/// always 404'd. Ensures `model` is loaded (reusing the same lazy-load path
/// `get_worker` uses for a transcribe request), then makes it the default
/// model every request-without-an-explicit-`model` resolves to.
pub async fn admin_switch_model(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AdminModelBody>,
) -> Result<Json<AdminModelResponse>, ApiError> {
    let already_loaded = matches!(
        state.registry.read().await.get(&req.model),
        Some(ModelState::Loaded { .. })
    );
    let load_seconds = if already_loaded {
        None
    } else {
        let started = std::time::Instant::now();
        get_worker(&state, &req.model).await?;
        Some(started.elapsed().as_secs_f64())
    };

    *state.default_model.write().await = Some(req.model.clone());

    Ok(Json(AdminModelResponse {
        status: "ok",
        model: req.model,
        load_seconds,
    }))
}

#[derive(Default)]
struct TranscribeParams {
    model: Option<String>,
    response_format: String,
    want_word_timestamps: bool,
}

/// Shortest audio sent to the model; see the guard in [`transcribe`].
pub const MIN_AUDIO_SECS: f64 = 0.25;

pub async fn transcribe(
    State(state): State<Arc<AppState>>,
    mut multipart: Multipart,
) -> Result<Response, ApiError> {
    let mut wav_bytes: Option<Vec<u8>> = None;
    let mut params = TranscribeParams {
        response_format: "json".to_string(),
        ..Default::default()
    };

    while let Some(field) = multipart.next_field().await.map_err(|e| {
        // `MultipartError`'s `Display` is the same fixed string
        // ("Error parsing `multipart/form-data` request") for every failure
        // mode -- body-limit overflow, bad boundary, truncated stream. The
        // actual cause only shows up via `source()`, so surface both.
        use std::error::Error as _;
        ApiError::BadRequest(match e.source() {
            Some(source) => format!("{e}: {source}"),
            None => e.to_string(),
        })
    })? {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "file" => {
                wav_bytes = Some(
                    field
                        .bytes()
                        .await
                        .map_err(|e| ApiError::BadRequest(e.to_string()))?
                        .to_vec(),
                );
            }
            "model" => {
                params.model = Some(field.text().await.unwrap_or_default());
            }
            "response_format" => {
                params.response_format = field.text().await.unwrap_or_else(|_| "json".to_string());
            }
            "timestamp_granularities[]" if field.text().await.unwrap_or_default() == "word" => {
                params.want_word_timestamps = true;
            }
            _ => {}
        }
    }

    let wav_bytes = wav_bytes.ok_or_else(|| ApiError::BadRequest("missing 'file' field".into()))?;
    // The SDK never sends `model` -- a managed runtime serves the single
    // model it was launched with (`VOICE_TYPER_MODEL`, captured as
    // `state.default_model`). An explicit `model` field is still honored as
    // an optional per-request override, for direct/standalone callers.
    let model_id = match params.model.clone() {
        Some(model) => Some(model),
        None => state.default_model.read().await.clone(),
    }
        .ok_or_else(|| {
            ApiError::BadRequest(
                "no 'model' field given and this instance has no default model configured".into(),
            )
        })?;

    let tmp_path = state.tmp_dir.join(format!("{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir_all(&state.tmp_dir)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;
    tokio::fs::write(&tmp_path, &wav_bytes)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;

    let read_path = tmp_path.clone();
    // `Wave` holds a raw pointer and isn't `Send`, so pull the (Send-able)
    // sample data out of it inside the same blocking closure. Try the fast
    // native WAV path first (unchanged behavior for the common case); fall
    // back to symphonia's general decoder for anything else (webm/opus from
    // the app's MediaRecorder fallback, ogg/vorbis, etc) so a caller never
    // has to know or care what format it sent.
    let wav_data = tokio::task::spawn_blocking(move || {
        sherpa_onnx::Wave::read(&read_path.to_string_lossy())
            .map(|w| (w.sample_rate(), w.samples().to_vec()))
    })
    .await
    .map_err(|e| ApiError::Internal(anyhow::anyhow!(e)))?;
    let _ = tokio::fs::remove_file(&tmp_path).await;

    let (sample_rate, samples) = match wav_data {
        Some(data) => data,
        None => tokio::task::spawn_blocking(move || crate::decode::decode_to_mono_f32(wav_bytes))
            .await
            .map_err(|e| ApiError::Internal(anyhow::anyhow!(e)))?
            .map_err(|e| ApiError::BadRequest(format!("could not decode 'file': {e}")))?,
    };
    if sample_rate <= 0 {
        return Err(ApiError::BadRequest(
            "could not decode 'file': invalid sample rate".into(),
        ));
    }
    let duration_secs = samples.len() as f64 / sample_rate as f64;

    // Too little audio yields zero encoder frames, and ONNX Runtime then throws
    // a C++ exception ("Invalid input shape: {0,128}") that Rust cannot catch:
    // the whole process aborts, taking every other queued request with it.
    // Nothing that short holds a word, so answer with an empty transcript.
    if duration_secs < MIN_AUDIO_SECS {
        let language = active_language(&state, &model_id).await?;
        return build_response(
            &params,
            &language,
            TranscribeResponse::default(),
            duration_secs,
        );
    }

    let jobs = get_worker(&state, &model_id).await?;
    let language = active_language(&state, &model_id).await?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    jobs.send(Job {
        request: TranscribeRequest {
            samples,
            sample_rate,
        },
        respond_to: tx,
    })
    .await
    .map_err(|_| {
        ApiError::Internal(anyhow::anyhow!(
            "model worker for {model_id} is not running"
        ))
    })?;

    let result: TranscribeResponse = tokio::time::timeout(Duration::from_secs(120), rx)
        .await
        .map_err(|_| ApiError::Internal(anyhow::anyhow!("transcription timed out")))?
        .map_err(|_| ApiError::Internal(anyhow::anyhow!("model worker dropped the request")))?;

    build_response(&params, &language, result, duration_secs)
}

/// The language currently baked into `model_id`'s loaded recognizer, or its
/// catalog default when not loaded yet -- matches what `get_worker`'s
/// lazy-load path would build it with, so a caller who never touched
/// `set_model_language` still sees today's exact reported value.
async fn active_language(state: &AppState, model_id: &str) -> Result<String, ApiError> {
    if let Some(ModelState::Loaded { language, .. }) = state.registry.read().await.get(model_id) {
        return Ok(language.clone());
    }
    let entry = find_entry(model_id)?;
    Ok(entry.default_language.to_string())
}

/// Builds the response. `text` format returns bare text (a convenience
/// beyond the protocol baseline, kept as-is); every other format
/// (`json`/`verbose_json`/unset) returns the *same* full protocol shape --
/// faster-whisper doesn't distinguish `response_format` either, it always
/// returns `{text, language, duration, segments}` unconditionally, so
/// matching that here is what makes the two engines indistinguishable at
/// the wire level (the entire point of `make-sherpad-protocol-conformant`).
///
/// `avg_logprob`/`no_speech_prob`/`compression_ratio` are Whisper-decoder
/// concepts sherpa-onnx's `OfflineRecognizerResult` doesn't expose for any
/// model family here (transducer models like Parakeet have no natural
/// equivalent) -- deliberately omitted (`null`) rather than fabricated; the
/// protocol marks them optional for exactly this reason.
fn build_response(
    params: &TranscribeParams,
    active_language: &str,
    result: TranscribeResponse,
    duration_secs: f64,
) -> Result<Response, ApiError> {
    if params.response_format == "text" {
        return Ok(result.text.into_response());
    }

    let language = if active_language == "auto" {
        serde_json::Value::Null
    } else {
        serde_json::Value::String(active_language.to_string())
    };

    let words = if params.want_word_timestamps {
        match (&result.timestamps, &result.durations) {
            (Some(ts), Some(du)) if ts.len() == result.tokens.len() => {
                let words: Vec<_> = result
                    .tokens
                    .iter()
                    .zip(ts.iter())
                    .zip(du.iter())
                    .map(|((tok, start), dur)| {
                        json!({ "word": tok, "start": start, "end": start + dur })
                    })
                    .collect();
                serde_json::Value::Array(words)
            }
            _ => serde_json::Value::Array(vec![]),
        }
    } else {
        serde_json::Value::Null
    };

    let segments = json!([{
        "text": result.text,
        "start": 0.0,
        "end": duration_secs,
        "avg_logprob": serde_json::Value::Null,
        "no_speech_prob": serde_json::Value::Null,
        "compression_ratio": serde_json::Value::Null,
        "words": words,
    }]);

    Ok(Json(json!({
        "text": result.text,
        "language": language,
        "duration": duration_secs,
        "segments": segments,
    }))
    .into_response())
}
