use std::fs;
use std::path::Path;
use std::sync::Arc;

use axum::{
    extract::{Path as UrlPath, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use rusqlite::params;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::Read as IoRead;
use uuid::Uuid;

use crate::app::App;
use crate::auth::authorized;
use crate::catalog::CatalogFile;
use crate::errors::{internal, ApiError, ApiResult};
use crate::operations::{operation_state, update_operation};
use crate::store::installed_file;

pub fn sha256_file(path: &Path) -> std::io::Result<String> {
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

pub async fn verify_model(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
) -> ApiResult<(StatusCode, Json<Value>)> {
    authorized(&headers, &app)?;
    let installed = installed_file(&app, &id)?.ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "model_not_installed",
            "Model is not installed",
        )
    })?;
    let path = installed.path.clone();
    let is_user_folder = installed.source == crate::store::SOURCE_USER_FOLDER;
    // Use the recorded quant/size/sha (from install/import time), not the
    // catalog's current default_quant, so a non-default install still verifies.
    let file = CatalogFile {
        filename: installed.filename.clone().unwrap_or_default(),
        quant: installed.quant.clone().unwrap_or_default(),
        size_bytes: installed.size_bytes.ok_or_else(|| {
            internal("Installed model is missing recorded size; run reconciliation")
        })?,
        sha256: installed.sha256.clone(),
    };
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
        let now = crate::store::now_ms();
        db.execute(
            "INSERT INTO operations(id,model_id,kind,state,error,progress_bytes,total_bytes,created_at,updated_at) VALUES(?1,?2,'verify','queued',NULL,0,?3,?4,?4)",
            params![op, id, file.size_bytes, now],
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
        let _inference = match task_app.inference.semaphore().acquire_owned().await {
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
        // A `user_folder` (drop-in) model is outside the managed store by
        // design: a verify mismatch marks it `needs_verification` in place
        // (no unregister, no quarantine move of the user's file). Selection
        // is refused while that flag is set until a refresh re-hashes it.
        if is_user_folder {
            if let Ok(db) = task_app.db.lock() {
                let _ = db.execute(
                    "UPDATE installed SET needs_verification=1 WHERE id=?1",
                    params![id],
                );
            }
            let _ = update_operation(
                &task_app,
                &task_op,
                "failed",
                Some("Drop-in model size or SHA-256 mismatch; needs re-verification"),
                0,
            );
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
