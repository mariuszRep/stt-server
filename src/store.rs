use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};

use crate::app::App;
use crate::catalog::{CatalogFile, CatalogModel};
use crate::errors::{internal, ApiResult};

pub const CURRENT_SCHEMA_VERSION: i64 = 2;

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone)]
pub struct InstalledFile {
    pub path: PathBuf,
    pub sha256: String,
    pub quant: Option<String>,
    pub filename: Option<String>,
    pub size_bytes: Option<u64>,
}

pub fn installed_file(app: &App, id: &str) -> ApiResult<Option<InstalledFile>> {
    let db = app.db.lock().map_err(internal)?;
    query_installed_file(&db, id).map_err(internal)
}

fn query_installed_file(db: &Connection, id: &str) -> rusqlite::Result<Option<InstalledFile>> {
    db.query_row(
        "SELECT path, sha256, quant, filename, size_bytes FROM installed WHERE id = ?1",
        params![id],
        |row| {
            Ok(InstalledFile {
                path: PathBuf::from(row.get::<_, String>(0)?),
                sha256: row.get(1)?,
                quant: row.get(2)?,
                filename: row.get(3)?,
                size_bytes: row.get::<_, Option<i64>>(4)?.map(|value| value as u64),
            })
        },
    )
    .optional()
}

pub fn installed_path(app: &App, id: &str) -> ApiResult<Option<PathBuf>> {
    Ok(installed_file(app, id)?.map(|file| file.path))
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

/// Store the installed file's catalog-recorded quant/filename/size alongside
/// its path and hash, and mark the owning operation completed with timestamps.
pub fn promote_verified_model(
    app: &App,
    operation_id: &str,
    model_id: &str,
    file: &CatalogFile,
    stage: &Path,
    bytes: u64,
) -> Result<(), String> {
    let destination = app
        .data_dir
        .join("models")
        .join(format!("{}.gguf", file.sha256));
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
            "INSERT INTO installed(id,path,sha256,quant,filename,size_bytes) VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                model_id,
                destination.to_string_lossy().as_ref(),
                file.sha256,
                file.quant,
                file.filename,
                file.size_bytes as i64
            ],
        )
        .map_err(|error| error.to_string())?;
    fs::rename(stage, &destination).map_err(|error| error.to_string())?;
    let now = now_ms();
    transaction
        .execute(
            "UPDATE operations SET state='completed', error=NULL, progress_bytes=?2, updated_at=?3, finished_at=?3 WHERE id=?1",
            params![operation_id, bytes, now],
        )
        .map_err(|error| error.to_string())?;
    if let Err(error) = transaction.commit() {
        let _ = fs::rename(&destination, stage);
        return Err(error.to_string());
    }
    Ok(())
}

fn user_version(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("PRAGMA user_version", [], |row| row.get(0))
}

fn set_user_version(conn: &Connection, version: i64) -> rusqlite::Result<()> {
    conn.execute_batch(&format!("PRAGMA user_version = {version};"))
}

fn table_exists(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
        params![name],
        |row| row.get::<_, i64>(0),
    )
    .map(|count| count > 0)
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut statement = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(names.iter().any(|name| name == column))
}

fn backup_db(data_dir: &Path, from_version: i64) -> Result<(), Box<dyn Error>> {
    let src = data_dir.join("state.db");
    let dst = data_dir.join(format!("state.db.bak-v{from_version}"));
    fs::copy(&src, &dst)?;
    Ok(())
}

