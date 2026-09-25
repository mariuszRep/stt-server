use std::sync::Arc;

use axum::{
    extract::{Path as UrlPath, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use rusqlite::params;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use crate::app::App;
use crate::auth::authorized;
use crate::catalog::{catalog_model, CatalogFile, CatalogModel};
use crate::errors::{internal, ApiError, ApiResult};
use crate::operations::{operation_state, update_operation};
use crate::store::{installed_path, promote_verified_model};
use crate::verify::sha256_file;

pub async fn install_model(
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

pub async fn download_model(
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
