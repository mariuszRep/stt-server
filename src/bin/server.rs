use std::{
    collections::HashMap,
    error::Error,
    fs::{self, OpenOptions},
    future::Future,
    io::{Cursor, Read, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    extract::{DefaultBodyLimit, Multipart, Path as UrlPath, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use rubato::{FftFixedIn, Resampler};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;
use transcribe_cpp::{backend_available, Backend, CancelToken, Model, ModelOptions, RunOptions};
use uuid::Uuid;

#[derive(Debug, Clone, Deserialize, Serialize)]
struct CatalogFile {
    filename: String,
    quant: String,
    size_bytes: u64,
    sha256: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct ModelClaims {
    streaming: bool,
    translate: bool,
    lang_detect: bool,
    timestamps: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct CatalogModel {
    id: String,
    revision: String,
    slug: String,
    name: String,
    architecture: String,
    family: String,
    license: String,
    languages: Vec<String>,
    capabilities: ModelClaims,
    speed_score: Option<u32>,
    accuracy_score: Option<u32>,
    files: Vec<CatalogFile>,
    default_quant: String,
    recommended: bool,
    recommended_rank: Option<u32>,
}

#[derive(Deserialize)]
struct Catalog {
    models: Vec<CatalogModel>,
}

#[derive(Clone, Serialize)]
struct BackendDiagnostic {
    observed_backend: String,
    fallback_reason: Option<String>,
}

struct LoadedModel {
    id: String,
    model: Model,
    diagnostic: BackendDiagnostic,
}

struct CancelWhenDropped(CancelToken);

impl Drop for CancelWhenDropped {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

struct App {
    catalog: Vec<CatalogModel>,
    db: Mutex<Connection>,
    loaded: Mutex<Option<LoadedModel>>,
    data_dir: PathBuf,
    token: String,
    inference: Arc<Semaphore>,
    selection: tokio::sync::Mutex<()>,
    http: reqwest::Client,
}

struct ImportGuard {
    app: Arc<App>,
    operation_id: String,
    stage: PathBuf,
    complete: bool,
}

impl Drop for ImportGuard {
    fn drop(&mut self) {
        if self.complete {
            return;
        }
        if self.stage.exists() {
            let quarantine = self.app.data_dir.join("quarantine");
            if fs::create_dir_all(&quarantine).is_ok() {
                let _ = fs::rename(
                    &self.stage,
                    quarantine.join(format!("{}-interrupted.gguf", self.operation_id)),
                );
            }
        }
        if let Ok(db) = self.app.db.lock() {
            let _ = db.execute(
                "UPDATE operations SET state='failed', error='Import interrupted' WHERE id=?1 AND state IN ('queued','running')",
                params![self.operation_id],
            );
        }
    }
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({"error": {"code": self.code, "message": self.message}})),
        )
            .into_response()
    }
}

type ApiResult<T> = Result<T, ApiError>;

fn internal(error: impl std::fmt::Display) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal_error",
        error.to_string(),
    )
}

fn authorized(headers: &HeaderMap, app: &App) -> ApiResult<()> {
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

fn catalog_model<'a>(app: &'a App, id: &str) -> ApiResult<&'a CatalogModel> {
    app.catalog
        .iter()
        .find(|model| model.slug == id)
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "model_not_found", "Unknown model ID"))
}

fn capability_matrix(model: &CatalogModel) -> Value {
    let english_fixed = model.slug == "parakeet-unified-en-0.6b";
    json!({
        "prompt": {"status": if english_fixed { "unsupported" } else { "unknown" }, "mechanism": null},
        "vocabulary": {"status": if english_fixed { "unsupported" } else { "unknown" }, "mechanism": null},
        "language_hint": {"status": if english_fixed { "unsupported" } else { "unknown" }, "scope": "request"},
        "language_detect": {"status": "unsupported", "model_claim": model.capabilities.lang_detect},
        "translation": {"status": "unsupported", "model_claim": model.capabilities.translate},
        "temperature": {"status": "unsupported"},
        "response_formats": {"json": "supported", "text": "unsupported", "verbose_json": "unsupported"},
        "timestamp_granularity": {"status": "unknown", "model_claim": model.capabilities.timestamps},
        "streaming": {"status": "unsupported", "model_claim": model.capabilities.streaming}
    })
}

fn model_view(model: &CatalogModel, installed: bool) -> Value {
    let file = model
        .files
        .iter()
        .find(|file| file.quant == model.default_quant);
    json!({
        "id": model.slug,
        "name": model.name,
        "upstream_id": model.id,
        "revision": model.revision,
        "architecture": model.architecture,
        "family": model.family,
        "license": model.license,
        "languages": model.languages,
        "model_capabilities": model.capabilities,
        "effective_capabilities": capability_matrix(model),
        "default_quant": model.default_quant,
        "size_bytes": file.map(|f| f.size_bytes),
        "speed_score": model.speed_score,
        "accuracy_score": model.accuracy_score,
        "benchmark_source": "Handy catalog generated 2026-08-17; scores are derived display values, not local measurements",
        "recommended_rank": model.recommended_rank,
        "installed": installed,
        "installable": model.slug == "parakeet-unified-en-0.6b" && file.is_some(),
    })
}