/// Run all pending schema migrations in one transaction. v1 is today's
/// unversioned schema (settings, installed(id,path,sha256),
/// operations(...progress_bytes,total_bytes)). v2 adds installed
/// quant/filename/size_bytes (backfilled by sha256 match against the
/// catalog) and operation timestamps created_at/updated_at/finished_at.
/// A non-empty older DB is backed up to state.db.bak-v<old> first.
pub fn migrate(
    conn: &mut Connection,
    catalog: &[CatalogModel],
    data_dir: &Path,
) -> Result<(), Box<dyn Error>> {
    let version = user_version(conn)?;
    if version >= CURRENT_SCHEMA_VERSION {
        return Ok(());
    }
    let had_pre_existing_schema = version == 0 && table_exists(conn, "installed")?;
    if had_pre_existing_schema {
        conn.execute_batch("PRAGMA wal_checkpoint(FULL);")?;
        backup_db(data_dir, 1)?;
    }
    let now = now_ms();
    let tx = conn.transaction()?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS installed(id TEXT PRIMARY KEY, path TEXT NOT NULL, sha256 TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS operations(id TEXT PRIMARY KEY, model_id TEXT NOT NULL, kind TEXT NOT NULL, state TEXT NOT NULL, error TEXT, progress_bytes INTEGER NOT NULL DEFAULT 0, total_bytes INTEGER NOT NULL DEFAULT 0);",
    )?;
    for (column, ddl) in [
        (
            "progress_bytes",
            "ALTER TABLE operations ADD COLUMN progress_bytes INTEGER NOT NULL DEFAULT 0",
        ),
        (
            "total_bytes",
            "ALTER TABLE operations ADD COLUMN total_bytes INTEGER NOT NULL DEFAULT 0",
        ),
    ] {
        if !column_exists(&tx, "operations", column)? {
            tx.execute(ddl, [])?;
        }
    }
    if !column_exists(&tx, "installed", "quant")? {
        tx.execute("ALTER TABLE installed ADD COLUMN quant TEXT", [])?;
    }
    if !column_exists(&tx, "installed", "filename")? {
        tx.execute("ALTER TABLE installed ADD COLUMN filename TEXT", [])?;
    }
    if !column_exists(&tx, "installed", "size_bytes")? {
        tx.execute("ALTER TABLE installed ADD COLUMN size_bytes INTEGER", [])?;
    }
    for column in ["created_at", "updated_at", "finished_at"] {
        if !column_exists(&tx, "operations", column)? {
            tx.execute(
                &format!("ALTER TABLE operations ADD COLUMN {column} INTEGER"),
                [],
            )?;
        }
    }
    {
        let mut statement = tx.prepare("SELECT id, sha256 FROM installed WHERE quant IS NULL")?;
        let rows: Vec<(String, String)> = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<_, _>>()?;
        drop(statement);
        for (id, sha256) in rows {
            let matched = catalog
                .iter()
                .find(|model| model.slug == id)
                .and_then(|model| {
                    model
                        .files
                        .iter()
                        .find(|file| file.sha256.eq_ignore_ascii_case(&sha256))
                });
            if let Some(file) = matched {
                tx.execute(
                    "UPDATE installed SET quant=?2, filename=?3, size_bytes=?4 WHERE id=?1",
                    params![id, file.quant, file.filename, file.size_bytes as i64],
                )?;
            }
            // No catalog match: leave the row for startup reconciliation to quarantine.
        }
    }
    tx.execute(
        "UPDATE operations SET created_at = COALESCE(created_at, ?1), updated_at = COALESCE(updated_at, ?1)",
        params![now],
    )?;
    tx.execute(
        "UPDATE operations SET finished_at = ?1 WHERE finished_at IS NULL AND state IN ('completed','failed','cancelled')",
        params![now],
    )?;
    tx.commit()?;
    set_user_version(conn, CURRENT_SCHEMA_VERSION)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::open_app_at;
    use uuid::Uuid;

    fn temp_dir() -> PathBuf {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        parent.join(format!("stt-server-next-test-{}", Uuid::new_v4()))
    }

    fn cleanup(path: PathBuf) {
        let resolved = path.canonicalize().unwrap();
        fs::remove_dir_all(resolved).unwrap();
    }

    fn make_catalog() -> Vec<CatalogModel> {
        serde_json::from_str::<crate::catalog::Catalog>(include_str!(
            "../catalog/handy-2026-08-17.json"
        ))
        .unwrap()
        .models
    }

    #[test]
    fn cancelled_operation_cannot_promote_a_model() {
        let path = temp_dir();
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
        let file = CatalogFile {
            filename: "model.gguf".to_owned(),
            quant: "Q8_0".to_owned(),
            size_bytes: 14,
            sha256: "fakehash".to_owned(),
        };
        assert!(
            promote_verified_model(&app, &op, "parakeet-unified-en-0.6b", &file, &stage, 14,)
                .is_err()
        );
        assert!(stage.exists());
        assert!(installed_path(&app, "parakeet-unified-en-0.6b")
            .unwrap()
            .is_none());
        drop(app);
        cleanup(path);
    }

    /// Simulates a v1 DB created with the original ad-hoc CREATE statements
    /// (no user_version tracking, no quant/filename/size_bytes/timestamps),
    /// including one installed row, then opens it through the new binary.
    #[test]
    fn migration_from_v1_backfills_and_backs_up() {
        let path = temp_dir();
        fs::create_dir_all(&path).unwrap();
        fs::create_dir_all(path.join("models")).unwrap();
        fs::create_dir_all(path.join("staging")).unwrap();
        let db_path = path.join("state.db");
        let catalog = make_catalog();
        let parakeet = catalog
            .iter()
            .find(|m| m.slug == "parakeet-unified-en-0.6b")
            .unwrap();
        let default_file = parakeet
            .files
            .iter()
            .find(|f| f.quant == parakeet.default_quant)
            .unwrap();
        let model_path = path.join("models").join("existing.gguf");
        fs::write(&model_path, b"pretend model bytes").unwrap();
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE settings(key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 CREATE TABLE installed(id TEXT PRIMARY KEY, path TEXT NOT NULL, sha256 TEXT NOT NULL);
                 CREATE TABLE operations(id TEXT PRIMARY KEY, model_id TEXT NOT NULL, kind TEXT NOT NULL, state TEXT NOT NULL, error TEXT, progress_bytes INTEGER NOT NULL DEFAULT 0, total_bytes INTEGER NOT NULL DEFAULT 0);",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO installed(id,path,sha256) VALUES(?1,?2,?3)",
                params![
                    "parakeet-unified-en-0.6b",
                    model_path.to_string_lossy().as_ref(),
                    default_file.sha256
                ],
            )
            .unwrap();
            let op = Uuid::new_v4().to_string();
            conn.execute(
                "INSERT INTO operations(id,model_id,kind,state,error,progress_bytes,total_bytes) VALUES(?1,'parakeet-unified-en-0.6b','install','completed',NULL,10,10)",
                params![op],
            )
            .unwrap();
            assert_eq!(user_version(&conn).unwrap(), 0);
        }

        let mut conn = Connection::open(&db_path).unwrap();
        migrate(&mut conn, &catalog, &path).unwrap();
        assert_eq!(user_version(&conn).unwrap(), CURRENT_SCHEMA_VERSION);
        assert!(path.join("state.db.bak-v1").exists());

        let installed = query_installed_file(&conn, "parakeet-unified-en-0.6b")
            .unwrap()
            .unwrap();
        assert_eq!(
            installed.quant.as_deref(),
            Some(default_file.quant.as_str())
        );
        assert_eq!(installed.size_bytes, Some(default_file.size_bytes));
        assert_eq!(
            installed.filename.as_deref(),
            Some(default_file.filename.as_str())
        );

        let (created, updated): (Option<i64>, Option<i64>) = conn
            .query_row(
                "SELECT created_at, updated_at FROM operations LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(created.is_some());
        assert!(updated.is_some());

        drop(conn);
        cleanup(path);
    }

    #[test]
    fn migration_leaves_unmatched_installed_row_for_reconciliation() {
        let path = temp_dir();
        fs::create_dir_all(&path).unwrap();
        let db_path = path.join("state.db");
        let catalog = make_catalog();
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE settings(key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 CREATE TABLE installed(id TEXT PRIMARY KEY, path TEXT NOT NULL, sha256 TEXT NOT NULL);
                 CREATE TABLE operations(id TEXT PRIMARY KEY, model_id TEXT NOT NULL, kind TEXT NOT NULL, state TEXT NOT NULL, error TEXT, progress_bytes INTEGER NOT NULL DEFAULT 0, total_bytes INTEGER NOT NULL DEFAULT 0);",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO installed(id,path,sha256) VALUES('parakeet-unified-en-0.6b','/tmp/nope.gguf','not-a-real-hash')",
                [],
            )
            .unwrap();
        }
        let mut conn = Connection::open(&db_path).unwrap();
        migrate(&mut conn, &catalog, &path).unwrap();
        let installed = query_installed_file(&conn, "parakeet-unified-en-0.6b")
            .unwrap()
            .unwrap();
        assert!(installed.quant.is_none());
        drop(conn);
        cleanup(path);
    }

    #[test]
    fn fresh_db_lands_directly_on_v2_without_backup() {
        let path = temp_dir();
        let app = open_app_at(path.clone()).unwrap();
        let version = {
            let db = app.db.lock().unwrap();
            user_version(&db).unwrap()
        };
        assert_eq!(version, CURRENT_SCHEMA_VERSION);
        assert!(!path.join("state.db.bak-v1").exists());
        drop(app);
        cleanup(path);
    }
}
