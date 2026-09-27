//! `POST /v1/local/models/import-user`: admin-only. Copies GGUF files from
//! another install's models folder (default: this OS user's own per-user
//! data folder, `crate::app::per_user_data_dir`) into *this* install's
//! managed store -- the machine-wide-from-per-user case the goal describes
//! ("when a machine-wide install is set up on a PC where a user already has
//! models, the admin can import those models into the shared folder after
//! verification, instead of downloading again"). Also works per-user ->
//! per-user with an explicit `--from`, harmlessly.
//!
//! Simplest-robust design (deliberately not opening the source's
//! `state.db`): the managed store and the drop-in folder share one models
//! folder per install (`<data dir>/models`, see `app::default_user_models_dir`),
//! so every installed GGUF -- catalog download, API import, or drop-in --
//! sits there regardless of its recorded `source`. Scanning that folder and
//! matching each file's actual SHA-256 against the catalog (the same
//! `catalog_match_by_hash` the drop-in refresh in `dropin.rs` uses) verifies
//! every copy against the catalog before it is marked installed, without
//! ever trusting the source's own database. A file that doesn't match any
//! catalog entry (e.g. an unsupported custom drop-in model) is reported
//! `unsupported` and left alone -- never copied, never deleted. The source
//! is never modified: only files under this install's own data dir are
//! written.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    Json,
};
use rusqlite::params;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::app::App;
use crate::auth::authorized;
use crate::catalog::{catalog_match_by_hash, CatalogFile};
use crate::errors::{internal, ApiError, ApiResult};
use crate::operations::update_operation_with_code;
use crate::store::{self, installed_path, now_ms, promote_verified_model_with_source};
use crate::verify::sha256_file;

/// Source recorded on a model installed by this route, distinguishing it
/// from a direct catalog download, an API `import`, or a user's own drop-in
/// registration.
pub const SOURCE_IMPORT_USER: &str = "import_user";

#[derive(Debug, Deserialize, Default)]
pub struct ImportUserRequest {
    /// The source install's data directory. Defaults to this OS user's own
    /// per-user data folder (`crate::app::per_user_data_dir`) when omitted
    /// or `null`.
    #[serde(default)]
    pub from: Option<String>,
}

pub async fn import_user_models(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(request): Json<ImportUserRequest>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    authorized(&headers, &app)?;
    let from = match request.from {
        Some(from) => PathBuf::from(from),
        None => crate::app::per_user_data_dir().ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "missing_from",
                "No 'from' path given and no per-user data directory could be resolved (LOCALAPPDATA is unset)",
            )
        })?,
    };
    if paths_equal(&from, &app.data_dir) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "same_data_dir",
            "Source and destination data directories are the same",
        ));
    }

    let op = Uuid::new_v4().to_string();
    {
        let db = app.db.lock().map_err(internal)?;
        let now = now_ms();
        db.execute(
            "INSERT INTO operations(id,model_id,kind,state,error,progress_bytes,total_bytes,progress_items,total_items,created_at,updated_at) VALUES(?1,'','import_user','queued',NULL,0,0,0,0,?2,?2)",
            params![op, now],
        )
        .map_err(internal)?;
    }
    let task_app = app.clone();
    let task_op = op.clone();
    tokio::spawn(async move {
        run_import_user(task_app, task_op, from).await;
    });
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({"operation_id":op,"state":"queued"})),
    ))
}

/// Compares two paths lexically (case-insensitively, as Windows paths are)
/// without touching disk -- mirrors `app::install_scope_for`'s approach so
/// this works even when one side doesn't exist yet.
fn paths_equal(a: &Path, b: &Path) -> bool {
    a.to_string_lossy()
        .eq_ignore_ascii_case(&b.to_string_lossy())
}

struct HashedCandidate {
    path: PathBuf,
    size: u64,
    sha256: String,
}

/// Hashes a candidate file off the async runtime, the same
/// hash-then-match-then-copy shape `dropin.rs::hash_and_probe` uses for the
/// drop-in scan.
fn hash_candidate(path: PathBuf) -> Result<HashedCandidate, String> {
    let size = std::fs::metadata(&path)
        .map_err(|error| error.to_string())?
        .len();
    let sha256 = sha256_file(&path).map_err(|error| error.to_string())?;
    Ok(HashedCandidate { path, size, sha256 })
}