fn installed_path(app: &App, id: &str) -> ApiResult<Option<PathBuf>> {
    let db = app.db.lock().map_err(internal)?;
    let path: Option<String> = db
        .query_row(
            "SELECT path FROM installed WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )
        .optional()
        .map_err(internal)?;
    Ok(path.map(PathBuf::from))
}

fn selected_id(app: &App) -> ApiResult<Option<String>> {
    let db = app.db.lock().map_err(internal)?;
    db.query_row(
        "SELECT value FROM settings WHERE key = 'selected_model'",
        [],
        |row| row.get(0),
    )
    .optional()
    .map_err(internal)
}

fn backend_preference(app: &App) -> ApiResult<String> {
    let db = app.db.lock().map_err(internal)?;
    db.query_row(
        "SELECT value FROM settings WHERE key='preferred_backend'",
        [],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .map(|value| value.unwrap_or_else(|| "auto".to_owned()))
    .map_err(internal)
}

async fn get_config(State(app): State<Arc<App>>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    authorized(&headers, &app)?;
    Ok(Json(json!({
        "bind":"127.0.0.1:54321",
        "preferred_backend":backend_preference(&app)?,
        "max_audio_bytes":40 * 1024 * 1024,
        "streaming":false
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigPatch {
    preferred_backend: String,
}

async fn patch_config(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(patch): Json<ConfigPatch>,
) -> ApiResult<Json<Value>> {
    authorized(&headers, &app)?;
    if !matches!(patch.preferred_backend.as_str(), "auto" | "cpu" | "vulkan") {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_backend",
            "preferred_backend must be auto, cpu, or vulkan",
        ));
    }
    let db = app.db.lock().map_err(internal)?;
    db.execute(
        "INSERT INTO settings(key,value) VALUES('preferred_backend',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![patch.preferred_backend],
    )
    .map_err(internal)?;
    Ok(Json(
        json!({"preferred_backend":patch.preferred_backend,"reload_required":true}),
    ))
}

async fn health() -> Json<Value> {
    Json(json!({"status": "ok"}))
}

async fn readiness(
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

async fn recommendations(
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
            let installed = installed_path(&app, &model.slug)?.is_some();
            Ok(model_view(model, installed))
        })
        .collect::<ApiResult<Vec<_>>>()?;
    Ok(Json(json!({"object":"list", "data":data})))
}

async fn local_models(State(app): State<Arc<App>>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    authorized(&headers, &app)?;
    let data = app
        .catalog
        .iter()
        .map(|model| {
            let installed = installed_path(&app, &model.slug)?.is_some();
            Ok(model_view(model, installed))
        })
        .collect::<ApiResult<Vec<_>>>()?;
    Ok(Json(json!({"object":"list", "data":data})))
}

async fn openai_models(State(app): State<Arc<App>>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    authorized(&headers, &app)?;
    let mut data = Vec::new();
    for model in &app.catalog {
        if installed_path(&app, &model.slug)?.is_some() {
            data.push(json!({"id":model.slug,"object":"model","owned_by":"local"}));
        }
    }
    Ok(Json(json!({"object":"list", "data":data})))
}

async fn selected_model(State(app): State<Arc<App>>, headers: HeaderMap) -> ApiResult<Json<Value>> {
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

async fn deselect_model(State(app): State<Arc<App>>, headers: HeaderMap) -> ApiResult<Json<Value>> {
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

fn update_operation(
    app: &App,
    id: &str,
    state: &str,
    error: Option<&str>,
    bytes: u64,
) -> ApiResult<()> {
    let db = app.db.lock().map_err(internal)?;
    db.execute(
        "UPDATE operations SET state=?2, error=?3, progress_bytes=?4 WHERE id=?1 AND state <> 'cancelled'",
        params![id, state, error, bytes],
    )
    .map_err(internal)?;
    Ok(())
}

fn promote_verified_model(
    app: &App,
    operation_id: &str,
    model_id: &str,
    sha256: &str,
    stage: &Path,
    bytes: u64,
) -> Result<(), String> {
    let destination = app.data_dir.join("models").join(format!("{sha256}.gguf"));
    let mut db = app.db.lock().map_err(|error| error.to_string())?;
    let transaction = db.transaction().map_err(|error| error.to_string())?;
    let state: String = transaction
        .query_row(
            "SELECT state FROM operations WHERE id=?1",
            params![operation_id],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if state != "running" {
        return Err(format!("Operation is {state}, not running"));
    }
    transaction
        .execute(
            "INSERT INTO installed(id,path,sha256) VALUES(?1,?2,?3)",
            params![model_id, destination.to_string_lossy().as_ref(), sha256],
        )
        .map_err(|error| error.to_string())?;
    fs::rename(stage, &destination).map_err(|error| error.to_string())?;
    transaction
        .execute(
            "UPDATE operations SET state='completed', error=NULL, progress_bytes=?2 WHERE id=?1",
            params![operation_id, bytes],
        )
        .map_err(|error| error.to_string())?;
    if let Err(error) = transaction.commit() {
        let _ = fs::rename(&destination, stage);
        return Err(error.to_string());
    }
    Ok(())
}

async fn operation(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    authorized(&headers, &app)?;
    let db = app.db.lock().map_err(internal)?;
    let record = db
        .query_row(
            "SELECT model_id, kind, state, error, progress_bytes, total_bytes FROM operations WHERE id=?1",
            params![id],
            |row| {
                Ok(json!({
                    "id":id,
                    "model_id":row.get::<_, String>(0)?,
                    "kind":row.get::<_, String>(1)?,
                    "state":row.get::<_, String>(2)?,
                    "error":row.get::<_, Option<String>>(3)?,
                    "progress_bytes":row.get::<_, u64>(4)?,
                    "total_bytes":row.get::<_, u64>(5)?
                }))
            },
        )
        .optional()
        .map_err(internal)?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "operation_not_found", "Unknown operation"))?;
    Ok(Json(record))
}

fn operation_state(app: &App, id: &str) -> ApiResult<Option<String>> {
    let db = app.db.lock().map_err(internal)?;
    db.query_row(
        "SELECT state FROM operations WHERE id=?1",
        params![id],
        |row| row.get(0),
    )
    .optional()
    .map_err(internal)
}

async fn cancel_operation(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    authorized(&headers, &app)?;
    match operation_state(&app, &id)?.as_deref() {
        Some("queued" | "running") => {
            let db = app.db.lock().map_err(internal)?;
            let changed = db.execute(
                "UPDATE operations SET state='cancelled' WHERE id=?1 AND state IN ('queued','running')",
                params![id],
            )
            .map_err(internal)?;
            if changed == 0 {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "operation_finished",
                    "Operation already finished",
                ));
            }
            Ok(Json(json!({"id":id,"state":"cancelled"})))
        }
        Some(_) => Err(ApiError::new(
            StatusCode::CONFLICT,
            "operation_finished",
            "Operation already finished",
        )),
        None => Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "operation_not_found",
            "Unknown operation",
        )),
    }
}

async fn install_model(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
) -> ApiResult<(StatusCode, Json<Value>)> {
    authorized(&headers, &app)?;
    let model = catalog_model(&app, &id)?.clone();
    if model.slug != "parakeet-unified-en-0.6b" {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "model_not_admitted",
            "This model has not passed admission",
        ));
    }
    if installed_path(&app, &id)?.is_some() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "already_installed",
            "Model is already installed",
        ));
    }
    let file = model
        .files
        .iter()
        .find(|file| file.quant == model.default_quant)
        .cloned()
        .ok_or_else(|| internal("Default quantization missing"))?;
    let op = Uuid::new_v4().to_string();
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
                "An operation for this model is already active",
            ));
        }
        db.execute(
            "INSERT INTO operations(id,model_id,kind,state,error,progress_bytes,total_bytes) VALUES(?1,?2,'install','queued',NULL,0,?3)",
            params![op, id, file.size_bytes],
        )
        .map_err(internal)?;
    }
    let task_app = app.clone();
    let task_op = op.clone();
    tokio::spawn(async move {
        if let Err(error) = download_model(task_app.clone(), model, file, &task_op).await {
            if operation_state(&task_app, &task_op)
                .ok()
                .flatten()
                .as_deref()
                != Some("cancelled")
            {
                let _ = update_operation(&task_app, &task_op, "failed", Some(&error), 0);
            }
        }
    });
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({"operation_id":op,"state":"queued"})),
    ))
}

