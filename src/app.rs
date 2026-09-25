use std::{
    error::Error,
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use rusqlite::{params, Connection, OptionalExtension};
use uuid::Uuid;

use crate::catalog::{Catalog, CatalogModel};
use crate::engine::{load_engine, LoadedModel};
use crate::queue::InferenceQueue;

pub struct App {
    pub catalog: Vec<CatalogModel>,
    pub db: Mutex<Connection>,
    pub loaded: Mutex<Option<LoadedModel>>,
    pub data_dir: PathBuf,
    pub token: String,
    /// FIFO queue for the single inference slot; see `crate::queue`. Its
    /// waiting-list bound and wait deadline are read live from `limits`.
    pub inference: InferenceQueue,
    /// Queue/inference limits: settable via `PATCH /v1/local/config` and
    /// overridable per-process by CLI flags (`--queue-max-waiting`,
    /// `--queue-wait-timeout-ms`, `--inference-timeout-ms`). Read at request
    /// time so a setting change (or process launched with flags) applies
    /// live, without a restart.
    pub limits: std::sync::RwLock<RuntimeLimits>,
    pub selection: tokio::sync::Mutex<()>,
    pub http: reqwest::Client,
}

/// Optional operational limits; `None` means unbounded/no-timeout, matching
/// the current shipping server's default behavior.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RuntimeLimits {
    pub queue_max_waiting: Option<usize>,
    pub queue_wait_timeout_ms: Option<u64>,
    pub inference_timeout_ms: Option<u64>,
}

pub fn is_service_mode() -> bool {
    std::env::args().nth(1).as_deref() == Some("service")
}

