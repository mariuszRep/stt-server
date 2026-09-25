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
    /// Fallback download hosts from the catalog, tried in order after
    /// HuggingFace (see `download::candidate_urls`).
    pub mirrors: Vec<String>,
    pub db: Mutex<Connection>,
    pub loaded: Mutex<Option<LoadedModel>>,
    pub data_dir: PathBuf,
    pub token: String,
    /// Bounded FIFO queue for the single inference slot; see `crate::queue`.
    pub inference: InferenceQueue,
    pub selection: tokio::sync::Mutex<()>,
    pub http: reqwest::Client,
}

pub fn data_dir() -> PathBuf {
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

fn reconcile_installed(
    db: &Connection,
    catalog: &[CatalogModel],
    data_dir: &Path,
) -> Result<(), Box<dyn Error>> {
    let mut query = db.prepare("SELECT id,path,sha256,quant FROM installed")?;
    let installed = query
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(query);
    let model_dir = fs::canonicalize(data_dir.join("models"))?;
    for (id, path, recorded_hash, recorded_quant) in installed {
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
    Ok(Arc::new(App {
        catalog: catalog.models,
        mirrors: catalog.mirrors,
        db: Mutex::new(db),
        loaded: Mutex::new(loaded),
        data_dir,
        token,
        inference: InferenceQueue::new(),
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
}