async fn verify_model(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
) -> ApiResult<(StatusCode, Json<Value>)> {
    authorized(&headers, &app)?;
    let model = catalog_model(&app, &id)?;
    let file = model
        .files
        .iter()
        .find(|file| file.quant == model.default_quant)
        .cloned()
        .ok_or_else(|| internal("Default quantization missing"))?;
    let path = installed_path(&app, &id)?.ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "model_not_installed",
            "Model is not installed",
        )
    })?;
    let op = Uuid::new_v4().to_string();
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
                "An operation for this model is already active",
            ));
        }
        db.execute(
            "INSERT INTO operations(id,model_id,kind,state,error,progress_bytes,total_bytes) VALUES(?1,?2,'verify','queued',NULL,0,?3)",
            params![op, id, file.size_bytes],
        )
        .map_err(internal)?;
    }
    let task_app = app.clone();
    let task_op = op.clone();
    tokio::spawn(async move {
        if operation_state(&task_app, &task_op)
            .ok()
            .flatten()
            .as_deref()
            == Some("cancelled")
        {
            return;
        }
        let _ = update_operation(&task_app, &task_op, "running", None, 0);
        let verify_path = path.clone();
        let actual = tokio::task::spawn_blocking(move || {
            let size = fs::metadata(&verify_path).map(|metadata| metadata.len());
            let hash = sha256_file(&verify_path);
            (size, hash)
        })
        .await;
        if operation_state(&task_app, &task_op)
            .ok()
            .flatten()
            .as_deref()
            == Some("cancelled")
        {
            return;
        }
        let valid = matches!(actual, Ok((Ok(size), Ok(ref hash))) if size == file.size_bytes && hash.eq_ignore_ascii_case(&file.sha256));
        if valid {
            let _ = update_operation(&task_app, &task_op, "completed", None, file.size_bytes);
            return;
        }
        let _selection = task_app.selection.lock().await;
        let _inference = match task_app.inference.clone().acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => return,
        };
        if operation_state(&task_app, &task_op)
            .ok()
            .flatten()
            .as_deref()
            == Some("cancelled")
        {
            return;
        }
        if let Ok(db) = task_app.db.lock() {
            let _ = db.execute("DELETE FROM installed WHERE id=?1", params![id]);
            let _ = db.execute(
                "DELETE FROM settings WHERE key='selected_model' AND value=?1",
                params![id],
            );
        }
        if let Ok(mut loaded) = task_app.loaded.lock() {
            if loaded.as_ref().is_some_and(|active| active.id == id) {
                *loaded = None;
            }
        }
        let quarantine = task_app.data_dir.join("quarantine");
        if fs::create_dir_all(&quarantine).is_ok() && path.exists() {
            let _ = fs::rename(
                &path,
                quarantine.join(format!("{task_op}-verify-failed.gguf")),
            );
        }
        let _ = update_operation(
            &task_app,
            &task_op,
            "failed",
            Some("Installed model size or SHA-256 mismatch"),
            0,
        );
    });
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({"operation_id":op,"state":"queued"})),
    ))
}

