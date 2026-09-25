use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{params, OptionalExtension};

use crate::app::App;
use crate::errors::{internal, ApiResult};

pub fn installed_path(app: &App, id: &str) -> ApiResult<Option<PathBuf>> {
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

pub fn selected_id(app: &App) -> ApiResult<Option<String>> {
    let db = app.db.lock().map_err(internal)?;
    db.query_row(
        "SELECT value FROM settings WHERE key = 'selected_model'",
        [],
        |row| row.get(0),
    )
    .optional()
    .map_err(internal)
}

pub fn backend_preference(app: &App) -> ApiResult<String> {
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

pub fn promote_verified_model(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::open_app_at;
    use uuid::Uuid;

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
}
