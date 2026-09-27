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
    /// User-level token (`user.token`): satisfies `AccessLevel::User` routes
    /// only. See `crate::auth::AccessLevel` and the "Access levels on a
    /// shared server" goal.
    pub user_token: String,
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
    /// Effective bind host/port this process resolved at startup (CLI flag >
    /// stored `bind_host`/`bind_port` setting > default 127.0.0.1:54321).
    /// `run_http` binds to this; `/v1/local/shutdown` uses it to decide
    /// whether a caller is loopback.
    pub bind_host: String,
    pub bind_port: u16,
    /// Resolved `network_mode` (CLI `--network` > stored setting > default
    /// `local`). Meaningless when `network_custom` is true (an explicit
    /// `--host` override always wins -- see `crate::network`).
    pub network_mode: crate::network::NetworkMode,
    /// True when this process was given an explicit `--host` (or a stored
    /// `bind_host` from the older phase-1a setting) -- the "advanced
    /// override" that takes precedence over `network_mode` entirely. Health
    /// reports mode `"custom"` in this case.
    pub network_custom: bool,
    /// Live network reachability report shown on `/health`. Set once at
    /// startup for `local`/`custom` (no detection needed) and kept current
    /// by a periodic background recheck for `lan`/`tailscale` (see
    /// `api::run_http_full`).
    pub network_state: std::sync::RwLock<crate::network::NetworkReport>,
    /// Set by `api::run_http_full` while serving; `POST /v1/local/shutdown`
    /// takes it and fires it to trigger axum's graceful shutdown.
    pub shutdown: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

pub const DEFAULT_BIND_HOST: &str = "127.0.0.1";
pub const DEFAULT_BIND_PORT: u16 = 54321;

/// CLI-supplied bind overrides (from `--host`/`--port`), taking precedence
/// over the stored `bind_host`/`bind_port` settings, which in turn take
/// precedence over the hard defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BindOverrides {
    pub host: Option<String>,
    pub port: Option<u16>,
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

/// Which install this process belongs to. See [`install_scope`] for how it
/// is decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallScope {
    /// Default: no admin required. Data lives under the running user's
    /// `%LOCALAPPDATA%`.
    PerUser,
    /// Program under `%ProgramFiles%`, data under `%ProgramData%`, shared by
    /// every user of the machine. Only a machine-wide install offers the
    /// Windows Service.
    MachineWide,
}

fn program_files_dir() -> PathBuf {
    std::env::var_os("ProgramFiles")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"))
}

fn programdata_dir() -> PathBuf {
    std::env::var_os("PROGRAMDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
}

/// `%ProgramFiles%\OpenVibeAI\STT Server`: where a machine-wide install's
/// executable lives.
pub fn machine_wide_program_dir() -> PathBuf {
    program_files_dir().join("OpenVibeAI").join("STT Server")
}

/// `%ProgramData%\OpenVibeAI\STT Server`: a machine-wide install's single
/// data folder.
pub fn machine_wide_data_dir() -> PathBuf {
    programdata_dir().join("OpenVibeAI").join("STT Server")
}

/// The old (pre-unification) machine-wide data folder name, kept only so an
/// existing install can be migrated forward; see [`migrate_dir_once`].
fn machine_wide_old_data_dir() -> PathBuf {
    programdata_dir().join("OpenVibeAI").join("STT Server Next")
}

/// Moves a pre-unification machine-wide data folder to the current name
/// before `service install` creates anything, so the old data is carried
/// over instead of being shadowed by a fresh empty folder.
pub fn migrate_machine_wide_data() -> PathBuf {
    migrate_dir_once(&machine_wide_data_dir(), &machine_wide_old_data_dir())
}

/// The old (pre-unification) machine-wide program folder, removed by
/// `service install` once the new one is in place.
pub fn machine_wide_old_program_dir() -> PathBuf {
    program_files_dir()
        .join("OpenVibeAI")
        .join("STT Server Next")
}

/// `%LOCALAPPDATA%\OpenVibeAI\STT Server`: a per-user install's single data
/// folder. `None` only when `LOCALAPPDATA` itself is unset (not expected on
/// real Windows).
fn per_user_data_dir_opt() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(|base| PathBuf::from(base).join("OpenVibeAI").join("STT Server"))
}

