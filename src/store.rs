use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

use crate::app::{App, RuntimeLimits};
use crate::catalog::{CatalogFile, CatalogModel};
use crate::errors::{internal, ApiResult};

pub const CURRENT_SCHEMA_VERSION: i64 = 4;

/// Source of an installed model row (v4). See "Integration rules" in the
/// drop-in models design: catalog/import rows backfill sensibly, new rows
/// created by `/v1/local/models/refresh` are always `user_folder`.
pub const SOURCE_CATALOG_DOWNLOAD: &str = "catalog_download";
pub const SOURCE_IMPORT: &str = "import";
pub const SOURCE_USER_FOLDER: &str = "user_folder";

/// Settings key for the user-writable drop-in models folder (v4).
pub const SETTING_USER_MODELS_DIR: &str = "user_models_dir";

/// Settings keys for the configurable bind address (phase 1a). Absent means
/// unset (the process falls back to its CLI flag, then the hard default).
pub const SETTING_BIND_HOST: &str = "bind_host";
pub const SETTING_BIND_PORT: &str = "bind_port";

/// Settings key for the "Network modes" setting (`local`/`lan`/`tailscale`).
/// See `crate::network`. Absent means unset (falls back to the CLI
/// `--network` flag, then the default `local`).
pub const SETTING_NETWORK_MODE: &str = "network_mode";

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone)]
pub struct InstalledFile {
    pub id: String,
    pub path: PathBuf,
    pub sha256: String,
    pub quant: Option<String>,
    pub filename: Option<String>,
    pub size_bytes: Option<u64>,
    /// `catalog_download`, `import`, or `user_folder` (v4).
    pub source: String,
    pub custom_name: Option<String>,
    pub custom_arch: Option<String>,
    pub custom_languages: Option<Vec<String>>,
    pub custom_claims: Option<Value>,
    pub mtime_ms: Option<i64>,
    pub needs_verification: bool,
}

impl InstalledFile {
    pub fn is_custom(&self) -> bool {
        self.custom_arch.is_some()
    }
}

pub fn installed_file(app: &App, id: &str) -> ApiResult<Option<InstalledFile>> {
    let db = app.db.lock().map_err(internal)?;
    query_installed_file(&db, id).map_err(internal)
}

pub fn all_installed(app: &App) -> ApiResult<Vec<InstalledFile>> {
    let db = app.db.lock().map_err(internal)?;
    let mut statement = db.prepare("SELECT id FROM installed").map_err(internal)?;
    let ids = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(internal)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(internal)?;
    drop(statement);
    ids.into_iter()
        .map(|id| {
            query_installed_file(&db, &id)
                .map_err(internal)
                .map(|row| row.expect("row just listed by id"))
        })
        .collect()
}

fn query_installed_file(db: &Connection, id: &str) -> rusqlite::Result<Option<InstalledFile>> {
    db.query_row(
        "SELECT path, sha256, quant, filename, size_bytes, source, custom_name, custom_arch, custom_languages, custom_claims, mtime_ms, needs_verification FROM installed WHERE id = ?1",
        params![id],
        |row| {
            let custom_languages: Option<String> = row.get(8)?;
            let custom_claims: Option<String> = row.get(9)?;
            Ok(InstalledFile {
                id: id.to_owned(),
                path: PathBuf::from(row.get::<_, String>(0)?),
                sha256: row.get(1)?,
                quant: row.get(2)?,
                filename: row.get(3)?,
                size_bytes: row.get::<_, Option<i64>>(4)?.map(|value| value as u64),
                source: row.get(5)?,
                custom_name: row.get(6)?,
                custom_arch: row.get(7)?,
                custom_languages: custom_languages
                    .and_then(|value| serde_json::from_str(&value).ok()),
                custom_claims: custom_claims.and_then(|value| serde_json::from_str(&value).ok()),
                mtime_ms: row.get(10)?,
                needs_verification: row.get::<_, i64>(11)? != 0,
            })
        },
    )
    .optional()
}

