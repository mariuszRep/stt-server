//! Journalled updater. The recovery worker is a protected copy of this same
//! executable, not a separately shipped program. No model files are modified.
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{cli::RunFlags, discovery, selfupdate, verify::sha256_file};

type Result<T> = std::result::Result<T, String>;
fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Launch {
    pub executable: PathBuf,
    pub service: bool,
    pub flags: RunFlags,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Prepared,
    Stopping,
    Snapshot,
    Replacing,
    Validating,
    RollingBack,
    Committed,
    Restored,
}
impl Phase {
    pub fn terminal(self) -> bool {
        matches!(self, Self::Committed | Self::Restored)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Journal {
    pub id: String,
    pub phase: Phase,
    pub data_dir: PathBuf,
    pub work_dir: PathBuf,
    pub launch: Launch,
    pub was_running: bool,
    pub ready_model: Option<String>,
    pub old_version: String,
    pub new_version: String,
    pub api_level: u32,
    pub network_mode: String,
    pub old_sha256: String,
    pub new_sha256: String,
    pub database_existed: bool,
    pub database_sha256: Option<String>,
    pub ready_timeout_seconds: u64,
    pub task_name: String,
    pub task_removed: bool,
    /// Only true after recovery registration has succeeded. Before this point
    /// the installed binary and database are untouched and startup stays usable.
    #[serde(default)]
    pub armed: bool,
    pub error: Option<String>,
}
impl Journal {
    pub fn path(&self) -> PathBuf {
        journal_path(&self.data_dir)
    }
    pub fn backup(&self) -> PathBuf {
        self.work_dir.join("previous.exe")
    }
    pub fn candidate(&self) -> PathBuf {
        self.work_dir.join("candidate.exe")
    }
    pub fn worker(&self) -> PathBuf {
        self.work_dir.join("recovery.exe")
    }
    pub fn snapshot(&self) -> PathBuf {
        self.work_dir.join("state.snapshot")
    }
    pub fn save(&self) -> Result<()> {
        let tmp = self.work_dir.join("journal.tmp");
        let mut file = File::create(&tmp).map_err(err)?;
        file.write_all(&serde_json::to_vec_pretty(self).map_err(err)?)
            .map_err(err)?;
        file.sync_all().map_err(err)?;
        drop(file);
        // work_dir and data_dir may be on different volumes: publish through
        // a protected, same-directory file and rename it atomically.
        let publish = self.data_dir.join(format!("update-{}.tmp", self.id));
        if !publish.exists() {
            File::create(&publish).map_err(err)?;
        }
        protect(&publish, self.launch.service)?;
        copy_sync(&tmp, &publish)?;
        atomic_replace(&publish, &self.path())
    }
    fn advance(&mut self, phase: Phase) -> Result<()> {
        self.phase = phase;
        self.save()
    }
}

pub fn journal_path(data_dir: &Path) -> PathBuf {
    data_dir.join("update-journal.json")
}
pub fn read_journal(data_dir: &Path) -> Result<Option<Journal>> {
    match fs::read(journal_path(data_dir)) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(err),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(err(e)),
    }
}

pub fn lock(data_dir: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(data_dir.join("update.lock"))
        .map_err(err)?;
    file.try_lock()
        .map_err(|_| "An update or startup is already in progress".to_owned())?;
    Ok(file)
}

/// Called before opening SQLite. Normal startup participates in the update
/// lock; only the explicitly launched validation/recovery server may bypass it.
pub fn startup_guard(data_dir: &Path) -> Result<Option<File>> {
    fs::create_dir_all(data_dir).map_err(err)?;
    if let Some(journal) = read_journal(data_dir)? {
        if !journal.phase.terminal() && journal.armed {
            let worker_launch =
                std::env::var("STT_NEXT_UPDATE_TRANSACTION").ok().as_deref() == Some(&journal.id);
            let service_launch = journal.launch.service && crate::app::is_service_mode();
            if matches!(journal.phase, Phase::Validating | Phase::RollingBack)
                && (worker_launch || service_launch)
                && fs::canonicalize(std::env::current_exe().map_err(err)?).map_err(err)?
                    == fs::canonicalize(&journal.launch.executable).map_err(err)?
            {
                return Ok(None);
            }
            return Err(
                "Update recovery is pending; normal startup is blocked until recovery completes"
                    .into(),
            );
        }
    }
    let guard = lock(data_dir)?;
    // Recheck after acquisition, closing the prepare/start race.
    if read_journal(data_dir)?.is_some_and(|j| !j.phase.terminal() && j.armed) {
        return Err("Update recovery is pending".into());
    }
    Ok(Some(guard))
}

pub fn maintenance(data_dir: &Path) -> bool {
    match read_journal(data_dir) {
        Ok(Some(j)) => !j.phase.terminal() && j.armed,
        Ok(None) => false,
        Err(_) => true, // a damaged journal must not allow writes before recovery
    }
}

pub fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let tmp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&tmp)
        .map_err(err)?;
    file.write_all(&serde_json::to_vec_pretty(value).map_err(err)?)
        .map_err(err)?;
    file.sync_all().map_err(err)?;
    drop(file);
    atomic_replace(&tmp, path)
}

