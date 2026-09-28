use std::error::Error;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    extract::{ConnectInfo, DefaultBodyLimit, Multipart, Path as UrlPath, State},
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
use crate::auth::{authorize, authorized, AccessLevel};
use crate::capabilities::{catalog_mismatch, EffectiveCaps};
use crate::catalog::{capability_matrix, catalog_model, model_view};
use crate::download::install_model;
use crate::dropin::run_refresh;
use crate::engine::{load_engine, BackendDiagnostic, CancelWhenDropped, LoadedModel};
use crate::errors::{classify_run_result, internal, ApiError, ApiResult, RunOutcome};
use crate::format::{format_response, DiagnosticsExtra, Formatted, LanguageEvidence};
use crate::import::import_model;
use crate::operations::{cancel_operation, operation};
use crate::run_plan::{plan as build_plan, Endpoint, ParsedRequest};
use crate::store::{
    all_installed, backend_preference, installed_file, is_valid_cors_origin, selected_id,
    user_models_dir_setting, SOURCE_USER_FOLDER,
};
use crate::verify::verify_model;

pub async fn get_config(State(app): State<Arc<App>>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    authorized(&headers, &app)?;
    let limits = crate::store::runtime_limits(&app)?;
    let user_models_dir = user_models_dir_setting(&app)?.or_else(|| {
        crate::app::default_user_models_dir().map(|path| path.to_string_lossy().into_owned())
    });
    let (stored_bind_host, stored_bind_port) = crate::store::bind_settings(&app)?;
    let stored_network_mode = crate::store::network_mode_setting(&app)?;
    Ok(Json(json!({
        "bind": format!("{}:{}", app.bind_host, app.bind_port),
        "bind_host": stored_bind_host,
        "bind_port": stored_bind_port,
        "network_mode": stored_network_mode.map(|mode| mode.as_str()),
        "preferred_backend":backend_preference(&app)?,
        "max_audio_bytes":40 * 1024 * 1024,
        "streaming":false,
        "cors_allowed_origins":app.cors_origins,
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
    /// Stored bind host/port (phase 1a). Takes effect on the next server
    /// start; a running process keeps its current bind.
    bind_host: Option<String>,
    bind_port: Option<i64>,
    /// "Network modes": `local` | `lan` | `tailscale`. Takes effect on the
    /// next server start; ignored by a process already running with an
    /// explicit `--host`/stored `bind_host` override (see `crate::network`).
    network_mode: Option<String>,
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
    if let Some(host) = &patch.bind_host {
        if !crate::store::validate_bind_host(host) {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_bind_host",
                "bind_host must be a valid IPv4 or IPv6 address",
            ));
        }
    }
    if let Some(port) = patch.bind_port {
        if !crate::store::validate_bind_port(port) {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_bind_port",
                "bind_port must be an integer from 1 to 65535",
            ));
        }
    }
    if let Some(mode) = &patch.network_mode {
        if crate::network::NetworkMode::parse(mode).is_none() {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_network_mode",
                "network_mode must be 'local', 'lan', or 'tailscale'",
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
        if let Some(host) = &patch.bind_host {
            db.execute(
                "INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![crate::store::SETTING_BIND_HOST, host],
            )
            .map_err(internal)?;
            response.insert("bind_host".to_owned(), json!(host));
            restart_required = true;
        }
        if let Some(port) = patch.bind_port {
            db.execute(
                "INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![crate::store::SETTING_BIND_PORT, port.to_string()],
            )
            .map_err(internal)?;
            response.insert("bind_port".to_owned(), json!(port));
            restart_required = true;
        }
        if let Some(mode) = &patch.network_mode {
            db.execute(
                "INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![crate::store::SETTING_NETWORK_MODE, mode],
            )
            .map_err(internal)?;
            response.insert("network_mode".to_owned(), json!(mode));
            restart_required = true;
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

/// The value `service.rs`/`server.rs` compare against so `start`/`status`
/// never mistake an unrelated program answering "200 OK" on the configured
/// port for this server (independent review L2): a `service` field identity
/// check, not just a successful status code.
pub const SERVICE_ID: &str = "stt-server-next";

/// Bumped whenever a client must change to keep working against this
/// server -- a breaking request/response shape change, a route removed, a
/// new required field, etc. Never bumped for additive, backward-compatible
/// changes (a new optional field, a new route). Clients read this from
/// `/health` (unauthenticated, so it's checkable before a token is even
/// available) and refuse or warn when it's lower than the level they
/// require; see `docs/client-contract.md` section 1.4.
pub const API_LEVEL: u32 = 1;

pub async fn health(State(app): State<Arc<App>>) -> Json<Value> {
    let network = app
        .network_state
        .read()
        .map(|state| state.to_json())
        .unwrap_or_else(|_| json!({"mode": app.network_mode.as_str(), "effective": "local"}));
    // `default_model`/`loaded_model` (the "openai-model-per-request" goal:
    // "the existing selected model becomes the default model ... /health and
    // /readiness report default_model and loaded_model"): best-effort, never
    // failing `/health` itself if the DB or lock is unavailable.
    let default_model = selected_id(&app).ok().flatten();
    let loaded_model = app
        .loaded
        .lock()
        .ok()
        .and_then(|loaded| loaded.as_ref().map(|active| active.id.clone()));
    Json(json!({
        "status": "ok",
        "service": SERVICE_ID,
        "version": env!("CARGO_PKG_VERSION"),
        "api_level": API_LEVEL,
        "network": network,
        "default_model": default_model,
        "loaded_model": loaded_model,
    }))
}

pub async fn readiness(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
) -> ApiResult<impl IntoResponse> {
    authorize(&headers, &app, AccessLevel::User)?;
    let default_model = selected_id(&app)?;
    {
        let loaded = app.loaded.lock().map_err(internal)?;
        if let Some(active) = loaded.as_ref() {
            return Ok((
                StatusCode::OK,
                Json(json!({
                    "status":"ready",
                    "model":active.id,
                    "backend":active.diagnostic,
                    "default_model": default_model,
                    "loaded_model": active.id,
                })),
            ));
        }
    }
    // No model loaded yet -- either the previously selected model is still
    // loading in the background (see `app::open_app_at_full`/
    // `engine::spawn_tracked_load`), or nothing is selected at all. Health
    // (`/health`) is always `ok` regardless of which of these applies; only
    // readiness distinguishes them, so a client's health card can show
    // "starting up (loading <model>, 42s)" instead of a bare "not ready".
    let loading = app.loading.lock().map_err(internal)?;
    if let Some(status) = loading.as_ref() {
        return Ok((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "status":"not_ready",
                "reason":"loading model",
                "model": status.model_id,
                "elapsed_ms": status.elapsed_ms(),
                "default_model": default_model,
                "loaded_model": Value::Null,
            })),
        ));
    }
    Ok((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({
            "status":"not_ready",
            "reason":"No selected model loaded",
            "default_model": default_model,
            "loaded_model": Value::Null,
        })),
    ))
}

/// `GET /v1/local/system`: OS/CPU/memory/GPU/process/server info for the
/// client's hardware/health card. See `crate::sysinfo` for field semantics
/// and what's omitted when unobtainable.
pub async fn system_info(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    authorize(&headers, &app, AccessLevel::User)?;
    let snapshot = tokio::task::spawn_blocking(crate::sysinfo::probe)
        .await
        .map_err(internal)?;
    let server = crate::sysinfo::ServerSection {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        api_level: API_LEVEL,
        host: app.bind_host.clone(),
        port: app.bind_port,
        data_dir: app.data_dir.display().to_string(),
    };
    Ok(Json(crate::sysinfo::to_json(&snapshot, &server)))
}

pub async fn recommendations(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    authorize(&headers, &app, AccessLevel::User)?;
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

/// Build the `/models/manage` view for an installed custom model (a
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
        "downloaded": true,
        "installed_quant": installed.quant,
        "installable": false,
        "recommended_rank": Value::Null,
        "needs_verification": installed.needs_verification,
        "file_path": installed.path.to_string_lossy(),
    })
}

/// Custom (non-catalog) installed models: rows with `custom_arch` set, i.e.
/// registered by `/models/manage/refresh` from a GGUF header probe rather
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
    authorize(&headers, &app, AccessLevel::User)?;
    let default_id = selected_id(&app)?;
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
            view["default"] = json!(default_id.as_deref() == Some(model.slug.as_str()));
            Ok(view)
        })
        .collect::<ApiResult<Vec<_>>>()?;
    for custom in installed_custom_models(&app)? {
        let is_default = default_id.as_deref() == Some(custom.id.as_str());
        let mut view = custom_model_view(&custom);
        view["default"] = json!(is_default);
        data.push(view);
    }
    Ok(Json(json!({"object":"list", "data":data})))
}

