use std::error::Error;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    extract::{DefaultBodyLimit, Multipart, Path as UrlPath, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use rusqlite::params;
use serde::Deserialize;
use serde_json::{json, Value};
use tower_http::cors::{AllowOrigin, CorsLayer};
use transcribe_cpp::CancelToken;
use uuid::Uuid;

use crate::app::{open_app, App};
use crate::audio::decode_wav;
use crate::auth::authorized;
use crate::capabilities::catalog_mismatch;
use crate::catalog::{capability_matrix, catalog_model, model_view};
use crate::download::install_model;
use crate::engine::{load_engine, CancelWhenDropped, LoadedModel};
use crate::errors::{classify_run_result, internal, ApiError, ApiResult, RunOutcome};
use crate::format::{format_response, DiagnosticsExtra, Formatted, LanguageEvidence};
use crate::import::import_model;
use crate::operations::{cancel_operation, operation};
use crate::run_plan::{plan as build_plan, Endpoint, ParsedRequest};
use crate::store::{
    backend_preference, cors_allowed_origins, installed_file, installed_path, is_valid_cors_origin,
    selected_id,
};
use crate::verify::verify_model;

pub async fn get_config(State(app): State<Arc<App>>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    authorized(&headers, &app)?;
    Ok(Json(json!({
        "bind":"127.0.0.1:54321",
        "preferred_backend":backend_preference(&app)?,
        "max_audio_bytes":40 * 1024 * 1024,
        "streaming":false,
        "cors_allowed_origins":cors_allowed_origins(&app)?
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigPatch {
    preferred_backend: Option<String>,
    cors_allowed_origins: Option<Vec<String>>,
}

pub async fn patch_config(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(patch): Json<ConfigPatch>,
) -> ApiResult<Json<Value>> {
    authorized(&headers, &app)?;
    let mut response = serde_json::Map::new();
    let mut restart_required = false;
    if let Some(backend) = &patch.preferred_backend {
        if !matches!(backend.as_str(), "auto" | "cpu" | "vulkan") {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_backend",
                "preferred_backend must be auto, cpu, or vulkan",
            ));
        }
    }
    if let Some(origins) = &patch.cors_allowed_origins {
        if origins.is_empty() {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_cors_origins",
                "cors_allowed_origins must contain at least one entry",
            ));
        }
        for origin in origins {
            if !is_valid_cors_origin(origin) {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "invalid_cors_origins",
                    format!("'{origin}' is not '*' or an http(s)://host[:port] origin"),
                ));
            }
        }
    }
    {
        let db = app.db.lock().map_err(internal)?;
        if let Some(backend) = &patch.preferred_backend {
            db.execute(
                "INSERT INTO settings(key,value) VALUES('preferred_backend',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![backend],
            )
            .map_err(internal)?;
            response.insert("preferred_backend".to_owned(), json!(backend));
            response.insert("reload_required".to_owned(), json!(true));
        }
        if let Some(origins) = &patch.cors_allowed_origins {
            let serialized = serde_json::to_string(origins).map_err(internal)?;
            db.execute(
                "INSERT INTO settings(key,value) VALUES('cors_allowed_origins',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![serialized],
            )
            .map_err(internal)?;
            response.insert("cors_allowed_origins".to_owned(), json!(origins));
            restart_required = true;
        }
    }
    response.insert("restart_required".to_owned(), json!(restart_required));
    Ok(Json(Value::Object(response)))
}

/// Build the CORS layer from the persisted `cors_allowed_origins` setting
/// (default `["*"]`). Applied at router construction time; a change via
/// `PATCH /v1/local/config` takes effect on the next server start.
fn cors_layer(origins: &[String]) -> CorsLayer {
    let allow_origin = if origins.iter().any(|origin| origin == "*") {
        AllowOrigin::any()
    } else {
        let parsed: Vec<axum::http::HeaderValue> = origins
            .iter()
            .filter_map(|origin| origin.parse().ok())
            .collect();
        AllowOrigin::list(parsed)
    };
    CorsLayer::new()
        .allow_origin(allow_origin)
        .allow_methods(tower_http::cors::Any)
        .allow_headers([
            axum::http::header::AUTHORIZATION,
            axum::http::header::CONTENT_TYPE,
        ])
}

