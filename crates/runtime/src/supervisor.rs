//! Managed runtime process lifecycle: spawn, health-check, log capture,
//! and graceful stop.
//!
//! Crash detection here is deliberately lazy rather than a background
//! watcher task: `tokio::process::Child` requires exclusive (`&mut`) access
//! to observe its exit status, and `ManagedInstance` is the single owner of
//! that handle so callers (`RuntimeManager`) can also `stop()`/inspect it.
//! Rather than fight that with a channel-mediated background task, liveness
//! is re-checked via `try_wait()` whenever `status()` is called — which is
//! exactly when a caller (a `GET /v1/providers/{id}/status` request) cares.

use std::collections::VecDeque;
use std::net::TcpListener;
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, Command};
use tokio::time::Instant;
use tracing::warn;

use crate::error::RuntimeError;

const LOG_TAIL_CAPACITY: usize = 200;
const HEALTH_POLL_INTERVAL: Duration = Duration::from_millis(250);
/// Default health-check window for `spawn()`; overridable per-call for
/// tests that intentionally exercise the failure path.
pub const DEFAULT_HEALTH_POLL_TIMEOUT: Duration = Duration::from_secs(30);
const STOP_GRACE_PERIOD: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeStatus {
    Starting,
    Running,
    Degraded,
    Crashed,
    Stopping,
    Stopped,
}

/// What to launch and how to confirm it's healthy.
pub struct SpawnSpec {
    pub program: std::path::PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Working directory for the spawned process, e.g. a vendored runtime's
    /// source root so its relative imports resolve. `None` inherits the
    /// control plane's own cwd.
    pub cwd: Option<std::path::PathBuf>,
    /// Path polled on the allocated port to confirm readiness, e.g. `/health`.
    pub health_path: String,
    /// Auth token this instance was started with, carried along for
    /// descriptor reconstruction (see `RuntimeManager`).
    pub auth_token: String,
}

pub struct ManagedInstance {
    child: Child,
    pub pid: u32,
    pub port: u16,
    pub auth_token: String,
    pub started_at: Instant,
    status: RuntimeStatus,
    log_tail: Arc<StdMutex<VecDeque<String>>>,
}

impl ManagedInstance {
    /// Re-checks the child's liveness and returns the current status. A
    /// crash is only observed the next time this (or `stop`) is called.
    pub fn status(&mut self) -> RuntimeStatus {
        if matches!(
            self.status,
            RuntimeStatus::Running | RuntimeStatus::Degraded
        ) {
            match self.child.try_wait() {
                Ok(Some(exit)) => {
                    warn!(
                        pid = self.pid,
                        exit = %exit,
                        "managed runtime process exited unexpectedly"
                    );
                    self.status = RuntimeStatus::Crashed;
                }
                Ok(None) => {}
                Err(e) => {
                    warn!(pid = self.pid, error = %e, "failed to poll managed runtime process");
                    self.status = RuntimeStatus::Crashed;
                }
            }
        }
        self.status
    }

    pub fn logs(&self, tail: usize) -> Vec<String> {
        let buf = self.log_tail.lock().expect("log_tail mutex poisoned");
        let skip = buf.len().saturating_sub(tail);
        buf.iter().skip(skip).cloned().collect()
    }

    /// Gracefully stop the managed process: SIGTERM (Unix) + grace window,
    /// then force-kill. Windows has no SIGTERM equivalent reachable without
    /// extra platform bindings, so it force-kills immediately — a
    /// documented v1 gap, not a silent one.
    pub async fn stop(&mut self) -> Result<(), RuntimeError> {
        self.status = RuntimeStatus::Stopping;
        terminate(self.pid);

        let deadline = Instant::now() + STOP_GRACE_PERIOD;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() >= deadline => {
                    warn!(
                        pid = self.pid,
                        "managed runtime did not exit within grace period, force-killing"
                    );
                    let _ = self.child.kill().await;
                    let _ = self.child.wait().await;
                    break;
                }
                Ok(None) => tokio::time::sleep(Duration::from_millis(100)).await,
                Err(e) => return Err(RuntimeError::Io(e)),
            }
        }
        self.status = RuntimeStatus::Stopped;
        Ok(())
    }
}

#[cfg(unix)]
fn terminate(pid: u32) {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;
    let _ = kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
}

#[cfg(not(unix))]
fn terminate(_pid: u32) {
    // No-op: the stop() grace-period loop falls through to a hard kill.
}

/// Bind an ephemeral loopback port, then release it so the child can bind
/// it immediately after. There's an unavoidable small race between release
/// and the child's own bind; acceptable for a local-only, single-user
/// control plane (retry-on-conflict is the caller's fallback, not handled
/// here).
pub fn allocate_loopback_port() -> Result<u16, RuntimeError> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(RuntimeError::Io)?;
    let port = listener.local_addr().map_err(RuntimeError::Io)?.port();
    drop(listener);
    Ok(port)
}