/// The capability view for a callable (downloaded+verified) model, for the
/// OpenAI-shaped list: the live `EffectiveCaps` view if this process has ever
/// loaded it (`App::live_caps`, refreshed every time it loads), falling back
/// to the catalog's static best answer otherwise. Custom (drop-in) models
/// have no catalog entry, so they fall back to their own unknown-by-default
/// view (`custom_model_view`'s `effective_capabilities`) when never loaded.
fn callable_capabilities(app: &App, id: &str, catalog_fallback: Option<Value>) -> ApiResult<Value> {
    if let Some(cached) = app.live_caps.lock().map_err(internal)?.get(id) {
        return Ok(cached.to_json());
    }
    Ok(catalog_fallback.unwrap_or_else(|| {
        json!({
            "prompt": {"status": "unknown"},
            "temperature": {"status": "unknown"},
            "language_hint": {"status": "unknown"},
            "language_detect": {"status": "unknown"},
            "translation": {"status": "unknown"},
            "timestamp_granularity": {"status": "unknown"},
            "streaming": {"status": "unsupported"},
            "response_formats": {"json": "supported", "text": "unsupported", "verbose_json": "unsupported"},
        })
    }))
}

/// One `GET /v1/models`/`GET /v1/models/{id}` entry for a callable
/// (downloaded, verified) model: OpenAI's `{id, object, owned_by}` shape plus
/// this server's extension fields `default`, `capabilities`, `languages`, and
/// `language_detect`. See `docs/client-contract.md`.
///
/// `languages` and `language_detect` are derived from the same `capabilities`
/// view above (so they agree with it): the live `EffectiveCaps` for this
/// model if this process has ever loaded it (`language_hint.languages` --
/// `ControlCapability::to_json` flattens its `extra` map into the control's
/// own object -- and `language_detect.status == "supported"`), falling back
/// to the catalog's static claim, or, for a custom/drop-in model with no
/// catalog entry, its own GGUF-header-derived languages and `lang_detect`
/// claim.
fn openai_model_entry(app: &App, id: &str, default_id: Option<&str>) -> ApiResult<Value> {
    let catalog_model = catalog_model(app, id).ok();
    let catalog_fallback = catalog_model.map(capability_matrix);
    let capabilities = callable_capabilities(app, id, catalog_fallback)?;

    let languages_from_capabilities = capabilities["language_hint"]["languages"]
        .as_array()
        .cloned();
    let language_detect_from_capabilities = match &capabilities["language_detect"]["status"] {
        Value::String(status) if status != "unknown" => Some(status == "supported"),
        _ => None,
    };

    let (languages, language_detect) = if let (Some(languages), Some(language_detect)) = (
        languages_from_capabilities,
        language_detect_from_capabilities,
    ) {
        (json!(languages), json!(language_detect))
    } else if let Some(model) = catalog_model {
        (
            json!(model.languages),
            json!(model.capabilities.lang_detect),
        )
    } else {
        let installed = installed_file(app, id)?;
        let languages = installed
            .as_ref()
            .and_then(|file| file.custom_languages.clone())
            .unwrap_or_default();
        let language_detect = installed
            .as_ref()
            .and_then(|file| file.custom_claims.as_ref())
            .and_then(|claims| claims.get("lang_detect"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        (json!(languages), json!(language_detect))
    };

    Ok(json!({
        "id": id,
        "object": "model",
        "owned_by": "local",
        "default": Some(id) == default_id,
        "capabilities": capabilities,
        "languages": languages,
        "language_detect": language_detect,
    }))
}

/// Every callable model id: downloaded and verified catalog models plus
/// registered custom (drop-in) models. A `needs_verification` model is never
/// callable and never listed here (matches the 409 a request for it gets).
fn callable_model_ids(app: &App) -> ApiResult<Vec<String>> {
    let mut ids = Vec::new();
    for model in &app.catalog {
        if let Some(installed) = installed_file(app, &model.slug)? {
            if !installed.needs_verification {
                ids.push(model.slug.clone());
            }
        }
    }
    for custom in installed_custom_models(app)? {
        if !custom.needs_verification {
            ids.push(custom.id);
        }
    }
    Ok(ids)
}

pub async fn openai_models(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    authorize(&headers, &app, AccessLevel::User)?;
    let default_id = selected_id(&app)?;
    let data = callable_model_ids(&app)?
        .into_iter()
        .map(|id| openai_model_entry(&app, &id, default_id.as_deref()))
        .collect::<ApiResult<Vec<_>>>()?;
    Ok(Json(json!({"object":"list", "data":data})))
}

/// `GET /v1/models/{id}`: one callable model, 404 `model_not_installed` if
/// `id` is unknown, not downloaded, or still `needs_verification`.
pub async fn openai_model_by_id(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    authorize(&headers, &app, AccessLevel::User)?;
    if !callable_model_ids(&app)?
        .iter()
        .any(|candidate| candidate == &id)
    {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "model_not_installed",
            "This model is not installed",
        ));
    }
    let default_id = selected_id(&app)?;
    Ok(Json(openai_model_entry(&app, &id, default_id.as_deref())?))
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
    authorize(&headers, &app, AccessLevel::User)?;
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
    // A background load already in progress (currently only the startup
    // reload kicked off by `open_app_at_full` -- see `engine::
    // spawn_tracked_load`) is rejected outright rather than queued: the
    // simplest sane behaviour, and consistent with "a request uses the model
    // it entered the queue with" -- there is no in-progress request here to
    // preserve, just an unfinished load with no cancellation support. The
    // client is expected to poll `/readiness` and retry once it clears.
    if let Some(status) = app.loading.lock().map_err(internal)?.as_ref() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "model_loading",
            "Another model is still loading; retry once it finishes",
        )
        .with_details(json!({"model": status.model_id, "elapsed_ms": status.elapsed_ms()})));
    }
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
            "This model needs verification before selecting; run verify, or refresh for a drop-in model",
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
    app.live_caps
        .lock()
        .map_err(internal)?
        .insert(loader_id.clone(), crate::engine::caps_for(&model));
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
    // A request with no `model` field at all is treated as `model=default`:
    // OpenAI-compatible clients generally send it, but this server's own SDK
    // client often omits it, and `default` already means "whichever model is
    // currently selected" everywhere else `model` is accepted.
    let model = model.unwrap_or_else(|| "default".to_owned());
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

/// Resolve `id` to an installed, verified model, or the specific errors a
/// per-request model choice can hit (see the "openai-model-per-request"
/// goal): 404 `model_not_installed` for an unknown or not-downloaded id, 409
/// `needs_verification` for one that is downloaded but not yet verified.
/// Never triggers a download or any other side effect.
fn require_installed_verified(app: &App, id: &str) -> ApiResult<crate::store::InstalledFile> {
    match installed_file(app, id)? {
        Some(file) if !file.needs_verification => Ok(file),
        Some(_) => Err(ApiError::new(
            StatusCode::CONFLICT,
            "needs_verification",
            "This model needs verification before it can serve requests; run verify, or refresh for a drop-in model",
        )),
        None => Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "model_not_installed",
            "This model is not installed",
        )),
    }
}

/// Resolve the `model` multipart field to the model id this request must run
/// against: `"default"` (or, per convention, an omitted field -- see
/// `parse_transcription_multipart`) resolves to the default model, anything
/// else names that model directly. When `"default"` has nothing configured
/// yet, preserves the original distinction between "the default is still
/// loading at startup" (503 `model_loading`) and "nothing is default at all"
/// (503 `server_not_ready`) -- this is about the *startup* reload, not a
/// per-request swap, so it is checked once, up front, without touching the
/// queue.
fn resolve_requested_model(app: &App, requested: &str) -> ApiResult<String> {
    if requested != "default" {
        return Ok(requested.to_owned());
    }
    if let Some(id) = selected_id(app)? {
        return Ok(id);
    }
    if let Ok(loading) = app.loading.lock() {
        if let Some(status) = loading.as_ref() {
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "model_loading",
                "The selected model is still loading",
            )
            .with_details(json!({"model": status.model_id, "elapsed_ms": status.elapsed_ms()})));
        }
    }
    let mut error = ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "server_not_ready",
        "No model loaded",
    );
    if let Ok(Some(operation_id)) = crate::operations::active_operation_id(app) {
        error = error.with_details(json!({"operation_id": operation_id}));
    }
    Err(error)
}