#[cfg(windows)]
fn atomic_replace(from: &Path, to: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    // Both buffers are NUL-terminated and remain live for this call.
    if unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        Err(err(std::io::Error::last_os_error()))
    } else {
        Ok(())
    }
}
#[cfg(not(windows))]
fn atomic_replace(from: &Path, to: &Path) -> Result<()> {
    fs::rename(from, to).map_err(err)
}

fn copy_sync(from: &Path, to: &Path) -> Result<()> {
    fs::copy(from, to).map_err(err)?;
    File::options()
        .write(true)
        .open(to)
        .map_err(err)?
        .sync_all()
        .map_err(err)
}
fn checked_hash(path: &Path, expected: &str) -> Result<()> {
    if sha256_file(path).map_err(err)? != expected {
        return Err(format!(
            "Backup/staged file hash mismatch: {}",
            path.display()
        ));
    }
    Ok(())
}

/// Microsoft command-line quoting (not shell quoting): including trailing
/// backslashes and embedded quotes. Used in registry and Task Scheduler XML.
pub fn quote_windows_arg(value: &str) -> String {
    let mut out = String::from("\"");
    let mut slashes = 0;
    for ch in value.chars() {
        if ch == '\\' {
            slashes += 1;
            continue;
        }
        out.extend(std::iter::repeat_n(
            '\\',
            if ch == '"' { slashes * 2 + 1 } else { slashes },
        ));
        slashes = 0;
        out.push(ch);
    }
    out.extend(std::iter::repeat_n('\\', slashes * 2));
    out.push('"');
    out
}
fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

pub trait TaskRunner {
    fn register(&self, journal: &Journal) -> Result<()>;
    fn remove(&self, journal: &Journal) -> Result<()>;
}
pub struct WindowsTasks;