async fn download_model(
    app: Arc<App>,
    model: CatalogModel,
    file: CatalogFile,
    op: &str,
) -> Result<(), String> {
    if operation_state(&app, op)
        .map_err(|error| error.message)?
        .as_deref()
        == Some("cancelled")
    {
        return Err("Cancelled by user".to_owned());
    }
    update_operation(&app, op, "running", None, 0).map_err(|error| error.message)?;
    let stage = app
        .data_dir
        .join("staging")
        .join(format!("{}.part", file.sha256));
    let mut offset = tokio::fs::metadata(&stage)
        .await
        .map(|info| info.len())
        .unwrap_or(0);
    if offset > file.size_bytes {
        tokio::fs::remove_file(&stage)
            .await
            .map_err(|error| error.to_string())?;
        offset = 0;
    }
    let url = format!(
        "https://huggingface.co/{}/resolve/{}/{}",
        model.id, model.revision, file.filename
    );
    let mut request = app.http.get(url);
    if offset > 0 {
        request = request.header(reqwest::header::RANGE, format!("bytes={offset}-"));
    }
    let mut response = request.send().await.map_err(|error| error.to_string())?;
    if !response.status().is_success() {
        return Err(format!("Model source returned HTTP {}", response.status()));
    }
    if offset > 0 && response.status() == reqwest::StatusCode::PARTIAL_CONTENT {
        let prefix = format!("bytes {offset}-");
        if !response
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with(&prefix))
        {
            return Err("Model source returned an invalid resume range".to_owned());
        }
    } else if offset > 0 && response.status() == reqwest::StatusCode::OK {
        offset = 0;
    } else if response.status() == reqwest::StatusCode::PARTIAL_CONTENT {
        return Err("Unexpected partial response".to_owned());
    }
    let mut output = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(offset > 0)
        .truncate(offset == 0)
        .open(&stage)
        .await
        .map_err(|error| error.to_string())?;
    let mut received = offset;
    while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
        if operation_state(&app, op)
            .map_err(|error| error.message)?
            .as_deref()
            == Some("cancelled")
        {
            return Err("Cancelled by user".to_owned());
        }
        received += chunk.len() as u64;
        if received > file.size_bytes {
            return Err("Model source exceeded catalog size".to_owned());
        }
        output
            .write_all(&chunk)
            .await
            .map_err(|error| error.to_string())?;
        update_operation(&app, op, "running", None, received).map_err(|error| error.message)?;
    }
    output.sync_all().await.map_err(|error| error.to_string())?;
    drop(output);
    if received != file.size_bytes {
        return Err(format!(
            "Incomplete model: {received} of {} bytes",
            file.size_bytes
        ));
    }
    let hash_path = stage.clone();
    let actual = tokio::task::spawn_blocking(move || sha256_file(&hash_path))
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())?;
    if !actual.eq_ignore_ascii_case(&file.sha256) {
        let quarantine = app.data_dir.join("quarantine");
        tokio::fs::create_dir_all(&quarantine)
            .await
            .map_err(|error| error.to_string())?;
        tokio::fs::rename(&stage, quarantine.join(format!("{op}-hash-mismatch.gguf")))
            .await
            .map_err(|error| error.to_string())?;
        return Err("Catalog SHA-256 mismatch".to_owned());
    }
    promote_verified_model(&app, op, &model.slug, &file.sha256, &stage, received)?;
    Ok(())
}

async fn import_model(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> ApiResult<(StatusCode, Json<Value>)> {
    authorized(&headers, &app)?;
    let mut model_id: Option<String> = None;
    let mut imported = false;
    let mut operation_id: Option<String> = None;
    while let Some(mut field) = multipart.next_field().await.map_err(|error| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_multipart",
            error.to_string(),
        )
    })? {
        match field.name().unwrap_or("") {
            "model" if model_id.is_none() && !imported => {
                let id = field.text().await.map_err(|error| {
                    ApiError::new(StatusCode::BAD_REQUEST, "invalid_model", error.to_string())
                })?;
                catalog_model(&app, &id)?;
                model_id = Some(id);
            }
            "file" if !imported => {
                let id = model_id.as_deref().ok_or_else(|| {
                    ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "missing_model",
                        "Send model before file",
                    )
                })?;
                if installed_path(&app, id)?.is_some() {
                    return Err(ApiError::new(
                        StatusCode::CONFLICT,
                        "already_installed",
                        "Model is already installed",
                    ));
                }
                let model = catalog_model(&app, id)?.clone();
                if model.slug != "parakeet-unified-en-0.6b" {
                    return Err(ApiError::new(
                        StatusCode::CONFLICT,
                        "model_not_admitted",
                        "This model has not passed admission",
                    ));
                }
                let expected = model
                    .files
                    .iter()
                    .find(|file| file.quant == model.default_quant)
                    .ok_or_else(|| internal("Default quantization missing"))?;
                let op = Uuid::new_v4().to_string();
                let stage = app.data_dir.join("staging").join(format!("{op}.part"));
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
                            "An operation for this model is already active",
                        ));
                    }
                    db.execute(
                        "INSERT INTO operations(id,model_id,kind,state,error,progress_bytes,total_bytes) VALUES(?1,?2,'import','running',NULL,0,?3)",
                        params![op, id, expected.size_bytes],
                    )
                    .map_err(internal)?;
                }
                operation_id = Some(op.clone());
                let mut guard = ImportGuard {
                    app: app.clone(),
                    operation_id: op.clone(),
                    stage: stage.clone(),
                    complete: false,
                };
                let mut output = tokio::fs::File::create(&stage).await.map_err(internal)?;
                let mut digest = Sha256::new();
                let mut size = 0_u64;
                while let Some(chunk) = field.chunk().await.map_err(|error| {
                    ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "invalid_multipart",
                        error.to_string(),
                    )
                })? {
                    size += chunk.len() as u64;
                    if size > expected.size_bytes {
                        update_operation(
                            &app,
                            &op,
                            "failed",
                            Some("File exceeds catalog size"),
                            size,
                        )?;
                        return Err(ApiError::new(
                            StatusCode::UNPROCESSABLE_ENTITY,
                            "size_mismatch",
                            "File exceeds catalog size",
                        ));
                    }
                    digest.update(&chunk);
                    output.write_all(&chunk).await.map_err(internal)?;
                    update_operation(&app, &op, "running", None, size)?;
                }
                output.sync_all().await.map_err(internal)?;
                drop(output);
                let actual = format!("{:x}", digest.finalize());
                if size != expected.size_bytes || !actual.eq_ignore_ascii_case(&expected.sha256) {
                    let quarantine = app.data_dir.join("quarantine");
                    tokio::fs::create_dir_all(&quarantine)
                        .await
                        .map_err(internal)?;
                    tokio::fs::rename(&stage, quarantine.join(format!("{op}.gguf")))
                        .await
                        .map_err(internal)?;
                    update_operation(
                        &app,
                        &op,
                        "failed",
                        Some("Catalog size or SHA-256 mismatch"),
                        size,
                    )?;
                    return Err(ApiError::new(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "hash_mismatch",
                        "Catalog size or SHA-256 mismatch",
                    ));
                }
                promote_verified_model(&app, &op, id, &expected.sha256, &stage, size)
                    .map_err(internal)?;
                guard.complete = true;
                imported = true;
            }
            name => {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "unexpected_field",
                    format!("Unexpected or duplicate field {name}"),
                ));
            }
        }
    }
    if !imported {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "missing_file",
            "model and file are required",
        ));
    }
    Ok((
        StatusCode::CREATED,
        Json(json!({"operation_id":operation_id,"model":model_id})),
    ))
}

