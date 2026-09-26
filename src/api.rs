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

use crate::app::App;
use crate::audio::decode_wav;
use crate::auth::authorized;
use crate::capabilities::catalog_mismatch;
use crate::catalog::{capability_matrix, catalog_model, model_view};
use crate::download::install_model;
use crate::dropin::run_refresh;
use crate::engine::{load_engine, CancelWhenDropped, LoadedModel};
use crate::errors::{classify_run_result, internal, ApiError, ApiResult, RunOutcome};
use crate::format::{format_response, DiagnosticsExtra, Formatted, LanguageEvidence};
use crate::import::import_model;
use crate::operations::{cancel_operation, operation};
use crate::run_plan::{plan as build_plan, Endpoint, ParsedRequest};
use crate::store::{
    all_installed, backend_preference, cors_allowed_origins, installed_file, installed_path,
    is_valid_cors_origin, selected_id, user_models_dir_setting, SOURCE_USER_FOLDER,
};
use crate::verify::verify_model;

pub async fn get_config(State(app): State<Arc<App>>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    authorized(&headers, &app)?;
    let limits = crate::store::runtime_limits(&app)?;
    let user_models_dir = user_models_dir_setting(&app)?.or_else(|| {
        crate::app::default_user_models_dir().map(|path| path.to_string_lossy().into_owned())
    });
    Ok(Json(json!({
        "bind":"127.0.0.1:54321",
        "preferred_backend":backend_preference(&app)?,
        "max_audio_bytes":40 * 1024 * 1024,
        "streaming":false,
        "cors_allowed_origins":cors_allowed_origins(&app)?,
        "queue_max_waiting": limits.queue_max_waiting,
        "queue_wait_timeout_ms": limits.queue_wait_timeout_ms,
        "inference_timeout_ms": limits.inference_timeout_ms,
        "user_models_dir": user_models_dir,
    })))
}