/// Read the persisted `user_models_dir` setting (v4), unvalidated.
pub fn user_models_dir_setting(app: &App) -> ApiResult<Option<String>> {
    let db = app.db.lock().map_err(internal)?;
    db.query_row(
        "SELECT value FROM settings WHERE key=?1",
        params![SETTING_USER_MODELS_DIR],
        |row| row.get(0),
    )
    .optional()
    .map_err(internal)
}

/// Store a JSON `result` blob on a finished operation (v4). Best-effort:
/// errors are surfaced to the caller but never block the operation's own
/// state transition.
pub fn set_operation_result(app: &App, operation_id: &str, result: &Value) -> ApiResult<()> {
    let db = app.db.lock().map_err(internal)?;
    let serialized = serde_json::to_string(result).map_err(internal)?;
    db.execute(
        "UPDATE operations SET result=?2 WHERE id=?1",
        params![operation_id, serialized],
    )
    .map_err(internal)?;
    Ok(())
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

/// Default CORS origin list when the setting has never been written.
pub fn default_cors_origins() -> Vec<String> {
    vec!["*".to_owned()]
}

/// Pure validation: an origin is either the wildcard `*` or an
/// `http(s)://host[:port]` origin with no path/query/fragment/credentials.
pub fn is_valid_cors_origin(origin: &str) -> bool {
    if origin == "*" {
        return true;
    }
    let Some(rest) = origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"))
    else {
        return false;
    };
    if rest.is_empty() || rest.contains('/') || rest.contains('@') || rest.contains(' ') {
        return false;
    }
    let host_part = rest.rsplit_once(':').map_or(rest, |(host, port)| {
        if port.is_empty() || !port.chars().all(|c| c.is_ascii_digit()) {
            return rest;
        }
        host
    });
    !host_part.is_empty()
}

pub fn cors_allowed_origins(app: &App) -> ApiResult<Vec<String>> {
    let db = app.db.lock().map_err(internal)?;
    let raw: Option<String> = db
        .query_row(
            "SELECT value FROM settings WHERE key='cors_allowed_origins'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(internal)?;
    match raw {
        Some(value) => serde_json::from_str(&value).map_err(internal),
        None => Ok(default_cors_origins()),
    }
}

/// Settings keys for the optional queue/inference limits (see
/// `app::RuntimeLimits`). Stored as decimal-string values; absent means
/// unset (unbounded/no-timeout).
pub const SETTING_QUEUE_MAX_WAITING: &str = "queue_max_waiting";
pub const SETTING_QUEUE_WAIT_TIMEOUT_MS: &str = "queue_wait_timeout_ms";
pub const SETTING_INFERENCE_TIMEOUT_MS: &str = "inference_timeout_ms";

fn read_setting_u64(db: &Connection, key: &str) -> rusqlite::Result<Option<u64>> {
    let raw: Option<String> = db
        .query_row(
            "SELECT value FROM settings WHERE key=?1",
            params![key],
            |row| row.get(0),
        )
        .optional()?;
    Ok(raw.and_then(|value| value.parse::<u64>().ok()))
}

/// Read the persisted queue/inference limits. Any field with no stored
/// setting (or an unparseable one) comes back `None` (unbounded/no-timeout).
pub fn read_runtime_limits(db: &Connection) -> rusqlite::Result<RuntimeLimits> {
    Ok(RuntimeLimits {
        queue_max_waiting: read_setting_u64(db, SETTING_QUEUE_MAX_WAITING)?
            .map(|value| value as usize),
        queue_wait_timeout_ms: read_setting_u64(db, SETTING_QUEUE_WAIT_TIMEOUT_MS)?,
        inference_timeout_ms: read_setting_u64(db, SETTING_INFERENCE_TIMEOUT_MS)?,
    })
}

/// Current in-memory limits (live view, reflecting any CLI override and any
/// `PATCH /v1/local/config` applied since process start).
pub fn runtime_limits(app: &App) -> ApiResult<RuntimeLimits> {
    app.limits.read().map(|limits| *limits).map_err(internal)
}

/// Pure validation for a settable positive-integer limit: `None` clears it,
/// `Some(0)` is invalid (limits are positive), anything else passes through.
pub fn validate_positive_limit(value: Option<i64>) -> Result<Option<u64>, &'static str> {
    match value {
        None => Ok(None),
        Some(v) if v > 0 => Ok(Some(v as u64)),
        Some(_) => Err("must be a positive integer or null"),
    }
}

/// Read the persisted `bind_host` / `bind_port` settings directly from an
/// open connection, unvalidated. Used at startup (before `App` exists) and
/// by `bind_settings` below (for the running `App`/config endpoint).
pub fn read_bind_settings(db: &Connection) -> rusqlite::Result<(Option<String>, Option<u16>)> {
    let host: Option<String> = db
        .query_row(
            "SELECT value FROM settings WHERE key=?1",
            params![SETTING_BIND_HOST],
            |row| row.get(0),
        )
        .optional()?;
    let port: Option<u16> = db
        .query_row(
            "SELECT value FROM settings WHERE key=?1",
            params![SETTING_BIND_PORT],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .and_then(|value| value.parse::<u16>().ok());
    Ok((host, port))
}

/// Read the persisted `bind_host` / `bind_port` settings, unvalidated.
pub fn bind_settings(app: &App) -> ApiResult<(Option<String>, Option<u16>)> {
    let db = app.db.lock().map_err(internal)?;
    read_bind_settings(&db).map_err(internal)
}

/// Pure validation: a bind host must parse as an IP address (v4 or v6); a
/// bind port must be a non-zero u16 (0..=65535, with 0 reserved by the OS
/// meaning "pick any port", which this server does not support).
pub fn validate_bind_host(host: &str) -> bool {
    host.parse::<std::net::IpAddr>().is_ok()
}

pub fn validate_bind_port(port: i64) -> bool {
    port > 0 && port <= u16::MAX as i64
}

/// Read the persisted `network_mode` setting directly from an open
/// connection, unvalidated (a stored value written by an older/incompatible
/// build that fails to parse is treated the same as absent). Used at startup
/// (before `App` exists).
pub fn read_network_mode_setting(
    db: &Connection,
) -> rusqlite::Result<Option<crate::network::NetworkMode>> {
    let raw: Option<String> = db
        .query_row(
            "SELECT value FROM settings WHERE key=?1",
            params![SETTING_NETWORK_MODE],
            |row| row.get(0),
        )
        .optional()?;
    Ok(raw.and_then(|value| crate::network::NetworkMode::parse(&value)))
}

/// Read the persisted `network_mode` setting, unvalidated.
pub fn network_mode_setting(app: &App) -> ApiResult<Option<crate::network::NetworkMode>> {
    let db = app.db.lock().map_err(internal)?;
    read_network_mode_setting(&db).map_err(internal)
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
    promote_verified_model_with_source(
        app,
        operation_id,
        model_id,
        file,
        stage,
        bytes,
        SOURCE_CATALOG_DOWNLOAD,
    )
}

/// Same as [`promote_verified_model`], but records an explicit `source`
/// (`catalog_download` or `import`).
pub fn promote_verified_model_with_source(
    app: &App,
    operation_id: &str,
    model_id: &str,
    file: &CatalogFile,
    stage: &Path,
    bytes: u64,
    source: &str,
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
            "INSERT INTO installed(id,path,sha256,quant,filename,size_bytes,source) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![
                model_id,
                destination.to_string_lossy().as_ref(),
                file.sha256,
                file.quant,
                file.filename,
                file.size_bytes as i64,
                source,
            ],
        )
        .map_err(|error| error.to_string())?;
    let mtime = crate::verify::file_mtime_ms(stage).map_err(|error| error.to_string())?;
    fs::rename(stage, &destination).map_err(|error| error.to_string())?;
    transaction
        .execute(
            "UPDATE installed SET mtime_ms=?2, needs_verification=0 WHERE id=?1",
            params![model_id, mtime],
        )
        .map_err(|error| error.to_string())?;
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

/// Machine-readable error codes surfaced on operation records. Kept as an
/// explicit list (rather than free-form strings) so API consumers can match
/// on them without parsing the human-readable message.
pub const ERROR_CODE_INSUFFICIENT_DISK_SPACE: &str = "insufficient_disk_space";
pub const ERROR_CODE_STALLED: &str = "stalled";
pub const ERROR_CODE_SOURCE_UNAVAILABLE: &str = "source_unavailable";
pub const ERROR_CODE_HASH_MISMATCH: &str = "hash_mismatch";
pub const ERROR_CODE_CANCELLED: &str = "cancelled";

/// Run all pending schema migrations in one transaction. v1 is today's
/// unversioned schema (settings, installed(id,path,sha256),
/// operations(...progress_bytes,total_bytes)). v2 adds installed
/// quant/filename/size_bytes (backfilled by sha256 match against the
/// catalog) and operation timestamps created_at/updated_at/finished_at.
/// v3 adds operations.error_code, a machine-readable companion to the
/// existing free-text error message (see store::ERROR_CODE_*).
/// v4 adds installed.source/custom_name/custom_arch/custom_languages/
/// custom_claims/mtime_ms/needs_verification (drop-in user-folder models,
/// see `crate::dropin`) and operations.progress_items/total_items/result.
/// A non-empty older DB is backed up to state.db.bak-v<old> first.
pub fn migrate(
    conn: &mut Connection,
    catalog: &[CatalogModel],
    data_dir: &Path,
) -> Result<(), Box<dyn Error>> {
    let version = user_version(conn)?;
    if version > CURRENT_SCHEMA_VERSION {
        return Err(format!(
            "This database was created by a newer version of stt-server-next (schema v{version}); \
             this executable only understands up to schema v{CURRENT_SCHEMA_VERSION}. \
             Update stt-server-next before opening this data directory, or point --data-dir at a \
             different (older-schema) directory."
        )
        .into());
    }
    if version == CURRENT_SCHEMA_VERSION {
        return Ok(());
    }
    let had_pre_existing_schema = version == 0 && table_exists(conn, "installed")?;
    if had_pre_existing_schema {
        conn.execute_batch("PRAGMA wal_checkpoint(FULL);")?;
        backup_db(data_dir, 1)?;
    } else if version > 0 && version < CURRENT_SCHEMA_VERSION {
        conn.execute_batch("PRAGMA wal_checkpoint(FULL);")?;
        backup_db(data_dir, version)?;
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
    if !column_exists(&tx, "operations", "error_code")? {
        tx.execute("ALTER TABLE operations ADD COLUMN error_code TEXT", [])?;
    }
    // v4: model sources (drop-in support) and durable operation results.
    if !column_exists(&tx, "installed", "source")? {
        tx.execute(
            "ALTER TABLE installed ADD COLUMN source TEXT NOT NULL DEFAULT 'catalog_download'",
            [],
        )?;
    }
    for column in [
        "custom_name",
        "custom_arch",
        "custom_languages",
        "custom_claims",
    ] {
        if !column_exists(&tx, "installed", column)? {
            tx.execute(
                &format!("ALTER TABLE installed ADD COLUMN {column} TEXT"),
                [],
            )?;
        }
    }
    if !column_exists(&tx, "installed", "mtime_ms")? {
        tx.execute("ALTER TABLE installed ADD COLUMN mtime_ms INTEGER", [])?;
    }
    if !column_exists(&tx, "installed", "needs_verification")? {
        tx.execute(
            "ALTER TABLE installed ADD COLUMN needs_verification INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    for (column, ddl) in [
        (
            "progress_items",
            "ALTER TABLE operations ADD COLUMN progress_items INTEGER NOT NULL DEFAULT 0",
        ),
        (
            "total_items",
            "ALTER TABLE operations ADD COLUMN total_items INTEGER NOT NULL DEFAULT 0",
        ),
        ("result", "ALTER TABLE operations ADD COLUMN result TEXT"),
    ] {
        if !column_exists(&tx, "operations", column)? {
            tx.execute(ddl, [])?;
        }
    }
    // Backfill existing rows sensibly: a row promoted by a completed `import`
    // operation is `import`; everything else defaults to `catalog_download`
    // via the column default above (drop-in `user_folder` rows are only ever
    // created going forward, by refresh).
    tx.execute(
        "UPDATE installed SET source='import' WHERE source='catalog_download' AND id IN (SELECT model_id FROM operations WHERE kind='import' AND state='completed')",
        [],
    )?;
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

    /// Simulates a v2 DB (as produced by the pre-hardening binary: no
    /// error_code column) and confirms the v2->v3 migration adds it, backs up
    /// state.db.bak-v2, and leaves existing rows' error_code NULL.
    #[test]
    fn migration_from_v2_adds_error_code_and_backs_up() {
        let path = temp_dir();
        fs::create_dir_all(&path).unwrap();
        let db_path = path.join("state.db");
        let catalog = make_catalog();
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE settings(key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 CREATE TABLE installed(id TEXT PRIMARY KEY, path TEXT NOT NULL, sha256 TEXT NOT NULL, quant TEXT, filename TEXT, size_bytes INTEGER);
                 CREATE TABLE operations(id TEXT PRIMARY KEY, model_id TEXT NOT NULL, kind TEXT NOT NULL, state TEXT NOT NULL, error TEXT, progress_bytes INTEGER NOT NULL DEFAULT 0, total_bytes INTEGER NOT NULL DEFAULT 0, created_at INTEGER, updated_at INTEGER, finished_at INTEGER);
                 PRAGMA user_version = 2;",
            )
            .unwrap();
            let op = Uuid::new_v4().to_string();
            conn.execute(
                "INSERT INTO operations(id,model_id,kind,state,error,progress_bytes,total_bytes,created_at,updated_at,finished_at) VALUES(?1,'parakeet-unified-en-0.6b','install','failed','boom',0,10,1,1,1)",
                params![op],
            )
            .unwrap();
        }
        let mut conn = Connection::open(&db_path).unwrap();
        migrate(&mut conn, &catalog, &path).unwrap();
        assert_eq!(user_version(&conn).unwrap(), CURRENT_SCHEMA_VERSION);
        assert!(path.join("state.db.bak-v2").exists());
        assert!(column_exists(&conn, "operations", "error_code").unwrap());
        let error_code: Option<String> = conn
            .query_row("SELECT error_code FROM operations LIMIT 1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(error_code.is_none());
        drop(conn);
        cleanup(path);
    }

    /// Simulates a v3 DB (as produced by the pre-drop-in binary: no
    /// source/custom_*/mtime_ms/needs_verification on `installed`, no
    /// progress_items/total_items/result on `operations`) and confirms the
    /// v3->v4 migration adds them, backs up state.db.bak-v3, backfills an
    /// `import`-sourced row from its completed operation, and defaults an
    /// ordinary row to `catalog_download`.
    #[test]
    fn migration_from_v3_adds_drop_in_columns_and_backfills_source() {
        let path = temp_dir();
        fs::create_dir_all(&path).unwrap();
        let db_path = path.join("state.db");
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE settings(key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 CREATE TABLE installed(id TEXT PRIMARY KEY, path TEXT NOT NULL, sha256 TEXT NOT NULL, quant TEXT, filename TEXT, size_bytes INTEGER);
                 CREATE TABLE operations(id TEXT PRIMARY KEY, model_id TEXT NOT NULL, kind TEXT NOT NULL, state TEXT NOT NULL, error TEXT, error_code TEXT, progress_bytes INTEGER NOT NULL DEFAULT 0, total_bytes INTEGER NOT NULL DEFAULT 0, created_at INTEGER, updated_at INTEGER, finished_at INTEGER);
                 PRAGMA user_version = 3;",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO installed(id,path,sha256,quant,filename,size_bytes) VALUES('imported-model','/tmp/imported.gguf','deadbeef',NULL,'imported.gguf',10)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO installed(id,path,sha256,quant,filename,size_bytes) VALUES('parakeet-unified-en-0.6b','/tmp/downloaded.gguf','feedface','Q8_0','model.gguf',20)",
                [],
            )
            .unwrap();
            let op = Uuid::new_v4().to_string();
            conn.execute(
                "INSERT INTO operations(id,model_id,kind,state,progress_bytes,total_bytes,created_at,updated_at,finished_at) VALUES(?1,'imported-model','import','completed',10,10,1,1,1)",
                params![op],
            )
            .unwrap();
        }
        let catalog = make_catalog();
        let mut conn = Connection::open(&db_path).unwrap();
        migrate(&mut conn, &catalog, &path).unwrap();
        assert_eq!(user_version(&conn).unwrap(), CURRENT_SCHEMA_VERSION);
        assert!(path.join("state.db.bak-v3").exists());
        for column in [
            "source",
            "custom_name",
            "custom_arch",
            "custom_languages",
            "custom_claims",
            "mtime_ms",
            "needs_verification",
        ] {
            assert!(
                column_exists(&conn, "installed", column).unwrap(),
                "missing {column}"
            );
        }
        for column in ["progress_items", "total_items", "result"] {
            assert!(
                column_exists(&conn, "operations", column).unwrap(),
                "missing {column}"
            );
        }
        let imported = query_installed_file(&conn, "imported-model")
            .unwrap()
            .unwrap();
        assert_eq!(imported.source, SOURCE_IMPORT);
        let downloaded = query_installed_file(&conn, "parakeet-unified-en-0.6b")
            .unwrap()
            .unwrap();
        assert_eq!(downloaded.source, SOURCE_CATALOG_DOWNLOAD);
        drop(conn);
        cleanup(path);
    }

    #[test]
    fn cors_origin_validation_accepts_wildcard_and_http_https_host_port() {
        assert!(is_valid_cors_origin("*"));
        assert!(is_valid_cors_origin("http://tauri.localhost"));
        assert!(is_valid_cors_origin("https://example.com:8443"));
        assert!(is_valid_cors_origin("http://127.0.0.1:1420"));
    }

    #[test]
    fn cors_origin_validation_rejects_paths_schemes_and_garbage() {
        assert!(!is_valid_cors_origin("ftp://example.com"));
        assert!(!is_valid_cors_origin("http://example.com/path"));
        assert!(!is_valid_cors_origin("example.com"));
        assert!(!is_valid_cors_origin(""));
        assert!(!is_valid_cors_origin("http://"));
        assert!(!is_valid_cors_origin("http://user@example.com"));
    }

    /// An older executable must refuse to open a database stamped with a
    /// newer schema version instead of silently treating it as compatible.
    #[test]
    fn migration_refuses_a_database_with_a_newer_schema_version() {
        let path = temp_dir();
        fs::create_dir_all(&path).unwrap();
        let db_path = path.join("state.db");
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(&format!(
                "PRAGMA user_version = {};",
                CURRENT_SCHEMA_VERSION + 1
            ))
            .unwrap();
        }
        let catalog = make_catalog();
        let mut conn = Connection::open(&db_path).unwrap();
        let error = migrate(&mut conn, &catalog, &path).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("newer version"), "{message}");
        assert!(!path.join("state.db.bak-v1").exists());
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
