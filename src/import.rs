use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use axum::{
    extract::{Multipart, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use rusqlite::params;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use crate::app::App;
use crate::auth::authorized;
use crate::catalog::{catalog_model, CatalogFile};
use crate::download::should_flush_progress;
use crate::errors::{internal, ApiError, ApiResult};
use crate::operations::update_operation;
use crate::store::{installed_path, promote_verified_model_with_source, SOURCE_IMPORT};

pub struct ImportGuard {
    pub app: Arc<App>,
    pub operation_id: String,
    pub stage: PathBuf,
    pub complete: bool,
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

pub async fn import_model(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> ApiResult<(StatusCode, Json<Value>)> {
    authorized(&headers, &app)?;
    let mut model_id: Option<String> = None;
    let mut requested_quant: Option<String> = None;
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
            "quant" if model_id.is_some() && requested_quant.is_none() && !imported => {
                let quant = field.text().await.map_err(|error| {
                    ApiError::new(StatusCode::BAD_REQUEST, "invalid_quant", error.to_string())
                })?;
                requested_quant = Some(quant);
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
                // Preselect the expected file when a quant was given; otherwise
                // any of the model's files is a candidate match and the size
                // cap during streaming is the largest candidate.
                let preselected: Option<CatalogFile> = match &requested_quant {
                    Some(quant) => Some(
                        model
                            .files
                            .iter()
                            .find(|file| &file.quant == quant)
                            .cloned()
                            .ok_or_else(|| {
                                ApiError::new(
                                    StatusCode::BAD_REQUEST,
                                    "invalid_quant",
                                    format!("Unknown quant '{quant}' for model {}", model.slug),
                                )
                            })?,
                    ),
                    None => None,
                };
                let cap = match &preselected {
                    Some(file) => file.size_bytes,
                    None => model
                        .files
                        .iter()
                        .map(|file| file.size_bytes)
                        .max()
                        .ok_or_else(|| internal("Model has no catalog files"))?,
                };
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
                    let now = crate::store::now_ms();
                    db.execute(
                        "INSERT INTO operations(id,model_id,kind,state,error,progress_bytes,total_bytes,created_at,updated_at) VALUES(?1,?2,'import','running',NULL,0,?3,?4,?4)",
                        params![op, id, cap, now],
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
                // Throttled the same way as `download.rs`'s streaming loop
                // (M3): writing progress to SQLite on every multipart chunk
                // takes the global `app.db` mutex hundreds of thousands of
                // times for a multi-GB import, stalling every other
                // request -- including transcription -- that also needs it.
                let mut last_flush = Instant::now();
                let mut bytes_since_flush = 0_u64;
                while let Some(chunk) = field.chunk().await.map_err(|error| {
                    ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "invalid_multipart",
                        error.to_string(),
                    )
                })? {
                    size += chunk.len() as u64;
                    if size > cap {
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
                    bytes_since_flush += chunk.len() as u64;
                    output.write_all(&chunk).await.map_err(internal)?;
                    if should_flush_progress(last_flush.elapsed(), bytes_since_flush, false) {
                        update_operation(&app, &op, "running", None, size)?;
                        last_flush = Instant::now();
                        bytes_since_flush = 0;
                    }
                }
                output.sync_all().await.map_err(internal)?;
                drop(output);
                update_operation(&app, &op, "running", None, size)?;
                let actual = format!("{:x}", digest.finalize());
                let matched = match &preselected {
                    Some(file) => {
                        if size == file.size_bytes && actual.eq_ignore_ascii_case(&file.sha256) {
                            Some(file.clone())
                        } else {
                            None
                        }
                    }
                    None => model
                        .files
                        .iter()
                        .find(|file| {
                            file.size_bytes == size && file.sha256.eq_ignore_ascii_case(&actual)
                        })
                        .cloned(),
                };
                let Some(matched_file) = matched else {
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
                };
                promote_verified_model_with_source(
                    &app,
                    &op,
                    id,
                    &matched_file,
                    &stage,
                    size,
                    SOURCE_IMPORT,
                )
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::router;
    use crate::app::open_app_at;
    use crate::catalog::CatalogModel;
    use crate::operations::operation_state;
    use axum::body::Body;
    use axum::http::Request;
    use sha2::Sha256;
    use tower::ServiceExt;

    fn fake_model(bytes: &[u8]) -> CatalogModel {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        serde_json::from_value(json!({
            "id": "org/fake-import-model", "revision": "abc123", "slug": "fake-import-model",
            "name": "Fake", "architecture": "whisper", "family": "whisper", "license": "mit",
            "languages": ["en"],
            "capabilities": {"streaming": false, "translate": false, "lang_detect": false, "timestamps": "none"},
            "speed_score": null, "accuracy_score": null,
            "files": [{
                "filename": "model.gguf", "quant": "Q4_K_M",
                "size_bytes": bytes.len(), "sha256": format!("{:x}", hasher.finalize()),
            }],
            "default_quant": "Q4_K_M", "recommended": false, "recommended_rank": null
        }))
        .unwrap()
    }

    fn import_body(boundary: &str, model: &str, file_bytes: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(b"Content-Disposition: form-data; name=\"model\"\r\n\r\n");
        body.extend_from_slice(model.as_bytes());
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            b"Content-Disposition: form-data; name=\"file\"; filename=\"model.gguf\"\r\n",
        );
        body.extend_from_slice(b"Content-Type: application/octet-stream\r\n\r\n");
        body.extend_from_slice(file_bytes);
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        body
    }

    /// M3: import progress is throttled (`should_flush_progress`, shared
    /// with `download.rs`) instead of writing SQLite on every multipart
    /// chunk. This exercises the whole streaming-then-force-flush path end
    /// to end and pins the outcome that matters to a client polling
    /// `GET /models/manage/operations/{id}`: the operation ends `completed` with
    /// `progress_bytes` equal to the full file size, not just whatever the
    /// last throttled write happened to catch.
    #[tokio::test]
    async fn import_completes_with_full_progress_recorded() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-test-{}", Uuid::new_v4()));
        let mut app = open_app_at(path.clone()).unwrap();
        let file_bytes = vec![0x42u8; 5_000];
        let model = fake_model(&file_bytes);
        Arc::get_mut(&mut app)
            .expect("sole owner before first clone")
            .catalog
            .push(model);
        let token = app.token.clone();
        let router = router(app.clone());
        let boundary = "X-BOUNDARY";
        let body = import_body(boundary, "fake-import-model", &file_bytes);
        let request = Request::builder()
            .method("POST")
            .uri("/models/manage/import")
            .header("authorization", format!("Bearer {token}"))
            .header(
                "content-type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(body))
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        let op = body["operation_id"].as_str().unwrap().to_owned();
        assert_eq!(
            operation_state(&app, &op).unwrap().as_deref(),
            Some("completed")
        );
        let progress: i64 = app
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT progress_bytes FROM operations WHERE id=?1",
                params![op],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(progress as u64, file_bytes.len() as u64);
        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    #[test]
    fn interrupted_import_is_failed_and_quarantined() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-test-{}", Uuid::new_v4()));
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
}