/// Deserializes a present JSON field (including an explicit `null`) as
/// `Some(inner)`, leaving an absent field as the outer `None` from
/// `#[serde(default)]`. This is what lets `ConfigPatch`'s optional-limit
/// fields distinguish "not sent" (leave unchanged) from `null` (clear).
fn deserialize_some<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigPatch {
    preferred_backend: Option<String>,
    cors_allowed_origins: Option<Vec<String>>,
    /// `None` (field absent): leave unchanged. `Some(None)` (`null`): clear
    /// to unbounded/no-timeout. `Some(Some(n))`: set to `n`.
    #[serde(default, deserialize_with = "deserialize_some")]
    queue_max_waiting: Option<Option<i64>>,
    #[serde(default, deserialize_with = "deserialize_some")]
    queue_wait_timeout_ms: Option<Option<i64>>,
    #[serde(default, deserialize_with = "deserialize_some")]
    inference_timeout_ms: Option<Option<i64>>,
    /// The drop-in models folder (see "Drop-in models and refresh"). Must be
    /// an absolute, existing directory, else 400 `invalid_user_models_dir`.
    user_models_dir: Option<String>,
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
    // Validate the three optional limits up front so a bad value in one
    // field rejects the whole patch before anything is written.
    let queue_max_waiting = match patch.queue_max_waiting {
        Some(inner) => Some(
            crate::store::validate_positive_limit(inner).map_err(|message| {
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "invalid_queue_max_waiting",
                    message,
                )
            })?,
        ),
        None => None,
    };
    let queue_wait_timeout_ms = match patch.queue_wait_timeout_ms {
        Some(inner) => Some(
            crate::store::validate_positive_limit(inner).map_err(|message| {
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "invalid_queue_wait_timeout_ms",
                    message,
                )
            })?,
        ),
        None => None,
    };
    let inference_timeout_ms = match patch.inference_timeout_ms {
        Some(inner) => Some(
            crate::store::validate_positive_limit(inner).map_err(|message| {
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "invalid_inference_timeout_ms",
                    message,
                )
            })?,
        ),
        None => None,
    };
    if let Some(dir) = &patch.user_models_dir {
        let path = std::path::Path::new(dir);
        if !path.is_absolute() || !path.is_dir() {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_user_models_dir",
                "user_models_dir must be an absolute, existing directory",
            ));
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
        if let Some(dir) = &patch.user_models_dir {
            db.execute(
                "INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![crate::store::SETTING_USER_MODELS_DIR, dir],
            )
            .map_err(internal)?;
            response.insert("user_models_dir".to_owned(), json!(dir));
        }
        for (key, value, response_key) in [
            (
                crate::store::SETTING_QUEUE_MAX_WAITING,
                queue_max_waiting,
                "queue_max_waiting",
            ),
            (
                crate::store::SETTING_QUEUE_WAIT_TIMEOUT_MS,
                queue_wait_timeout_ms,
                "queue_wait_timeout_ms",
            ),
            (
                crate::store::SETTING_INFERENCE_TIMEOUT_MS,
                inference_timeout_ms,
                "inference_timeout_ms",
            ),
        ] {
            let Some(value) = value else { continue };
            match value {
                Some(number) => {
                    db.execute(
                        "INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                        params![key, number.to_string()],
                    )
                    .map_err(internal)?;
                }
                None => {
                    db.execute("DELETE FROM settings WHERE key=?1", params![key])
                        .map_err(internal)?;
                }
            }
            response.insert(response_key.to_owned(), json!(value));
        }
    }
    // Applied live (no restart needed): the queue and inference timeout read
    // `app.limits` at request time.
    if patch.queue_max_waiting.is_some()
        || patch.queue_wait_timeout_ms.is_some()
        || patch.inference_timeout_ms.is_some()
    {
        let mut limits = app.limits.write().map_err(internal)?;
        if let Some(value) = queue_max_waiting {
            limits.queue_max_waiting = value.map(|v| v as usize);
        }
        if let Some(value) = queue_wait_timeout_ms {
            limits.queue_wait_timeout_ms = value;
        }
        if let Some(value) = inference_timeout_ms {
            limits.inference_timeout_ms = value;
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

/// Build the `/v1/local/models` view for an installed custom model (a
/// drop-in file whose header, not the catalog, is the source of its
/// metadata). See "Integration rules": `source`, `custom: true`, an
/// `evidence: "gguf_header"` capability view, `installable: false`, and
/// `recommended_rank: null`.
fn custom_model_view(installed: &crate::store::InstalledFile) -> Value {
    let claims = installed.custom_claims.clone().unwrap_or(Value::Null);
    let languages = installed.custom_languages.clone().unwrap_or_default();
    let multi_language = languages.len() > 1;
    json!({
        "id": installed.id,
        "name": installed.custom_name.clone().unwrap_or_else(|| installed.id.clone()),
        "architecture": installed.custom_arch,
        "languages": languages,
        "source": installed.source,
        "custom": true,
        "evidence": "gguf_header",
        "model_capabilities": claims,
        "effective_capabilities": {
            "prompt": {"status": "unknown"},
            "temperature": {"status": "unknown"},
            "language_hint": {"status": if multi_language { "unknown" } else { "unsupported" }},
            "language_detect": {"status": "unknown"},
            "translation": {"status": "unknown"},
            "timestamp_granularity": {"status": "unknown"},
            "streaming": {"status": "unsupported"},
            "response_formats": {"json": "supported", "text": "unsupported", "verbose_json": "unsupported"},
        },
        "installed": true,
        "installed_quant": installed.quant,
        "installable": false,
        "recommended_rank": Value::Null,
        "needs_verification": installed.needs_verification,
        "file_path": installed.path.to_string_lossy(),
    })
}

/// Custom (non-catalog) installed models: rows with `custom_arch` set, i.e.
/// registered by `/v1/local/models/refresh` from a GGUF header probe rather
/// than a catalog match.
fn installed_custom_models(app: &App) -> ApiResult<Vec<crate::store::InstalledFile>> {
    Ok(all_installed(app)?
        .into_iter()
        .filter(|row| row.is_custom())
        .collect())
}

pub async fn local_models(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    authorized(&headers, &app)?;
    let mut data = app
        .catalog
        .iter()
        .map(|model| {
            let installed = installed_file(&app, &model.slug)?;
            let mut view = model_view(
                model,
                installed
                    .as_ref()
                    .and_then(|file| file.quant.clone())
                    .as_deref(),
            );
            if let Some(installed) = &installed {
                view["source"] = json!(installed.source);
                view["needs_verification"] = json!(installed.needs_verification);
            }
            if let Some(effective) = effective_view_if_loaded(&app, &model.slug)? {
                view["effective_capabilities"] = effective;
            }
            Ok(view)
        })
        .collect::<ApiResult<Vec<_>>>()?;
    for custom in installed_custom_models(&app)? {
        data.push(custom_model_view(&custom));
    }
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
    for custom in installed_custom_models(&app)? {
        data.push(json!({"id":custom.id,"object":"model","owned_by":"local"}));
    }
    Ok(Json(json!({"object":"list", "data":data})))
}

/// True when `id` is either a known catalog model, or an installed custom
/// (drop-in) model registered by refresh. Used where an endpoint must accept
/// any installed model regardless of source (select/load/remove).
fn known_or_installed(app: &App, id: &str) -> ApiResult<()> {
    if catalog_model(app, id).is_ok() {
        return Ok(());
    }
    if installed_file(app, id)?.is_some() {
        return Ok(());
    }
    Err(ApiError::new(
        StatusCode::NOT_FOUND,
        "model_not_found",
        "Unknown model ID",
    ))
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
    known_or_installed(&app, &id)?;
    let installed = installed_file(&app, &id)?.ok_or_else(|| {
        ApiError::new(
            StatusCode::CONFLICT,
            "model_not_installed",
            "Install the model first",
        )
    })?;
    if installed.needs_verification {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "needs_verification",
            "This drop-in model changed on disk; refresh to re-verify it before selecting",
        ));
    }
    let path = installed.path.clone();
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
    known_or_installed(&app, &id)?;
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
    let installed = installed_file(&app, &id)?.ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "model_not_installed",
            "Model is not installed",
        )
    })?;
    // A `user_folder` (drop-in) model is unregistered only; its file lives
    // outside the managed store by design and is never touched.
    if installed.source == SOURCE_USER_FOLDER {
        let db = app.db.lock().map_err(internal)?;
        db.execute("DELETE FROM installed WHERE id=?1", params![id])
            .map_err(internal)?;
        return Ok(Json(
            json!({"model":id,"removed":true,"file_deleted":false}),
        ));
    }
    let path = installed.path.clone();
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
    Ok(Json(json!({"model":id,"removed":true,"file_deleted":true})))
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

    // Read live: a value set (or cleared) via `PATCH /v1/local/config`, or a
    // CLI override supplied at process start, applies to this request
    // without a restart. Defaults are unbounded/no-timeout.
    let limits = *app.limits.read().map_err(internal)?;
    let wait_timeout = limits.queue_wait_timeout_ms.map(Duration::from_millis);
    let (permit, queue_wait_ms) = app
        .inference
        .acquire(limits.queue_max_waiting, wait_timeout)
        .await
        .map_err(|error| match error {
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
    // Bug 1: only a *defaulted* verbose_json segment-timestamp choice may be
    // silently downgraded when the engine rejects it — never an explicit
    // `timestamp_granularities` request. Decided once, up front, from the
    // plan alone (the pure `should_retry_without_timestamps`); actually
    // exercised below only if the run comes back `Unsupported`.
    let retry_eligible = crate::run_plan::should_retry_without_timestamps(
        plan.response_format,
        plan.timestamps,
        plan.timestamps_explicit,
        "engine_unsupported",
    );
    let inference_started = Instant::now();
    let worker = tokio::task::spawn_blocking(
        move || -> transcribe_cpp::Result<(transcribe_cpp::Transcript, bool)> {
            let _permit = permit;
            let mut session = model.session()?;
            session.set_cancel_token(&cancellation);
            match session.run(&pcm, &run_options) {
                Err(transcribe_cpp::Error::Unsupported(message)) if retry_eligible => {
                    // Retry once, in the same queue slot (no re-queue): same
                    // model, same session/permit hold, `TimestampKind::None`
                    // in place of the rejected default.
                    let mut retry_options = run_options.clone();
                    retry_options.timestamps = transcribe_cpp::TimestampKind::None;
                    let mut retry_session = match model.session() {
                        Ok(s) => s,
                        Err(_) => return Err(transcribe_cpp::Error::Unsupported(message)),
                    };
                    retry_session.set_cancel_token(&cancellation);
                    match retry_session.run(&pcm, &retry_options) {
                        Ok(transcript) => Ok((transcript, true)),
                        Err(_) => Err(transcribe_cpp::Error::Unsupported(message)),
                    }
                }
                Ok(transcript) => Ok((transcript, false)),
                Err(other) => Err(other),
            }
        },
    );
    let run_result: transcribe_cpp::Result<(transcribe_cpp::Transcript, bool)> =
        match limits.inference_timeout_ms {
            Some(timeout_ms) => tokio::time::timeout(Duration::from_millis(timeout_ms), worker)
                .await
                .map_err(|_| {
                    ApiError::new(
                        StatusCode::GATEWAY_TIMEOUT,
                        "inference_timeout",
                        format!("Inference exceeded the {timeout_ms}ms limit"),
                    )
                })?
                .map_err(internal)?,
            None => worker.await.map_err(internal)?,
        };
    let inference_ms = inference_started.elapsed().as_millis() as u64;

    let mut timestamps_unavailable = false;
    let run_result: transcribe_cpp::Result<transcribe_cpp::Transcript> = match run_result {
        Ok((transcript, retried)) => {
            timestamps_unavailable = retried;
            Ok(transcript)
        }
        Err(error) => Err(error),
    };

    let (transcript, truncated) = match classify_run_result(run_result) {
        RunOutcome::Success(transcript) => (transcript, false),
        RunOutcome::Truncated(transcript) => (transcript, true),
        RunOutcome::Failed(error) => return Err(error),
    };

    if timestamps_unavailable {
        // Bug 1: the engine has now demonstrably rejected this loaded
        // model's own advertised timestamp granularity. Update the cached
        // effective caps under the same brief lock that guards `App::loaded`
        // so later requests against this same loaded model stop being told
        // `timestamp_granularity` is supported.
        if let Ok(mut active) = app.loaded.lock() {
            if let Some(loaded) = active.as_mut() {
                if loaded.id == active_id {
                    loaded.caps.mark_timestamp_granularity_rejected();
                }
            }
        }
    }

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
        language_hint_applied: plan
            .language_hint_provided
            .then_some(plan.language_hint_applied),
        applied_language: plan.applied_language.clone(),
        language_evidence,
        timestamps_unavailable,
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

/// `POST /v1/local/models/refresh`: a durable operation (kind `refresh`)
/// that scans the drop-in `user_models_dir` for new/changed/removed `.gguf`
/// files. See `crate::dropin` for the scan/registration rules.
pub async fn refresh_models(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
) -> ApiResult<(StatusCode, Json<Value>)> {
    authorized(&headers, &app)?;
    let op = Uuid::new_v4().to_string();
    {
        let db = app.db.lock().map_err(internal)?;
        let now = crate::store::now_ms();
        db.execute(
            "INSERT INTO operations(id,model_id,kind,state,error,progress_bytes,total_bytes,progress_items,total_items,created_at,updated_at) VALUES(?1,'','refresh','queued',NULL,0,0,0,0,?2,?2)",
            params![op, now],
        )
        .map_err(internal)?;
    }
    let task_app = app.clone();
    let task_op = op.clone();
    tokio::spawn(async move {
        run_refresh(task_app, task_op).await;
    });
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({"operation_id":op,"state":"queued"})),
    ))
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
        .route("/v1/local/models/refresh", post(refresh_models))
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
    run_http_with_overrides(crate::app::RuntimeLimits::default(), shutdown).await
}