/// Streams `source` into a fresh staging file under this app's `staging`
/// dir, off the async runtime. Kept separate from the hash step above
/// because the source's hash is already known by then; this just needs a
/// plain copy (the destination is verified again by `sha256_file` after the
/// copy, matching `import.rs`'s "trust nothing, re-check after the write"
/// pattern for anything crossing a process/filesystem boundary).
fn copy_to_staging(source: &Path, staging: &Path) -> Result<(), String> {
    if let Some(parent) = staging.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    std::fs::copy(source, staging)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn set_progress_items(app: &App, operation_id: &str, done: u64, total: u64) -> Result<(), String> {
    let db = app.db.lock().map_err(|error| error.to_string())?;
    db.execute(
        "UPDATE operations SET progress_items=?2, total_items=?3, updated_at=?4 WHERE id=?1 AND state <> 'cancelled'",
        params![operation_id, done, total, now_ms()],
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

fn is_cancelled(app: &App, operation_id: &str) -> bool {
    crate::operations::operation_state(app, operation_id)
        .ok()
        .flatten()
        .as_deref()
        == Some("cancelled")
}

/// Entry point run inside the `tokio::spawn`ed task backing
/// `POST /v1/local/models/import-user`. A missing source models folder is
/// treated as an empty source (nothing to import), the same way `dropin.rs`
/// treats a missing drop-in folder -- it simply means the source install has
/// no models yet, not an error.
pub async fn run_import_user(app: Arc<App>, operation_id: String, from: PathBuf) {
    let _ = update_operation_with_code(&app, &operation_id, "running", None, None, 0);

    let source_models_dir = from.join("models");
    let entries = match std::fs::read_dir(&source_models_dir) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("gguf"))
            })
            .collect::<Vec<_>>(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            let _ = update_operation_with_code(
                &app,
                &operation_id,
                "failed",
                Some(&format!(
                    "Could not read source models folder {}: {error}",
                    source_models_dir.display()
                )),
                None,
                0,
            );
            return;
        }
    };

    let mut imported = Vec::new();
    let mut skipped = Vec::new();
    let mut unsupported = Vec::new();
    let total = entries.len() as u64;

    for (index, path) in entries.into_iter().enumerate() {
        if is_cancelled(&app, &operation_id) {
            return;
        }
        let _ = set_progress_items(&app, &operation_id, index as u64, total);

        let candidate = {
            let hash_path = path.clone();
            match tokio::task::spawn_blocking(move || hash_candidate(hash_path)).await {
                Ok(Ok(candidate)) => candidate,
                Ok(Err(error)) => {
                    unsupported.push(json!({"path": path.display().to_string(), "reason": error}));
                    continue;
                }
                Err(error) => {
                    unsupported.push(
                        json!({"path": path.display().to_string(), "reason": error.to_string()}),
                    );
                    continue;
                }
            }
        };

        let Some((model, file)) =
            catalog_match_by_hash(&app.catalog, candidate.size, &candidate.sha256)
        else {
            unsupported.push(json!({
                "path": candidate.path.display().to_string(),
                "reason": "No catalog model matches this file's size and SHA-256",
            }));
            continue;
        };
        let model_id = model.slug.clone();
        let file: CatalogFile = file.clone();

        match installed_path(&app, &model_id) {
            Ok(Some(_)) => {
                skipped.push(json!({"model": model_id, "reason": "already_installed"}));
                continue;
            }
            Ok(None) => {}
            Err(error) => {
                unsupported.push(json!({"model": model_id, "reason": format!("{error:?}")}));
                continue;
            }
        }

        match import_one(&app, &operation_id, &model_id, &file, &candidate.path).await {
            Ok(()) => imported.push(json!({"model": model_id})),
            Err(error) => {
                unsupported.push(json!({"model": model_id, "reason": error}));
            }
        }
    }

    let _ = set_progress_items(&app, &operation_id, total, total);
    let result = json!({
        "imported": imported,
        "skipped": skipped,
        "unsupported": unsupported,
    });
    let _ = store::set_operation_result(&app, &operation_id, &result);
    let _ = update_operation_with_code(&app, &operation_id, "completed", None, None, 0);
}