pub async fn health() -> Json<Value> {
    Json(json!({"status": "ok"}))
}

pub async fn readiness(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
) -> ApiResult<impl IntoResponse> {
    authorized(&headers, &app)?;
    let loaded = app.loaded.lock().map_err(internal)?;
    match loaded.as_ref() {
        Some(active) => Ok((
            StatusCode::OK,
            Json(json!({"status":"ready", "model":active.id, "backend":active.diagnostic})),
        )),
        None => Ok((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"status":"not_ready", "reason":"No selected model loaded"})),
        )),
    }
}

pub async fn recommendations(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    authorized(&headers, &app)?;
    let mut models: Vec<_> = app
        .catalog
        .iter()
        .filter(|model| model.recommended)
        .collect();
    models.sort_by_key(|model| model.recommended_rank.unwrap_or(u32::MAX));
    let data = models
        .into_iter()
        .map(|model| {
            let installed = installed_file(&app, &model.slug)?;
            Ok(model_view(
                model,
                installed.and_then(|file| file.quant).as_deref(),
            ))
        })
        .collect::<ApiResult<Vec<_>>>()?;
    Ok(Json(json!({"object":"list", "data":data})))
}

/// The effective capability view (plus `catalog_mismatch`) for the currently
/// loaded model, if `id` names it; `None` otherwise (unloaded, or a different
/// model is loaded), letting the caller fall back to the catalog view.
fn effective_view_if_loaded(app: &App, id: &str) -> ApiResult<Option<Value>> {
    let loaded = app.loaded.lock().map_err(internal)?;
    Ok(loaded
        .as_ref()
        .filter(|active| active.id == id)
        .map(|active| {
            let mut view = active.caps.to_json();
            let model = app.catalog.iter().find(|model| model.slug == id);
            if let Some(model) = model {
                view["catalog_mismatch"] = json!(catalog_mismatch(model, &active.caps));
            }
            view
        }))
}

pub async fn local_models(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    authorized(&headers, &app)?;
    let data = app
        .catalog
        .iter()
        .map(|model| {
            let installed = installed_file(&app, &model.slug)?;
            let mut view = model_view(model, installed.and_then(|file| file.quant).as_deref());
            if let Some(effective) = effective_view_if_loaded(&app, &model.slug)? {
                view["effective_capabilities"] = effective;
            }
            Ok(view)
        })
        .collect::<ApiResult<Vec<_>>>()?;
    Ok(Json(json!({"object":"list", "data":data})))
}

pub async fn openai_models(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    authorized(&headers, &app)?;
    let mut data = Vec::new();
    for model in &app.catalog {
        if installed_path(&app, &model.slug)?.is_some() {
            data.push(json!({"id":model.slug,"object":"model","owned_by":"local"}));
        }
    }
    Ok(Json(json!({"object":"list", "data":data})))
}

pub async fn selected_model(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    authorized(&headers, &app)?;
    let Some(id) = selected_id(&app)? else {
        return Ok(Json(json!({"model":null, "effective_capabilities":null})));
    };
    let model = catalog_model(&app, &id)?;
    let diagnostic = {
        let loaded = app.loaded.lock().map_err(internal)?;
        loaded
            .as_ref()
            .filter(|active| active.id == id)
            .map(|active| active.diagnostic.clone())
    };
    let effective_capabilities = match effective_view_if_loaded(&app, &id)? {
        Some(effective) => effective,
        None => capability_matrix(model),
    };
    Ok(Json(json!({
        "model":id,
        "effective_capabilities":effective_capabilities,
        "backend":diagnostic
    })))
}

/// How long `deselect_model` waits for an in-flight transcription to finish
/// before giving up with `model_in_use`.
const DESELECT_WAIT_TIMEOUT: Duration = Duration::from_secs(30);