fn load_engine(path: &Path, preference: &str) -> Result<(Model, BackendDiagnostic), String> {
    if preference != "cpu" && backend_available(Backend::Vulkan) {
        match Model::load_with(
            path,
            &ModelOptions {
                backend: Backend::Vulkan,
                ..Default::default()
            },
        ) {
            Ok(model) => {
                let observed_backend = model.backend().to_string();
                return Ok((
                    model,
                    BackendDiagnostic {
                        observed_backend,
                        fallback_reason: None,
                    },
                ));
            }
            Err(vulkan_error) => {
                let model = Model::load_with(
                    path,
                    &ModelOptions {
                        backend: Backend::Cpu,
                        ..Default::default()
                    },
                )
                .map_err(|cpu_error| format!("Vulkan: {vulkan_error}; CPU: {cpu_error}"))?;
                return Ok((
                    model,
                    BackendDiagnostic {
                        observed_backend: "CPU".to_owned(),
                        fallback_reason: Some(vulkan_error.to_string()),
                    },
                ));
            }
        }
    }
    let model = Model::load_with(
        path,
        &ModelOptions {
            backend: Backend::Cpu,
            ..Default::default()
        },
    )
    .map_err(|error| error.to_string())?;
    Ok((
        model,
        BackendDiagnostic {
            observed_backend: "CPU".to_owned(),
            fallback_reason: if preference == "cpu" {
                None
            } else {
                Some("Vulkan backend unavailable".to_owned())
            },
        },
    ))
}

async fn select_model(
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

async fn remove_model(
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
    let model_root = fs::canonicalize(app.data_dir.join("models")).map_err(internal)?;
    if !fs::canonicalize(&path)
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

fn decode_wav(bytes: &[u8]) -> ApiResult<Vec<f32>> {
    let mut reader = hound::WavReader::new(Cursor::new(bytes)).map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_audio",
            "A valid WAV file is required",
        )
    })?;
    let spec = reader.spec();
    if !(1..=2).contains(&spec.channels) || !(8_000..=192_000).contains(&spec.sample_rate) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "unsupported_audio",
            "WAV must have one or two channels and a sample rate from 8 to 192 kHz",
        ));
    }
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int if spec.bits_per_sample == 16 => reader
            .samples::<i16>()
            .map(|sample| sample.map(|value| f32::from(value) / 32768.0))
            .collect::<Result<Vec<_>, _>>(),
        hound::SampleFormat::Int if spec.bits_per_sample == 24 => reader
            .samples::<i32>()
            .map(|sample| sample.map(|value| value as f32 / 8_388_608.0))
            .collect::<Result<Vec<_>, _>>(),
        hound::SampleFormat::Float if spec.bits_per_sample == 32 => {
            reader.samples::<f32>().collect::<Result<Vec<_>, _>>()
        }
        _ => {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "unsupported_audio",
                "Only 16/24-bit PCM and 32-bit float WAV are supported",
            ));
        }
    }
    .map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_audio",
            "Malformed WAV samples",
        )
    })?;
    if !samples.len().is_multiple_of(spec.channels as usize)
        || !samples.iter().all(|s| s.is_finite())
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_audio",
            "WAV has incomplete frames or non-finite samples",
        ));
    }
    let frame_count = samples.len() / spec.channels as usize;
    if frame_count < (spec.sample_rate / 10) as usize {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "audio_too_short",
            "At least 100 ms of audio is required",
        ));
    }
    if frame_count > spec.sample_rate as usize * 10 * 60 {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "audio_too_long",
            "Audio exceeds the ten-minute limit",
        ));
    }
    let mono: Vec<f32> = samples
        .chunks_exact(spec.channels as usize)
        .map(|frame| frame.iter().sum::<f32>() / spec.channels as f32)
        .collect();
    if spec.sample_rate == 16_000 {
        return Ok(mono);
    }
    let output_len = (mono.len() as u64 * 16_000 / u64::from(spec.sample_rate)) as usize;
    let mut resampler =
        FftFixedIn::<f32>::new(spec.sample_rate as usize, 16_000, 1024, 1, 1).map_err(internal)?;
    let delay = resampler.output_delay();
    let mut output = Vec::with_capacity(output_len + delay + 1024);
    for chunk in mono.chunks(1024) {
        let converted = if chunk.len() == 1024 {
            resampler.process(&[chunk], None)
        } else {
            resampler.process_partial(Some(&[chunk]), None)
        }
        .map_err(internal)?;
        output.extend_from_slice(&converted[0]);
    }
    while output.len() < output_len + delay {
        let converted = resampler
            .process_partial::<&[f32]>(None, None)
            .map_err(internal)?;
        output.extend_from_slice(&converted[0]);
    }
    Ok(output[delay..delay + output_len].to_vec())
}

