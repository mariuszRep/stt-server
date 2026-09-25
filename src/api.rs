use std::collections::HashMap;
use std::error::Error;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{DefaultBodyLimit, Multipart, Path as UrlPath, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{delete, get, post},
    Json, Router,
};
use rusqlite::params;
use serde::Deserialize;
use serde_json::{json, Value};
use tower_http::cors::{AllowOrigin, CorsLayer};
use transcribe_cpp::{CancelToken, RunOptions};
use uuid::Uuid;

use crate::app::{open_app, App};
use crate::audio::decode_wav;
use crate::auth::authorized;
use crate::catalog::{capability_matrix, catalog_model, model_view};
use crate::download::install_model;
use crate::engine::{load_engine, CancelWhenDropped, LoadedModel};
use crate::errors::{internal, ApiError, ApiResult};
use crate::import::import_model;
use crate::operations::{cancel_operation, operation};
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
            Ok(model_view(
                model,
                installed.and_then(|file| file.quant).as_deref(),
            ))
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
    let loaded = app.loaded.lock().map_err(internal)?;
    let diagnostic = loaded
        .as_ref()
        .filter(|active| active.id == id)
        .map(|active| active.diagnostic.clone());
    Ok(Json(json!({
        "model":id,
        "effective_capabilities":capability_matrix(model),
        "backend":diagnostic
    })))
}

pub async fn deselect_model(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    authorized(&headers, &app)?;
    let _selection = app.selection.lock().await;
    let _inference = app.inference.clone().try_acquire_owned().map_err(|_| {
        ApiError::new(
            StatusCode::CONFLICT,
            "model_in_use",
            "Inference must finish before unloading the model",
        )
    })?;
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
    let _selection = app.selection.lock().await;
    let _inference = app
        .inference
        .clone()
        .acquire_owned()
        .await
        .map_err(internal)?;
    catalog_model(&app, &id)?;
    let path = installed_path(&app, &id)?.ok_or_else(|| {
        ApiError::new(
            StatusCode::CONFLICT,
            "model_not_installed",
            "Install the model first",
        )
    })?;
    let preference = backend_preference(&app)?;
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
    *app.loaded.lock().map_err(internal)? = Some(LoadedModel {
        id: id.clone(),
        model,
        diagnostic: diagnostic.clone(),
    });
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

pub async fn transcriptions(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> ApiResult<Json<Value>> {
    authorized(&headers, &app)?;
    let mut fields: HashMap<String, Vec<u8>> = HashMap::new();
    while let Some(field) = multipart.next_field().await.map_err(|error| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_multipart",
            error.to_string(),
        )
    })? {
        let name = field.name().unwrap_or("").to_owned();
        if fields.contains_key(&name) {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "duplicate_field",
                name,
            ));
        }
        let bytes = field.bytes().await.map_err(|error| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_multipart",
                error.to_string(),
            )
        })?;
        fields.insert(name, bytes.to_vec());
    }
    for name in fields.keys() {
        if !matches!(name.as_str(), "file" | "model" | "response_format") {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "unsupported_capability",
                format!("Field {name} is not supported by the selected model"),
            ));
        }
    }
    let model_id = std::str::from_utf8(fields.get("model").ok_or_else(|| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "missing_model",
            "model is required",
        )
    })?)
    .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "invalid_model", "Invalid model ID"))?;
    if let Some(format) = fields.get("response_format") {
        if format != b"json" {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "unsupported_capability",
                "Only response_format=json is supported",
            ));
        }
    }
    let file = fields.get("file").ok_or_else(|| {
        ApiError::new(StatusCode::BAD_REQUEST, "missing_file", "file is required")
    })?;
    let pcm = decode_wav(file)?;
    let model = {
        let active = app.loaded.lock().map_err(internal)?;
        let loaded = active.as_ref().ok_or_else(|| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "server_not_ready",
                "No model loaded",
            )
        })?;
        if model_id != "default" && model_id != loaded.id {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "model_not_active",
                "The requested model is not active",
            ));
        }
        loaded.model.clone()
    };
    let permit = app.inference.clone().try_acquire_owned().map_err(|_| {
        ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "inference_busy",
            "Inference is busy",
        )
    })?;
    let cancellation = CancelToken::new();
    let _cancel_on_disconnect = CancelWhenDropped(cancellation.clone());
    let worker = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let mut session = model.session().map_err(|error| error.to_string())?;
        session.set_cancel_token(&cancellation);
        session
            .run(&pcm, &RunOptions::default())
            .map(|result| result.text)
            .map_err(|error| error.to_string())
    });
    let text = tokio::time::timeout(Duration::from_secs(180), worker)
        .await
        .map_err(|_| {
            ApiError::new(
                StatusCode::GATEWAY_TIMEOUT,
                "inference_timeout",
                "Inference exceeded the 180-second limit",
            )
        })?
        .map_err(internal)?
        .map_err(|error| {
            ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "inference_failed", error)
        })?;
    Ok(Json(json!({"text":text})))
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
}