/// Public accessor for `models import-user`'s default `--from`: the
/// invoking OS user's own per-user data folder, regardless of this
/// process's own [`install_scope`] (a machine-wide install's admin runs
/// `import-user` to pull *from* a per-user folder into its own machine-wide
/// one). `None` only when `LOCALAPPDATA` itself is unset.
pub fn per_user_data_dir() -> Option<PathBuf> {
    per_user_data_dir_opt()
}

/// The old (pre-unification) per-user data folder name -- it was missing the
/// `OpenVibeAI` publisher folder entirely. Kept only for migration.
fn per_user_old_data_dir_opt() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|base| PathBuf::from(base).join("STT Server Next"))
}

/// Decide the install scope for `exe_dir` (the directory containing the
/// running executable, or `None` if it could not be determined): machine-wide
/// if it is the machine-wide program folder, or a marker file
/// (`.machine-wide-install`) written there at install time is present
/// (covers a copied/renamed exe still belonging to that install); per-user
/// otherwise. Kept separate from [`install_scope`] so scope resolution is
/// testable without depending on `std::env::current_exe`.
pub fn install_scope_for(exe_dir: Option<&Path>) -> InstallScope {
    let Some(dir) = exe_dir else {
        return InstallScope::PerUser;
    };
    // Compared lexically (case-insensitively, as Windows paths are), not via
    // `fs::canonicalize`: this must work without touching disk or requiring
    // the machine-wide program folder to exist, e.g. in tests.
    let is_machine_wide_dir = dir
        .to_string_lossy()
        .eq_ignore_ascii_case(&machine_wide_program_dir().to_string_lossy());
    if is_machine_wide_dir || dir.join(".machine-wide-install").exists() {
        InstallScope::MachineWide
    } else {
        InstallScope::PerUser
    }
}

/// Decide this process's install scope from its own executable path.
///
/// Choice made here (simple and deterministic, per the goal): scope is
/// derived purely from *where this executable is*, never from how it was
/// launched -- so `run`/`start`/`stop`/`status`/`models`/`update`/`autostart`
/// and the Windows Service (which always runs the machine-wide copy) all
/// agree on one scope, and therefore one data folder, without needing to
/// pass scope around explicitly.
pub fn install_scope() -> InstallScope {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.to_path_buf()));
    install_scope_for(exe_dir.as_deref())
}

/// Move `old_dir` to `new_dir` if `new_dir` doesn't exist yet but `old_dir`
/// does (a pre-unification install under the old folder name). Never deletes
/// anything: if the rename fails (e.g. a file inside is open), the old
/// directory is left in place and used as-is rather than losing data or
/// silently starting a second, empty data folder.
fn migrate_dir_once(new_dir: &Path, old_dir: &Path) -> PathBuf {
    if new_dir.exists() || !old_dir.exists() {
        return new_dir.to_path_buf();
    }
    if let Some(parent) = new_dir.parent() {
        if fs::create_dir_all(parent).is_err() {
            return old_dir.to_path_buf();
        }
    }
    match fs::rename(old_dir, new_dir) {
        Ok(()) => {
            eprintln!(
                "migrated data folder {} -> {}",
                old_dir.display(),
                new_dir.display()
            );
            new_dir.to_path_buf()
        }
        Err(error) => {
            eprintln!(
                "could not migrate data folder {} -> {} ({error}); continuing to use {}",
                old_dir.display(),
                new_dir.display(),
                old_dir.display()
            );
            old_dir.to_path_buf()
        }
    }
}

/// This install's single data folder: every mode (run/start/stop/status/
/// models/update CLI, autostart, service) resolves to the same path for a
/// given scope. `STT_NEXT_DATA_DIR` always overrides, taking precedence over
/// scope resolution entirely.
pub fn data_dir() -> PathBuf {
    if let Some(over) = std::env::var_os("STT_NEXT_DATA_DIR") {
        return PathBuf::from(over);
    }
    match install_scope() {
        InstallScope::MachineWide => {
            migrate_dir_once(&machine_wide_data_dir(), &machine_wide_old_data_dir())
        }
        InstallScope::PerUser => match per_user_data_dir_opt() {
            Some(new_dir) => match per_user_old_data_dir_opt() {
                Some(old_dir) => migrate_dir_once(&new_dir, &old_dir),
                None => new_dir,
            },
            None => PathBuf::from(".stt-server-next"),
        },
    }
}