/// How often `bind_or_swap_model` polls while waiting out a background load
/// for the same model it needs (see the loop's comment below).
const MODEL_SWAP_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Called once this request holds the sole inference permit: make `target`
/// the loaded model, swapping it in if some other model (or nothing) is
/// loaded, then return a clone of the handle, id, backend diagnostic and
/// effective capabilities to run against. Never runs two loads at once and
/// never swaps mid-request -- the permit this request holds is the same one
/// a running inference holds, so nothing else can be mid-run while this
/// function is loading or swapping.
///
/// If a background load already in progress (currently only the startup
/// reload -- see `engine::spawn_tracked_load`) targets this exact model, this
/// waits for it instead of loading a second copy, bounded by
/// `wait_timeout` (the same `queue_wait_timeout_ms` setting the queue itself
/// honours) -- only once that is exceeded does this give up with 503
/// `model_loading`. A load this function starts itself is a plain blocking
/// call with no such wait: it either finishes or fails outright.
async fn bind_or_swap_model(
    app: &App,
    target: &str,
    wait_timeout: Option<Duration>,
) -> ApiResult<(
    transcribe_cpp::Model,
    String,
    BackendDiagnostic,
    EffectiveCaps,
)> {
    let mut waited = Duration::ZERO;
    loop {
        {
            let active = app.loaded.lock().map_err(internal)?;
            if let Some(loaded) = active.as_ref() {
                if loaded.id == target {
                    return Ok((
                        loaded.model.clone(),
                        loaded.id.clone(),
                        loaded.diagnostic.clone(),
                        loaded.caps.clone(),
                    ));
                }
            }
        }
        let loading_same_target = app
            .loading
            .lock()
            .map_err(internal)?
            .as_ref()
            .map(|status| status.model_id == target)
            .unwrap_or(false);
        if loading_same_target {
            if let Some(timeout) = wait_timeout {
                if waited >= timeout {
                    return Err(ApiError::new(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "model_loading",
                        "The requested model is still loading",
                    )
                    .with_details(json!({"model": target})));
                }
            }
            tokio::time::sleep(MODEL_SWAP_POLL_INTERVAL).await;
            waited += MODEL_SWAP_POLL_INTERVAL;
            continue;
        }
        // Nothing is already loading this exact model: load it ourselves.
        // Re-check installed/verified here too -- cheap, and closes the race
        // where a model was removed or started verification between the
        // pre-queue check and this request's turn.
        let installed = require_installed_verified(app, target)?;
        let path = installed.path.clone();
        let preference = backend_preference(app)?;
        let target_owned = target.to_owned();
        let load_result = tokio::task::spawn_blocking(move || load_engine(&path, &preference))
            .await
            .map_err(internal)?;
        match load_result {
            Ok((model, diagnostic)) => {
                let caps = crate::engine::caps_for(&model);
                app.live_caps
                    .lock()
                    .map_err(internal)?
                    .insert(target_owned.clone(), caps);
                *app.loaded.lock().map_err(internal)? =
                    Some(LoadedModel::new(target_owned, model, diagnostic));
                // Loop back around to read it out of `app.loaded` uniformly.
            }
            Err(error) => {
                // The previous model, if any, was never touched -- only this
                // request fails.
                return Err(
                    ApiError::new(StatusCode::CONFLICT, "model_load_failed", error)
                        .with_details(json!({"model": target})),
                );
            }
        }
    }
}

/// The shared pipeline behind `/v1/audio/transcriptions` and
/// `/v1/audio/translations`: parse -> decode -> resolve the requested model
/// -> queue -> bind/swap the loaded model -> plan -> run -> format. See
/// `parity-design.md` "Handler" and the "openai-model-per-request" goal.
async fn transcribe_or_translate(
    app: Arc<App>,
    headers: HeaderMap,
    multipart: Multipart,
    endpoint: Endpoint,
) -> ApiResult<Response> {
    authorize(&headers, &app, AccessLevel::User)?;
    let fields = parse_transcription_multipart(multipart).await?;

    // Decode/validate happens before joining the queue; only the actual
    // inference run below waits for a turn.
    let pcm = decode_wav(&fields.file)?;
    let samples = pcm.len();
    let audio_ms = (pcm.len() as u64 * 1000) / 16_000;

    // Resolve which model this request wants (never a download, never a
    // load yet) and confirm it is callable before this request ever joins
    // the queue.
    let target = resolve_requested_model(&app, &fields.model)?;
    require_installed_verified(&app, &target)?;

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

    // Now that this request holds the sole inference permit, make sure the
    // model it asked for is the one loaded -- swapping if needed. This is
    // the only place a swap happens: never before a request has its turn,
    // never while another request is mid-run (the permit rules that out).
    let (model, active_id, backend, caps) = bind_or_swap_model(&app, &target, wait_timeout).await?;

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

/// `POST /models/manage/refresh`: a durable operation (kind `refresh`)
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
    // The effective allow-list already resolved at `open_app_at_full` time
    // (CLI `--cors-origin` > stored `cors_allowed_origins` setting > default
    // empty). A `PATCH /v1/local/config` write to the setting takes effect
    // on the next start (`restart_required: true` in its response), matching
    // `bind_host`/`bind_port`.
    let cors = cors_layer(&app.cors_origins);
    Router::new()
        .route("/health", get(health))
        .route("/v1/local/update/validation", get(update_validation))
        .route("/readiness", get(readiness))
        .route("/v1/models", get(openai_models))
        .route("/v1/models/{id}", get(openai_model_by_id))
        .route(
            "/v1/audio/transcriptions",
            post(transcriptions).layer(DefaultBodyLimit::max(40 * 1024 * 1024)),
        )
        .route(
            "/v1/audio/translations",
            post(translations).layer(DefaultBodyLimit::max(40 * 1024 * 1024)),
        )
        .route("/v1/local/system", get(system_info))
        .route("/v1/local/config", get(get_config).patch(patch_config))
        .route("/v1/local/shutdown", post(shutdown_endpoint))
        .route("/models/manage", get(local_models))
        .route("/models/manage/refresh", post(refresh_models))
        .route("/models/manage/recommendations", get(recommendations))
        .route(
            "/models/manage/default",
            get(selected_model).delete(deselect_model),
        )
        .route("/models/manage/{id}/default", post(select_model))
        .route("/models/manage/{id}", delete(remove_model))
        .route("/models/manage/{id}/download", post(install_model))
        .route("/models/manage/{id}/verify", post(verify_model))
        .route(
            "/models/manage/import",
            post(import_model).layer(DefaultBodyLimit::max(3 * 1024 * 1024 * 1024usize)),
        )
        .route(
            "/models/manage/import-user",
            post(crate::import_user::import_user_models),
        )
        .route("/models/manage/operations/{id}", get(operation))
        .route(
            "/models/manage/operations/{id}/cancel",
            post(cancel_operation),
        )
        .layer(cors)
        .layer(axum::middleware::from_fn_with_state(
            app.clone(),
            update_gate,
        ))
        .with_state(app)
}

async fn update_validation(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    authorized(&headers, &app)?;
    let ready_model = app
        .loaded
        .lock()
        .map_err(internal)?
        .as_ref()
        .map(|m| m.id.clone());
    let loading = app.loading.lock().map_err(internal)?.is_some();
    let journal = crate::update_transaction::read_journal(&app.data_dir).map_err(internal)?;
    let active_operations: i64 = app
        .db
        .lock()
        .map_err(internal)?
        .query_row(
            "SELECT count(*) FROM operations WHERE state IN ('queued','running')",
            [],
            |r| r.get(0),
        )
        .map_err(internal)?;
    Ok(Json(json!({
        "version": env!("CARGO_PKG_VERSION"), "api_level": API_LEVEL,
        "network_mode": if app.network_custom { "custom" } else { app.network_mode.as_str() },
        "ready_model": ready_model, "loading": loading, "active_operations": active_operations,
        "transaction": journal.filter(|j| !j.phase.terminal()).map(|j| j.id),
    })))
}

async fn update_gate(
    State(app): State<Arc<App>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if crate::update_transaction::maintenance(&app.data_dir) {
        let allowed = matches!(
            (request.method().as_str(), request.uri().path()),
            (
                "GET",
                "/health" | "/readiness" | "/v1/local/update/validation"
            ) | ("POST", "/v1/local/shutdown")
        );
        if !allowed {
            if let Err(error) = authorize(request.headers(), &app, AccessLevel::User) {
                return error.into_response();
            }
            return ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "update_in_progress",
                "Update validation/recovery is in progress",
            )
            .into_response();
        }
    }
    next.run(request).await
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
    run_http_full(
        crate::app::data_dir(),
        cli_overrides,
        crate::app::BindOverrides::default(),
        None,
        crate::app::install_scope(),
        shutdown,
    )
    .await
    .map_err(|error| -> Box<dyn Error> { Box::new(std::io::Error::other(error.to_string())) })
}

/// Errors distinguished so the CLI can map them to the spec's exit codes:
/// single-instance lock held (3) vs bind failure / port in use (4).
#[derive(Debug)]
pub enum ServeError {
    AlreadyRunning,
    BindFailed(std::io::Error),
    Other(Box<dyn Error>),
}

impl std::fmt::Display for ServeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServeError::AlreadyRunning => write!(f, "another instance holds the data dir's lock"),
            ServeError::BindFailed(e) => write!(f, "failed to bind: {e}"),
            ServeError::Other(e) => write!(f, "{e}"),
        }
    }
}
impl std::error::Error for ServeError {}