pub async fn deselect_model(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    authorized(&headers, &app)?;
    let _selection = app.selection.lock().await;
    let _inference = tokio::time::timeout(
        DESELECT_WAIT_TIMEOUT,
        app.inference.semaphore().acquire_owned(),
    )
    .await
    .map_err(|_| {
        ApiError::new(
            StatusCode::CONFLICT,
            "model_in_use",
            "Inference did not finish before the unload timeout",
        )
    })?
    .map_err(internal)?;
    {
        let db = app.db.lock().map_err(internal)?;
        db.execute("DELETE FROM settings WHERE key='selected_model'", [])
            .map_err(internal)?;
    }
    *app.loaded.lock().map_err(internal)? = None;
    Ok(Json(json!({"model":null,"loaded":false})))
}

pub async fn select_model(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    authorized(&headers, &app)?;
    // The selection lock only serializes concurrent selections; it is never
    // held across the (possibly slow) model load, and the inference permit
    // is never taken here at all. The old model, still referenced by
    // `app.loaded`, keeps serving in-flight and newly queued transcriptions
    // while the new one loads in a blocking thread. Only the brief swap of
    // `app.loaded` below is exclusive, so dictation is never blocked for the
    // duration of a model load. Note: while both are resident (between the
    // new model finishing its load and the swap), memory usage is briefly
    // the sum of both models -- acceptable per README.
    let _selection = app.selection.lock().await;
    catalog_model(&app, &id)?;
    let path = installed_path(&app, &id)?.ok_or_else(|| {
        ApiError::new(
            StatusCode::CONFLICT,
            "model_not_installed",
            "Install the model first",
        )
    })?;
    let preference = backend_preference(&app)?;
    let loader_id = id.clone();
    // Load off the async runtime, then swap the whole `LoadedModel` in one
    // lock (the same shape as `engine::load_and_swap`, tested generically
    // there with a fake loader). The old model is never locked out and keeps
    // serving until this swap.
    let (model, diagnostic) = tokio::task::spawn_blocking(move || load_engine(&path, &preference))
        .await
        .map_err(internal)?
        .map_err(|error| ApiError::new(StatusCode::CONFLICT, "model_load_failed", error))?;
    {
        let db = app.db.lock().map_err(internal)?;
        db.execute(
            "INSERT INTO settings(key,value) VALUES('selected_model',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![id],
        )
        .map_err(internal)?;
    }
    *app.loaded.lock().map_err(internal)? =
        Some(LoadedModel::new(loader_id, model, diagnostic.clone()));
    Ok(Json(json!({"model":id,"backend":diagnostic})))
}

pub async fn remove_model(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    authorized(&headers, &app)?;
    let _selection = app.selection.lock().await;
    catalog_model(&app, &id)?;
    {
        let db = app.db.lock().map_err(internal)?;
        let active: i64 = db
            .query_row(
                "SELECT count(*) FROM operations WHERE model_id=?1 AND state IN ('queued','running')",
                params![id],
                |row| row.get(0),
            )
            .map_err(internal)?;
        if active > 0 {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "operation_conflict",
                "Model has an active operation",
            ));
        }
    }
    if selected_id(&app)?.as_deref() == Some(id.as_str()) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "model_in_use",
            "The selected model cannot be removed",
        ));
    }
    let path = installed_path(&app, &id)?.ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "model_not_installed",
            "Model is not installed",
        )
    })?;
    let model_root = std::fs::canonicalize(app.data_dir.join("models")).map_err(internal)?;
    if !std::fs::canonicalize(&path)
        .map_err(internal)?
        .starts_with(&model_root)
    {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "unowned_model_path",
            "Refusing to remove a file outside the model store",
        ));
    }
    let stage = app
        .data_dir
        .join("staging")
        .join(format!("{}.deleting", Uuid::new_v4()));
    tokio::fs::rename(&path, &stage).await.map_err(internal)?;
    let removed = {
        let db = app.db.lock().map_err(internal)?;
        db.execute("DELETE FROM installed WHERE id=?1", params![id])
    };
    if let Err(error) = removed {
        let _ = tokio::fs::rename(&stage, &path).await;
        return Err(internal(error));
    }
    tokio::fs::remove_file(&stage).await.map_err(internal)?;
    Ok(Json(json!({"model":id,"removed":true})))
}