/// Default drop-in `user_models_dir` when the setting has never been written
/// (user decision 2026-09-25): inside this install's single data folder, at
/// `<data dir>\models` -- the same folder the managed store uses, so a user
/// can drop files in next to ones already downloaded/imported. Per-user this
/// is `%LOCALAPPDATA%\OpenVibeAI\STT Server\models`; machine-wide this is
/// `%ProgramData%\OpenVibeAI\STT Server\models`, shared by every user of the
/// machine rather than tied to whichever user happened to install the
/// service. `STT_NEXT_USER_MODELS_DIR_DEFAULT` overrides this for tests,
/// mirroring `STT_NEXT_DATA_DIR`'s role for the data directory.
pub fn default_user_models_dir() -> Option<PathBuf> {
    if let Some(over) = std::env::var_os("STT_NEXT_USER_MODELS_DIR_DEFAULT") {
        return Some(PathBuf::from(over));
    }
    Some(data_dir().join("models"))
}

/// Restrict `auth.token` to the current user account, whatever the data
/// folder's inherited ACLs are (M6: a custom `--data-dir`, e.g. on a
/// non-system drive, may otherwise inherit a broader ACL than
/// `%LOCALAPPDATA%` normally has, and a token reachable by another local
/// account matters because a token-holder has LAN reach). Applied on every
/// open, not just creation, so an existing install self-heals. Skipped in
/// service mode: `service::install` already applies the SYSTEM/Administrators/
/// installing-user ACL appropriate for a LocalSystem-run service, and running
/// this as `SYSTEM` (the service's own account) would instead strip that
/// down to `SYSTEM`-only and lock the installing user out.
#[cfg(windows)]
fn restrict_token_file_acl(path: &Path) -> Result<(), Box<dyn Error>> {
    let owner = match (std::env::var("USERDOMAIN"), std::env::var("USERNAME")) {
        (Ok(domain), Ok(user)) => format!("{domain}\\{user}"),
        _ => std::env::var("USERNAME")?,
    };
    let status = std::process::Command::new("icacls")
        .arg(path)
        .args(["/inheritance:r", "/grant:r", &format!("{owner}:R")])
        .status()?;
    if !status.success() {
        return Err(format!("Could not protect ACL on {}", path.display()).into());
    }
    Ok(())
}

#[cfg(not(windows))]
fn restrict_token_file_acl(_path: &Path) -> Result<(), Box<dyn Error>> {
    Ok(())
}

/// Admin token: `<data dir>\auth.token`. See [`token_file_named`].
pub fn token_file(dir: &Path) -> Result<String, Box<dyn Error>> {
    token_file_named(dir, "auth.token")
}

/// User token: `<data dir>\user.token`. Same generation/persistence approach
/// as the admin token, created alongside it so every install (per-user or
/// machine-wide) has both -- a per-user install has no separate use for it
/// today (the owner already has full access via `auth.token`), but keeping
/// generation uniform avoids a special case, and it becomes relevant if that
/// install is later shared.
pub fn user_token_file(dir: &Path) -> Result<String, Box<dyn Error>> {
    token_file_named(dir, "user.token")
}

fn token_file_named(dir: &Path, file_name: &str) -> Result<String, Box<dyn Error>> {
    let path = dir.join(file_name);
    let result = match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut file) => {
            let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
            file.write_all(token.as_bytes())?;
            Ok(token)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let mut token = String::new();
            OpenOptions::new()
                .read(true)
                .open(&path)?
                .read_to_string(&mut token)?;
            if token.len() != 64 {
                return Err("Invalid token file".into());
            }
            Ok(token)
        }
        Err(error) => Err(error.into()),
    };
    if result.is_ok() && !is_service_mode() {
        restrict_token_file_acl(&path)?;
    }
    result
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
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            db.execute("DELETE FROM installed WHERE id=?1", params![id])?;
            db.execute(
                "DELETE FROM settings WHERE key='selected_model' AND value=?1",
                params![id],
            )?;
        }
        Err(error) => {
            eprintln!("model {id} temporarily unavailable: {error}; verify or refresh to retry");
            db.execute(
                "UPDATE installed SET needs_verification=1 WHERE id=?1",
                params![id],
            )?;
        }
        Ok(info) => {
            let mtime_ms = info
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_millis() as i64);
            let changed = mtime_ms.is_none()
                || Some(info.len()) != recorded_size
                || mtime_ms != recorded_mtime;
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
        // Startup only checks the fingerprint recorded after verification.
        // Missing/changed/inaccessible files stay registered, but cannot load.
        let unchanged = expected.is_some_and(|file| {
            file.sha256.eq_ignore_ascii_case(&recorded_hash)
                && recorded_size == Some(file.size_bytes as i64)
                && fs::canonicalize(&artifact).is_ok_and(|p| p.starts_with(&model_dir))
                && fs::metadata(&artifact).is_ok_and(|m| m.len() == file.size_bytes)
                && crate::verify::file_mtime_ms(&artifact)
                    .ok()
                    .is_some_and(|mtime| Some(mtime) == recorded_mtime)
        });
        if !unchanged {
            db.execute(
                "UPDATE installed SET needs_verification=1 WHERE id=?1",
                params![id],
            )?;
            eprintln!("model {id} needs verification; file and saved selection preserved");
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
    open_app_at_full(data_dir, cli_overrides, BindOverrides::default(), None)
}

