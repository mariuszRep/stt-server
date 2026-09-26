use std::path::PathBuf;
use std::time::Duration;

use stt_server_next::app::{self, BindOverrides, DEFAULT_BIND_HOST, DEFAULT_BIND_PORT};
use stt_server_next::cli::{self, AutostartAction, Command, RunFlags, ServiceAction};
use stt_server_next::discovery;

fn effective_data_dir(flags: &RunFlags) -> PathBuf {
    flags.data_dir.clone().unwrap_or_else(app::data_dir)
}

fn effective_data_dir_opt(data_dir: &Option<PathBuf>) -> PathBuf {
    data_dir.clone().unwrap_or_else(app::data_dir)
}

fn bind_overrides(flags: &RunFlags) -> BindOverrides {
    BindOverrides {
        host: flags.host.clone(),
        port: flags.port,
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = match cli::parse(&args) {
        Ok(command) => command,
        Err(error) => {
            eprintln!("error: {}", error.message);
            eprint!("{}", cli::USAGE);
            std::process::exit(error.exit_code);
        }
    };
    let code = dispatch(command).await;
    std::process::exit(code);
}

async fn dispatch(command: Command) -> i32 {
    match command {
        Command::Help => {
            print!("{}", cli::USAGE);
            0
        }
        Command::Run(flags) => cmd_run(flags).await,
        Command::Start(flags) => cmd_start(flags).await,
        Command::Stop { data_dir } => cmd_stop(effective_data_dir_opt(&data_dir)).await,
        Command::Restart(flags) => cmd_restart(flags).await,
        Command::Status { json, data_dir } => {
            cmd_status(effective_data_dir_opt(&data_dir), json).await
        }
        Command::Autostart { action, flags } => cmd_autostart(action, flags),
        Command::Service(action) => cmd_service(action),
    }
}

// ---------------------------------------------------------------------------
// run: foreground, single-instance, LAN-guarded server.
// ---------------------------------------------------------------------------

async fn cmd_run(flags: RunFlags) -> i32 {
    let data_dir = effective_data_dir(&flags);
    let overrides = bind_overrides(&flags);
    let limits = flags.limits;
    let result = stt_server_next::api::run_http_full(data_dir, limits, overrides, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await;
    match result {
        Ok(()) => 0,
        Err(stt_server_next::api::ServeError::AlreadyRunning) => {
            eprintln!("error: another instance is already running on this data directory");
            3
        }
        Err(stt_server_next::api::ServeError::BindFailed(error)) => {
            eprintln!("error: failed to bind: {error}");
            4
        }
        Err(other) => {
            eprintln!("error: {other}");
            1
        }
    }
}

// ---------------------------------------------------------------------------
// start: spawn `run` detached, wait for /health.
// ---------------------------------------------------------------------------

async fn cmd_start(flags: RunFlags) -> i32 {
    let data_dir = effective_data_dir(&flags);
    let port = flags.port.unwrap_or(DEFAULT_BIND_PORT);
    let host = flags
        .host
        .clone()
        .unwrap_or_else(|| DEFAULT_BIND_HOST.to_owned());

    if let Some(info) = discovery::read_server_json(&data_dir) {
        if discovery::pid_is_alive(info.pid) && health_ok(&probe_host(&info.host), info.port).await
        {
            println!(
                "already running: pid {} on {}:{}",
                info.pid, info.host, info.port
            );
            return 0;
        }
        // Stale: clean it up before spawning a fresh instance.
        discovery::remove_server_json(&data_dir);
    }

    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            eprintln!("error: could not resolve current executable: {error}");
            return 1;
        }
    };
    let mut args: Vec<String> = vec!["run".to_owned()];
    args.push("--data-dir".to_owned());
    args.push(data_dir.display().to_string());
    args.push("--port".to_owned());
    args.push(port.to_string());
    args.push("--host".to_owned());
    args.push(host.clone());
    if let Some(v) = flags.limits.queue_max_waiting {
        args.push("--queue-max-waiting".to_owned());
        args.push(v.to_string());
    }
    if let Some(v) = flags.limits.queue_wait_timeout_ms {
        args.push("--queue-wait-timeout-ms".to_owned());
        args.push(v.to_string());
    }
    if let Some(v) = flags.limits.inference_timeout_ms {
        args.push("--inference-timeout-ms".to_owned());
        args.push(v.to_string());
    }

    let mut command = std::process::Command::new(&exe);
    command.args(&args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW | DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    command.stdin(std::process::Stdio::null());
    command.stdout(std::process::Stdio::null());
    command.stderr(std::process::Stdio::null());

    let child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            eprintln!("error: could not spawn detached server: {error}");
            return 1;
        }
    };
    // Detach: we do not wait() on the child (it outlives this process).
    let spawned_pid = child.id();
    std::mem::forget(child);

    let probe = probe_host(&host);
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if health_ok(&probe, port).await {
            println!("started: pid {spawned_pid} on {host}:{port}");
            return 0;
        }
        if std::time::Instant::now() >= deadline {
            eprintln!("error: server did not become healthy within 30s");
            return 1;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// The address this CLI should probe over HTTP for a given configured bind
/// host: a wildcard bind (`0.0.0.0`/`::`) is not itself connectable, so probe
/// loopback instead (the server always also accepts loopback connections).
fn probe_host(host: &str) -> String {
    match host {
        "0.0.0.0" | "::" => DEFAULT_BIND_HOST.to_owned(),
        other => other.to_owned(),
    }
}

/// True only when something that identifies itself as this server answers
/// `/health` (independent review L2): checking the status code alone would
/// misreport an unrelated program already listening on the configured port
/// as a running `stt-server-next` instance.
async fn health_ok(host: &str, port: u16) -> bool {
    let url = format!("http://{host}:{port}/health");
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
    {
        Ok(client) => client,
        Err(_) => return false,
    };
    let Ok(response) = client.get(&url).send().await else {
        return false;
    };
    if !response.status().is_success() {
        return false;
    }
    let Ok(body) = response.json::<serde_json::Value>().await else {
        return false;
    };
    body.get("service").and_then(|v| v.as_str()) == Some(stt_server_next::api::SERVICE_ID)
}

// ---------------------------------------------------------------------------
// stop: authenticated graceful shutdown, confirmed by the data-folder lock.
// ---------------------------------------------------------------------------

async fn cmd_stop(data_dir: PathBuf) -> i32 {
    match discovery::acquire_lock(&data_dir) {
        Ok(_lock) => {
            discovery::remove_server_json(&data_dir);
            println!("not running (stale discovery information cleared)");
            return 0;
        }
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
        Err(error) => {
            eprintln!("error: cannot determine server ownership: {error}");
            return 1;
        }
    }
    let Some(info) = discovery::read_server_json(&data_dir) else {
        eprintln!("error: server is starting or discovery information is unavailable; retry stop");
        return 1;
    };
    let probe = probe_host(&info.host);
    let result = async {
        let token = std::fs::read_to_string(data_dir.join("auth.token"))?;
        reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()?
            .post(format!("http://{probe}:{}/v1/local/shutdown", info.port))
            .bearer_auth(token.trim())
            .send()
            .await?
            .error_for_status()?;
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;
    if let Err(error) = result {
        eprintln!("error: graceful shutdown failed: {error}; no process was forcibly terminated");
        return 1;
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        match discovery::acquire_lock(&data_dir) {
            Ok(_lock) => {
                discovery::remove_server_json(&data_dir);
                println!("stopped via shutdown endpoint");
                return 0;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => {
                eprintln!("error: cannot confirm shutdown: {error}");
                return 1;
            }
        }
        if std::time::Instant::now() >= deadline {
            eprintln!("error: shutdown is still pending; retry status or stop; no process was forcibly terminated");
            return 1;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

// ---------------------------------------------------------------------------
// restart: stop then start.
// ---------------------------------------------------------------------------

async fn cmd_restart(flags: RunFlags) -> i32 {
    let data_dir = effective_data_dir(&flags);
    let stop_code = cmd_stop(data_dir).await;
    if stop_code != 0 {
        return stop_code;
    }
    cmd_start(flags).await
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

async fn cmd_status(data_dir: PathBuf, json: bool) -> i32 {
    let Some(info) = discovery::read_server_json(&data_dir) else {
        report_not_running(json);
        return 1;
    };
    let alive = discovery::pid_is_alive(info.pid);
    let healthy = alive && health_ok(&probe_host(&info.host), info.port).await;
    if !healthy {
        discovery::remove_server_json(&data_dir);
        report_not_running(json);
        return 1;
    }
    if json {
        println!(
            "{}",
            serde_json::json!({
                "running": true,
                "pid": info.pid,
                "host": info.host,
                "port": info.port,
                "data_dir": data_dir.display().to_string(),
                "version": info.version,
            })
        );
    } else {
        println!(
            "running: pid {} on {}:{} (data dir: {})",
            info.pid,
            info.host,
            info.port,
            data_dir.display()
        );
    }
    0
}

fn report_not_running(json: bool) {
    if json {
        println!("{}", serde_json::json!({"running": false}));
    } else {
        println!("not running");
    }
}

// ---------------------------------------------------------------------------
// autostart
// ---------------------------------------------------------------------------

#[cfg(windows)]
fn cmd_autostart(action: AutostartAction, flags: RunFlags) -> i32 {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            eprintln!("error: could not resolve current executable: {error}");
            return 1;
        }
    };
    match action {
        AutostartAction::Enable => {
            let data_dir = effective_data_dir(&flags);
            match stt_server_next::autostart::enable(
                &exe,
                &data_dir,
                flags.port,
                flags.host.as_deref(),
            ) {
                Ok(()) => {
                    println!("autostart enabled");
                    0
                }
                Err(error) => {
                    eprintln!("error: {error}");
                    1
                }
            }
        }
        AutostartAction::Disable => match stt_server_next::autostart::disable() {
            Ok(()) => {
                println!("autostart disabled");
                0
            }
            Err(error) => {
                eprintln!("error: {error}");
                1
            }
        },
        AutostartAction::Status => match stt_server_next::autostart::query() {
            Ok(Some(value)) => {
                println!("enabled: {value}");
                0
            }
            Ok(None) => {
                println!("disabled");
                0
            }
            Err(error) => {
                eprintln!("error: {error}");
                1
            }
        },
    }
}

#[cfg(not(windows))]
fn cmd_autostart(_action: AutostartAction, _flags: RunFlags) -> i32 {
    eprintln!("error: autostart is only supported on Windows");
    2
}

// ---------------------------------------------------------------------------
// service (existing Windows service host; unsupported elsewhere).
// ---------------------------------------------------------------------------

#[cfg(windows)]
fn cmd_service(action: ServiceAction) -> i32 {
    let result = match action {
        ServiceAction::Install => stt_server_next::service::install(),
        ServiceAction::Uninstall => stt_server_next::service::uninstall(),
        ServiceAction::Run => stt_server_next::service::dispatch(),
    };
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("error: {error}");
            1
        }
    }
}

#[cfg(not(windows))]
fn cmd_service(_action: ServiceAction) -> i32 {
    eprintln!("error: the Windows service host is only supported on Windows");
    2
}

#[cfg(test)]
mod recovery_tests {
    use super::*;

    #[tokio::test]
    async fn stale_live_pid_is_never_killed() {
        let dir = std::env::temp_dir().join(format!("stt-stop-test-{}", uuid::Uuid::new_v4()));
        // Simulates PID reuse by pointing stale discovery at this test process.
        discovery::write_server_json(&dir, &discovery::ServerInfo::new("127.0.0.1".into(), 54499))
            .unwrap();
        assert_eq!(cmd_stop(dir.clone()).await, 0);
        assert!(!dir.join("server.json").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn held_lock_without_endpoint_fails_safely() {
        let dir = std::env::temp_dir().join(format!("stt-stop-test-{}", uuid::Uuid::new_v4()));
        let lock = discovery::acquire_lock(&dir).unwrap();
        discovery::write_server_json(&dir, &discovery::ServerInfo::new("127.0.0.1".into(), 54499))
            .unwrap();
        assert_eq!(cmd_stop(dir.clone()).await, 1);
        assert!(dir.join("server.json").exists());
        drop(lock);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// L2: `start`/`status` must not mistake an unrelated program answering
    /// "200 OK" on the configured port for this server. Spins up a plain
    /// axum router (no `service` field) on an ephemeral port and confirms
    /// `health_ok` reports it as *not* this server, then confirms a real
    /// `/health` handler with the right `service` field does pass.
    #[tokio::test]
    async fn health_ok_requires_service_identity_not_just_200() {
        let impostor = axum::Router::new().route(
            "/health",
            axum::routing::get(|| async { axum::Json(serde_json::json!({"status": "ok"})) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            axum::serve(listener, impostor).await.unwrap();
        });
        assert!(!health_ok("127.0.0.1", port).await);
        server.abort();

        let real =
            axum::Router::new().route("/health", axum::routing::get(stt_server_next::api::health));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            axum::serve(listener, real).await.unwrap();
        });
        assert!(health_ok("127.0.0.1", port).await);
        server.abort();
    }
}