/// The multipart fields both `/v1/audio/transcriptions` and
/// `/v1/audio/translations` accept, already validated for shape (duplicates,
/// unknown fields) but not yet planned against a model's capabilities.
struct TranscriptionFields {
    file: Vec<u8>,
    model: String,
    language: Option<String>,
    prompt: Option<String>,
    temperature: Option<f32>,
    response_format: Option<String>,
    timestamp_granularities: Vec<String>,
}

fn invalid_multipart(error: impl std::fmt::Display) -> ApiError {
    ApiError::new(
        StatusCode::BAD_REQUEST,
        "invalid_multipart",
        error.to_string(),
    )
}

fn duplicate_field(name: &str) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, "duplicate_field", name.to_string())
}

/// Parse and validate the shared multipart shape. Field-order rule: unknown
/// fields are rejected as they're seen (422 `unsupported_capability`) before
/// any capability-aware planning happens; a duplicate of a non-repeatable
/// field is rejected immediately as 400 `duplicate_field`.
/// `timestamp_granularities` / `timestamp_granularities[]` are the one
/// repeatable field.
async fn parse_transcription_multipart(mut multipart: Multipart) -> ApiResult<TranscriptionFields> {
    let mut file: Option<Vec<u8>> = None;
    let mut model: Option<String> = None;
    let mut language: Option<String> = None;
    let mut prompt: Option<String> = None;
    let mut temperature_raw: Option<String> = None;
    let mut response_format: Option<String> = None;
    let mut timestamp_granularities: Vec<String> = Vec::new();

    while let Some(field) = multipart.next_field().await.map_err(invalid_multipart)? {
        let name = field.name().unwrap_or("").to_owned();
        match name.as_str() {
            "file" => {
                if file.is_some() {
                    return Err(duplicate_field("file"));
                }
                file = Some(field.bytes().await.map_err(invalid_multipart)?.to_vec());
            }
            "model" => {
                if model.is_some() {
                    return Err(duplicate_field("model"));
                }
                model = Some(field.text().await.map_err(invalid_multipart)?);
            }
            "language" => {
                if language.is_some() {
                    return Err(duplicate_field("language"));
                }
                language = Some(field.text().await.map_err(invalid_multipart)?);
            }
            "prompt" => {
                if prompt.is_some() {
                    return Err(duplicate_field("prompt"));
                }
                prompt = Some(field.text().await.map_err(invalid_multipart)?);
            }
            "temperature" => {
                if temperature_raw.is_some() {
                    return Err(duplicate_field("temperature"));
                }
                temperature_raw = Some(field.text().await.map_err(invalid_multipart)?);
            }
            "response_format" => {
                if response_format.is_some() {
                    return Err(duplicate_field("response_format"));
                }
                response_format = Some(field.text().await.map_err(invalid_multipart)?);
            }
            "timestamp_granularities" | "timestamp_granularities[]" => {
                timestamp_granularities.push(field.text().await.map_err(invalid_multipart)?);
            }
            other => {
                return Err(ApiError::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "unsupported_capability",
                    format!("Field {other} is not supported"),
                ));
            }
        }
    }

    let file = file.ok_or_else(|| {
        ApiError::new(StatusCode::BAD_REQUEST, "missing_file", "file is required")
    })?;
    let model = model.ok_or_else(|| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "missing_model",
            "model is required",
        )
    })?;
    let temperature = match temperature_raw {
        Some(raw) => Some(raw.parse::<f32>().map_err(|_| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_temperature",
                "temperature must be a number",
            )
        })?),
        None => None,
    };

    Ok(TranscriptionFields {
        file,
        model,
        language,
        prompt,
        temperature,
        response_format,
        timestamp_granularities,
    })
}

fn formatted_into_response(formatted: Formatted) -> Response {
    match formatted {
        Formatted::Json(value) => Json(value).into_response(),
        Formatted::PlainText(text) => (
            [(
                axum::http::header::CONTENT_TYPE,
                "text/plain; charset=utf-8",
            )],
            text,
        )
            .into_response(),
    }
}