/// Full opener: also resolves the effective bind host/port from
/// `bind_overrides` (CLI `--host`/`--port`) > stored settings > default, and
/// the effective `network_mode` from `network_override` (CLI `--network`) >
/// the stored `network_mode` setting > default `local`. Does not itself
/// perform any network detection (see `api::run_http_full`, which owns
/// binding and resolves `network_state` from live detection for `lan`/
/// `tailscale`); here `network_state` is only ever set to the no-detection-
/// needed `local`/`custom` reports so this opener stays fast and I/O-free
/// for the many callers (tests, CLI subcommands) that never serve traffic.
pub fn open_app_at_full(
    data_dir: PathBuf,
    cli_overrides: RuntimeLimits,
    bind_overrides: BindOverrides,
    network_override: Option<crate::network::NetworkMode>,
) -> Result<Arc<App>, Box<dyn Error>> {
    let catalog: Catalog = serde_json::from_str(include_str!("../catalog/handy-2026-08-17.json"))?;
    fs::create_dir_all(data_dir.join("models"))?;
    fs::create_dir_all(data_dir.join("staging"))?;
    let token = token_file(&data_dir)?;
    let user_token = user_token_file(&data_dir)?;
    let mut db = Connection::open(data_dir.join("state.db"))?;
    db.execute_batch("PRAGMA journal_mode=WAL;")?;
    crate::store::migrate(&mut db, &catalog.models, &data_dir)?;
    let now = crate::store::now_ms();
    db.execute("UPDATE operations SET state='failed', error='Interrupted by service restart', updated_at=?1, finished_at=?1 WHERE state IN ('queued','running')", params![now])?;
    reconcile_interrupted_imports(&db, &data_dir)?;
    reconcile_installed(&db, &catalog.models, &data_dir)?;
    let selected: Option<(String, String)> = db
        .query_row(
            "SELECT i.id,i.path FROM installed i JOIN settings s ON s.key='selected_model' AND s.value=i.id WHERE i.needs_verification=0",
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
    let (stored_host, stored_port) = crate::store::read_bind_settings(&db)?;
    let network_custom = bind_overrides.host.is_some() || stored_host.is_some();
    let bind_host = bind_overrides
        .host
        .or(stored_host)
        .unwrap_or_else(|| DEFAULT_BIND_HOST.to_owned());
    let bind_port = bind_overrides
        .port
        .or(stored_port)
        .unwrap_or(DEFAULT_BIND_PORT);
    let stored_network_mode = crate::store::read_network_mode_setting(&db)?;
    let network_mode = crate::network::resolve_mode(network_override, stored_network_mode);
    let network_state = if network_custom {
        crate::network::NetworkReport::custom(&bind_host)
    } else {
        crate::network::NetworkReport::local(network_mode)
    };
    Ok(Arc::new(App {
        catalog: catalog.models,
        db: Mutex::new(db),
        loaded: Mutex::new(loaded),
        data_dir,
        token,
        user_token,
        inference: InferenceQueue::new(),
        limits: std::sync::RwLock::new(limits),
        selection: tokio::sync::Mutex::new(()),
        // No blanket total-request timeout: a 48 GB model download must not
        // be killed just because it is still progressing. Staleness is
        // instead bounded per-chunk by download::STALL_TIMEOUT.
        http: reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .build()?,
        bind_host,
        bind_port,
        network_mode,
        network_custom,
        network_state: std::sync::RwLock::new(network_state),
        shutdown: Mutex::new(None),
    }))
}

#[cfg(test)]
mod install_scope_tests {
    use super::*;

    #[test]
    fn no_exe_dir_is_per_user() {
        assert_eq!(install_scope_for(None), InstallScope::PerUser);
    }

    #[test]
    fn arbitrary_dir_without_marker_is_per_user() {
        let dir = std::env::temp_dir().join(format!("stt-scope-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(install_scope_for(Some(&dir)), InstallScope::PerUser);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// The marker file is what lets a machine-wide install still be
    /// recognized even if its executable is not literally sitting in
    /// `machine_wide_program_dir()` (e.g. under a test's own temp dir).
    #[test]
    fn marker_file_makes_a_dir_machine_wide() {
        let dir = std::env::temp_dir().join(format!("stt-scope-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(".machine-wide-install"), b"").unwrap();
        assert_eq!(install_scope_for(Some(&dir)), InstallScope::MachineWide);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn machine_wide_program_dir_itself_is_machine_wide() {
        // No filesystem access needed: the machine-wide program folder is
        // matched lexically, not via `fs::canonicalize`, so this holds even
        // when that folder doesn't exist on the test machine (or the test
        // isn't running elevated).
        let dir = machine_wide_program_dir();
        assert_eq!(install_scope_for(Some(&dir)), InstallScope::MachineWide);
    }

    /// Every mode of one install must resolve to the same data folder.
    /// `data_dir()` itself reads the real `install_scope()`/env, so this
    /// exercises the same underlying folder choice each mode goes through,
    /// pinned to the two scopes directly rather than depending on process
    /// launch args.
    #[test]
    fn per_user_and_machine_wide_scopes_resolve_to_distinct_stable_folders() {
        let per_user = per_user_data_dir_opt().unwrap();
        let machine_wide = machine_wide_data_dir();
        assert_ne!(per_user, machine_wide);
        assert!(per_user.ends_with("STT Server"));
        assert!(machine_wide.ends_with("STT Server"));
        assert!(per_user.to_string_lossy().contains("OpenVibeAI"));
        assert!(machine_wide.to_string_lossy().contains("OpenVibeAI"));
        // Calling twice must be stable (no per-call randomness/side effects).
        assert_eq!(per_user, per_user_data_dir_opt().unwrap());
        assert_eq!(machine_wide, machine_wide_data_dir());
    }

    /// Not run through `data_dir()`/env overrides directly (both are process-
    /// global and this suite runs tests in parallel); instead pins the
    /// invariant `default_user_models_dir` relies on: the drop-in default is
    /// always `<this install's data folder>/models`, for either scope.
    #[test]
    fn default_user_models_dir_nests_under_each_scopes_data_dir() {
        assert!(machine_wide_data_dir()
            .join("models")
            .starts_with(machine_wide_data_dir()));
        assert!(per_user_data_dir_opt()
            .unwrap()
            .join("models")
            .starts_with(per_user_data_dir_opt().unwrap()));
    }

    #[test]
    fn migrate_dir_once_moves_old_into_new_without_deleting_contents() {
        let root = std::env::temp_dir().join(format!("stt-migrate-test-{}", Uuid::new_v4()));
        let old_dir = root.join("old").join("STT Server Next");
        let new_dir = root.join("new").join("STT Server");
        fs::create_dir_all(old_dir.join("models")).unwrap();
        fs::write(old_dir.join("models").join("a.gguf"), b"model bytes").unwrap();

        let resolved = migrate_dir_once(&new_dir, &old_dir);

        assert_eq!(resolved, new_dir);
        assert!(!old_dir.exists(), "old folder must be moved, not copied");
        assert!(new_dir.join("models").join("a.gguf").exists());
        assert_eq!(
            fs::read(new_dir.join("models").join("a.gguf")).unwrap(),
            b"model bytes"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn migrate_dir_once_leaves_new_dir_alone_when_it_already_exists() {
        let root = std::env::temp_dir().join(format!("stt-migrate-test-{}", Uuid::new_v4()));
        let old_dir = root.join("old");
        let new_dir = root.join("new");
        fs::create_dir_all(old_dir.join("models")).unwrap();
        fs::write(old_dir.join("models").join("old.gguf"), b"old").unwrap();
        fs::create_dir_all(new_dir.join("models")).unwrap();
        fs::write(new_dir.join("models").join("new.gguf"), b"new").unwrap();

        let resolved = migrate_dir_once(&new_dir, &old_dir);

        assert_eq!(resolved, new_dir);
        // Neither an existing new install's data nor an old one still on
        // disk is ever deleted; the old folder is simply left untouched.
        assert!(old_dir.join("models").join("old.gguf").exists());
        assert!(new_dir.join("models").join("new.gguf").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn migrate_dir_once_is_a_noop_when_neither_dir_exists() {
        let root = std::env::temp_dir().join(format!("stt-migrate-test-{}", Uuid::new_v4()));
        let old_dir = root.join("old");
        let new_dir = root.join("new");
        assert_eq!(migrate_dir_once(&new_dir, &old_dir), new_dir);
        assert!(!old_dir.exists());
        assert!(!new_dir.exists());
    }
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

#[cfg(test)]
mod recovery_tests {
    use super::*;

    #[test]
    fn managed_startup_checks_metadata_without_reading_contents() {
        let dir = std::env::temp_dir().join(format!("stt-recovery-test-{}", Uuid::new_v4()));
        let app = open_app_at(dir.clone()).unwrap();
        let mut model = app.catalog[0].clone();
        model.files[0].size_bytes = 4;
        let file = &model.files[0];
        let artifact = dir.join("models/test.gguf");
        // Deliberately not the catalog hash: an unchanged fingerprint means no
        // startup content read. Explicit verification remains responsible for hashes.
        fs::write(&artifact, b"test").unwrap();
        let mtime = crate::verify::file_mtime_ms(&artifact).unwrap();
        let db = app.db.lock().unwrap();
        db.execute("INSERT INTO installed(id,path,sha256,quant,size_bytes,source,mtime_ms) VALUES(?1,?2,?3,?4,4,'import',?5)",
            params![model.slug, artifact.to_string_lossy(), file.sha256, file.quant, mtime]).unwrap();
        reconcile_installed(&db, &[model.clone()], &dir).unwrap();
        let needs = || {
            db.query_row("SELECT needs_verification FROM installed", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap()
        };
        assert_eq!(needs(), 0);
        db.execute("UPDATE installed SET mtime_ms=NULL", [])
            .unwrap();
        reconcile_installed(&db, &[model.clone()], &dir).unwrap();
        assert_eq!(needs(), 1);
        assert!(artifact.exists());
        db.execute(
            "UPDATE installed SET needs_verification=0,mtime_ms=?1",
            params![mtime],
        )
        .unwrap();
        fs::write(&artifact, b"changed size").unwrap();
        reconcile_installed(&db, &[model], &dir).unwrap();
        assert_eq!(needs(), 1);
        assert!(artifact.exists());
        drop(db);
        drop(app);
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn startup_preserves_locked_model_and_saved_selection() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = std::env::temp_dir().join(format!("stt-recovery-test-{}", Uuid::new_v4()));
        let app = open_app_at(dir.clone()).unwrap();
        let artifact = dir.join("locked.gguf");
        fs::write(&artifact, b"test").unwrap();
        let lock = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&artifact)
            .unwrap();
        {
            let db = app.db.lock().unwrap();
            db.execute("INSERT INTO installed(id,path,sha256,source,size_bytes,mtime_ms) VALUES('locked',?1,'x','user_folder',4,0)", params![artifact.to_string_lossy()]).unwrap();
            db.execute(
                "INSERT INTO settings(key,value) VALUES('selected_model','locked')",
                [],
            )
            .unwrap();
        }
        drop(app);
        let reopened = open_app_at(dir.clone()).unwrap();
        assert!(reopened.loaded.lock().unwrap().is_none());
        let db = reopened.db.lock().unwrap();
        assert_eq!(
            db.query_row(
                "SELECT value FROM settings WHERE key='selected_model'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            "locked"
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM installed", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        drop(db);
        drop(reopened);
        drop(lock);
        assert!(artifact.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    /// M6: `auth.token` must be restricted to the current user regardless of
    /// the data folder's own (possibly inherited, possibly broad) ACLs --
    /// this is the scenario a custom `--data-dir` on a non-system drive can
    /// hit. `token_file` applies an explicit, non-inherited ACL on every
    /// open (not just creation), so this also covers an existing install
    /// that predates the fix.
    #[test]
    fn token_file_acl_is_restricted_to_current_user_only() {
        let dir = std::env::temp_dir().join(format!("stt-token-acl-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        token_file(&dir).unwrap();
        let path = dir.join("auth.token");
        let output = std::process::Command::new("icacls")
            .arg(&path)
            .output()
            .unwrap();
        let listing = String::from_utf8_lossy(&output.stdout).to_lowercase();
        assert!(
            !listing.contains("everyone") && !listing.contains("\\users:"),
            "expected no broad grant, got: {listing}"
        );
        let user = std::env::var("USERNAME").unwrap().to_lowercase();
        assert!(
            listing.contains(&user),
            "expected the current user to be granted access, got: {listing}"
        );
        // Re-opening (existing file) self-heals the same way, not just at
        // creation.
        token_file(&dir).unwrap();
        let output = std::process::Command::new("icacls")
            .arg(&path)
            .output()
            .unwrap();
        let listing = String::from_utf8_lossy(&output.stdout).to_lowercase();
        assert!(listing.contains(&user));
        fs::remove_dir_all(dir).unwrap();
    }

    /// `user_token_file` creates `user.token` alongside `auth.token`, using
    /// the same generation approach (64-char, stable across re-opens), and
    /// they must differ. See "Access levels on a shared server".
    #[test]
    fn user_token_file_creates_a_distinct_stable_token() {
        let dir = std::env::temp_dir().join(format!("stt-user-token-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let admin_token = token_file(&dir).unwrap();
        let user_token = user_token_file(&dir).unwrap();
        assert_eq!(user_token.len(), 64);
        assert_ne!(admin_token, user_token);
        assert!(dir.join("user.token").exists());
        // Re-opening returns the same token rather than regenerating it.
        assert_eq!(user_token_file(&dir).unwrap(), user_token);
        fs::remove_dir_all(dir).unwrap();
    }

    /// `App::open_app_at_full` (the common opener) must populate both
    /// tokens, since routes are gated on `app.user_token` as well as
    /// `app.token`.
    #[test]
    fn open_app_populates_both_admin_and_user_tokens() {
        let dir = std::env::temp_dir().join(format!("stt-app-tokens-test-{}", Uuid::new_v4()));
        let app = open_app_at(dir.clone()).unwrap();
        assert_eq!(app.token.len(), 64);
        assert_eq!(app.user_token.len(), 64);
        assert_ne!(app.token, app.user_token);
        drop(app);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn default_open_resolves_local_mode_and_is_not_custom() {
        let dir = std::env::temp_dir().join(format!("stt-app-network-test-{}", Uuid::new_v4()));
        let app = open_app_at(dir.clone()).unwrap();
        assert_eq!(app.network_mode, crate::network::NetworkMode::Local);
        assert!(!app.network_custom);
        assert_eq!(app.network_state.read().unwrap().mode, "local");
        drop(app);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn explicit_host_override_is_reported_as_custom_regardless_of_network_override() {
        let dir = std::env::temp_dir().join(format!("stt-app-network-test-{}", Uuid::new_v4()));
        let app = open_app_at_full(
            dir.clone(),
            RuntimeLimits::default(),
            BindOverrides {
                host: Some("0.0.0.0".to_owned()),
                port: None,
            },
            Some(crate::network::NetworkMode::Tailscale),
        )
        .unwrap();
        assert!(app.network_custom);
        assert_eq!(app.network_state.read().unwrap().mode, "custom");
        drop(app);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn network_override_resolves_when_no_host_override_is_given() {
        let dir = std::env::temp_dir().join(format!("stt-app-network-test-{}", Uuid::new_v4()));
        let app = open_app_at_full(
            dir.clone(),
            RuntimeLimits::default(),
            BindOverrides::default(),
            Some(crate::network::NetworkMode::Lan),
        )
        .unwrap();
        assert!(!app.network_custom);
        assert_eq!(app.network_mode, crate::network::NetworkMode::Lan);
        drop(app);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_previously_stored_bind_host_setting_is_also_treated_as_custom() {
        let dir = std::env::temp_dir().join(format!("stt-app-network-test-{}", Uuid::new_v4()));
        {
            let app = open_app_at(dir.clone()).unwrap();
            app.db
                .lock()
                .unwrap()
                .execute(
                    "INSERT INTO settings(key,value) VALUES(?1,?2)",
                    params![crate::store::SETTING_BIND_HOST, "0.0.0.0"],
                )
                .unwrap();
            drop(app);
        }
        let reopened = open_app_at(dir.clone()).unwrap();
        assert!(reopened.network_custom);
        assert_eq!(reopened.network_state.read().unwrap().mode, "custom");
        drop(reopened);
        fs::remove_dir_all(dir).unwrap();
    }
}
