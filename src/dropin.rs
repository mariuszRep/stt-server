//! `POST /models/manage/refresh`: scans the user-writable drop-in models
//! folder (and, trivially, keeps the managed store's own reconciliation
//! untouched) for `.gguf` files a user copied in by hand, hashing and
//! probing each one before it becomes selectable. See the "Drop-in models
//! and refresh" section of the design goal for the full rule set; this
//! module implements the "Refresh rules" list precisely:
//!
//! 1. Skip a file already registered with the same path, size and mtime.
//! 2. Hash it (`spawn_blocking`), reporting item progress on the operation.
//! 3. A size+SHA-256 catalog match registers it as that catalog model/quant,
//!    `source: "user_folder"`, in place (no move/copy); if that catalog
//!    model is already installed from the managed store, it is a duplicate.
//! 4. Otherwise probe the GGUF header (`crate::gguf_probe`, ported from
//!    Handy). A supported architecture registers a custom model; anything
//!    else is listed with a reason and left untouched.
//! 5. A previously registered file that disappeared is unregistered (and
//!    deselected/unloaded); one whose size or mtime changed is re-hashed and
//!    re-probed here (unlike the cheap startup reconciliation in `app.rs`,
//!    which only flags `needs_verification` without re-hashing).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};

use crate::app::App;
use crate::catalog::catalog_match_by_hash;
use crate::gguf_probe::{custom_model_id, probe_gguf_file};
use crate::operations::update_operation_with_code;
use crate::store::{self, now_ms, SOURCE_CATALOG_DOWNLOAD, SOURCE_IMPORT, SOURCE_USER_FOLDER};
use crate::verify::sha256_file;

/// Error code recorded on the operation when no `user_models_dir` is
/// configured and none can be defaulted (only possible if `LOCALAPPDATA`
/// itself is unset in a per-user install; see `app::default_user_models_dir`,
/// which otherwise always defaults inside this install's single data
/// folder, for either scope).
pub const ERROR_CODE_USER_MODELS_DIR_NOT_CONFIGURED: &str = "user_models_dir_not_configured";

fn file_mtime_ms(path: &Path) -> Option<i64> {
    std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis() as i64)
}

/// Unregister `id` and, if it is currently selected/loaded, deselect and
/// unload it too (rule 5's "unregister (and deselect/unload if selected)").
fn unregister_and_deselect(app: &App, id: &str) -> Result<(), String> {
    let db = app.db.lock().map_err(|error| error.to_string())?;
    db.execute("DELETE FROM installed WHERE id=?1", params![id])
        .map_err(|error| error.to_string())?;
    db.execute(
        "DELETE FROM settings WHERE key='selected_model' AND value=?1",
        params![id],
    )
    .map_err(|error| error.to_string())?;
    drop(db);
    if let Ok(mut loaded) = app.loaded.lock() {
        if loaded.as_ref().is_some_and(|active| active.id == id) {
            *loaded = None;
        }
    }
    Ok(())
}

struct RegisteredRow {
    id: String,
    path: String,
    size_bytes: Option<u64>,
    mtime_ms: Option<i64>,
    needs_verification: bool,
}