pub fn data_dir() -> PathBuf {
    if is_service_mode() {
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

/// Default drop-in `user_models_dir` when the setting has never been written
/// (user decision 2026-09-25): `%LOCALAPPDATA%\OpenVibeAI\STT Server\models`
/// of the installing/running user, in *normal* (non-service) runs only. A
/// LocalSystem service run has no useful per-user `LOCALAPPDATA`, so there is
/// no default in service mode; `service::install` instead records the
/// installing user's path explicitly as the `user_models_dir` setting.
/// `STT_NEXT_USER_MODELS_DIR_DEFAULT` overrides this for tests, mirroring
/// `STT_NEXT_DATA_DIR`'s role for the data directory.
pub fn default_user_models_dir() -> Option<PathBuf> {
    if let Some(over) = std::env::var_os("STT_NEXT_USER_MODELS_DIR_DEFAULT") {
        return Some(PathBuf::from(over));
    }
    if is_service_mode() {
        return None;
    }
    std::env::var_os("LOCALAPPDATA").map(|base| {
        PathBuf::from(base)
            .join("OpenVibeAI")
            .join("STT Server")
            .join("models")
    })
}

pub fn token_file(dir: &Path) -> Result<String, Box<dyn Error>> {
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

/// Reconcile a single `user_folder` (drop-in) row at startup: rule 5 from the
/// drop-in models design applies here, and only here -- these files live
/// outside the managed store by design, so startup reconciliation never
/// quarantines or moves them. It only checks existence/size/mtime (no
/// re-hash; that is refresh's job, see `crate::dropin`):
/// - the file disappeared -> unregister (and deselect/unload if selected);
/// - the file's size or mtime changed -> mark `needs_verification` in place,
///   which blocks selection until an explicit refresh re-hashes it;
/// - otherwise leave the row untouched.
fn reconcile_user_folder_row(
    db: &Connection,
    id: &str,
    path: &str,
    recorded_size: Option<u64>,
    recorded_mtime: Option<i64>,
) -> Result<(), Box<dyn Error>> {
    let artifact = PathBuf::from(path);
    let metadata = fs::metadata(&artifact);
    match metadata {
        Err(_) => {
            db.execute("DELETE FROM installed WHERE id=?1", params![id])?;
            db.execute(
                "DELETE FROM settings WHERE key='selected_model' AND value=?1",
                params![id],
            )?;
        }
        Ok(info) => {
            let mtime_ms = info
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_millis() as i64);
            let changed = Some(info.len()) != recorded_size || mtime_ms != recorded_mtime;
            if changed {
                db.execute(
                    "UPDATE installed SET needs_verification=1 WHERE id=?1",
                    params![id],
                )?;
            }
        }
    }
    Ok(())
}

fn reconcile_installed(
    db: &Connection,
    catalog: &[CatalogModel],
    data_dir: &Path,
) -> Result<(), Box<dyn Error>> {
    let mut query =
        db.prepare("SELECT id,path,sha256,quant,source,size_bytes,mtime_ms FROM installed")?;
    let installed = query
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<i64>>(5)?,
                row.get::<_, Option<i64>>(6)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(query);
    let model_dir = fs::canonicalize(data_dir.join("models"))?;
    for (id, path, recorded_hash, recorded_quant, source, recorded_size, recorded_mtime) in
        installed
    {
        if source == crate::store::SOURCE_USER_FOLDER {
            reconcile_user_folder_row(
                db,
                &id,
                &path,
                recorded_size.map(|v| v as u64),
                recorded_mtime,
            )?;
            continue;
        }
        let artifact = PathBuf::from(&path);
        // Use the recorded quant (not the model's current default_quant) so a
        // model installed at a non-default quant reconciles against the file
        // it actually has on disk.
        let expected = recorded_quant.as_deref().and_then(|quant| {
            catalog
                .iter()
                .find(|model| model.slug == id)
                .and_then(|model| model.files.iter().find(|file| file.quant == quant))
        });
        let owned_path = fs::canonicalize(&artifact)
            .ok()
            .is_some_and(|resolved| resolved.starts_with(&model_dir));
        let verified = if let Some(file) = expected {
            owned_path
                && file.sha256.eq_ignore_ascii_case(&recorded_hash)
                && fs::metadata(&artifact).is_ok_and(|info| info.len() == file.size_bytes)
                && crate::verify::sha256_file(&artifact)
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

pub fn open_app() -> Result<Arc<App>, Box<dyn Error>> {
    open_app_at(data_dir())
}

pub fn open_app_at(data_dir: PathBuf) -> Result<Arc<App>, Box<dyn Error>> {
    open_app_at_with_overrides(data_dir, RuntimeLimits::default())
}

/// Same as [`open_app_at`], but `cli_overrides` (from the binary's optional
/// `--queue-max-waiting`/`--queue-wait-timeout-ms`/`--inference-timeout-ms`
/// flags) takes precedence, field by field, over the persisted settings for
/// this process only; the persisted settings are left untouched.
pub fn open_app_at_with_overrides(
    data_dir: PathBuf,
    cli_overrides: RuntimeLimits,
) -> Result<Arc<App>, Box<dyn Error>> {
    let catalog: Catalog = serde_json::from_str(include_str!("../catalog/handy-2026-08-17.json"))?;
    fs::create_dir_all(data_dir.join("models"))?;
    fs::create_dir_all(data_dir.join("staging"))?;
    let token = token_file(&data_dir)?;
    let mut db = Connection::open(data_dir.join("state.db"))?;
    db.execute_batch("PRAGMA journal_mode=WAL;")?;
    crate::store::migrate(&mut db, &catalog.models, &data_dir)?;
    let now = crate::store::now_ms();
    db.execute("UPDATE operations SET state='failed', error='Interrupted by service restart', updated_at=?1, finished_at=?1 WHERE state IN ('queued','running')", params![now])?;
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
            Ok(Ok((model, diagnostic))) => Some(LoadedModel::new(id, model, diagnostic)),
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
    let stored_limits = crate::store::read_runtime_limits(&db)?;
    let limits = RuntimeLimits {
        queue_max_waiting: cli_overrides
            .queue_max_waiting
            .or(stored_limits.queue_max_waiting),
        queue_wait_timeout_ms: cli_overrides
            .queue_wait_timeout_ms
            .or(stored_limits.queue_wait_timeout_ms),
        inference_timeout_ms: cli_overrides
            .inference_timeout_ms
            .or(stored_limits.inference_timeout_ms),
    };
    Ok(Arc::new(App {
        catalog: catalog.models,
        db: Mutex::new(db),
        loaded: Mutex::new(loaded),
        data_dir,
        token,
        inference: InferenceQueue::new(),
        limits: std::sync::RwLock::new(limits),
        selection: tokio::sync::Mutex::new(()),
        // No blanket total-request timeout: a 48 GB model download must not
        // be killed just because it is still progressing. Staleness is
        // instead bounded per-chunk by download::STALL_TIMEOUT.
        http: reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .build()?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operations::operation_state;
    use crate::store::selected_id;

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

    /// Startup reconciliation must never quarantine or move a `user_folder`
    /// (drop-in) file -- it lives outside the managed store by design. An
    /// untouched file's row is left exactly as registered; a changed file is
    /// flagged `needs_verification` in place (no hashing, no move); a
    /// disappeared file is unregistered.
    #[test]
    fn startup_reconcile_leaves_user_folder_files_untouched() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()));
        let dropdir = parent.join(format!("stt-server-next-dropin-{}", Uuid::new_v4()));
        fs::create_dir_all(&dropdir).unwrap();
        let kept = dropdir.join("kept.gguf");
        fs::write(&kept, b"kept bytes").unwrap();
        let changed = dropdir.join("changed.gguf");
        fs::write(&changed, b"original bytes").unwrap();

        let app = open_app_at(path.clone()).unwrap();
        {
            let db = app.db.lock().unwrap();
            let kept_meta = fs::metadata(&kept).unwrap();
            let kept_mtime = kept_meta
                .modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as i64;
            db.execute(
                "INSERT INTO installed(id,path,sha256,source,size_bytes,mtime_ms) VALUES('kept-model',?1,'x','user_folder',?2,?3)",
                params![kept.to_string_lossy().as_ref(), kept_meta.len() as i64, kept_mtime],
            )
            .unwrap();
            // Recorded size deliberately wrong to simulate a file that
            // changed on disk since it was registered.
            db.execute(
                "INSERT INTO installed(id,path,sha256,source,size_bytes,mtime_ms) VALUES('changed-model',?1,'x','user_folder',1,0)",
                params![changed.to_string_lossy().as_ref()],
            )
            .unwrap();
        }
        drop(app);
        // Reopen: startup reconciliation runs again on open.
        let reopened = open_app_at(path.clone()).unwrap();
        let db = reopened.db.lock().unwrap();

        // Untouched file: row and file both survive unchanged.
        let kept_row: (String, i64) = db
            .query_row(
                "SELECT path, needs_verification FROM installed WHERE id='kept-model'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(kept_row.0, kept.to_string_lossy());
        assert_eq!(kept_row.1, 0);
        assert!(kept.exists(), "user_folder file must never be moved");

        // Changed file: flagged needs_verification, never moved/quarantined.
        let changed_flag: i64 = db
            .query_row(
                "SELECT needs_verification FROM installed WHERE id='changed-model'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(changed_flag, 1);
        assert!(changed.exists(), "user_folder file must never be moved");
        assert!(
            !path.join("quarantine").exists()
                || fs::read_dir(path.join("quarantine")).unwrap().count() == 0
        );

        drop(db);
        drop(reopened);
        fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
        fs::remove_dir_all(dropdir.canonicalize().unwrap()).unwrap();
    }
}