/// The shared pipeline behind `/v1/audio/transcriptions` and
/// `/v1/audio/translations`: parse -> decode -> grab the loaded model +
/// effective capabilities -> plan -> queue -> run -> format. See
/// `parity-design.md` "Handler".
async fn transcribe_or_translate(
    app: Arc<App>,
    headers: HeaderMap,
    multipart: Multipart,
    endpoint: Endpoint,
) -> ApiResult<Response> {
    authorized(&headers, &app)?;
    let fields = parse_transcription_multipart(multipart).await?;

    // Decode/validate happens before joining the queue; only the actual
    // inference run below waits for a turn.
    let pcm = decode_wav(&fields.file)?;
    let samples = pcm.len();
    let audio_ms = (pcm.len() as u64 * 1000) / 16_000;

    // The model binds when the request is admitted: clone the handle (and
    // its effective capabilities) before queueing, so the model can be
    // swapped underneath without affecting an in-flight or already-queued
    // request.
    let (model, active_id, backend, caps) = {
        let active = app.loaded.lock().map_err(internal)?;
        let loaded = active.as_ref().ok_or_else(|| {
            let mut error = ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "server_not_ready",
                "No model loaded",
            );
            if let Ok(Some(operation_id)) = crate::operations::active_operation_id(&app) {
                error = error.with_details(json!({"operation_id": operation_id}));
            }
            error
        })?;
        if fields.model != "default" && fields.model != loaded.id {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "model_not_active",
                "The requested model is not active",
            ));
        }
        (
            loaded.model.clone(),
            loaded.id.clone(),
            loaded.diagnostic.clone(),
            loaded.caps.clone(),
        )
    };

    let parsed = ParsedRequest {
        language: fields.language,
        prompt: fields.prompt,
        temperature: fields.temperature,
        response_format: fields.response_format,
        timestamp_granularities: fields.timestamp_granularities,
    };
    let tokenizer_model = model.clone();
    let tokenize = move |text: &str| -> Option<usize> {
        tokenizer_model
            .tokenize(text)
            .ok()
            .map(|tokens| tokens.len())
    };
    let plan = build_plan(&parsed, &caps, endpoint, Some(&tokenize))?;

    let (permit, queue_wait_ms) = app.inference.acquire().await.map_err(|error| match error {
        crate::queue::QueueError::Full => ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "queue_full",
            "Too many transcriptions are already waiting",
        ),
        crate::queue::QueueError::Timeout => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "queue_timeout",
            "Timed out waiting for a turn in the inference queue",
        ),
    })?;
    let cancellation = CancelToken::new();
    let _cancel_on_disconnect = CancelWhenDropped(cancellation.clone());
    let run_options = plan.to_run_options();
    let inference_started = Instant::now();
    let worker = tokio::task::spawn_blocking(
        move || -> transcribe_cpp::Result<transcribe_cpp::Transcript> {
            let _permit = permit;
            let mut session = model.session()?;
            session.set_cancel_token(&cancellation);
            session.run(&pcm, &run_options)
        },
    );
    let run_result = tokio::time::timeout(Duration::from_secs(180), worker)
        .await
        .map_err(|_| {
            ApiError::new(
                StatusCode::GATEWAY_TIMEOUT,
                "inference_timeout",
                "Inference exceeded the 180-second limit",
            )
        })?
        .map_err(internal)?;
    let inference_ms = inference_started.elapsed().as_millis() as u64;

    let (transcript, truncated) = match classify_run_result(run_result) {
        RunOutcome::Success(transcript) => (transcript, false),
        RunOutcome::Truncated(transcript) => (transcript, true),
        RunOutcome::Failed(error) => return Err(error),
    };

    let language_evidence = LanguageEvidence::resolve(
        plan.language_evidence_hint.as_deref(),
        transcript.language.is_some(),
    );
    let extra = DiagnosticsExtra {
        queue_wait_ms,
        inference_ms,
        audio_ms,
        model: active_id,
        backend: backend.observed_backend,
        fallback_reason: backend.fallback_reason,
        mel_ms: transcript.timings.mel_ms,
        encode_ms: transcript.timings.encode_ms,
        decode_ms: transcript.timings.decode_ms,
        truncated,
        prompt_applied: plan.initial_prompt.is_some(),
        language_hint_applied: plan.language_hint_applied,
        applied_language: plan.applied_language.clone(),
        language_evidence,
    };

    let formatted = format_response(&transcript, &plan, samples, &extra);
    Ok(formatted_into_response(formatted))
}