/// Full foreground server run: opens `App` at `data_dir` with the given
/// overrides, acquires the single-instance lock, enforces the LAN token
/// guard, writes `server.json`, serves until `shutdown` resolves, then
/// cleans up the discovery file. Used by both `run` and the detached process
/// `start` spawns.
///
/// Port fallback (several users on one PC, see the `install-scope-and-
/// shared-access` goal): a per-user install (`scope`) whose port was not
/// explicitly requested (`bind_overrides.port.is_none()` -- it came from the
/// default or a stored setting, not `--port`) falls back to an OS-assigned
/// free loopback port when the preferred one is already taken (typically by
/// another user's server, or a machine-wide one). A machine-wide install
/// keeps its fixed port and always fails clearly if it's taken -- it has
/// priority. An explicit `--port` also always fails clearly rather than
/// silently moving to a different port the caller didn't ask for.
pub async fn run_http_full(
    data_dir: std::path::PathBuf,
    cli_overrides: crate::app::RuntimeLimits,
    bind_overrides: crate::app::BindOverrides,
    network_override: Option<crate::network::NetworkMode>,
    scope: crate::app::InstallScope,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), ServeError> {
    // Single-instance guard: held for the process lifetime (the returned
    // file's lock releases on drop/exit).
    let startup_guard = crate::update_transaction::startup_guard(&data_dir)
        .map_err(|e| ServeError::Other(e.into()))?;
    let _lock =
        crate::discovery::acquire_lock(&data_dir).map_err(|_| ServeError::AlreadyRunning)?;

    let mut app = crate::app::open_app_at_full(
        data_dir.clone(),
        cli_overrides,
        bind_overrides.clone(),
        network_override,
    )
    .map_err(ServeError::Other)?;

    // "Network modes" (see `crate::network`): an explicit `--host`/stored
    // `bind_host` override always wins and is left exactly as
    // `open_app_at_full` resolved it (`network_custom` is already true and
    // `network_state` already reports mode "custom"). Otherwise the resolved
    // `network_mode` decides the actual bind host: `local` forces loopback,
    // `lan`/`tailscale` both bind every interface and rely on the
    // `network_gate` middleware plus live detection to decide, per request,
    // whether a non-loopback caller is currently allowed (see
    // `crate::network::peer_allowed_non_loopback` for why this -- rather
    // than dynamically rebinding sockets as the network changes -- is the
    // chosen enforcement mechanism).
    if !app.network_custom {
        let forced_host = match app.network_mode {
            crate::network::NetworkMode::Local => crate::app::DEFAULT_BIND_HOST.to_owned(),
            crate::network::NetworkMode::Lan | crate::network::NetworkMode::Tailscale => {
                "0.0.0.0".to_owned()
            }
        };
        Arc::get_mut(&mut app)
            .expect("no other Arc<App> clone exists yet")
            .bind_host = forced_host;
        if !matches!(app.network_mode, crate::network::NetworkMode::Local) {
            // Resolve the initial live report synchronously (bounded by
            // `network::DETECT_TIMEOUT`) so the very first requests already
            // see an accurate `/health` and enforcement decision, rather
            // than waiting for the first periodic tick.
            let initial = refresh_network_state(app.network_mode).await;
            *app.network_state
                .write()
                .map_err(|e| ServeError::Other(Box::new(std::io::Error::other(e.to_string()))))? =
                initial;
        }
    }

    // LAN guard: refuse to start on a non-loopback bind unless the token is
    // present and non-empty. `token_file`/`open_app_at_full` already fail if
    // the token can't be created/read, but re-check explicitly here so a
    // future refactor of that path can't silently weaken this guard.
    let loopback = crate::discovery::is_loopback_host(&app.bind_host);
    if !loopback {
        if app.token.is_empty() {
            return Err(ServeError::Other(
                "refusing to bind a non-loopback address without a usable auth token".into(),
            ));
        }
        eprintln!(
            "warning: binding to {}:{} is reachable from the network; all routes except /health require the bearer token",
            app.bind_host, app.bind_port
        );
    }

    let preferred_address: SocketAddr = format!("{}:{}", app.bind_host, app.bind_port)
        .parse()
        .map_err(|e| ServeError::Other(Box::new(e)))?;
    let listener = match tokio::net::TcpListener::bind(preferred_address).await {
        Ok(listener) => listener,
        Err(error)
            if error.kind() == std::io::ErrorKind::AddrInUse
                && scope == crate::app::InstallScope::PerUser
                && bind_overrides.port.is_none() =>
        {
            let fallback_address: SocketAddr = format!("{}:0", app.bind_host)
                .parse()
                .map_err(|e| ServeError::Other(Box::new(e)))?;
            let listener = tokio::net::TcpListener::bind(fallback_address)
                .await
                .map_err(ServeError::BindFailed)?;
            let actual_port = listener
                .local_addr()
                .map_err(ServeError::BindFailed)?
                .port();
            eprintln!(
                "port {} is already in use; bound a free port instead: {}",
                app.bind_port, actual_port
            );
            // Still the sole owner of this Arc (no clone taken yet), so this
            // mutates the same App the router below and every request handler
            // will share -- `/health`, `/v1/local/config`, and `server.json`
            // all end up reporting the port actually bound, not the one that
            // was merely preferred.
            Arc::get_mut(&mut app)
                .expect("no other Arc<App> clone exists yet")
                .bind_port = actual_port;
            listener
        }
        Err(error) => return Err(ServeError::BindFailed(error)),
    };
    let address = listener.local_addr().map_err(ServeError::BindFailed)?;
    // The network-reachability gate is only meaningful for real serving (it
    // needs a real peer address via `ConnectInfo`, provided below by
    // `into_make_service_with_connect_info`); the shared `router()` used
    // directly by unit tests stays ungated so those tests are unaffected.
    let router = router(app.clone()).layer(axum::middleware::from_fn_with_state(
        app.clone(),
        network_gate,
    ));
    println!(
        "listening on {address}; token file: {}",
        app.data_dir.join("auth.token").display()
    );
    let mut info = crate::discovery::ServerInfo::new(app.bind_host.clone(), app.bind_port);
    let launch = crate::update_transaction::Launch {
        executable: std::env::current_exe().map_err(|e| ServeError::Other(e.into()))?,
        service: crate::app::is_service_mode(),
        flags: crate::cli::RunFlags {
            data_dir: Some(data_dir.clone()),
            host: bind_overrides.host,
            port: bind_overrides.port,
            network: network_override,
            limits: cli_overrides,
            cors_origins: bind_overrides.cors_origins.unwrap_or_default(),
        },
    };
    crate::update_transaction::atomic_json(&data_dir.join("last-launch.json"), &launch)
        .map_err(|e| ServeError::Other(e.into()))?;
    info.launch = Some(launch);
    if let Err(error) = crate::discovery::write_server_json(&data_dir, &info) {
        eprintln!("warning: could not write server.json: {error}");
    }
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    drop(startup_guard);
    *app.shutdown.lock().unwrap() = Some(shutdown_tx);
    // Periodic recheck (every `network::RECHECK_INTERVAL`) so a `lan`/
    // `tailscale` server picks up a network change (a laptop moving from a
    // Private home network to a Public coffee-shop one, or Tailscale
    // starting up after the server did) without a restart. Aborted
    // automatically when the task's `Arc<App>` is the last reference to drop
    // (server shutdown) since it's a plain detached `tokio::spawn`, not
    // tracked further -- there is nothing to cancel explicitly because it
    // only ever reads live state and writes `network_state`, both cheap and
    // safe to do right up until process exit.
    if !app.network_custom && !matches!(app.network_mode, crate::network::NetworkMode::Local) {
        let recheck_app = app.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(crate::network::RECHECK_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                let report = refresh_network_state(recheck_app.network_mode).await;
                if let Ok(mut state) = recheck_app.network_state.write() {
                    *state = report;
                }
            }
        });
    }
    let combined_shutdown = async move {
        tokio::select! {
            _ = shutdown => {},
            _ = shutdown_rx => {},
        }
    };
    let result = axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(combined_shutdown)
    .await;
    crate::discovery::remove_server_json(&data_dir);
    result.map_err(|e| ServeError::Other(Box::new(e)))
}

/// Runs the live detection for `mode` (`lan`/`tailscale` only) off the async
/// executor thread (both detectors shell out and poll synchronously) and
/// turns the result into the report shown on `/health` and used by
/// `network_gate`. Never panics or hangs: `spawn_blocking` failing (executor
/// shutting down) is treated the same as detection failing.
async fn refresh_network_state(mode: crate::network::NetworkMode) -> crate::network::NetworkReport {
    match mode {
        crate::network::NetworkMode::Local => crate::network::NetworkReport::local(mode),
        crate::network::NetworkMode::Lan => {
            let is_private = tokio::task::spawn_blocking(|| {
                crate::network::detect_lan_private(crate::network::DETECT_TIMEOUT)
            })
            .await
            .unwrap_or(false);
            if is_private {
                crate::network::NetworkReport::lan(Vec::new())
            } else {
                crate::network::NetworkReport::local_fallback(
                    mode,
                    "no active network connection is Private or Domain -- staying local-only",
                )
            }
        }
        crate::network::NetworkMode::Tailscale => {
            let address = tokio::task::spawn_blocking(|| {
                crate::network::detect_tailscale_ipv4(crate::network::DETECT_TIMEOUT)
            })
            .await
            .unwrap_or(None);
            match address {
                Some(ip) => crate::network::NetworkReport::tailscale(ip),
                None => crate::network::NetworkReport::local_fallback(
                    mode,
                    "tailscale is not running or has no address -- staying local-only",
                ),
            }
        }
    }
}