/// Same as [`run_http`], but `cli_overrides` (from the binary's optional
/// queue/inference flags) takes precedence over the persisted settings for
/// this process; see `app::open_app_at_with_overrides`.
pub async fn run_http_with_overrides(
    cli_overrides: crate::app::RuntimeLimits,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), Box<dyn Error>> {
    let app = crate::app::open_app_at_with_overrides(crate::app::data_dir(), cli_overrides)?;
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
    async fn get_config_defaults_to_unbounded_limits() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = open_app_at(path.clone()).unwrap();
        let token = app.token.clone();
        let router = router(app.clone());
        let request = Request::builder()
            .method("GET")
            .uri("/v1/local/config")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(body["queue_max_waiting"].is_null());
        assert!(body["queue_wait_timeout_ms"].is_null());
        assert!(body["inference_timeout_ms"].is_null());
        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn patch_config_sets_and_clears_limits_live_and_round_trips_via_get() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = open_app_at(path.clone()).unwrap();
        let token = app.token.clone();
        let router = router(app.clone());

        let set_request = Request::builder()
            .method("PATCH")
            .uri("/v1/local/config")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"queue_max_waiting":5,"queue_wait_timeout_ms":2000,"inference_timeout_ms":30000}"#,
            ))
            .unwrap();
        let response = router.clone().oneshot(set_request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        // No restart needed: these are applied live via app.limits.
        assert_eq!(body["restart_required"], false);
        assert_eq!(body["queue_max_waiting"], 5);
        assert_eq!(body["queue_wait_timeout_ms"], 2000);
        assert_eq!(body["inference_timeout_ms"], 30000);
        {
            let limits = *app.limits.read().unwrap();
            assert_eq!(limits.queue_max_waiting, Some(5));
            assert_eq!(limits.queue_wait_timeout_ms, Some(2000));
            assert_eq!(limits.inference_timeout_ms, Some(30000));
        }

        let get_request = Request::builder()
            .method("GET")
            .uri("/v1/local/config")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let response = router.clone().oneshot(get_request).await.unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["queue_max_waiting"], 5);
        assert_eq!(body["queue_wait_timeout_ms"], 2000);
        assert_eq!(body["inference_timeout_ms"], 30000);

        let clear_request = Request::builder()
            .method("PATCH")
            .uri("/v1/local/config")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"queue_max_waiting":null}"#))
            .unwrap();
        let response = router.clone().oneshot(clear_request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(body["queue_max_waiting"].is_null());
        assert_eq!(
            app.limits.read().unwrap().queue_max_waiting,
            None,
            "null should clear the setting"
        );
        // The other two limits are untouched by a patch that omits them.
        assert_eq!(app.limits.read().unwrap().queue_wait_timeout_ms, Some(2000));

        // The router holds its own clone of `Arc<App>` (hence its own handle
        // on the SQLite connection); drop it before `app` so the last handle
        // is actually released before removing the directory (Windows will
        // not delete a file still open by another handle).
        drop(router);
        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn patch_config_rejects_zero_and_negative_limits() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = open_app_at(path.clone()).unwrap();
        let token = app.token.clone();
        let router = router(app.clone());
        for body in [
            r#"{"queue_max_waiting":0}"#,
            r#"{"queue_wait_timeout_ms":-1}"#,
            r#"{"inference_timeout_ms":0}"#,
        ] {
            let request = Request::builder()
                .method("PATCH")
                .uri("/v1/local/config")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap();
            let response = router.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "body: {body}");
        }
        // See the comment in the previous test: drop the router's own
        // `Arc<App>` clone before removing the directory on Windows.
        drop(router);
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

    #[tokio::test]
    async fn patch_config_rejects_nonexistent_or_relative_user_models_dir() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = open_app_at(path.clone()).unwrap();
        let token = app.token.clone();
        let router = router(app.clone());
        for body in [
            r#"{"user_models_dir":"relative\\path"}"#,
            r#"{"user_models_dir":"C:\\definitely-not-a-real-directory-xyz-123"}"#,
        ] {
            let request = Request::builder()
                .method("PATCH")
                .uri("/v1/local/config")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap();
            let response = router.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "body: {body}");
        }
        drop(router);
        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn patch_config_accepts_and_round_trips_an_existing_absolute_user_models_dir() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = open_app_at(path.clone()).unwrap();
        let token = app.token.clone();
        let router = router(app.clone());
        let dropdir = parent.join(format!("stt-server-next-dropin-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dropdir).unwrap();
        let dropdir_json = serde_json::to_string(&dropdir.to_string_lossy()).unwrap();

        let request = Request::builder()
            .method("PATCH")
            .uri("/v1/local/config")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(format!(
                "{{\"user_models_dir\":{dropdir_json}}}"
            )))
            .unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let get_request = Request::builder()
            .method("GET")
            .uri("/v1/local/config")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let response = router.clone().oneshot(get_request).await.unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["user_models_dir"], dropdir.to_string_lossy().as_ref());

        drop(router);
        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
        std::fs::remove_dir_all(dropdir.canonicalize().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn refresh_returns_202_and_the_operation_completes() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = open_app_at(path.clone()).unwrap();
        let token = app.token.clone();
        let router = router(app.clone());
        // No user_models_dir configured and no LOCALAPPDATA default in this
        // test process, so the operation completes fast (an unconfigured
        // folder is a `failed` terminal state, still reachable via polling).
        let request = Request::builder()
            .method("POST")
            .uri("/v1/local/models/refresh")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        let operation_id = body["operation_id"].as_str().unwrap().to_owned();

        let mut state = String::new();
        for _ in 0..100 {
            let get_request = Request::builder()
                .method("GET")
                .uri(format!("/v1/local/operations/{operation_id}"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap();
            let response = router.clone().oneshot(get_request).await.unwrap();
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let record: Value = serde_json::from_slice(&bytes).unwrap();
            state = record["state"].as_str().unwrap().to_owned();
            if state != "queued" && state != "running" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            state == "completed" || state == "failed",
            "operation did not finish: {state}"
        );

        drop(router);
        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn remove_of_user_folder_model_keeps_the_file() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = open_app_at(path.clone()).unwrap();
        let token = app.token.clone();
        let dropdir = parent.join(format!("stt-server-next-dropin-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dropdir).unwrap();
        let file_path = dropdir.join("mine.gguf");
        std::fs::write(&file_path, b"user file bytes").unwrap();
        {
            let db = app.db.lock().unwrap();
            db.execute(
                "INSERT INTO installed(id,path,sha256,source) VALUES('custom-mine-abc12345',?1,'x','user_folder')",
                rusqlite::params![file_path.to_string_lossy().as_ref()],
            )
            .unwrap();
        }
        let router = router(app.clone());
        let request = Request::builder()
            .method("DELETE")
            .uri("/v1/local/models/custom-mine-abc12345")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["file_deleted"], false);
        assert!(file_path.exists());
        assert!(installed_file(&app, "custom-mine-abc12345")
            .unwrap()
            .is_none());

        drop(router);
        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
        std::fs::remove_dir_all(dropdir.canonicalize().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn select_of_a_needs_verification_model_is_refused() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = open_app_at(path.clone()).unwrap();
        let token = app.token.clone();
        let dropdir = parent.join(format!("stt-server-next-dropin-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dropdir).unwrap();
        let file_path = dropdir.join("mine.gguf");
        std::fs::write(&file_path, b"user file bytes").unwrap();
        {
            let db = app.db.lock().unwrap();
            db.execute(
                "INSERT INTO installed(id,path,sha256,source,needs_verification) VALUES('custom-mine-abc12345',?1,'x','user_folder',1)",
                rusqlite::params![file_path.to_string_lossy().as_ref()],
            )
            .unwrap();
        }
        let router = router(app.clone());
        let request = Request::builder()
            .method("POST")
            .uri("/v1/local/models/custom-mine-abc12345/select")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["code"], "needs_verification");

        drop(router);
        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
        std::fs::remove_dir_all(dropdir.canonicalize().unwrap()).unwrap();
    }
}