pub async fn transcriptions(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    multipart: Multipart,
) -> ApiResult<Response> {
    transcribe_or_translate(app, headers, multipart, Endpoint::Transcriptions).await
}

pub async fn translations(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    multipart: Multipart,
) -> ApiResult<Response> {
    transcribe_or_translate(app, headers, multipart, Endpoint::Translations).await
}

pub fn router(app: Arc<App>) -> Router {
    let origins =
        cors_allowed_origins(&app).unwrap_or_else(|_| crate::store::default_cors_origins());
    let cors = cors_layer(&origins);
    Router::new()
        .route("/health", get(health))
        .route("/readiness", get(readiness))
        .route("/v1/models", get(openai_models))
        .route(
            "/v1/audio/transcriptions",
            post(transcriptions).layer(DefaultBodyLimit::max(40 * 1024 * 1024)),
        )
        .route(
            "/v1/audio/translations",
            post(translations).layer(DefaultBodyLimit::max(40 * 1024 * 1024)),
        )
        .route("/v1/local/recommendations", get(recommendations))
        .route("/v1/local/config", get(get_config).patch(patch_config))
        .route("/v1/local/models", get(local_models))
        .route(
            "/v1/local/models/selected",
            get(selected_model).delete(deselect_model),
        )
        .route("/v1/local/models/{id}/select", post(select_model))
        .route("/v1/local/models/{id}/load", post(select_model))
        .route("/v1/local/models/{id}", delete(remove_model))
        .route("/v1/local/models/{id}/install", post(install_model))
        .route("/v1/local/models/{id}/verify", post(verify_model))
        .route(
            "/v1/local/models/import",
            post(import_model).layer(DefaultBodyLimit::max(3 * 1024 * 1024 * 1024usize)),
        )
        .route("/v1/local/operations/{id}", get(operation))
        .route("/v1/local/operations/{id}/cancel", post(cancel_operation))
        .layer(cors)
        .with_state(app)
}

pub async fn run_http(
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), Box<dyn Error>> {
    let app = open_app()?;
    let router = router(app.clone());
    let address: SocketAddr = "127.0.0.1:54321".parse()?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    println!(
        "listening on {address}; token file: {}",
        app.data_dir.join("auth.token").display()
    );
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}

#[cfg(test)]
mod router_tests {
    use super::*;
    use crate::app::open_app_at;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;
    use uuid::Uuid;