fn registered_user_folder_rows(app: &App) -> Result<Vec<RegisteredRow>, String> {
    let db = app.db.lock().map_err(|error| error.to_string())?;
    let mut statement = db
        .prepare(
            "SELECT id,path,size_bytes,mtime_ms,needs_verification FROM installed WHERE source=?1",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(params![SOURCE_USER_FOLDER], |row| {
            Ok(RegisteredRow {
                id: row.get(0)?,
                path: row.get(1)?,
                size_bytes: row.get::<_, Option<i64>>(2)?.map(|v| v as u64),
                mtime_ms: row.get(3)?,
                needs_verification: row.get::<_, i64>(4)? != 0,
            })
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    Ok(rows)
}

/// Hash + probe a candidate file off the async runtime (rule 2: "spawn_blocking").
fn hash_and_probe(
    path: PathBuf,
) -> Result<(u64, String, Result<crate::gguf_probe::GgufProbe, String>), String> {
    let size = std::fs::metadata(&path)
        .map(|meta| meta.len())
        .map_err(|error| error.to_string())?;
    let before = crate::verify::file_mtime_ms(&path).map_err(|error| error.to_string())?;
    let sha256 = sha256_file(&path).map_err(|error| error.to_string())?;
    let probe = probe_gguf_file(&path);
    let after = crate::verify::file_mtime_ms(&path).map_err(|error| error.to_string())?;
    if before != after
        || std::fs::metadata(&path)
            .map_err(|error| error.to_string())?
            .len()
            != size
    {
        return Err("File changed while being read; retry refresh after copying finishes".into());
    }
    Ok((size, sha256, probe))
}

/// Register (insert or update) a `user_folder` installed row.
#[allow(clippy::too_many_arguments)]
fn upsert_user_folder_row(
    app: &App,
    id: &str,
    path: &Path,
    sha256: &str,
    quant: Option<&str>,
    filename: &str,
    size_bytes: u64,
    mtime_ms: Option<i64>,
    custom_name: Option<&str>,
    custom_arch: Option<&str>,
    custom_languages: Option<&Value>,
    custom_claims: Option<&Value>,
) -> Result<(), String> {
    let db = app.db.lock().map_err(|error| error.to_string())?;
    db.execute(
        "INSERT INTO installed(id,path,sha256,quant,filename,size_bytes,source,custom_name,custom_arch,custom_languages,custom_claims,mtime_ms,needs_verification)
         VALUES(?1,?2,?3,?4,?5,?6,'user_folder',?7,?8,?9,?10,?11,0)
         ON CONFLICT(id) DO UPDATE SET path=excluded.path, sha256=excluded.sha256, quant=excluded.quant,
             filename=excluded.filename, size_bytes=excluded.size_bytes, source='user_folder',
             custom_name=excluded.custom_name, custom_arch=excluded.custom_arch,
             custom_languages=excluded.custom_languages, custom_claims=excluded.custom_claims,
             mtime_ms=excluded.mtime_ms, needs_verification=0",
        params![
            id,
            path.to_string_lossy().as_ref(),
            sha256,
            quant,
            filename,
            size_bytes as i64,
            custom_name,
            custom_arch,
            custom_languages.map(|v| v.to_string()),
            custom_claims.map(|v| v.to_string()),
            mtime_ms,
        ],
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

/// Is `id` already installed via the managed store (catalog download or
/// import), i.e. not itself a `user_folder` row? Used to detect rule 3's
/// "already installed from the managed store" duplicate case.
fn installed_from_managed_store(app: &App, id: &str) -> Result<bool, String> {
    let db = app.db.lock().map_err(|error| error.to_string())?;
    let source: Option<String> = db
        .query_row(
            "SELECT source FROM installed WHERE id=?1",
            params![id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    Ok(matches!(
        source.as_deref(),
        Some(SOURCE_CATALOG_DOWNLOAD) | Some(SOURCE_IMPORT)
    ))
}

/// The durable result of a refresh operation, matching the `result` shape
/// stored on the operation record.
fn build_result(
    registered: Vec<Value>,
    duplicates: Vec<Value>,
    unsupported: Vec<Value>,
    removed: Vec<Value>,
    changed: Vec<Value>,
) -> Value {
    json!({
        "registered": registered,
        "duplicates": duplicates,
        "unsupported": unsupported,
        "removed": removed,
        "changed": changed,
    })
}

/// Run the actual scan/registration work, given a resolved, existing drop
/// folder. Pure orchestration: all blocking I/O for a single candidate file
/// goes through `hash_and_probe` on a blocking thread.
async fn scan_and_register(
    app: &Arc<App>,
    operation_id: &str,
    dir: &Path,
    catalog: &[crate::catalog::CatalogModel],
) -> Result<Value, String> {
    // Fail the operation if the folder itself cannot be enumerated.
    let entries =
        std::fs::read_dir(dir).map_err(|error| format!("Cannot read model folder: {error}"))?;
    let mut registered = Vec::new();
    let mut duplicates = Vec::new();
    let mut unsupported = Vec::new();
    let mut removed = Vec::new();
    let mut changed = Vec::new();

    // Rule 5, first half: reconcile already-registered rows against the
    // current directory state. `seen` collects canonical paths that are
    // already correctly registered (or just got re-registered here) so the
    // directory scan below doesn't re-process them as brand-new files.
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let existing = registered_user_folder_rows(app)?;
    let total_candidates = existing.len();
    let mut done_items = 0u64;
    for row in existing {
        let path = PathBuf::from(&row.path);
        let current = std::fs::metadata(&path);
        match current {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                unregister_and_deselect(app, &row.id)?;
                removed.push(json!({"id": row.id, "path": row.path}));
            }
            Err(error) => {
                app.db
                    .lock()
                    .map_err(|error| error.to_string())?
                    .execute(
                        "UPDATE installed SET needs_verification=1 WHERE id=?1",
                        params![row.id],
                    )
                    .map_err(|error| error.to_string())?;
                unsupported.push(
                    json!({"path": row.path, "reason": error.to_string(), "retryable": true}),
                );
                seen.insert(path.canonicalize().unwrap_or(path));
            }
            Ok(metadata) => {
                let mtime = file_mtime_ms(&path);
                if !row.needs_verification
                    && Some(metadata.len()) == row.size_bytes
                    && mtime == row.mtime_ms
                {
                    seen.insert(path.canonicalize().unwrap_or(path));
                } else {
                    // Changed: re-hash and re-probe (unlike the cheap
                    // startup reconciliation, refresh actually re-verifies).
                    let hash_path = path.clone();
                    let checked = tokio::task::spawn_blocking(move || hash_and_probe(hash_path))
                        .await
                        .map_err(|error| error.to_string())?;
                    let (size, sha256, probe) = match checked {
                        Ok(result) => result,
                        Err(error) => {
                            app.db
                                .lock()
                                .map_err(|error| error.to_string())?
                                .execute(
                                    "UPDATE installed SET needs_verification=1 WHERE id=?1",
                                    params![row.id],
                                )
                                .map_err(|error| error.to_string())?;
                            unsupported.push(
                                json!({"path": row.path, "reason": error, "retryable": true}),
                            );
                            seen.insert(path.canonicalize().unwrap_or(path));
                            done_items += 1;
                            let _ = set_progress_items(
                                app,
                                operation_id,
                                done_items,
                                total_candidates as u64,
                            );
                            continue;
                        }
                    };
                    if let Some((model, file)) = catalog_match_by_hash(catalog, size, &sha256) {
                        upsert_user_folder_row(
                            app,
                            &row.id,
                            &path,
                            &sha256,
                            Some(&file.quant),
                            &file.filename,
                            size,
                            mtime,
                            None,
                            None,
                            None,
                            None,
                        )?;
                        changed.push(
                            json!({"id": row.id, "path": row.path, "catalog_id": model.slug}),
                        );
                    } else if let Ok(probe) = probe.as_ref() {
                        if probe.is_supported() {
                            let name_source = probe
                                .display_name
                                .clone()
                                .unwrap_or_else(|| file_stem(&path));
                            let new_id = custom_model_id(&name_source, &sha256);
                            let claims = json!({
                                "streaming": probe.supports_streaming,
                                "translate": probe.supports_translation,
                                "lang_detect": probe.supports_language_detect,
                            });
                            upsert_user_folder_row(
                                app,
                                &new_id,
                                &path,
                                &sha256,
                                None,
                                &file_name(&path),
                                size,
                                mtime,
                                probe.display_name.as_deref(),
                                Some(&probe.architecture),
                                Some(&json!(probe.languages)),
                                Some(&claims),
                            )?;
                            if new_id != row.id {
                                unregister_and_deselect(app, &row.id)?;
                            }
                            changed.push(json!({"id": new_id, "path": row.path}));
                        } else {
                            unregister_and_deselect(app, &row.id)?;
                            unsupported
                                .push(json!({"path": row.path, "reason": format!("unsupported architecture '{}'", probe.architecture)}));
                        }
                    } else {
                        unregister_and_deselect(app, &row.id)?;
                        unsupported.push(
                            json!({"path": row.path, "reason": probe.err().unwrap_or_default()}),
                        );
                    }
                    seen.insert(path.canonicalize().unwrap_or(path));
                }
            }
        }
        done_items += 1;
        let _ = set_progress_items(app, operation_id, done_items, total_candidates as u64);
    }

    // Rule 1-4: scan the directory for `.gguf` files not already accounted
    // for above, ignoring `.part` (in-progress download/upload artifacts).
    let mut candidates: Vec<PathBuf> = Vec::new();
    {
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    unsupported.push(json!({"path": dir.to_string_lossy(), "reason": error.to_string(), "retryable": true}));
                    continue;
                }
            };
            let path = entry.path();
            // A locked file may fail metadata lookup; still try it so the
            // result reports an individual access error rather than hiding it.
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let is_gguf = path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("gguf"));
            if !is_gguf {
                continue;
            }
            let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
            if seen.contains(&canonical) {
                continue;
            }
            candidates.push(path);
        }
    }

    let total = total_candidates as u64 + candidates.len() as u64;
    let _ = set_progress_items(app, operation_id, done_items, total);

    for path in candidates {
        let hash_path = path.clone();
        let checked = tokio::task::spawn_blocking(move || hash_and_probe(hash_path))
            .await
            .map_err(|error| error.to_string())?;
        let (size, sha256, probe) = match checked {
            Ok(result) => result,
            Err(error) => {
                unsupported.push(
                    json!({"path": path.to_string_lossy(), "reason": error, "retryable": true}),
                );
                done_items += 1;
                let _ = set_progress_items(app, operation_id, done_items, total);
                continue;
            }
        };
        if let Some((model, file)) = catalog_match_by_hash(catalog, size, &sha256) {
            if installed_from_managed_store(app, &model.slug)? {
                duplicates.push(json!({
                    "path": path.to_string_lossy(),
                    "catalog_id": model.slug,
                    "quant": file.quant,
                }));
            } else {
                let mtime = file_mtime_ms(&path);
                upsert_user_folder_row(
                    app,
                    &model.slug,
                    &path,
                    &sha256,
                    Some(&file.quant),
                    &file.filename,
                    size,
                    mtime,
                    None,
                    None,
                    None,
                    None,
                )?;
                registered.push(json!({
                    "id": model.slug,
                    "quant": file.quant,
                    "source": "user_folder",
                    "path": path.to_string_lossy(),
                }));
            }
        } else {
            match probe {
                Ok(probe) if probe.is_supported() => {
                    let name_source = probe
                        .display_name
                        .clone()
                        .unwrap_or_else(|| file_stem(&path));
                    let id = custom_model_id(&name_source, &sha256);
                    let mtime = file_mtime_ms(&path);
                    let claims = json!({
                        "streaming": probe.supports_streaming,
                        "translate": probe.supports_translation,
                        "lang_detect": probe.supports_language_detect,
                    });
                    upsert_user_folder_row(
                        app,
                        &id,
                        &path,
                        &sha256,
                        None,
                        &file_name(&path),
                        size,
                        mtime,
                        probe.display_name.as_deref(),
                        Some(&probe.architecture),
                        Some(&json!(probe.languages)),
                        Some(&claims),
                    )?;
                    registered.push(json!({
                        "id": id,
                        "quant": Value::Null,
                        "source": "user_folder",
                        "custom": true,
                        "architecture": probe.architecture,
                        "path": path.to_string_lossy(),
                    }));
                }
                Ok(probe) => {
                    unsupported.push(json!({
                        "path": path.to_string_lossy(),
                        "reason": format!("unsupported architecture '{}'", probe.architecture),
                    }));
                }
                Err(reason) => {
                    unsupported.push(json!({"path": path.to_string_lossy(), "reason": reason}));
                }
            }
        }
        done_items += 1;
        let _ = set_progress_items(app, operation_id, done_items, total);
    }

    Ok(build_result(
        registered,
        duplicates,
        unsupported,
        removed,
        changed,
    ))
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "model".to_string())
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "model.gguf".to_string())
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

/// Entry point run inside the `tokio::spawn`ed task backing
/// `POST /models/manage/refresh`. Resolves the effective `user_models_dir`
/// (persisted setting, else the process default), then scans it. A missing
/// directory is treated as an empty scan (nothing to register) rather than
/// an error -- it simply hasn't been created yet.
pub async fn run_refresh(app: Arc<App>, operation_id: String) {
    let _ = update_operation_with_code(&app, &operation_id, "running", None, None, 0);

    let configured = match store::user_models_dir_setting(&app) {
        Ok(value) => value,
        Err(error) => {
            let _ = update_operation_with_code(
                &app,
                &operation_id,
                "failed",
                Some(&error.message),
                None,
                0,
            );
            return;
        }
    };
    let dir = configured
        .map(PathBuf::from)
        .or_else(crate::app::default_user_models_dir);
    let Some(dir) = dir else {
        let _ = update_operation_with_code(
            &app,
            &operation_id,
            "failed",
            Some("user_models_dir is not configured"),
            Some(ERROR_CODE_USER_MODELS_DIR_NOT_CONFIGURED),
            0,
        );
        return;
    };

    let result = if dir.exists() {
        scan_and_register(&app, &operation_id, &dir, &app.catalog).await
    } else {
        Ok(build_result(vec![], vec![], vec![], vec![], vec![]))
    };

    match result {
        Ok(value) => {
            let _ = store::set_operation_result(&app, &operation_id, &value);
            let _ = update_operation_with_code(&app, &operation_id, "completed", None, None, 0);
        }
        Err(error) => {
            let _ =
                update_operation_with_code(&app, &operation_id, "failed", Some(&error), None, 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::open_app_at;
    use crate::catalog::{CatalogFile, ModelClaims};
    use sha2::{Digest, Sha256};

    fn temp_dir() -> PathBuf {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        parent.join(format!("stt-server-test-{}", uuid::Uuid::new_v4()))
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn refresh_skips_locked_files_and_recovers_on_retry() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = temp_dir();
        let app = open_app_at(dir.clone()).unwrap();
        let folder = dir.join("drop");
        std::fs::create_dir_all(&folder).unwrap();
        let good = folder.join("good.gguf");
        let locked = folder.join("locked.gguf");
        std::fs::write(&good, fake_gguf("whisper", "Good")).unwrap();
        std::fs::write(&locked, fake_gguf("whisper", "Locked")).unwrap();
        let op = uuid::Uuid::new_v4().to_string();
        app.db
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO operations(id,model_id,kind,state) VALUES(?1,'','refresh','running')",
                params![op],
            )
            .unwrap();
        let handle = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&locked)
            .unwrap();
        let first = scan_and_register(&app, &op, &folder, &[]).await.unwrap();
        assert_eq!(first["registered"].as_array().unwrap().len(), 1);
        assert_eq!(first["unsupported"].as_array().unwrap().len(), 1);
        assert_eq!(first["unsupported"][0]["retryable"], true);
        drop(handle);
        let second = scan_and_register(&app, &op, &folder, &[]).await.unwrap();
        assert_eq!(second["registered"].as_array().unwrap().len(), 1);
        // Also exercise an already registered, subsequently locked file.
        app.db
            .lock()
            .unwrap()
            .execute("UPDATE installed SET needs_verification=1", [])
            .unwrap();
        let handle = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&locked)
            .unwrap();
        let third = scan_and_register(&app, &op, &folder, &[]).await.unwrap();
        assert_eq!(third["unsupported"].as_array().unwrap().len(), 1);
        assert_eq!(
            app.db
                .lock()
                .unwrap()
                .query_row("SELECT count(*) FROM installed", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
        drop(handle);
        let fourth = scan_and_register(&app, &op, &folder, &[]).await.unwrap();
        assert!(fourth["unsupported"].as_array().unwrap().is_empty());
        assert_eq!(
            app.db
                .lock()
                .unwrap()
                .query_row("SELECT sum(needs_verification) FROM installed", [], |r| r
                    .get::<_, i64>(
                    0
                ))
                .unwrap(),
            0
        );
        assert!(scan_and_register(&app, &op, &folder.join("missing"), &[])
            .await
            .is_err());
        drop(app);
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    /// A tiny fake catalog with one model/file, hashed from `bytes`, for
    /// tests that need a size+SHA-256 catalog match without a real,
    /// multi-gigabyte model file.
    fn fake_catalog(slug: &str, bytes: &[u8]) -> Vec<crate::catalog::CatalogModel> {
        vec![crate::catalog::CatalogModel {
            id: format!("fake/{slug}"),
            revision: "main".to_owned(),
            slug: slug.to_owned(),
            name: "Fake Model".to_owned(),
            architecture: "whisper".to_owned(),
            family: "fake".to_owned(),
            license: "mit".to_owned(),
            languages: vec!["en".to_owned()],
            capabilities: ModelClaims {
                streaming: false,
                translate: false,
                lang_detect: false,
                timestamps: "none".to_owned(),
            },
            speed_score: None,
            accuracy_score: None,
            files: vec![CatalogFile {
                filename: "fake.gguf".to_owned(),
                quant: "Q8_0".to_owned(),
                size_bytes: bytes.len() as u64,
                sha256: sha256_hex(bytes),
            }],
            default_quant: "Q8_0".to_owned(),
            recommended: false,
            recommended_rank: None,
            mirrors: None,
        }]
    }

    /// Minimal valid GGUF bytes with the header fields the drop-in prober
    /// reads. `architecture` drives whether this is a known/supported arch.
    fn fake_gguf(architecture: &str, name: &str) -> Vec<u8> {
        crate::gguf_probe::tests::build_test_gguf(
            architecture,
            Some(name),
            &["en"],
            Some(false),
            Some(false),
            Some(false),
        )
    }

    #[tokio::test]
    async fn catalog_match_registers_as_user_folder_with_catalog_identity() {
        let path = temp_dir();
        let app = open_app_at(path.clone()).unwrap();
        let dropdir = path.join("dropin");
        std::fs::create_dir_all(&dropdir).unwrap();
        let bytes = b"pretend catalog model bytes".to_vec();
        let file_path = dropdir.join("my-model.gguf");
        std::fs::write(&file_path, &bytes).unwrap();
        let catalog = fake_catalog("fake-catalog-model", &bytes);

        let op = uuid::Uuid::new_v4().to_string();
        {
            let db = app.db.lock().unwrap();
            db.execute(
                "INSERT INTO operations(id,model_id,kind,state) VALUES(?1,'','refresh','running')",
                params![op],
            )
            .unwrap();
        }
        let result = scan_and_register(&app, &op, &dropdir, &catalog)
            .await
            .unwrap();
        assert_eq!(result["registered"].as_array().unwrap().len(), 1);
        assert_eq!(result["registered"][0]["id"], "fake-catalog-model");
        assert_eq!(result["registered"][0]["source"], "user_folder");

        let installed = crate::store::installed_file(&app, "fake-catalog-model")
            .unwrap()
            .unwrap();
        assert_eq!(installed.source, SOURCE_USER_FOLDER);
        assert_eq!(installed.quant.as_deref(), Some("Q8_0"));
        assert_eq!(installed.path, file_path);

        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn duplicate_of_installed_catalog_model_is_not_reregistered() {
        let path = temp_dir();
        let app = open_app_at(path.clone()).unwrap();
        let dropdir = path.join("dropin");
        std::fs::create_dir_all(&dropdir).unwrap();
        let bytes = b"already installed catalog bytes".to_vec();
        let file_path = dropdir.join("dup.gguf");
        std::fs::write(&file_path, &bytes).unwrap();
        let catalog = fake_catalog("fake-dup-model", &bytes);
        let file = &catalog[0].files[0];

        let stage_dir = path.join("staging");
        std::fs::create_dir_all(&stage_dir).unwrap();
        let stage_file = stage_dir.join("staged.gguf");
        std::fs::write(&stage_file, &bytes).unwrap();
        let install_op = uuid::Uuid::new_v4().to_string();
        app.db.lock().unwrap().execute("INSERT INTO operations(id,model_id,kind,state) VALUES(?1,'fake-dup-model','install','running')", params![install_op]).unwrap();
        crate::store::promote_verified_model_with_source(
            &app,
            &install_op,
            "fake-dup-model",
            file,
            &stage_file,
            bytes.len() as u64,
            SOURCE_CATALOG_DOWNLOAD,
        )
        .unwrap();

        let op = uuid::Uuid::new_v4().to_string();
        app.db
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO operations(id,model_id,kind,state) VALUES(?1,'','refresh','running')",
                params![op],
            )
            .unwrap();
        let result = scan_and_register(&app, &op, &dropdir, &catalog)
            .await
            .unwrap();
        assert_eq!(result["registered"].as_array().unwrap().len(), 0);
        assert_eq!(result["duplicates"].as_array().unwrap().len(), 1);
        assert_eq!(result["duplicates"][0]["catalog_id"], "fake-dup-model");

        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn known_architecture_registers_a_custom_model_with_expected_id_format() {
        let path = temp_dir();
        let app = open_app_at(path.clone()).unwrap();
        let dropdir = path.join("dropin");
        std::fs::create_dir_all(&dropdir).unwrap();
        let bytes = fake_gguf("parakeet", "My Custom Parakeet");
        let sha256 = sha256_hex(&bytes);
        let file_path = dropdir.join("custom.gguf");
        std::fs::write(&file_path, &bytes).unwrap();

        let op = uuid::Uuid::new_v4().to_string();
        app.db
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO operations(id,model_id,kind,state) VALUES(?1,'','refresh','running')",
                params![op],
            )
            .unwrap();
        let result = scan_and_register(&app, &op, &dropdir, &[]).await.unwrap();
        let registered = result["registered"].as_array().unwrap();
        assert_eq!(registered.len(), 1);
        let expected_id = custom_model_id("My Custom Parakeet", &sha256);
        assert_eq!(registered[0]["id"], expected_id);
        assert!(expected_id.starts_with("custom-my-custom-parakeet-"));

        let installed = crate::store::installed_file(&app, &expected_id)
            .unwrap()
            .unwrap();
        assert_eq!(installed.source, SOURCE_USER_FOLDER);
        assert_eq!(installed.custom_arch.as_deref(), Some("parakeet"));
        assert_eq!(installed.custom_name.as_deref(), Some("My Custom Parakeet"));

        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn unsupported_architecture_is_listed_not_registered() {
        let path = temp_dir();
        let app = open_app_at(path.clone()).unwrap();
        let dropdir = path.join("dropin");
        std::fs::create_dir_all(&dropdir).unwrap();
        let bytes = fake_gguf("llama", "Not A Speech Model");
        let file_path = dropdir.join("unsupported.gguf");
        std::fs::write(&file_path, &bytes).unwrap();

        let op = uuid::Uuid::new_v4().to_string();
        app.db
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO operations(id,model_id,kind,state) VALUES(?1,'','refresh','running')",
                params![op],
            )
            .unwrap();
        let result = scan_and_register(&app, &op, &dropdir, &[]).await.unwrap();
        assert_eq!(result["registered"].as_array().unwrap().len(), 0);
        let unsupported = result["unsupported"].as_array().unwrap();
        assert_eq!(unsupported.len(), 1);
        assert!(unsupported[0]["reason"].as_str().unwrap().contains("llama"));
        assert!(file_path.exists(), "unsupported file must be left in place");
        assert!(crate::store::all_installed(&app).unwrap().is_empty());

        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn disappeared_registered_file_is_unregistered_and_deselected() {
        let path = temp_dir();
        let app = open_app_at(path.clone()).unwrap();
        let dropdir = path.join("dropin");
        std::fs::create_dir_all(&dropdir).unwrap();
        let bytes = fake_gguf("parakeet", "Vanishing Model");
        let file_path = dropdir.join("vanish.gguf");
        std::fs::write(&file_path, &bytes).unwrap();

        let op1 = uuid::Uuid::new_v4().to_string();
        app.db
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO operations(id,model_id,kind,state) VALUES(?1,'','refresh','running')",
                params![op1],
            )
            .unwrap();
        let result = scan_and_register(&app, &op1, &dropdir, &[]).await.unwrap();
        let id = result["registered"][0]["id"].as_str().unwrap().to_owned();

        {
            let db = app.db.lock().unwrap();
            db.execute(
                "INSERT INTO settings(key,value) VALUES('selected_model',?1)",
                params![id],
            )
            .unwrap();
        }
        std::fs::remove_file(&file_path).unwrap();

        let op2 = uuid::Uuid::new_v4().to_string();
        app.db
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO operations(id,model_id,kind,state) VALUES(?1,'','refresh','running')",
                params![op2],
            )
            .unwrap();
        let result = scan_and_register(&app, &op2, &dropdir, &[]).await.unwrap();
        assert_eq!(result["removed"].as_array().unwrap().len(), 1);
        assert!(crate::store::installed_file(&app, &id).unwrap().is_none());
        assert!(crate::store::selected_id(&app).unwrap().is_none());

        drop(app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }
}