fn hidden_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    command.stdin(Stdio::null());
    command
}
fn bounded_output(command: &mut Command) -> Result<std::process::Output> {
    // These utilities produce small output. Use files rather than pipes so a
    // stalled child cannot fill a pipe before the timeout is enforced.
    let log = std::env::temp_dir().join(format!("stt-command-{}.log", uuid::Uuid::new_v4()));
    let file = File::create(&log).map_err(err)?;
    command.stdout(file.try_clone().map_err(err)?).stderr(file);
    let mut child = command.spawn().map_err(err)?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let result = loop {
        if let Some(status) = child.try_wait().map_err(err)? {
            break Ok(std::process::Output {
                status,
                stdout: fs::read(&log).unwrap_or_default(),
                stderr: Vec::new(),
            });
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break Err("Windows command timed out".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let _ = fs::remove_file(log);
    result
}
fn run_command(command: &mut Command) -> Result<()> {
    let output = bounded_output(command)?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

#[cfg(windows)]
fn owner_sid() -> Result<String> {
    let output = bounded_output(hidden_command("whoami.exe").args(["/user", "/fo", "csv", "/nh"]))?;
    String::from_utf8_lossy(&output.stdout)
        .split(['"', ',', '\r', '\n'])
        .find(|s| s.starts_with("S-1-"))
        .map(str::to_owned)
        .ok_or_else(|| "Could not identify recovery-task owner".into())
}
#[cfg(windows)]
fn protect(path: &Path, machine: bool) -> Result<()> {
    let owner = if machine {
        "*S-1-5-18".into()
    } else {
        format!("*{}", owner_sid()?)
    };
    let grant = if path.is_dir() { "(OI)(CI)F" } else { "F" };
    run_command(hidden_command("icacls.exe").arg(path).args([
        "/inheritance:r",
        "/grant:r",
        &format!("{owner}:{grant}"),
        &format!("*S-1-5-32-544:{grant}"),
    ]))
}
#[cfg(not(windows))]
fn protect(path: &Path, _machine: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(
        path,
        fs::Permissions::from_mode(if path.is_dir() { 0o700 } else { 0o600 }),
    )
    .map_err(err)
}

pub fn task_xml(journal: &Journal, sid: &str) -> String {
    let (principal, trigger, level, logon) = if journal.launch.service {
        (
            "S-1-5-18".to_owned(),
            "<BootTrigger><Enabled>true</Enabled></BootTrigger>".to_owned(),
            "HighestAvailable",
            "ServiceAccount",
        )
    } else {
        (
            xml(sid),
            format!(
                "<LogonTrigger><Enabled>true</Enabled><UserId>{}</UserId></LogonTrigger>",
                xml(sid)
            ),
            "LeastPrivilege",
            "InteractiveToken",
        )
    };
    let arguments = format!(
        "__update-worker {} --recover",
        quote_windows_arg(&journal.data_dir.display().to_string())
    );
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task"><Triggers>{trigger}</Triggers><Principals><Principal id="Owner"><UserId>{principal}</UserId><LogonType>{logon}</LogonType><RunLevel>{level}</RunLevel></Principal></Principals><Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>false</StopIfGoingOnBatteries><StartWhenAvailable>true</StartWhenAvailable><ExecutionTimeLimit>PT0S</ExecutionTimeLimit></Settings><Actions Context="Owner"><Exec><Command>{}</Command><Arguments>{}</Arguments></Exec></Actions></Task>"#,
        xml(&journal.worker().display().to_string()),
        xml(&arguments)
    )
}
impl TaskRunner for WindowsTasks {
    fn register(&self, journal: &Journal) -> Result<()> {
        #[cfg(windows)]
        {
            let task_file = journal.work_dir.join("recovery-task.xml");
            fs::write(&task_file, task_xml(journal, &owner_sid()?)).map_err(err)?;
            run_command(
                hidden_command("schtasks.exe")
                    .args(["/Create", "/TN", &journal.task_name, "/XML"])
                    .arg(task_file),
            )
        }
        #[cfg(not(windows))]
        {
            let _ = journal;
            Err("Automatic update recovery requires Windows".into())
        }
    }
    fn remove(&self, journal: &Journal) -> Result<()> {
        #[cfg(windows)]
        {
            // Query before deletion makes repeated cleanup idempotent. If the
            // scheduler is unavailable, do not confuse that with task absence.
            let output = bounded_output(
                hidden_command("schtasks.exe").args(["/Query", "/FO", "CSV", "/NH"]),
            )?;
            if !output.status.success() {
                return Err("Could not query recovery task state".into());
            }
            if !String::from_utf8_lossy(&output.stdout).contains(&journal.task_name) {
                return Ok(());
            }
            run_command(hidden_command("schtasks.exe").args([
                "/Delete",
                "/TN",
                &journal.task_name,
                "/F",
            ]))
        }
        #[cfg(not(windows))]
        {
            let _ = journal;
            Err("Automatic update recovery requires Windows".into())
        }
    }
}

fn probe_url(info: &discovery::ServerInfo, path: &str) -> String {
    let host = match info.host.as_str() {
        "0.0.0.0" | "::" => "127.0.0.1",
        other => other,
    };
    if host.contains(':') {
        format!("http://[{host}]:{}{path}", info.port)
    } else {
        format!("http://{host}:{}{path}", info.port)
    }
}
pub async fn validation(data_dir: &Path) -> Result<Value> {
    let info = discovery::read_server_json(data_dir).ok_or("Server discovery unavailable")?;
    let token = fs::read_to_string(data_dir.join("auth.token")).map_err(err)?;
    reqwest::Client::new()
        .get(probe_url(&info, "/v1/local/update/validation"))
        .bearer_auth(token.trim())
        .timeout(Duration::from_secs(3))
        .send()
        .await
        .map_err(err)?
        .error_for_status()
        .map_err(err)?
        .json()
        .await
        .map_err(err)
}

/// All ordinary service stop/start operations go through SCM, preserving its
/// account, configuration and recovery policy rather than launching `run`.
fn service_command(action: &str) -> Result<()> {
    run_command(hidden_command("sc.exe").args([action, "OpenVibeSttNext"]))
}

pub async fn stop(journal: &Journal) -> Result<()> {
    if let Ok(guard) = discovery::acquire_lock(&journal.data_dir) {
        discovery::remove_server_json(&journal.data_dir);
        drop(guard);
        return Ok(());
    }
    if journal.launch.service {
        service_command("stop")?;
    } else {
        let info = discovery::read_server_json(&journal.data_dir)
            .ok_or("Cannot safely identify server to stop")?;
        let token = fs::read_to_string(journal.data_dir.join("auth.token")).map_err(err)?;
        reqwest::Client::new()
            .post(probe_url(&info, "/v1/local/shutdown"))
            .bearer_auth(token.trim())
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .map_err(err)?
            .error_for_status()
            .map_err(err)?;
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(guard) = discovery::acquire_lock(&journal.data_dir) {
            discovery::remove_server_json(&journal.data_dir);
            drop(guard);
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("Shutdown not confirmed; files and database were not restored".into());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn start(journal: &Journal) -> Result<()> {
    if journal.launch.service {
        return service_command("start");
    }
    let mut command = hidden_command(&journal.launch.executable);
    command
        .arg("run")
        .args(journal.launch.flags.arguments(&journal.data_dir))
        .env("STT_NEXT_UPDATE_TRANSACTION", &journal.id);
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(journal.work_dir.join("server.log"))
        .map_err(err)?;
    command.stdout(log.try_clone().map_err(err)?).stderr(log);
    // The server outlives this handle; it is stopped by the authenticated API.
    let _child = command.spawn().map_err(err)?;
    Ok(())
}

pub fn validate_response(journal: &Journal, body: &Value, rollback: bool) -> Result<bool> {
    let expected = if rollback {
        &journal.old_version
    } else {
        &journal.new_version
    };
    if body["version"].as_str() != Some(expected) {
        return Err("Started executable reports the wrong version".into());
    }
    if body["api_level"].as_u64().unwrap_or(0) < journal.api_level as u64 {
        return Err("Candidate API level is incompatible".into());
    }
    if body["network_mode"].as_str() != Some(&journal.network_mode) {
        return Err("Network policy changed during update".into());
    }
    if body["transaction"].as_str() != Some(&journal.id) {
        return Err("Validation answered by a different server instance".into());
    }
    if let Some(model) = &journal.ready_model {
        if body["ready_model"].as_str() != Some(model) {
            if body["loading"].as_bool() == Some(true) {
                return Ok(false);
            }
            return Err("Previously ready model failed to load".into());
        }
    }
    Ok(true)
}
async fn wait_ready(journal: &Journal, rollback: bool) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(journal.ready_timeout_seconds);
    loop {
        if let Ok(body) = validation(&journal.data_dir).await {
            if validate_response(journal, &body, rollback)? {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            return Err("Timed out validating the updated server".into());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn snapshot_database(journal: &mut Journal) -> Result<()> {
    let path = journal.data_dir.join("state.db");
    journal.database_existed = path.exists();
    if journal.database_existed {
        let conn = rusqlite::Connection::open(&path).map_err(err)?;
        let busy: i64 = conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))
            .map_err(err)?;
        if busy != 0 {
            return Err("Database checkpoint is busy".into());
        }
        drop(conn);
        copy_sync(&path, &journal.snapshot())?;
        journal.database_sha256 = Some(sha256_file(&journal.snapshot()).map_err(err)?);
    }
    journal.advance(Phase::Snapshot)
}

fn restore_database(journal: &Journal) -> Result<()> {
    if let Some(hash) = &journal.database_sha256 {
        checked_hash(&journal.snapshot(), hash)?;
        // SQLite sidecars belong to the failed candidate, never the snapshot.
        for name in ["state.db-wal", "state.db-shm"] {
            let path = journal.data_dir.join(name);
            if path.exists() {
                fs::remove_file(path).map_err(err)?;
            }
        }
        let staged = journal.data_dir.join("state.db.restore");
        copy_sync(&journal.snapshot(), &staged)?;
        atomic_replace(&staged, &journal.data_dir.join("state.db"))?;
    } else if !journal.database_existed {
        // Preserve any failed first-start database as evidence, not a purge.
        for name in ["state.db", "state.db-wal", "state.db-shm"] {
            let source = journal.data_dir.join(name);
            if source.exists() {
                atomic_replace(&source, &journal.work_dir.join(format!("failed-{name}")))?;
            }
        }
    }
    Ok(())
}

fn replace_binary(journal: &Journal) -> Result<()> {
    checked_hash(&journal.candidate(), &journal.new_sha256)?;
    checked_hash(&journal.backup(), &journal.old_sha256)?;
    // Windows allows renaming an executing CLI image but not overwriting it.
    // The independent recovery copy and journal cover the gap between renames.
    if journal.launch.executable.exists() {
        atomic_replace(
            &journal.launch.executable,
            &journal.work_dir.join("retired.exe"),
        )?;
    }
    atomic_replace(&journal.candidate(), &journal.launch.executable)
}
fn restore_binary(journal: &Journal) -> Result<()> {
    checked_hash(&journal.backup(), &journal.old_sha256)?;
    let staged = journal.work_dir.join("restore.exe");
    copy_sync(&journal.backup(), &staged)?;
    atomic_replace(&staged, &journal.launch.executable)
}

async fn apply(journal: &mut Journal) -> Result<()> {
    journal.advance(Phase::Stopping)?;
    stop(journal).await?;
    {
        let _server_lock = discovery::acquire_lock(&journal.data_dir).map_err(err)?;
        snapshot_database(journal)?;
        journal.advance(Phase::Replacing)?;
        replace_binary(journal)?;
        journal.advance(Phase::Validating)?;
    }
    start(journal)?;
    wait_ready(journal, false).await?;
    if !journal.was_running {
        stop(journal).await?;
    }
    journal.advance(Phase::Committed)
}

async fn rollback(journal: &mut Journal) -> Result<()> {
    // Persist intent before touching anything, making a second interruption safe.
    journal.advance(Phase::RollingBack)?;
    stop(journal).await?;
    {
        let _server_lock = discovery::acquire_lock(&journal.data_dir).map_err(err)?;
        restore_binary(journal)?;
        restore_database(journal)?;
    }
    if journal.was_running {
        start(journal)?;
        wait_ready(journal, true).await?;
    }
    journal.advance(Phase::Restored)
}

fn cleanup(journal: &mut Journal, tasks: &impl TaskRunner) -> Result<()> {
    if !journal.task_removed {
        tasks.remove(journal)?;
        journal.task_removed = true;
        journal.save()?;
    }
    Ok(())
}

/// A run worker applies only a freshly prepared transaction. A recovery worker
/// always rolls back any uncommitted transaction, never guesses it succeeded.
pub async fn worker(data_dir: &Path, recover: bool, tasks: &impl TaskRunner) -> Result<()> {
    let _lock = lock(data_dir)?;
    let mut journal = read_journal(data_dir)?.ok_or("No update journal")?;
    validate_journal(&journal, data_dir)?;
    if journal.phase.terminal() {
        return cleanup(&mut journal, tasks);
    }
    if !journal.armed {
        journal.advance(Phase::Restored)?;
        return cleanup(&mut journal, tasks);
    }
    let result = if !recover && journal.phase == Phase::Prepared {
        apply(&mut journal).await
    } else {
        Err("Interrupted update; restoring previous version".into())
    };
    if let Err(cause) = result {
        journal.error = Some(cause.clone());
        journal.save()?;
        if let Err(recovery) = rollback(&mut journal).await {
            journal.error = Some(format!("{cause}; recovery failed: {recovery}"));
            journal.save()?;
            return Err(journal.error.clone().unwrap());
        }
        cleanup(&mut journal, tasks)?;
        return Err(format!("{cause}; previous version restored"));
    }
    cleanup(&mut journal, tasks)
}

fn validate_journal(journal: &Journal, data_dir: &Path) -> Result<()> {
    if fs::canonicalize(&journal.data_dir).map_err(err)?
        != fs::canonicalize(data_dir).map_err(err)?
    {
        return Err("Journal data directory mismatch".into());
    }
    let parent = journal
        .launch
        .executable
        .parent()
        .ok_or("Executable has no directory")?;
    if journal.work_dir != parent.join(format!(".stt-update-{}", journal.id))
        || uuid::Uuid::parse_str(&journal.id).is_err()
    {
        return Err("Invalid update artifact paths".into());
    }
    checked_hash(&journal.worker(), &journal.old_sha256)
}

/// Preparation is side-effect-free for a running server until the recovery
/// artifacts and task exist. Caller holds update.lock until worker handoff.
pub async fn prepare(
    data_dir: &Path,
    release: &selfupdate::ReleaseInfo,
    timeout: u64,
    tasks: &impl TaskRunner,
) -> Result<Journal> {
    if let Some(mut old) = read_journal(data_dir)? {
        if !old.phase.terminal() && old.armed {
            return Err("Previous update requires recovery before another update".into());
        }
        cleanup(&mut old, tasks)?;
    }
    let executable = fs::canonicalize(std::env::current_exe().map_err(err)?).map_err(err)?;
    let data_dir = fs::canonicalize(data_dir).map_err(err)?;
    let stopped_guard = discovery::acquire_lock(&data_dir).ok();
    let was_running = stopped_guard.is_none();
    let launch: Launch = if was_running {
        discovery::read_server_json(&data_dir).and_then(|i| i.launch)
            .ok_or("Running server lacks launch metadata; restart it with this version before updating")?
    } else {
        match fs::read(data_dir.join("last-launch.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(err)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Launch {
                executable: executable.clone(),
                service: false,
                flags: RunFlags::default(),
            },
            Err(e) => return Err(err(e)),
        }
    };
    if fs::canonicalize(&launch.executable).map_err(err)? != executable {
        return Err("Executable does not own this data directory".into());
    }
    let baseline = if was_running {
        Some(validation(&data_dir).await?)
    } else {
        None
    };
    if baseline
        .as_ref()
        .is_some_and(|b| b["active_operations"].as_u64().unwrap_or(1) != 0)
    {
        return Err("Finish or cancel model operations before updating".into());
    }
    let id = uuid::Uuid::new_v4().to_string();
    let work_dir = executable
        .parent()
        .unwrap()
        .join(format!(".stt-update-{id}"));
    fs::create_dir(&work_dir).map_err(err)?;
    protect(&work_dir, launch.service)?;
    let size = fs::metadata(&executable).map_err(err)?.len();
    let required = size
        .saturating_mul(4)
        .saturating_add(release.exe.size.unwrap_or(size))
        .saturating_add(64 * 1024 * 1024);
    if fs4::available_space(&work_dir).map_err(err)? < required {
        return Err("Insufficient space beside executable for safe update".into());
    }
    let db_size = fs::metadata(data_dir.join("state.db"))
        .map(|m| m.len())
        .unwrap_or(0);
    if fs4::available_space(&data_dir).map_err(err)?
        < db_size.saturating_mul(2).saturating_add(16 * 1024 * 1024)
        || fs4::available_space(&work_dir).map_err(err)? < required.saturating_add(db_size)
    {
        return Err("Insufficient space for database rollback snapshot".into());
    }
    let staged = selfupdate::download_and_verify(&reqwest::Client::new(), release, &work_dir)
        .await
        .map_err(err)?;
    atomic_replace(&staged, &work_dir.join("candidate.exe"))?;
    copy_sync(&executable, &work_dir.join("previous.exe"))?;
    copy_sync(&executable, &work_dir.join("recovery.exe"))?;
    let network_mode = if let Some(body) = &baseline {
        body["network_mode"]
            .as_str()
            .ok_or("Missing network policy")?
            .to_owned()
    } else {
        saved_network_mode(&data_dir, &launch.flags)?
    };
    let journal = Journal {
        id: id.clone(),
        phase: Phase::Prepared,
        data_dir: data_dir.clone(),
        work_dir,
        launch,
        was_running,
        ready_model: baseline
            .as_ref()
            .and_then(|b| b["ready_model"].as_str())
            .map(str::to_owned),
        old_version: env!("CARGO_PKG_VERSION").into(),
        new_version: release.version.clone(),
        api_level: crate::api::API_LEVEL,
        network_mode,
        old_sha256: sha256_file(&executable).map_err(err)?,
        new_sha256: String::new(),
        database_existed: data_dir.join("state.db").exists(),
        database_sha256: None,
        ready_timeout_seconds: timeout,
        task_name: format!("OpenVibeSTT-Recovery-{id}"),
        task_removed: false,
        armed: false,
        error: None,
    };
    let mut journal = journal;
    journal.new_sha256 = sha256_file(&journal.candidate()).map_err(err)?;
    journal.save()?;
    protect(&journal.path(), journal.launch.service)?;
    if let Err(error) = tasks.register(&journal) {
        journal.error = Some(format!("Recovery task registration failed: {error}"));
        journal.advance(Phase::Restored)?;
        // Registration may have succeeded before reporting an error; keep the
        // terminal journal so its next invocation performs cleanup only.
        return Err(journal.error.unwrap());
    }
    journal.armed = true;
    journal.save()?;
    drop(stopped_guard);
    Ok(journal)
}

fn saved_network_mode(data_dir: &Path, flags: &RunFlags) -> Result<String> {
    use rusqlite::OptionalExtension;
    let path = data_dir.join("state.db");
    let setting = |key: &str| -> Result<Option<String>> {
        if !path.exists() {
            return Ok(None);
        }
        let conn = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(err)?;
        conn.query_row("SELECT value FROM settings WHERE key=?1", [key], |r| {
            r.get(0)
        })
        .optional()
        .map_err(err)
    };
    if flags.host.is_some() || setting("bind_host")?.is_some() {
        return Ok("custom".into());
    }
    Ok(flags
        .network
        .map(|n| n.as_str().to_owned())
        .or(setting("network_mode")?)
        .unwrap_or_else(|| "local".into()))
}

pub async fn install(data_dir: PathBuf, yes: bool, json_output: bool, timeout: u64) -> Result<()> {
    let check = selfupdate::check_latest(
        &reqwest::Client::new(),
        &selfupdate::default_update_endpoint(),
    )
    .await
    .map_err(err)?;
    if !check.update_available || !yes {
        let reason = if check.update_available {
            "confirmation_required"
        } else {
            "already_up_to_date"
        };
        println!(
            "{}",
            json!({"installed":false,"reason":reason,"current_version":check.current_version,"latest_version":check.release.version})
        );
        return Ok(());
    }
    fs::create_dir_all(&data_dir).map_err(err)?;
    let guard = lock(&data_dir)?;
    let journal = prepare(&data_dir, &check.release, timeout, &WindowsTasks).await?;
    let log = File::create(journal.work_dir.join("worker.log")).map_err(err)?;
    let mut command = hidden_command(journal.worker());
    command
        .arg("__update-worker")
        .arg(&journal.data_dir)
        .stdout(log.try_clone().map_err(err)?)
        .stderr(log);
    let mut child = command.spawn().map_err(err)?;
    drop(guard);
    // The worker owns recovery even if this CLI is closed. It waits for our
    // lock to be released before changing anything.
    loop {
        if let Some(status) = child.try_wait().map_err(err)? {
            let result = read_journal(&journal.data_dir)?.ok_or("Missing update result")?;
            if json_output {
                println!(
                    "{}",
                    json!({"installed": result.phase == Phase::Committed,"phase":result.phase,"error":result.error})
                );
            }
            if !status.success() {
                return Err(result.error.unwrap_or_else(|| {
                    format!("Update worker failed; see {}", journal.work_dir.display())
                }));
            }
            if !json_output {
                println!("Update completed: {}", result.new_version);
            }
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

pub async fn worker_entry(data_dir: &Path, recover: bool) -> Result<()> {
    // Parent still briefly owns the lock when spawning us.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match lock(data_dir) {
            Ok(guard) => {
                drop(guard);
                break;
            }
            Err(_) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await
            }
            Err(e) => return Err(e),
        }
    }
    worker(data_dir, recover, &WindowsTasks).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::NetworkMode;

    fn temp_dir(label: &str) -> PathBuf {
        std::env::temp_dir().canonicalize().unwrap().join(format!(
            "stt-update-transaction-test-{label}-{}",
            uuid::Uuid::new_v4()
        ))
    }

    fn sample_journal(work_dir: PathBuf, data_dir: PathBuf) -> Journal {
        let id = uuid::Uuid::new_v4().to_string();
        Journal {
            id: id.clone(),
            phase: Phase::Prepared,
            data_dir,
            work_dir,
            launch: Launch {
                executable: PathBuf::from(r"C:\bin\stt-server-next.exe"),
                service: false,
                flags: RunFlags::default(),
            },
            was_running: false,
            ready_model: None,
            old_version: "1.0.0".into(),
            new_version: "1.1.0".into(),
            api_level: 1,
            network_mode: "local".into(),
            old_sha256: String::new(),
            new_sha256: String::new(),
            database_existed: false,
            database_sha256: None,
            ready_timeout_seconds: 5,
            task_name: format!("Task-{id}"),
            task_removed: false,
            armed: true,
            error: None,
        }
    }

    // --- validate_response: the decision function behind "expected-version
    // mismatch -> rollback" and "readiness failure -> rollback" (exercised
    // end-to-end with a real spawned server in tests/update_transaction.rs). ---

    #[test]
    fn validate_response_rejects_wrong_version() {
        let journal = sample_journal(PathBuf::from("w"), PathBuf::from("d"));
        let body = json!({
            "version": "9.9.9",
            "api_level": 1,
            "network_mode": "local",
            "transaction": journal.id,
        });
        let error = validate_response(&journal, &body, false).unwrap_err();
        assert!(error.contains("wrong version"));
    }

    #[test]
    fn validate_response_accepts_expected_version_with_no_ready_model_requirement() {
        let journal = sample_journal(PathBuf::from("w"), PathBuf::from("d"));
        let body = json!({
            "version": journal.new_version,
            "api_level": journal.api_level,
            "network_mode": journal.network_mode,
            "transaction": journal.id,
        });
        assert!(validate_response(&journal, &body, false).unwrap());
    }

    #[test]
    fn validate_response_keeps_waiting_while_ready_model_is_still_loading() {
        let mut journal = sample_journal(PathBuf::from("w"), PathBuf::from("d"));
        journal.ready_model = Some("whisper-base".into());
        let body = json!({
            "version": journal.new_version,
            "api_level": journal.api_level,
            "network_mode": journal.network_mode,
            "transaction": journal.id,
            "loading": true,
        });
        assert!(!validate_response(&journal, &body, false).unwrap());
    }

    #[test]
    fn validate_response_fails_when_the_ready_model_never_comes_back() {
        let mut journal = sample_journal(PathBuf::from("w"), PathBuf::from("d"));
        journal.ready_model = Some("whisper-base".into());
        let body = json!({
            "version": journal.new_version,
            "api_level": journal.api_level,
            "network_mode": journal.network_mode,
            "transaction": journal.id,
            "loading": false,
        });
        let error = validate_response(&journal, &body, false).unwrap_err();
        assert!(error.contains("failed to load"));
    }

    #[test]
    fn validate_response_rejects_a_widened_network_policy() {
        let journal = sample_journal(PathBuf::from("w"), PathBuf::from("d"));
        let body = json!({
            "version": journal.new_version,
            "api_level": journal.api_level,
            "network_mode": "lan",
            "transaction": journal.id,
        });
        let error = validate_response(&journal, &body, false).unwrap_err();
        assert!(error.contains("Network policy"));
    }

    #[test]
    fn validate_response_rejects_a_response_from_a_different_transaction() {
        let journal = sample_journal(PathBuf::from("w"), PathBuf::from("d"));
        let body = json!({
            "version": journal.new_version,
            "api_level": journal.api_level,
            "network_mode": journal.network_mode,
            "transaction": "some-other-transaction",
        });
        let error = validate_response(&journal, &body, false).unwrap_err();
        assert!(error.contains("different server instance"));
    }

    // --- Journal persistence must survive a crash mid-update, including the
    // full launch settings (network/host/port/cors/data-dir) so recovery
    // relaunches with the exact same exposure. ---

    #[test]
    fn journal_round_trips_full_launch_settings_through_save_and_read() {
        let data_dir = temp_dir("journal-roundtrip");
        fs::create_dir_all(&data_dir).unwrap();
        let mut journal = sample_journal(data_dir.clone(), data_dir.clone());
        journal.launch.flags = RunFlags {
            port: Some(54499),
            host: Some("0.0.0.0".into()),
            network: Some(NetworkMode::Lan),
            data_dir: Some(data_dir.clone()),
            cors_origins: vec!["https://a.example".into(), "https://b.example".into()],
            ..Default::default()
        };
        journal.save().unwrap();

        let reloaded = read_journal(&data_dir).unwrap().unwrap();
        assert_eq!(reloaded.launch.flags, journal.launch.flags);
        let built = reloaded.launch.flags.arguments(&data_dir);
        assert!(built.windows(2).any(|w| w == ["--network", "lan"]));
        assert!(built.windows(2).any(|w| w == ["--host", "0.0.0.0"]));
        assert!(built.windows(2).any(|w| w == ["--port", "54499"]));
        assert!(built
            .windows(2)
            .filter(|w| w[0] == "--cors-origin")
            .map(|w| w[1].as_str())
            .eq(["https://a.example", "https://b.example"]));

        let _ = fs::remove_dir_all(&data_dir);
    }

    // --- snapshot_database / restore_database: the database backup that lets
    // an old executable (which refuses a newer schema) start again after a
    // rollback. ---

    #[test]
    fn database_snapshot_is_restored_verbatim_on_rollback() {
        let data_dir = temp_dir("db-snapshot");
        let work_dir = data_dir.join("work");
        fs::create_dir_all(&work_dir).unwrap();
        let db_path = data_dir.join("state.db");
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute("CREATE TABLE marker (v TEXT)", []).unwrap();
            conn.execute("INSERT INTO marker VALUES ('original')", [])
                .unwrap();
        }
        let mut journal = sample_journal(work_dir, data_dir.clone());
        snapshot_database(&mut journal).unwrap();
        assert!(journal.database_existed);
        assert!(journal.database_sha256.is_some());
        assert_eq!(journal.phase, Phase::Snapshot);

        // Simulate the new version migrating the database to something else.
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute("UPDATE marker SET v = 'migrated-by-new-version'", [])
                .unwrap();
        }

        restore_database(&journal).unwrap();
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let value: String = conn
            .query_row("SELECT v FROM marker", [], |r| r.get(0))
            .unwrap();
        assert_eq!(value, "original");

        let _ = fs::remove_dir_all(&journal.data_dir);
    }

    #[test]
    fn restore_database_preserves_a_failed_first_run_database_when_none_existed_before() {
        let data_dir = temp_dir("db-first-run");
        let work_dir = data_dir.join("work");
        fs::create_dir_all(&work_dir).unwrap();
        let journal = sample_journal(work_dir.clone(), data_dir.clone());
        assert!(!journal.database_existed);
        // The new version created a database on its first (aborted) start.
        fs::write(data_dir.join("state.db"), b"new-version-schema").unwrap();

        restore_database(&journal).unwrap();
        assert!(!data_dir.join("state.db").exists());
        assert_eq!(
            fs::read(work_dir.join("failed-state.db")).unwrap(),
            b"new-version-schema"
        );

        let _ = fs::remove_dir_all(&data_dir);
    }

    // --- replace_binary / restore_binary: the exe swap itself, hash-checked
    // on both ends so a corrupted stand-in file is never installed. ---

    #[test]
    fn replace_binary_then_restore_binary_round_trips_exe_contents() {
        let data_dir = temp_dir("exe-swap");
        let work_dir = data_dir.join("work");
        fs::create_dir_all(&work_dir).unwrap();
        let exe_path = data_dir.join("stt-server-next.exe");
        fs::write(&exe_path, b"old exe bytes").unwrap();

        let mut journal = sample_journal(work_dir, data_dir.clone());
        journal.launch.executable = exe_path.clone();
        fs::write(journal.candidate(), b"new exe bytes").unwrap();
        journal.old_sha256 = sha256_file(&exe_path).unwrap();
        journal.new_sha256 = sha256_file(&journal.candidate()).unwrap();
        fs::write(journal.backup(), b"old exe bytes").unwrap();

        replace_binary(&journal).unwrap();
        assert_eq!(fs::read(&exe_path).unwrap(), b"new exe bytes");

        restore_binary(&journal).unwrap();
        assert_eq!(fs::read(&exe_path).unwrap(), b"old exe bytes");

        let _ = fs::remove_dir_all(&journal.data_dir);
    }

    #[test]
    fn replace_binary_rejects_a_corrupted_candidate() {
        let data_dir = temp_dir("exe-corrupt");
        let work_dir = data_dir.join("work");
        fs::create_dir_all(&work_dir).unwrap();
        let exe_path = data_dir.join("stt-server-next.exe");
        fs::write(&exe_path, b"old exe bytes").unwrap();

        let mut journal = sample_journal(work_dir, data_dir.clone());
        journal.launch.executable = exe_path.clone();
        fs::write(journal.candidate(), b"corrupted").unwrap();
        journal.old_sha256 = sha256_file(&exe_path).unwrap();
        // Wrong on purpose: the download's checksum, not the corrupted bytes.
        journal.new_sha256 = "0".repeat(64);
        fs::write(journal.backup(), b"old exe bytes").unwrap();

        let error = replace_binary(&journal).unwrap_err();
        assert!(error.contains("hash mismatch"));
        // Untouched: a rejected candidate must never replace the running exe.
        assert_eq!(fs::read(&exe_path).unwrap(), b"old exe bytes");

        let _ = fs::remove_dir_all(&journal.data_dir);
    }
}