    #[tokio::test]
    async fn options_preflight_succeeds_without_token_and_reports_cors_headers() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = open_app_at(path.clone()).unwrap();
        let router = router(app.clone());
        let request = Request::builder()
            .method("OPTIONS")
            .uri("/v1/audio/transcriptions")
            .header("origin", "http://tauri.localhost")
            .header("access-control-request-method", "POST")
            .header("access-control-request-headers", "authorization")
            .body(Body::empty())
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        assert!(response.status().is_success());
        assert!(response
            .headers()
            .contains_key("access-control-allow-origin"));
        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn get_protected_route_without_token_is_unauthorized() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = open_app_at(path.clone()).unwrap();
        let router = router(app.clone());
        let request = Request::builder()
            .method("GET")
            .uri("/v1/local/config")
            .body(Body::empty())
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn patch_config_rejects_a_bad_origin() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = open_app_at(path.clone()).unwrap();
        let token = app.token.clone();
        let router = router(app.clone());
        let request = Request::builder()
            .method("PATCH")
            .uri("/v1/local/config")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"cors_allowed_origins":["not-a-url"]}"#))
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn patch_config_accepts_valid_origins_and_requires_restart() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = open_app_at(path.clone()).unwrap();
        let token = app.token.clone();
        let router = router(app.clone());
        let request = Request::builder()
            .method("PATCH")
            .uri("/v1/local/config")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"cors_allowed_origins":["http://tauri.localhost"]}"#,
            ))
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["restart_required"], true);
        assert_eq!(body["cors_allowed_origins"][0], "http://tauri.localhost");
        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn install_with_invalid_quant_returns_400() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = open_app_at(path.clone()).unwrap();
        let token = app.token.clone();
        let router = router(app.clone());
        let request = Request::builder()
            .method("POST")
            .uri("/v1/local/models/parakeet-unified-en-0.6b/install")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"quant":"not-a-real-quant"}"#))
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        drop(app);
        let resolved = path.canonicalize().unwrap();
        std::fs::remove_dir_all(resolved).unwrap();
    }

    /// A minimal valid WAV: 16 kHz mono 16-bit PCM, 200 ms of silence.
    fn sample_wav_bytes() -> Vec<u8> {
        let mut cursor = std::io::Cursor::new(Vec::new());
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        {
            let mut writer = hound::WavWriter::new(&mut cursor, spec).unwrap();
            for _ in 0..3_200 {
                writer.write_sample(0i16).unwrap();
            }
            writer.finalize().unwrap();
        }
        cursor.into_inner()
    }

    /// Build a `multipart/form-data` body from text fields plus one `file`
    /// field carrying `wav_bytes`. `text_fields` may repeat a key (e.g.
    /// `timestamp_granularities`) to produce multiple parts with that name.
    fn multipart_body(boundary: &str, text_fields: &[(&str, &str)], wav_bytes: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        for (name, value) in text_fields {
            body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            body.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
            );
            body.extend_from_slice(value.as_bytes());
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            b"Content-Disposition: form-data; name=\"file\"; filename=\"audio.wav\"\r\n",
        );
        body.extend_from_slice(b"Content-Type: audio/wav\r\n\r\n");
        body.extend_from_slice(wav_bytes);
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        body
    }

    #[tokio::test]
    async fn translations_without_token_is_unauthorized() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = open_app_at(path.clone()).unwrap();
        let router = router(app.clone());
        let boundary = "X-BOUNDARY";
        let body = multipart_body(boundary, &[("model", "default")], &sample_wav_bytes());
        let request = Request::builder()
            .method("POST")
            .uri("/v1/audio/translations")
            .header(
                "content-type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(body))
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn translations_with_token_and_no_model_loaded_is_server_not_ready() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = open_app_at(path.clone()).unwrap();
        let token = app.token.clone();
        let router = router(app.clone());
        let boundary = "X-BOUNDARY";
        let body = multipart_body(boundary, &[("model", "default")], &sample_wav_bytes());
        let request = Request::builder()
            .method("POST")
            .uri("/v1/audio/translations")
            .header("authorization", format!("Bearer {token}"))
            .header(
                "content-type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(body))
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["code"], "server_not_ready");
        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn transcriptions_unknown_field_is_422_before_model_check() {
        // Validation order: multipart field-shape validation (unknown field)
        // happens before the loaded-model / capability-aware planning check,
        // so this is 422 even with no model loaded.
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = open_app_at(path.clone()).unwrap();
        let token = app.token.clone();
        let router = router(app.clone());
        let boundary = "X-BOUNDARY";
        let body = multipart_body(
            boundary,
            &[("model", "default"), ("vocabulary", "hello")],
            &sample_wav_bytes(),
        );
        let request = Request::builder()
            .method("POST")
            .uri("/v1/audio/transcriptions")
            .header("authorization", format!("Bearer {token}"))
            .header(
                "content-type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(body))
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["code"], "unsupported_capability");
        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn transcriptions_duplicate_field_is_400() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = open_app_at(path.clone()).unwrap();
        let token = app.token.clone();
        let router = router(app.clone());
        let boundary = "X-BOUNDARY";
        let body = multipart_body(
            boundary,
            &[("model", "default"), ("model", "default")],
            &sample_wav_bytes(),
        );
        let request = Request::builder()
            .method("POST")
            .uri("/v1/audio/transcriptions")
            .header("authorization", format!("Bearer {token}"))
            .header(
                "content-type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(body))
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["code"], "duplicate_field");
        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }
}