async fn transcriptions(
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

fn data_dir() -> PathBuf {
    if std::env::args().nth(1).as_deref() == Some("service") {
        return std::env::var_os("PROGRAMDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
            .join("OpenVibeAI")
            .join("STT Server Next");
    }
    std::env::var_os("STT_NEXT_DATA_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("LOCALAPPDATA").map(|base| PathBuf::from(base).join("STT Server Next"))
        })
        .unwrap_or_else(|| PathBuf::from(".stt-server-next"))
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn reconcile_installed(
    db: &Connection,
    catalog: &[CatalogModel],
    data_dir: &Path,
) -> Result<(), Box<dyn Error>> {
    let mut query = db.prepare("SELECT id,path,sha256 FROM installed")?;
    let installed = query
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(query);
    let model_dir = fs::canonicalize(data_dir.join("models"))?;
    for (id, path, recorded_hash) in installed {
        let artifact = PathBuf::from(&path);
        let expected = catalog
            .iter()
            .find(|model| model.slug == id)
            .and_then(|model| {
                model
                    .files
                    .iter()
                    .find(|file| file.quant == model.default_quant)
            });
        let owned_path = fs::canonicalize(&artifact)
            .ok()
            .is_some_and(|resolved| resolved.starts_with(&model_dir));
        let verified = if let Some(file) = expected {
            owned_path
                && file.sha256.eq_ignore_ascii_case(&recorded_hash)
                && fs::metadata(&artifact).is_ok_and(|info| info.len() == file.size_bytes)
                && sha256_file(&artifact)
                    .is_ok_and(|actual| actual.eq_ignore_ascii_case(&file.sha256))
        } else {
            false
        };
        if !verified {
            db.execute("DELETE FROM installed WHERE id=?1", params![id])?;
            db.execute(
                "DELETE FROM settings WHERE key='selected_model' AND value=?1",
                params![id],
            )?;
            if owned_path {
                let quarantine = data_dir.join("quarantine");
                fs::create_dir_all(&quarantine)?;
                fs::rename(
                    &artifact,
                    quarantine.join(format!("{}-invalid.gguf", Uuid::new_v4())),
                )?;
            }
        }
    }
    Ok(())
}

fn reconcile_interrupted_imports(db: &Connection, data_dir: &Path) -> Result<(), Box<dyn Error>> {
    let mut query =
        db.prepare("SELECT id FROM operations WHERE kind='import' AND state='failed'")?;
    let operations = query
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(query);
    for operation in operations {
        if Uuid::parse_str(&operation).is_err() {
            continue;
        }
        let stage = data_dir.join("staging").join(format!("{operation}.part"));
        if stage.exists() {
            let quarantine = data_dir.join("quarantine");
            fs::create_dir_all(&quarantine)?;
            fs::rename(stage, quarantine.join(format!("{operation}-restart.gguf")))?;
        }
    }
    Ok(())
}

fn token_file(dir: &Path) -> Result<String, Box<dyn Error>> {
    let path = dir.join("auth.token");
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut file) => {
            let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
            file.write_all(token.as_bytes())?;
            Ok(token)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let mut token = String::new();
            OpenOptions::new()
                .read(true)
                .open(path)?
                .read_to_string(&mut token)?;
            if token.len() != 64 {
                return Err("Invalid token file".into());
            }
            Ok(token)
        }
        Err(error) => Err(error.into()),
    }
}

fn open_app() -> Result<Arc<App>, Box<dyn Error>> {
    open_app_at(data_dir())
}