/// Rejects a non-loopback, non-`/health` request when the live network
/// report says the current mode doesn't currently allow it (see
/// `crate::network::peer_allowed_non_loopback`). Applied as an extra layer
/// in `run_http_full` only -- see the comment where it's attached.
async fn network_gate(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if request.uri().path() == "/health" || caller_is_loopback(peer) {
        return next.run(request).await;
    }
    let report = match app.network_state.read() {
        Ok(state) => state.clone(),
        Err(_) => return internal("network state lock poisoned").into_response(),
    };
    if crate::network::peer_allowed_non_loopback(
        app.network_mode,
        app.network_custom,
        &report,
        peer.ip(),
    ) {
        return next.run(request).await;
    }
    ApiError::new(
        StatusCode::FORBIDDEN,
        "network_not_private",
        format!(
            "not reachable from another device right now ({})",
            report
                .reason
                .as_deref()
                .unwrap_or("network mode does not currently allow this")
        ),
    )
    .into_response()
}

/// Pure decision used by [`shutdown_endpoint`] (and exercised directly by
/// tests without spinning up a real connection): only a loopback peer
/// address may request shutdown, regardless of token validity.
pub fn caller_is_loopback(peer: SocketAddr) -> bool {
    peer.ip().is_loopback()
}

/// `POST /v1/local/shutdown`: triggers graceful axum shutdown. Loopback-only
/// (rejected with 403 even when the bearer token is valid) and otherwise
/// token-protected like every other mutating route.
pub async fn shutdown_endpoint(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    if !caller_is_loopback(peer) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "loopback_only",
            "The shutdown endpoint only accepts loopback callers",
        ));
    }
    authorized(&headers, &app)?;
    if let Some(sender) = app.shutdown.lock().map_err(internal)?.take() {
        let _ = sender.send(());
    }
    Ok(Json(json!({"stopping": true})))
}

#[cfg(test)]
mod router_tests {
    use super::*;
    use crate::app::open_app_at;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;
    use uuid::Uuid;