/// Spawn a managed runtime process and block until it reports healthy (or
/// the health-check window elapses, in which case it's killed and an error
/// is returned carrying the last captured log lines).
pub async fn spawn(spec: SpawnSpec, port: u16) -> Result<ManagedInstance, RuntimeError> {
    spawn_with_timeout(spec, port, DEFAULT_HEALTH_POLL_TIMEOUT).await
}

/// Same as [`spawn`], with an explicit health-check timeout — used directly
/// by tests that need the failure path to resolve quickly.
pub async fn spawn_with_timeout(
    spec: SpawnSpec,
    port: u16,
    health_timeout: Duration,
) -> Result<ManagedInstance, RuntimeError> {
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .envs(spec.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(cwd) = &spec.cwd {
        command.current_dir(cwd);
    }

    let mut child = command.spawn().map_err(RuntimeError::Io)?;
    let pid = child
        .id()
        .ok_or_else(|| RuntimeError::RuntimeStartFailed("spawned process has no pid".into()))?;

    let log_tail = Arc::new(StdMutex::new(VecDeque::with_capacity(LOG_TAIL_CAPACITY)));

    if let Some(stdout) = child.stdout.take() {
        spawn_log_reader(stdout, log_tail.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        spawn_log_reader(stderr, log_tail.clone());
    }

    let health_url = format!("http://127.0.0.1:{port}{}", spec.health_path);
    if let Err(e) = wait_for_health(&health_url, health_timeout).await {
        let tail = log_tail
            .lock()
            .expect("log_tail mutex poisoned")
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        let _ = child.kill().await;
        let _ = child.wait().await;
        return Err(RuntimeError::RuntimeStartFailed(format!(
            "{e}; last logs:\n{tail}"
        )));
    }

    Ok(ManagedInstance {
        child,
        pid,
        port,
        auth_token: spec.auth_token,
        started_at: Instant::now(),
        status: RuntimeStatus::Running,
        log_tail,
    })
}

fn spawn_log_reader<R>(reader: R, log_tail: Arc<StdMutex<VecDeque<String>>>)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let mut buf = log_tail.lock().expect("log_tail mutex poisoned");
            if buf.len() == LOG_TAIL_CAPACITY {
                buf.pop_front();
            }
            buf.push_back(line);
        }
    });
}

async fn wait_for_health(url: &str, timeout: Duration) -> Result<(), RuntimeError> {
    let client = reqwest::Client::new();
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(resp) = client.get(url).timeout(Duration::from_secs(2)).send().await {
            if resp.status().is_success() {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            return Err(RuntimeError::RuntimeStartFailed(format!(
                "runtime did not become healthy within {timeout:?} (polled {url})"
            )));
        }
        tokio::time::sleep(HEALTH_POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stands in for a managed runtime process without depending on the
    /// real (Python) one: `python3 -m http.server` answers 200 on `/` for
    /// any GET, which is enough to exercise spawn/health-poll/logs/stop.
    fn fake_runtime_spec(port: u16) -> SpawnSpec {
        SpawnSpec {
            program: "python3".into(),
            args: vec![
                "-m".into(),
                "http.server".into(),
                port.to_string(),
                "--bind".into(),
                "127.0.0.1".into(),
            ],
            env: vec![],
            cwd: None,
            health_path: "/".into(),
            auth_token: "test-token".into(),
        }
    }

    #[tokio::test]
    async fn spawn_reaches_running_and_stop_terminates_it() {
        let port = allocate_loopback_port().unwrap();
        let mut instance = spawn(fake_runtime_spec(port), port).await.unwrap();

        assert_eq!(instance.status(), RuntimeStatus::Running);
        assert_eq!(instance.port, port);
        assert_eq!(instance.auth_token, "test-token");

        instance.stop().await.unwrap();
        assert_eq!(instance.status(), RuntimeStatus::Stopped);
    }

    #[tokio::test]
    async fn status_detects_external_process_exit_as_crashed() {
        let port = allocate_loopback_port().unwrap();
        let mut instance = spawn(fake_runtime_spec(port), port).await.unwrap();

        // Simulate a crash: kill the process out from under the supervisor
        // rather than going through stop().
        instance.child.kill().await.unwrap();
        instance.child.wait().await.unwrap();

        assert_eq!(instance.status(), RuntimeStatus::Crashed);
    }

    #[tokio::test]
    async fn spawn_fails_with_logs_when_health_check_never_succeeds() {
        let port = allocate_loopback_port().unwrap();
        // This process never binds the port or answers /health, so every
        // health-poll attempt fails until the (short, test-only) timeout.
        let spec = SpawnSpec {
            program: "python3".into(),
            args: vec![
                "-u".into(), // unbuffered stdout, so the log reader sees the print immediately
                "-c".into(),
                "import time; print('booting'); time.sleep(60)".into(),
            ],
            env: vec![],
            cwd: None,
            health_path: "/health".into(),
            auth_token: "unused".into(),
        };

        let result = spawn_with_timeout(spec, port, Duration::from_millis(1500)).await;

        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("health check should time out"),
        };
        let message = err.to_string();
        assert!(message.contains("did not become healthy"), "{message}");
        assert!(
            message.contains("booting"),
            "expected captured stdout in error, got: {message}"
        );
    }
}