fn open_app_at(data_dir: PathBuf) -> Result<Arc<App>, Box<dyn Error>> {
    let catalog: Catalog =
        serde_json::from_str(include_str!("../../catalog/handy-2026-08-17.json"))?;
    fs::create_dir_all(data_dir.join("models"))?;
    fs::create_dir_all(data_dir.join("staging"))?;
    let token = token_file(&data_dir)?;
    let db = Connection::open(data_dir.join("state.db"))?;
    db.execute_batch(
        "PRAGMA journal_mode=WAL;
         CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS installed(id TEXT PRIMARY KEY, path TEXT NOT NULL, sha256 TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS operations(id TEXT PRIMARY KEY, model_id TEXT NOT NULL, kind TEXT NOT NULL, state TEXT NOT NULL, error TEXT, progress_bytes INTEGER NOT NULL DEFAULT 0, total_bytes INTEGER NOT NULL DEFAULT 0);",
    )?;
    let mut columns = db.prepare("PRAGMA table_info(operations)")?;
    let names = columns
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(columns);
    if !names.iter().any(|name| name == "progress_bytes") {
        db.execute(
            "ALTER TABLE operations ADD COLUMN progress_bytes INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !names.iter().any(|name| name == "total_bytes") {
        db.execute(
            "ALTER TABLE operations ADD COLUMN total_bytes INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    db.execute("UPDATE operations SET state='failed', error='Interrupted by service restart' WHERE state IN ('queued','running')", [])?;
    reconcile_interrupted_imports(&db, &data_dir)?;
    reconcile_installed(&db, &catalog.models, &data_dir)?;
    let selected: Option<(String, String)> = db
        .query_row(
            "SELECT i.id,i.path FROM installed i JOIN settings s ON s.key='selected_model' AND s.value=i.id",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let preference: String = db
        .query_row(
            "SELECT value FROM settings WHERE key='preferred_backend'",
            [],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or_else(|| "auto".to_owned());
    let loaded = selected.and_then(|(id, path)| {
        let load = std::thread::Builder::new()
            .name("model-reload".to_owned())
            .stack_size(16 * 1024 * 1024)
            .spawn({
                let preference = preference.clone();
                move || load_engine(Path::new(&path), &preference)
            });
        match load.and_then(|worker| {
            worker
                .join()
                .map_err(|_| std::io::Error::other("model reload thread panicked"))
        }) {
            Ok(Ok((model, diagnostic))) => Some(LoadedModel {
                id,
                model,
                diagnostic,
            }),
            Ok(Err(error)) => {
                eprintln!("selected model could not reload: {error}");
                None
            }
            Err(error) => {
                eprintln!("selected model reload failed: {error}");
                None
            }
        }
    });
    Ok(Arc::new(App {
        catalog: catalog.models,
        db: Mutex::new(db),
        loaded: Mutex::new(loaded),
        data_dir,
        token,
        inference: Arc::new(Semaphore::new(1)),
        selection: tokio::sync::Mutex::new(()),
        http: reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(3600))
            .build()?,
    }))
}

async fn run_http(
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), Box<dyn Error>> {
    let app = open_app()?;
    let router = Router::new()
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
        .with_state(app.clone());
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    #[cfg(windows)]
    match std::env::args().nth(1).as_deref() {
        Some("service") => return service_host::dispatch(),
        Some("install") => return service_host::install(),
        Some("uninstall") => return service_host::uninstall(),
        _ => {}
    }
    run_http(async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
}

#[cfg(windows)]
mod service_host {
    use super::*;
    use std::{ffi::OsString, process::Command, sync::mpsc, thread};
    use windows_service::{
        define_windows_service,
        service::{
            ServiceAccess, ServiceAction, ServiceActionType, ServiceControl, ServiceControlAccept,
            ServiceErrorControl, ServiceExitCode, ServiceFailureActions, ServiceFailureResetPeriod,
            ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
        },
        service_control_handler::{self, ServiceControlHandlerResult},
        service_dispatcher,
        service_manager::{ServiceManager, ServiceManagerAccess},
    };

    const NAME: &str = "OpenVibeSttNext";
    define_windows_service!(ffi_service_main, service_main);

    pub fn dispatch() -> Result<(), Box<dyn Error>> {
        service_dispatcher::start(NAME, ffi_service_main)?;
        Ok(())
    }

    fn service_main(_args: Vec<OsString>) {
        if let Err(error) = run_service() {
            let log_path = data_dir().join("logs");
            let _ = fs::create_dir_all(&log_path);
            if let Ok(mut log) = OpenOptions::new()
                .append(true)
                .create(true)
                .open(log_path.join("service.log"))
            {
                let _ = writeln!(log, "Service failure: {error}");
            }
        }
    }

    fn service_status(state: ServiceState, accepted: ServiceControlAccept) -> ServiceStatus {
        ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: accepted,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::from_secs(10),
            process_id: None,
        }
    }

    fn run_service() -> Result<(), Box<dyn Error>> {
        let (stop_tx, stop_rx) = mpsc::channel();
        let status = service_control_handler::register(NAME, move |event| match event {
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            ServiceControl::Stop => {
                let _ = stop_tx.send(());
                ServiceControlHandlerResult::NoError
            }
            _ => ServiceControlHandlerResult::NotImplemented,
        })?;
        status.set_service_status(service_status(
            ServiceState::StartPending,
            ServiceControlAccept::empty(),
        ))?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        status.set_service_status(service_status(
            ServiceState::Running,
            ServiceControlAccept::STOP,
        ))?;
        let result = runtime.block_on(run_http(async move {
            let _ = tokio::task::spawn_blocking(move || stop_rx.recv()).await;
        }));
        status.set_service_status(service_status(
            ServiceState::Stopped,
            ServiceControlAccept::empty(),
        ))?;
        result
    }

    fn install_dir() -> PathBuf {
        std::env::var_os("ProgramFiles")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"))
            .join("OpenVibeAI")
            .join("STT Server Next")
    }

    fn owner_name() -> Result<String, Box<dyn Error>> {
        Ok(format!(
            "{}\\{}",
            std::env::var("USERDOMAIN")?,
            std::env::var("USERNAME")?
        ))
    }

    fn icacls(path: &Path, rules: &[String]) -> Result<(), Box<dyn Error>> {
        let status = Command::new("icacls").arg(path).args(rules).status()?;
        if !status.success() {
            return Err(format!("Could not protect ACL on {}", path.display()).into());
        }
        Ok(())
    }

    pub fn install() -> Result<(), Box<dyn Error>> {
        let install_dir = install_dir();
        fs::create_dir_all(&install_dir)?;
        let binary = install_dir.join("stt-server-next.exe");
        let source = std::env::current_exe()?;
        if source != binary {
            fs::copy(source, &binary)?;
        }
        let data = std::env::var_os("PROGRAMDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
            .join("OpenVibeAI")
            .join("STT Server Next");
        fs::create_dir_all(&data)?;
        let token_path = data.join("auth.token");
        let owner = owner_name()?;
        icacls(
            &data,
            &[
                "/inheritance:r".to_owned(),
                "/grant:r".to_owned(),
                "SYSTEM:(OI)(CI)F".to_owned(),
                "Administrators:(OI)(CI)F".to_owned(),
                format!("{owner}:RX"),
            ],
        )?;
        token_file(&data)?;
        icacls(
            &token_path,
            &[
                "/inheritance:r".to_owned(),
                "/grant:r".to_owned(),
                "SYSTEM:F".to_owned(),
                "Administrators:F".to_owned(),
                format!("{owner}:R"),
            ],
        )?;
        let manager = ServiceManager::local_computer(
            None::<&str>,
            ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
        )?;
        let info = ServiceInfo {
            name: OsString::from(NAME),
            display_name: OsString::from("OpenVibe STT Server Next"),
            service_type: ServiceType::OWN_PROCESS,
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path: binary,
            launch_arguments: vec![OsString::from("service")],
            dependencies: vec![],
            account_name: None,
            account_password: None,
        };
        let service = manager.create_service(
            &info,
            ServiceAccess::CHANGE_CONFIG | ServiceAccess::START | ServiceAccess::QUERY_STATUS,
        )?;
        service.set_description("Local GGUF batch transcription and model management")?;
        service.update_failure_actions(ServiceFailureActions {
            reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(24 * 60 * 60)),
            reboot_msg: None,
            command: None,
            actions: Some(vec![
                ServiceAction {
                    action_type: ServiceActionType::Restart,
                    delay: Duration::from_secs(5),
                },
                ServiceAction {
                    action_type: ServiceActionType::Restart,
                    delay: Duration::from_secs(15),
                },
                ServiceAction {
                    action_type: ServiceActionType::None,
                    delay: Duration::ZERO,
                },
            ]),
        })?;
        service.start(&[] as &[OsString])?;
        println!(
            "Installed and started {NAME}; token: {}",
            token_path.display()
        );
        Ok(())
    }

    pub fn uninstall() -> Result<(), Box<dyn Error>> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
        let service = manager.open_service(
            NAME,
            ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
        )?;
        if service.query_status()?.current_state != ServiceState::Stopped {
            service.stop()?;
            for _ in 0..20 {
                if service.query_status()?.current_state == ServiceState::Stopped {
                    break;
                }
                thread::sleep(Duration::from_secs(1));
            }
        }
        service.delete()?;
        drop(service);
        let binary = install_dir().join("stt-server-next.exe");
        if binary.exists() {
            fs::remove_file(binary)?;
        }
        println!("Removed {NAME}; model and state data preserved");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_downmixes_and_resamples_before_inference() {
        let mut cursor = Cursor::new(Vec::new());
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        {
            let mut writer = hound::WavWriter::new(&mut cursor, spec).unwrap();
            for _ in 0..48_000 {
                writer.write_sample(16_384_i16).unwrap();
                writer.write_sample(-16_384_i16).unwrap();
            }
            writer.finalize().unwrap();
        }
        let decoded = decode_wav(&cursor.into_inner()).unwrap();
        assert_eq!(decoded.len(), 16_000);
        assert!(decoded.iter().all(|sample| sample.abs() < 0.0001));
    }

    #[test]
    fn interrupted_import_is_failed_and_quarantined() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = open_app_at(path.clone()).unwrap();
        let op = Uuid::new_v4().to_string();
        let stage = path.join("staging").join(format!("{op}.part"));
        fs::write(&stage, b"incomplete model").unwrap();
        app.db
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO operations(id,model_id,kind,state,error) VALUES(?1,'parakeet-unified-en-0.6b','import','running',NULL)",
                params![op],
            )
            .unwrap();
        drop(ImportGuard {
            app: app.clone(),
            operation_id: op.clone(),
            stage: stage.clone(),
            complete: false,
        });
        assert!(!stage.exists());
        assert!(path
            .join("quarantine")
            .join(format!("{op}-interrupted.gguf"))
            .exists());
        assert_eq!(
            operation_state(&app, &op).unwrap().as_deref(),
            Some("failed")
        );
        assert!(installed_path(&app, "parakeet-unified-en-0.6b")
            .unwrap()
            .is_none());
        drop(app);
        let resolved = path.canonicalize().unwrap();
        assert!(resolved.starts_with(&parent));
        fs::remove_dir_all(resolved).unwrap();
    }

    #[test]
    fn cancelled_operation_cannot_promote_a_model() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = open_app_at(path.clone()).unwrap();
        let op = Uuid::new_v4().to_string();
        let stage = path.join("staging").join(format!("{op}.part"));
        fs::write(&stage, b"verified bytes").unwrap();
        app.db
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO operations(id,model_id,kind,state,error) VALUES(?1,'parakeet-unified-en-0.6b','import','cancelled',NULL)",
                params![op],
            )
            .unwrap();
        assert!(promote_verified_model(
            &app,
            &op,
            "parakeet-unified-en-0.6b",
            "fakehash",
            &stage,
            14,
        )
        .is_err());
        assert!(stage.exists());
        assert!(installed_path(&app, "parakeet-unified-en-0.6b")
            .unwrap()
            .is_none());
        drop(app);
        let resolved = path.canonicalize().unwrap();
        assert!(resolved.starts_with(&parent));
        fs::remove_dir_all(resolved).unwrap();
    }

    #[test]
    fn restart_quarantines_an_interrupted_import() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = open_app_at(path.clone()).unwrap();
        let op = Uuid::new_v4().to_string();
        let stage = path.join("staging").join(format!("{op}.part"));
        fs::write(&stage, b"partial upload").unwrap();
        app.db
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO operations(id,model_id,kind,state,error) VALUES(?1,'parakeet-unified-en-0.6b','import','running',NULL)",
                params![op],
            )
            .unwrap();
        drop(app);
        let reopened = open_app_at(path.clone()).unwrap();
        assert_eq!(
            operation_state(&reopened, &op).unwrap().as_deref(),
            Some("failed")
        );
        assert!(!stage.exists());
        assert!(path
            .join("quarantine")
            .join(format!("{op}-restart.gguf"))
            .exists());
        drop(reopened);
        let resolved = path.canonicalize().unwrap();
        assert!(resolved.starts_with(&parent));
        fs::remove_dir_all(resolved).unwrap();
    }

    #[test]
    fn first_start_is_empty_and_offline() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let app = open_app_at(path.clone()).unwrap();
        assert!(selected_id(&app).unwrap().is_none());
        assert!(app.loaded.lock().unwrap().is_none());
        assert_eq!(
            app.db
                .lock()
                .unwrap()
                .query_row("SELECT count(*) FROM operations", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(fs::read_dir(path.join("models")).unwrap().count(), 0);
        assert_eq!(fs::read_dir(path.join("staging")).unwrap().count(), 0);
        drop(app);
        let resolved = path.canonicalize().unwrap();
        assert!(resolved.starts_with(&parent));
        assert!(resolved
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("stt-server-next-test-"));
        fs::remove_dir_all(resolved).unwrap();
    }

    #[test]
    fn recommendations_are_curated_and_capabilities_are_conservative() {
        let catalog: Catalog =
            serde_json::from_str(include_str!("../../catalog/handy-2026-08-17.json")).unwrap();
        let mut recommended: Vec<_> = catalog
            .models
            .iter()
            .filter(|model| model.recommended)
            .collect();
        recommended.sort_by_key(|model| model.recommended_rank.unwrap_or(u32::MAX));
        assert_eq!(
            recommended.first().unwrap().slug,
            "parakeet-unified-en-0.6b"
        );
        let parakeet = recommended.first().unwrap();
        let matrix = capability_matrix(parakeet);
        assert_eq!(matrix["prompt"]["status"], "unsupported");
        assert_eq!(matrix["vocabulary"]["status"], "unsupported");
        assert_eq!(matrix["streaming"]["status"], "unsupported");
        let nemotron = catalog
            .models
            .iter()
            .find(|model| model.slug == "nemotron-3.5-asr-streaming-0.6b")
            .unwrap();
        assert_eq!(
            capability_matrix(nemotron)["streaming"]["status"],
            "unsupported"
        );
        assert_eq!(
            capability_matrix(nemotron)["language_detect"]["status"],
            "unsupported"
        );
    }
}