    /// Default (nothing stored, no CLI override): no browser origin is
    /// allowed, so a preflight for an arbitrary origin gets no CORS headers.
    /// See the "locked down by default" decision, 2026-09-27.
    #[tokio::test]
    async fn options_preflight_default_locked_down_reports_no_cors_headers() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = open_app_at(path.clone()).unwrap();
        assert!(app.cors_origins.is_empty());
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
        assert!(!response
            .headers()
            .contains_key("access-control-allow-origin"));
        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    /// A request with no `Origin` header at all (every non-browser client:
    /// Voice Typer, SDK, CLI, curl) is unaffected by the CORS layer either
    /// way; it still needs the token like any other protected route, but
    /// gets no CORS-related rejection or header.
    #[tokio::test]
    async fn request_without_origin_header_is_unaffected_by_cors() {
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
        assert!(!response
            .headers()
            .contains_key("access-control-allow-origin"));
        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    /// Once an origin is explicitly allowed (via the persisted setting, as a
    /// real client would get it there through `PATCH /v1/local/config`), its
    /// preflight succeeds with the expected headers (methods, Authorization,
    /// Content-Type) and an actual request from that origin gets
    /// `Access-Control-Allow-Origin` back; a different, non-allowed origin
    /// still gets no CORS headers.
    #[tokio::test]
    async fn allowed_origin_gets_cors_headers_and_preflight_others_do_not() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = crate::app::open_app_at_full(
            path.clone(),
            crate::app::RuntimeLimits::default(),
            crate::app::BindOverrides {
                host: None,
                port: None,
                cors_origins: Some(vec!["http://localhost:3000".to_owned()]),
            },
            None,
        )
        .unwrap();
        let router = router(app.clone());

        let preflight = Request::builder()
            .method("OPTIONS")
            .uri("/v1/audio/transcriptions")
            .header("origin", "http://localhost:3000")
            .header("access-control-request-method", "POST")
            .header(
                "access-control-request-headers",
                "authorization,content-type",
            )
            .body(Body::empty())
            .unwrap();
        let response = router.clone().oneshot(preflight).await.unwrap();
        assert!(response.status().is_success());
        assert_eq!(
            response
                .headers()
                .get("access-control-allow-origin")
                .unwrap(),
            "http://localhost:3000"
        );
        let allow_headers = response
            .headers()
            .get("access-control-allow-headers")
            .unwrap()
            .to_str()
            .unwrap()
            .to_ascii_lowercase();
        assert!(allow_headers.contains("authorization"));
        assert!(allow_headers.contains("content-type"));

        let other_origin_preflight = Request::builder()
            .method("OPTIONS")
            .uri("/v1/audio/transcriptions")
            .header("origin", "http://evil.example")
            .header("access-control-request-method", "POST")
            .body(Body::empty())
            .unwrap();
        let response = router.oneshot(other_origin_preflight).await.unwrap();
        assert!(!response
            .headers()
            .contains_key("access-control-allow-origin"));

        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    /// `"*"` is only ever the enforced allow-list when explicitly set (never
    /// the implicit default): an app opened with it explicitly configured
    /// allows any origin's preflight.
    #[tokio::test]
    async fn wildcard_when_explicitly_set_allows_any_origin() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = crate::app::open_app_at_full(
            path.clone(),
            crate::app::RuntimeLimits::default(),
            crate::app::BindOverrides {
                host: None,
                port: None,
                cors_origins: Some(vec!["*".to_owned()]),
            },
            None,
        )
        .unwrap();
        let router = router(app.clone());
        let preflight = Request::builder()
            .method("OPTIONS")
            .uri("/v1/audio/transcriptions")
            .header("origin", "http://anything.example")
            .header("access-control-request-method", "POST")
            .body(Body::empty())
            .unwrap();
        let response = router.oneshot(preflight).await.unwrap();
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
            .uri("/models/manage/parakeet-unified-en-0.6b/download")
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

    /// A transcription naming a model id that is neither a known catalog
    /// model nor an installed custom one is refused before ever joining the
    /// queue -- see the "openai-model-per-request" goal's "not installed /
    /// unknown -> 404 model_not_installed" rule. Never a download.
    #[tokio::test]
    async fn transcription_naming_an_unknown_model_is_404_model_not_installed() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = open_app_at(path.clone()).unwrap();
        let token = app.token.clone();
        let router = router(app.clone());
        let boundary = "X-BOUNDARY";
        let body = multipart_body(
            boundary,
            &[("model", "not-a-real-model-id")],
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
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["code"], "model_not_installed");
        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    /// A transcription naming a model that is installed but still
    /// `needs_verification` is refused with `409 needs_verification`, the
    /// same check `select` already makes -- never silently served and never
    /// auto-verified.
    #[tokio::test]
    async fn transcription_naming_an_unverified_model_is_409_needs_verification() {
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
        let boundary = "X-BOUNDARY";
        let body = multipart_body(
            boundary,
            &[("model", "custom-mine-abc12345")],
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
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["code"], "needs_verification");
        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    /// `GET /v1/models` lists only callable (installed and verified) models,
    /// never a `needs_verification` one, and marks the default with
    /// `"default": true`. `GET /v1/models/{id}` mirrors the same rule for one
    /// model, 404 for a non-callable id.
    #[tokio::test]
    async fn openai_models_lists_only_callable_models_with_default_flag() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = open_app_at(path.clone()).unwrap();
        let token = app.token.clone();
        let dropdir = parent.join(format!("stt-server-next-dropin-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dropdir).unwrap();
        let verified_path = dropdir.join("verified.gguf");
        std::fs::write(&verified_path, b"verified file bytes").unwrap();
        let unverified_path = dropdir.join("unverified.gguf");
        std::fs::write(&unverified_path, b"unverified file bytes").unwrap();
        {
            let db = app.db.lock().unwrap();
            db.execute(
                "INSERT INTO installed(id,path,sha256,source,custom_arch) VALUES('custom-verified-aaaaaaaa',?1,'x','user_folder','whisper')",
                rusqlite::params![verified_path.to_string_lossy().as_ref()],
            )
            .unwrap();
            db.execute(
                "INSERT INTO installed(id,path,sha256,source,needs_verification,custom_arch) VALUES('custom-unverified-bbbbbbbb',?1,'x','user_folder',1,'whisper')",
                rusqlite::params![unverified_path.to_string_lossy().as_ref()],
            )
            .unwrap();
            db.execute(
                "INSERT INTO settings(key,value) VALUES('selected_model','custom-verified-aaaaaaaa')",
                [],
            )
            .unwrap();
        }
        let router = router(app.clone());
        let request = Request::builder()
            .method("GET")
            .uri("/v1/models")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        let ids: Vec<&str> = body["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["id"].as_str().unwrap())
            .collect();
        assert!(ids.contains(&"custom-verified-aaaaaaaa"));
        assert!(
            !ids.contains(&"custom-unverified-bbbbbbbb"),
            "an unverified model must never be listed as callable"
        );
        let default_entry = body["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["id"] == "custom-verified-aaaaaaaa")
            .unwrap();
        assert_eq!(default_entry["object"], "model");
        assert_eq!(default_entry["owned_by"], "local");
        assert_eq!(default_entry["default"], true);
        assert!(default_entry["capabilities"].is_object());

        // GET /v1/models/{id}: 200 for the callable one, 404 for the
        // unverified (not-yet-callable) one.
        let get_one = |id: &'static str, token: String| {
            let router = router.clone();
            async move {
                let request = Request::builder()
                    .method("GET")
                    .uri(format!("/v1/models/{id}"))
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap();
                router.oneshot(request).await.unwrap()
            }
        };
        let response = get_one("custom-verified-aaaaaaaa", token.clone()).await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["default"], true);

        let response = get_one("custom-unverified-bbbbbbbb", token.clone()).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        drop(router);
        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    /// `GET /v1/models/{id}` reports top-level `languages`/`language_detect`
    /// for a callable catalog model, falling back to the catalog's static
    /// claim when this process has never loaded it.
    #[tokio::test]
    async fn openai_model_entry_reports_catalog_languages_when_unloaded() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = open_app_at(path.clone()).unwrap();
        let token = app.token.clone();
        let model = app.catalog[0].clone();
        {
            let db = app.db.lock().unwrap();
            db.execute(
                "INSERT INTO installed(id,path,sha256,source) VALUES(?1,'unused','x','catalog_download')",
                rusqlite::params![model.slug],
            )
            .unwrap();
        }
        let router = router(app.clone());
        let request = Request::builder()
            .method("GET")
            .uri(format!("/v1/models/{}", model.slug))
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["languages"], json!(model.languages));
        assert_eq!(
            body["language_detect"],
            json!(model.capabilities.lang_detect)
        );

        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    /// `GET /v1/models/{id}` prefers the live `EffectiveCaps` view for
    /// `languages`/`language_detect` once this process has loaded the model,
    /// even when it differs from the catalog's static claim.
    #[tokio::test]
    async fn openai_model_entry_reports_live_languages_when_loaded() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = open_app_at(path.clone()).unwrap();
        let token = app.token.clone();
        let model = app.catalog[0].clone();
        {
            let db = app.db.lock().unwrap();
            db.execute(
                "INSERT INTO installed(id,path,sha256,source) VALUES(?1,'unused','x','catalog_download')",
                rusqlite::params![model.slug],
            )
            .unwrap();
        }
        let live_languages = vec!["zz".to_string(), "yy".to_string()];
        let live_caps = crate::capabilities::EffectiveCaps::new(crate::capabilities::LoadedCaps {
            arch: model.architecture.clone(),
            languages: live_languages.clone(),
            translate_target_languages: vec![],
            supports_translate: false,
            supports_language_detect: true,
            max_timestamp_kind: crate::capabilities::TimestampGranularity::None,
            feature_initial_prompt_flag: false,
            whisper_ext_accepted: None,
            prompt_max_tokens: None,
            timestamp_granularity_rejected: false,
        });
        app.live_caps
            .lock()
            .unwrap()
            .insert(model.slug.clone(), live_caps);

        let router = router(app.clone());
        let request = Request::builder()
            .method("GET")
            .uri(format!("/v1/models/{}", model.slug))
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["languages"], json!(live_languages));
        assert_eq!(body["language_detect"], json!(true));

        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    /// `/readiness` while a background load is in progress (see
    /// `engine::spawn_tracked_load`/`app::open_app_at_full`): 503 with a
    /// `"loading model"` reason, the loading model's id, and `elapsed_ms`,
    /// distinct from the plain "no selected model" case above.
    #[tokio::test]
    async fn readiness_reports_loading_model_and_elapsed_while_loading() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = open_app_at(path.clone()).unwrap();
        *app.loading.lock().unwrap() = Some(crate::engine::LoadingStatus::new(
            "voxtral-small-24b".to_owned(),
        ));
        let token = app.token.clone();
        let router = router(app.clone());
        let request = Request::builder()
            .method("GET")
            .uri("/readiness")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["status"], "not_ready");
        assert_eq!(body["reason"], "loading model");
        assert_eq!(body["model"], "voxtral-small-24b");
        assert!(body["elapsed_ms"].is_u64());
        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    /// A transcription request while the selected model is still loading
    /// gets the dedicated `model_loading` 503, not the generic
    /// `server_not_ready` -- see docs/client-contract.md's error table.
    #[tokio::test]
    async fn transcription_while_model_is_loading_is_503_model_loading() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = open_app_at(path.clone()).unwrap();
        *app.loading.lock().unwrap() =
            Some(crate::engine::LoadingStatus::new("slow-model".to_owned()));
        let token = app.token.clone();
        let router = router(app.clone());
        let boundary = "X-BOUNDARY";
        let body = multipart_body(boundary, &[("model", "default")], &sample_wav_bytes());
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
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["code"], "model_loading");
        assert_eq!(body["error"]["details"]["model"], "slow-model");
        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    /// Selecting another model while a background load is already in
    /// progress is rejected outright (409 `model_loading`) rather than
    /// queued -- the simplest sane behaviour for a load with no
    /// cancellation support. See `select_model`.
    #[tokio::test]
    async fn select_model_while_loading_is_409_model_loading() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = open_app_at(path.clone()).unwrap();
        *app.loading.lock().unwrap() =
            Some(crate::engine::LoadingStatus::new("slow-model".to_owned()));
        let token = app.token.clone();
        let router = router(app.clone());
        let request = Request::builder()
            .method("POST")
            .uri("/models/manage/some-other-model/default")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["code"], "model_loading");
        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn transcriptions_without_model_field_defaults_to_default_model() {
        // A request with no `model` field at all used to be a 400
        // `missing_model`. It's now treated as `model=default`: reaching the
        // loaded-model check (503 `server_not_ready`, no model loaded in this
        // test) proves the multipart parse no longer rejects it.
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = open_app_at(path.clone()).unwrap();
        let token = app.token.clone();
        let router = router(app.clone());
        let boundary = "X-BOUNDARY";
        let body = multipart_body(boundary, &[], &sample_wav_bytes());
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
            .uri("/models/manage/refresh")
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
                .uri(format!("/models/manage/operations/{operation_id}"))
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
            .uri("/models/manage/custom-mine-abc12345")
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
            .uri("/models/manage/custom-mine-abc12345/default")
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

    fn with_peer(mut request: Request<Body>, peer: SocketAddr) -> Request<Body> {
        request.extensions_mut().insert(ConnectInfo(peer));
        request
    }

    fn loopback_peer() -> SocketAddr {
        "127.0.0.1:55555".parse().unwrap()
    }

    fn lan_peer() -> SocketAddr {
        "192.168.1.50:55555".parse().unwrap()
    }

    #[test]
    fn caller_is_loopback_accepts_v4_and_v6_loopback_only() {
        assert!(caller_is_loopback("127.0.0.1:1".parse().unwrap()));
        assert!(caller_is_loopback("[::1]:1".parse().unwrap()));
        assert!(!caller_is_loopback(lan_peer()));
        assert!(!caller_is_loopback("0.0.0.0:1".parse().unwrap()));
    }

    #[tokio::test]
    async fn health_reports_version_and_api_level() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = open_app_at(path.clone()).unwrap();
        let router = router(app.clone());
        let request = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(body["api_level"], API_LEVEL);
        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    /// `default_model`/`loaded_model` (the "openai-model-per-request" goal):
    /// both `null` when nothing has ever been selected or loaded.
    #[tokio::test]
    async fn health_and_readiness_report_default_and_loaded_model_fields() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = open_app_at(path.clone()).unwrap();
        let token = app.token.clone();
        let router = router(app.clone());

        let request = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(body["default_model"].is_null());
        assert!(body["loaded_model"].is_null());

        let request = Request::builder()
            .method("GET")
            .uri("/readiness")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(body["default_model"].is_null());
        assert!(body["loaded_model"].is_null());
        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn health_reports_local_mode_by_default_with_no_reason_or_addresses() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = open_app_at(path.clone()).unwrap();
        let router = router(app.clone());
        let request = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["network"]["mode"], "local");
        assert_eq!(body["network"]["effective"], "local");
        assert!(body["network"].get("reason").is_none());
        assert!(body["network"].get("addresses").is_none());
        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn health_reports_custom_mode_for_an_explicit_host_override() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = crate::app::open_app_at_full(
            path.clone(),
            crate::app::RuntimeLimits::default(),
            crate::app::BindOverrides {
                host: Some("0.0.0.0".to_owned()),
                port: None,
                cors_origins: None,
            },
            None,
        )
        .unwrap();
        let router = router(app.clone());
        let request = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["network"]["mode"], "custom");
        assert_eq!(body["network"]["effective"], "custom");
        assert_eq!(body["network"]["addresses"][0], "0.0.0.0");
        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    /// `network_gate` is only attached in `run_http_full`, not the shared
    /// `router()` (see the comment where it's attached) -- built here
    /// directly the same way, so these tests exercise it without spinning up
    /// a real TCP listener.
    fn gated_router(app: Arc<App>) -> Router {
        router(app.clone()).layer(axum::middleware::from_fn_with_state(app, network_gate))
    }

    #[tokio::test]
    async fn network_gate_allows_health_from_anywhere_ungated() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = open_app_at(path.clone()).unwrap();
        let router = gated_router(app.clone());
        let request = with_peer(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
            lan_peer(),
        );
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn network_gate_rejects_lan_peer_when_mode_is_local() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = open_app_at(path.clone()).unwrap();
        let token = app.token.clone();
        let router = gated_router(app.clone());
        let request = with_peer(
            Request::builder()
                .uri("/v1/local/system")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
            lan_peer(),
        );
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["code"], "network_not_private");
        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn network_gate_allows_loopback_peer_regardless_of_mode() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = open_app_at(path.clone()).unwrap();
        let token = app.token.clone();
        let router = gated_router(app.clone());
        let request = with_peer(
            Request::builder()
                .uri("/v1/local/system")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
            loopback_peer(),
        );
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn network_gate_allows_lan_peer_once_the_live_report_says_lan() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = crate::app::open_app_at_full(
            path.clone(),
            crate::app::RuntimeLimits::default(),
            crate::app::BindOverrides::default(),
            Some(crate::network::NetworkMode::Lan),
        )
        .unwrap();
        let token = app.token.clone();
        // Simulate what the initial-detection step in `run_http_full` (or a
        // periodic recheck) would write once Windows reports a Private
        // network -- this test does not shell out to PowerShell.
        *app.network_state.write().unwrap() = crate::network::NetworkReport::lan(Vec::new());
        let router = gated_router(app.clone());
        let request = with_peer(
            Request::builder()
                .uri("/v1/local/system")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
            lan_peer(),
        );
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn network_gate_allows_a_custom_host_override_from_any_peer() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = crate::app::open_app_at_full(
            path.clone(),
            crate::app::RuntimeLimits::default(),
            crate::app::BindOverrides {
                host: Some("0.0.0.0".to_owned()),
                port: None,
                cors_origins: None,
            },
            None,
        )
        .unwrap();
        let token = app.token.clone();
        let router = gated_router(app.clone());
        let request = with_peer(
            Request::builder()
                .uri("/v1/local/system")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
            lan_peer(),
        );
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn config_patch_rejects_invalid_network_mode() {
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
            .body(Body::from(r#"{"network_mode":"public"}"#))
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["code"], "invalid_network_mode");
        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn config_patch_accepts_a_valid_network_mode_and_reports_restart_required() {
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
            .body(Body::from(r#"{"network_mode":"lan"}"#))
            .unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["network_mode"], "lan");
        assert_eq!(body["restart_required"], true);

        let request = Request::builder()
            .uri("/v1/local/config")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["network_mode"], "lan");
        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn shutdown_without_token_is_unauthorized() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = open_app_at(path.clone()).unwrap();
        let router = router(app.clone());
        let request = with_peer(
            Request::builder()
                .method("POST")
                .uri("/v1/local/shutdown")
                .body(Body::empty())
                .unwrap(),
            loopback_peer(),
        );
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn shutdown_from_non_loopback_peer_is_forbidden_even_with_a_valid_token() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = open_app_at(path.clone()).unwrap();
        let token = app.token.clone();
        let router = router(app.clone());
        let request = with_peer(
            Request::builder()
                .method("POST")
                .uri("/v1/local/shutdown")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
            lan_peer(),
        );
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn shutdown_from_loopback_with_valid_token_succeeds_and_fires_the_signal() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = open_app_at(path.clone()).unwrap();
        let token = app.token.clone();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        *app.shutdown.lock().unwrap() = Some(tx);
        let router = router(app.clone());
        let request = with_peer(
            Request::builder()
                .method("POST")
                .uri("/v1/local/shutdown")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
            loopback_peer(),
        );
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        rx.await.expect("shutdown signal should have fired");
        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn config_get_reports_effective_and_stored_bind() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = crate::app::open_app_at_full(
            path.clone(),
            crate::app::RuntimeLimits::default(),
            crate::app::BindOverrides {
                host: Some("0.0.0.0".to_owned()),
                port: Some(54400),
                cors_origins: None,
            },
            None,
        )
        .unwrap();
        let token = app.token.clone();
        let router = router(app.clone());
        let request = Request::builder()
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
        assert_eq!(body["bind"], "0.0.0.0:54400");
        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn config_patch_rejects_invalid_bind_host_and_port() {
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
            .body(Body::from(r#"{"bind_host":"not-an-ip"}"#))
            .unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let request = Request::builder()
            .method("PATCH")
            .uri("/v1/local/config")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"bind_port":0}"#))
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn config_patch_accepts_valid_bind_host_and_port_and_reports_restart_required() {
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
            .body(Body::from(r#"{"bind_host":"0.0.0.0","bind_port":54402}"#))
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["restart_required"], true);
        let (stored_host, stored_port) = crate::store::bind_settings(&app).unwrap();
        assert_eq!(stored_host.as_deref(), Some("0.0.0.0"));
        assert_eq!(stored_port, Some(54402));
        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    /// Route classification for the "Access levels on a shared server" goal
    /// (see `docs/client-contract.md`'s route table). Each entry is
    /// (method, path, body) for a route that must be reachable with the
    /// user token; junk ids are fine since `authorize` runs before any
    /// business logic touches them.
    fn user_routes() -> Vec<(&'static str, &'static str, Option<&'static str>)> {
        vec![
            ("GET", "/readiness", None),
            ("GET", "/v1/models", None),
            ("GET", "/models/manage", None),
            ("GET", "/models/manage/default", None),
            ("GET", "/v1/local/system", None),
            ("GET", "/models/manage/recommendations", None),
            ("GET", "/models/manage/operations/not-a-real-id", None),
        ]
    }

    /// Admin-only routes: install, import, verify, cancel, select, unload,
    /// remove, refresh drop-in, PATCH config, shutdown. `import` needs a
    /// well-formed (if empty) multipart body or axum's extractor rejects the
    /// request before `authorize` ever runs.
    fn admin_routes() -> Vec<(&'static str, &'static str, Option<&'static str>)> {
        vec![
            ("GET", "/v1/local/config", None),
            ("PATCH", "/v1/local/config", Some(r#"{}"#)),
            ("POST", "/models/manage/refresh", None),
            ("DELETE", "/models/manage/default", None),
            ("POST", "/models/manage/not-a-real-id/default", None),
            ("DELETE", "/models/manage/not-a-real-id", None),
            ("POST", "/models/manage/not-a-real-id/download", None),
            ("POST", "/models/manage/not-a-real-id/verify", None),
            (
                "POST",
                "/models/manage/operations/not-a-real-id/cancel",
                None,
            ),
        ]
    }

    fn build_request(
        method: &str,
        path: &str,
        body: Option<&'static str>,
        token: &str,
    ) -> Request<Body> {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header("authorization", format!("Bearer {token}"));
        if body.is_some() {
            builder = builder.header("content-type", "application/json");
        }
        builder
            .body(body.map(Body::from).unwrap_or_else(Body::empty))
            .unwrap()
    }

    #[tokio::test]
    async fn user_token_is_allowed_on_every_user_route() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = open_app_at(path.clone()).unwrap();
        let user_token = app.user_token.clone();
        for (method, route_path, body) in user_routes() {
            let router = router(app.clone());
            let request = build_request(method, route_path, body, &user_token);
            let response = router.oneshot(request).await.unwrap();
            assert_ne!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "{method} {route_path} should accept the user token"
            );
            assert_ne!(
                response.status(),
                StatusCode::FORBIDDEN,
                "{method} {route_path} should accept the user token"
            );
        }
        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn user_token_is_forbidden_with_admin_required_on_every_admin_route() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = open_app_at(path.clone()).unwrap();
        let user_token = app.user_token.clone();
        for (method, route_path, body) in admin_routes() {
            let router = router(app.clone());
            let request = build_request(method, route_path, body, &user_token);
            let response = router.oneshot(request).await.unwrap();
            assert_eq!(
                response.status(),
                StatusCode::FORBIDDEN,
                "{method} {route_path} should reject the user token with 403"
            );
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let json_body: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(
                json_body["error"]["code"], "admin_required",
                "{method} {route_path} should report admin_required"
            );
        }

        // The shutdown endpoint is loopback-gated ahead of auth, so it needs
        // its own request with peer info.
        let shutdown_router = router(app.clone());
        let request = with_peer(
            Request::builder()
                .method("POST")
                .uri("/v1/local/shutdown")
                .header("authorization", format!("Bearer {user_token}"))
                .body(Body::empty())
                .unwrap(),
            loopback_peer(),
        );
        let response = shutdown_router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json_body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json_body["error"]["code"], "admin_required");

        // Import needs a syntactically valid (if empty) multipart body.
        let import_router = router(app.clone());
        let request = Request::builder()
            .method("POST")
            .uri("/models/manage/import")
            .header("authorization", format!("Bearer {user_token}"))
            .header("content-type", "multipart/form-data; boundary=X")
            .body(Body::from("--X--\r\n"))
            .unwrap();
        let response = import_router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn admin_token_is_allowed_on_every_route_user_and_admin() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = open_app_at(path.clone()).unwrap();
        let admin_token = app.token.clone();
        for (method, route_path, body) in
            user_routes().into_iter().chain(admin_routes().into_iter())
        {
            let router = router(app.clone());
            let request = build_request(method, route_path, body, &admin_token);
            let response = router.oneshot(request).await.unwrap();
            assert_ne!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "{method} {route_path} should accept the admin token"
            );
            assert_ne!(
                response.status(),
                StatusCode::FORBIDDEN,
                "{method} {route_path} should accept the admin token"
            );
        }
        // `refresh` (and possibly other admin routes above) spawns a
        // background `tokio::spawn` task that touches the data dir; give it
        // a moment to finish before deleting the dir, or Windows can refuse
        // the delete with "used by another process" (see
        // `refresh_returns_202_and_the_operation_completes` for the same
        // pattern, polled instead of slept there because it asserts on the
        // outcome; here we only need it to be done, not what it did).
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    /// The old `/v1/local/models*`, `/v1/local/recommendations` and
    /// `/v1/local/operations*` paths were removed outright (no clients yet,
    /// per the `openai-model-per-request` goal's second slice) and must 404,
    /// not fall through to some other handler.
    #[tokio::test]
    async fn old_local_models_paths_are_gone() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = open_app_at(path.clone()).unwrap();
        let token = app.token.clone();
        let old_paths: Vec<(&str, &str)> = vec![
            ("GET", "/v1/local/models"),
            ("GET", "/v1/local/models/selected"),
            ("DELETE", "/v1/local/models/selected"),
            ("GET", "/v1/local/recommendations"),
            ("POST", "/v1/local/models/refresh"),
            ("POST", "/v1/local/models/not-a-real-id/select"),
            ("POST", "/v1/local/models/not-a-real-id/load"),
            ("POST", "/v1/local/models/not-a-real-id/install"),
            ("POST", "/v1/local/models/not-a-real-id/verify"),
            ("DELETE", "/v1/local/models/not-a-real-id"),
            ("POST", "/v1/local/models/import"),
            ("POST", "/v1/local/models/import-user"),
            ("GET", "/v1/local/operations/not-a-real-id"),
            ("POST", "/v1/local/operations/not-a-real-id/cancel"),
        ];
        for (method, route_path) in old_paths {
            let router = router(app.clone());
            let request = build_request(method, route_path, None, &token);
            let response = router.oneshot(request).await.unwrap();
            assert_eq!(
                response.status(),
                StatusCode::NOT_FOUND,
                "{method} {route_path} should 404: the old path was removed"
            );
        }
        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    /// Axum matches a literal path segment ahead of a `{id}` capture
    /// regardless of registration order, but this pins that behaviour for
    /// every static suffix under `/models/manage` that could otherwise be
    /// shadowed by `/models/manage/{id}` or `/models/manage/{id}/...`: an id
    /// literally named `default`, `refresh`, `import`, `import-user`,
    /// `recommendations` or `operations` must never reach `remove_model`/
    /// `select_model`/etc instead of the intended static handler.
    #[tokio::test]
    async fn static_models_manage_routes_take_precedence_over_id_capture() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = open_app_at(path.clone()).unwrap();
        let token = app.token.clone();

        // GET /models/manage/default must hit `selected_model`, not
        // `remove_model` misrouted, or `openai_model_by_id`-style 404 for an
        // id literally called "default". With nothing selected yet this is a
        // 200 with a "no default set" shape (or 404 with a `no_default`-style
        // code), never the generic `model_not_installed` a stray id lookup
        // would produce.
        let router1 = router(app.clone());
        let request = build_request("GET", "/models/manage/default", None, &token);
        let response = router1.oneshot(request).await.unwrap();
        assert_ne!(
            response.status(),
            StatusCode::NOT_FOUND,
            "GET /models/manage/default must not be swallowed by /models/manage/{{id}}"
        );

        // POST /models/manage/refresh must hit `refresh_models` (202/200),
        // never `select_model` treating "refresh" as a model id (which would
        // 404 model_not_installed with the same status but is the wrong
        // handler -- checked indirectly via the recommendations/import routes
        // below returning their own distinct shapes).
        let router2 = router(app.clone());
        let request = build_request("POST", "/models/manage/refresh", None, &token);
        let response = router2.oneshot(request).await.unwrap();
        assert!(
            response.status() == StatusCode::OK || response.status() == StatusCode::ACCEPTED,
            "POST /models/manage/refresh must hit refresh_models, got {}",
            response.status()
        );

        // GET /models/manage/recommendations must hit `recommendations`
        // (200), not `openai_model_by_id`/`remove_model`-style handling of an
        // id named "recommendations".
        let router3 = router(app.clone());
        let request = build_request("GET", "/models/manage/recommendations", None, &token);
        let response = router3.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // GET /models/manage/operations/{id} must hit `operation`'s own
        // not-found handling for a bogus operation id, distinguishable from
        // routing failure only in that the route exists at all -- covered by
        // `user_routes()` above; here we additionally check the literal
        // "operations" segment isn't captured as a model id by asserting the
        // cancel route is also reachable (POST, admin token).
        let router4 = router(app.clone());
        let request = build_request(
            "POST",
            "/models/manage/operations/not-a-real-id/cancel",
            None,
            &token,
        );
        let response = router4.oneshot(request).await.unwrap();
        // A bogus operation id is itself a legitimate 404, so status alone
        // can't distinguish "routed to cancel_operation, which reported
        // operation_not_found" from "the route never matched"; the error
        // code can.
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["code"], "operation_not_found");

        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }
}

/// Port fallback for several users on one PC (see the `install-scope-and-
/// shared-access` goal, "Several users on one PC"): a per-user server whose
/// port was only preferred (not given via an explicit `--port`) falls back
/// to a free loopback port when the preferred one is taken; a machine-wide
/// server, and any server given an explicit `--port`, always fails clearly
/// instead.
#[cfg(test)]
mod port_fallback_tests {
    use super::*;
    use uuid::Uuid;

    fn temp_dir() -> std::path::PathBuf {
        std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("stt-port-fallback-test-{}", Uuid::new_v4()))
    }

    /// Occupies a loopback port and hands back both the port number and the
    /// listener (dropping it would free the port again).
    async fn occupy_a_port() -> (u16, tokio::net::TcpListener) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        (port, listener)
    }

    #[tokio::test]
    async fn per_user_falls_back_to_a_free_port_when_preferred_one_is_busy() {
        let (busy_port, _occupying_listener) = occupy_a_port().await;
        let data_dir = temp_dir();

        // Persist the "preferred" (busy) port as a stored setting -- picked
        // up the same way a real default/configured port would be -- before
        // the server ever opens, so it isn't a race with `run_http_full`
        // reading it at startup. Crucially this is a stored setting, not an
        // explicit `--port` (`bind_overrides.port` stays `None` below), which
        // is what makes fallback eligible at all.
        {
            let app = crate::app::open_app_at(data_dir.clone()).unwrap();
            app.db
                .lock()
                .unwrap()
                .execute(
                    "INSERT INTO settings(key,value) VALUES(?1,?2)",
                    params![crate::store::SETTING_BIND_PORT, busy_port.to_string()],
                )
                .unwrap();
            drop(app);
        }

        let dir = data_dir.clone();
        let task = tokio::spawn(async move {
            let _ = run_http_full(
                dir,
                Default::default(),
                crate::app::BindOverrides {
                    host: Some("127.0.0.1".to_owned()),
                    port: None,
                    cors_origins: None,
                },
                None,
                crate::app::InstallScope::PerUser,
                std::future::pending::<()>(),
            )
            .await;
        });

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let info = loop {
            if let Some(info) = crate::discovery::read_server_json(&data_dir) {
                break info;
            }
            if std::time::Instant::now() >= deadline {
                task.abort();
                panic!("server never wrote server.json");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };

        assert_ne!(
            info.port, busy_port,
            "must not have bound the already-occupied port"
        );
        assert!(info.port > 0);

        task.abort();
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[tokio::test]
    async fn explicit_port_fails_clearly_instead_of_falling_back() {
        let (busy_port, _occupying_listener) = occupy_a_port().await;
        let data_dir = temp_dir();
        let overrides = crate::app::BindOverrides {
            host: Some("127.0.0.1".to_owned()),
            port: Some(busy_port),
            cors_origins: None,
        };
        let result = run_http_full(
            data_dir.clone(),
            Default::default(),
            overrides,
            None,
            crate::app::InstallScope::PerUser,
            std::future::pending::<()>(),
        )
        .await;
        assert!(matches!(result, Err(ServeError::BindFailed(_))));
        assert!(crate::discovery::read_server_json(&data_dir).is_none());
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[tokio::test]
    async fn machine_wide_fails_clearly_instead_of_falling_back() {
        let (busy_port, _occupying_listener) = occupy_a_port().await;
        let data_dir = temp_dir();
        let overrides = crate::app::BindOverrides {
            host: Some("127.0.0.1".to_owned()),
            // Not an explicit `--port` -- only the scope should be what
            // stops the fallback here.
            port: None,
            cors_origins: None,
        };
        {
            let app = crate::app::open_app_at(data_dir.clone()).unwrap();
            app.db
                .lock()
                .unwrap()
                .execute(
                    "INSERT INTO settings(key,value) VALUES(?1,?2)",
                    params![crate::store::SETTING_BIND_PORT, busy_port.to_string()],
                )
                .unwrap();
            drop(app);
        }
        let result = run_http_full(
            data_dir.clone(),
            Default::default(),
            overrides,
            None,
            crate::app::InstallScope::MachineWide,
            std::future::pending::<()>(),
        )
        .await;
        assert!(matches!(result, Err(ServeError::BindFailed(_))));
        assert!(crate::discovery::read_server_json(&data_dir).is_none());
        let _ = std::fs::remove_dir_all(&data_dir);
    }
}
