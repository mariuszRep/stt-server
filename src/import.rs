use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

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
use crate::catalog::catalog_model;
use crate::errors::{internal, ApiError, ApiResult};
use crate::operations::update_operation;
use crate::store::{installed_path, promote_verified_model};

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::open_app_at;
    use crate::operations::operation_state;

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
}