/// Copies one already-hash-matched source file into staging, re-verifies it
/// (the copy itself, not just the source, must match the catalog -- guards
/// against a truncated/corrupted copy) and promotes it into the managed
/// store with `source: "import_user"`. The source file is never touched: a
/// verification failure quarantines the *copy*, and any I/O error simply
/// leaves the staged copy (if any) and the source both in place.
async fn import_one(
    app: &Arc<App>,
    operation_id: &str,
    model_id: &str,
    file: &CatalogFile,
    source_path: &Path,
) -> Result<(), String> {
    let stage = app
        .data_dir
        .join("staging")
        .join(format!("{operation_id}-{model_id}.part"));
    {
        let source = source_path.to_path_buf();
        let staging = stage.clone();
        tokio::task::spawn_blocking(move || copy_to_staging(&source, &staging))
            .await
            .map_err(|error| error.to_string())??;
    }
    let verify_stage = stage.clone();
    let (size, sha256) = tokio::task::spawn_blocking(move || {
        let size = std::fs::metadata(&verify_stage)
            .map_err(|error| error.to_string())?
            .len();
        let sha256 = sha256_file(&verify_stage).map_err(|error| error.to_string())?;
        Ok::<_, String>((size, sha256))
    })
    .await
    .map_err(|error| error.to_string())??;

    if size != file.size_bytes || !sha256.eq_ignore_ascii_case(&file.sha256) {
        let quarantine = app.data_dir.join("quarantine");
        let _ = std::fs::create_dir_all(&quarantine);
        let _ = std::fs::rename(
            &stage,
            quarantine.join(format!("{operation_id}-{model_id}-hash-mismatch.gguf")),
        );
        return Err("Copied file's SHA-256 no longer matches the catalog".to_owned());
    }

    // `promote_verified_model_with_source` requires a 'running' operation to
    // promote into, matching the shared install/import/verify contract; give
    // each imported model its own short-lived sub-operation row so several
    // models can be promoted under the one parent `import_user` operation
    // without them fighting over a single row's state.
    let sub_op = Uuid::new_v4().to_string();
    {
        let db = app.db.lock().map_err(|error| error.to_string())?;
        let now = now_ms();
        db.execute(
            "INSERT INTO operations(id,model_id,kind,state,error,progress_bytes,total_bytes,created_at,updated_at) VALUES(?1,?2,'import_user',?3,NULL,0,?4,?5,?5)",
            params![sub_op, model_id, "running", file.size_bytes as i64, now],
        )
        .map_err(|error| error.to_string())?;
    }
    let promoted = promote_verified_model_with_source(
        app,
        &sub_op,
        model_id,
        file,
        &stage,
        size,
        SOURCE_IMPORT_USER,
    );
    if let Ok(db) = app.db.lock() {
        let _ = db.execute("DELETE FROM operations WHERE id=?1", params![sub_op]);
    }
    promoted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::router;
    use crate::app::open_app_at;
    use crate::catalog::CatalogModel;
    use crate::operations::operation_state;
    use crate::store::installed_file;
    use axum::body::Body;
    use axum::http::Request;
    use sha2::{Digest, Sha256};
    use tower::ServiceExt;

    fn temp_dir() -> PathBuf {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()))
    }

    fn fake_model(slug: &str, bytes: &[u8]) -> CatalogModel {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        serde_json::from_value(json!({
            "id": format!("org/{slug}"), "revision": "abc123", "slug": slug,
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

    async fn post_import_user(app: &Arc<App>, from: &Path) -> Value {
        let token = app.token.clone();
        let router = router(app.clone());
        let request = Request::builder()
            .method("POST")
            .uri("/v1/local/models/import-user")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"from": from.display().to_string()}).to_string(),
            ))
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn wait_terminal(app: &Arc<App>, op: &str) -> String {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if let Some(state) = operation_state(app, op).unwrap() {
                    if matches!(state.as_str(), "completed" | "failed" | "cancelled") {
                        return state;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn import_user_copies_and_verifies_and_skips_existing() {
        let source_dir = temp_dir();
        let dest_dir = temp_dir();
        std::fs::create_dir_all(source_dir.join("models")).unwrap();
        let bytes_a = vec![0xABu8; 4_096];
        let bytes_b = vec![0xCDu8; 8_192];
        std::fs::write(source_dir.join("models").join("a.gguf"), &bytes_a).unwrap();
        std::fs::write(source_dir.join("models").join("b.gguf"), &bytes_b).unwrap();

        let mut app = open_app_at(dest_dir.clone()).unwrap();
        {
            let catalog = Arc::get_mut(&mut app).expect("sole owner before first clone");
            catalog.catalog.push(fake_model("model-a", &bytes_a));
            catalog.catalog.push(fake_model("model-b", &bytes_b));
        }

        let body = post_import_user(&app, &source_dir).await;
        let op = body["operation_id"].as_str().unwrap().to_owned();
        assert_eq!(wait_terminal(&app, &op).await, "completed");

        let installed_a = installed_file(&app, "model-a").unwrap().unwrap();
        assert_eq!(installed_a.source, SOURCE_IMPORT_USER);
        assert!(installed_a.path.exists());
        let installed_b = installed_file(&app, "model-b").unwrap().unwrap();
        assert_eq!(installed_b.source, SOURCE_IMPORT_USER);

        // Source files are completely untouched.
        assert_eq!(
            std::fs::read(source_dir.join("models").join("a.gguf")).unwrap(),
            bytes_a
        );
        assert_eq!(
            std::fs::read(source_dir.join("models").join("b.gguf")).unwrap(),
            bytes_b
        );

        // A second run skips both models -- already installed.
        let body2 = post_import_user(&app, &source_dir).await;
        let op2 = body2["operation_id"].as_str().unwrap().to_owned();
        assert_eq!(wait_terminal(&app, &op2).await, "completed");
        let db = app.db.lock().unwrap();
        let result: String = db
            .query_row(
                "SELECT result FROM operations WHERE id=?1",
                params![op2],
                |row| row.get(0),
            )
            .unwrap();
        drop(db);
        let result: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(result["imported"].as_array().unwrap().len(), 0);
        assert_eq!(result["skipped"].as_array().unwrap().len(), 2);

        drop(app);
        std::fs::remove_dir_all(&source_dir).unwrap();
        std::fs::remove_dir_all(dest_dir.canonicalize().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn import_user_quarantines_a_source_file_whose_hash_the_catalog_does_not_match() {
        let source_dir = temp_dir();
        let dest_dir = temp_dir();
        std::fs::create_dir_all(source_dir.join("models")).unwrap();
        // Deliberately unmatched: no catalog entry has this hash/size.
        std::fs::write(
            source_dir.join("models").join("unknown.gguf"),
            b"not a catalog model",
        )
        .unwrap();

        let app = open_app_at(dest_dir.clone()).unwrap();
        let body = post_import_user(&app, &source_dir).await;
        let op = body["operation_id"].as_str().unwrap().to_owned();
        assert_eq!(wait_terminal(&app, &op).await, "completed");
        let db = app.db.lock().unwrap();
        let result: String = db
            .query_row(
                "SELECT result FROM operations WHERE id=?1",
                params![op],
                |row| row.get(0),
            )
            .unwrap();
        drop(db);
        let result: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(result["imported"].as_array().unwrap().len(), 0);
        assert_eq!(result["unsupported"].as_array().unwrap().len(), 1);
        // Untouched, not quarantined -- it was never matched, so it was
        // never copied in the first place.
        assert!(source_dir.join("models").join("unknown.gguf").exists());
        assert!(!dest_dir.join("quarantine").exists());

        drop(app);
        std::fs::remove_dir_all(&source_dir).unwrap();
        std::fs::remove_dir_all(dest_dir.canonicalize().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn import_user_missing_source_folder_completes_with_nothing_imported() {
        let source_dir = temp_dir(); // never created
        let dest_dir = temp_dir();
        let app = open_app_at(dest_dir.clone()).unwrap();
        let body = post_import_user(&app, &source_dir).await;
        let op = body["operation_id"].as_str().unwrap().to_owned();
        assert_eq!(wait_terminal(&app, &op).await, "completed");
        drop(app);
        std::fs::remove_dir_all(dest_dir.canonicalize().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn import_user_requires_admin_token() {
        let dest_dir = temp_dir();
        let app = open_app_at(dest_dir.clone()).unwrap();
        let router = router(app.clone());
        let request = Request::builder()
            .method("POST")
            .uri("/v1/local/models/import-user")
            .header("authorization", format!("Bearer {}", app.user_token))
            .header("content-type", "application/json")
            .body(Body::from(json!({"from": "C:\\nowhere"}).to_string()))
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["code"], "admin_required");
        drop(app);
        std::fs::remove_dir_all(dest_dir.canonicalize().unwrap()).unwrap();
    }
}
