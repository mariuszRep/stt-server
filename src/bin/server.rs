use std::path::PathBuf;
use std::time::Duration;

use stt_server_next::app::{self, BindOverrides, DEFAULT_BIND_HOST};
use stt_server_next::cli::{
    self, AutostartAction, Command, ModelsCommand, RunFlags, ServiceAction, UpdateCommand,
};
use stt_server_next::discovery;
use stt_server_next::model_cli::{self, ConnectError};
use stt_server_next::selfupdate;

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
        // `None` (not `Some(vec![])`) when `--cors-origin` was never given,
        // so it loses precedence to the stored setting rather than forcing
        // the default empty (locked-down) list -- see `App::cors_origins`.
        cors_origins: (!flags.cors_origins.is_empty()).then(|| flags.cors_origins.clone()),
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
        Command::Health { json, data_dir } => {
            cmd_health(effective_data_dir_opt(&data_dir), json).await
        }
        Command::Models(models_command) => cmd_models(models_command).await,
        Command::Update(update_command) => cmd_update(update_command).await,
    }
}

// ---------------------------------------------------------------------------
// update: check GitHub Releases (or STT_NEXT_UPDATE_URL for local rehearsal),
// then, on `install`, download+verify, stop, replace this executable, and
// restart -- rolling back automatically if the new version never becomes
// healthy. See `src/selfupdate.rs` for the download/verify/replace/rollback
// primitives; this function is just the orchestration glue, mirroring how
// `cmd_stop`/`cmd_start` are the glue over `discovery`/`api`.
// ---------------------------------------------------------------------------

async fn cmd_update(command: UpdateCommand) -> i32 {
    match command {
        UpdateCommand::Check { json } => cmd_update_check(json).await,
        UpdateCommand::Install {
            yes,
            json,
            data_dir,
        } => cmd_update_install(effective_data_dir_opt(&data_dir), yes, json).await,
    }
}

fn update_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

async fn cmd_update_check(json: bool) -> i32 {
    let http = update_http_client();
    let endpoint = selfupdate::default_update_endpoint();
    match selfupdate::check_latest(&http, &endpoint).await {
        Ok(check) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "current_version": check.current_version,
                        "latest_version": check.release.version,
                        "update_available": check.update_available,
                    })
                );
            } else if check.update_available {
                println!(
                    "update available: {} -> {}",
                    check.current_version, check.release.version
                );
                println!("run `stt-server-next update install --yes` to install it");
            } else {
                println!(
                    "up to date: {} is the latest release",
                    check.current_version
                );
            }
            0
        }
        Err(error) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({"error": {"code": "update_check_failed", "message": error.to_string()}})
                );
            } else {
                eprintln!("error: could not check for updates: {error}");
            }
            1
        }
    }
}

async fn cmd_update_install(data_dir: PathBuf, yes: bool, json: bool) -> i32 {
    let http = update_http_client();
    let endpoint = selfupdate::default_update_endpoint();
    let check = match selfupdate::check_latest(&http, &endpoint).await {
        Ok(check) => check,
        Err(error) => {
            eprintln!("error: could not check for updates: {error}");
            return 1;
        }
    };
    if !check.update_available {
        if json {
            println!(
                "{}",
                serde_json::json!({"installed": false, "reason": "already_up_to_date", "current_version": check.current_version})
            );
        } else {
            println!(
                "already up to date: {} is the latest release",
                check.current_version
            );
        }
        return 0;
    }
    if !yes {
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "installed": false,
                    "reason": "confirmation_required",
                    "current_version": check.current_version,
                    "latest_version": check.release.version,
                })
            );
        } else {
            println!(
                "update available: {} -> {}",
                check.current_version, check.release.version
            );
            println!("re-run with --yes to download, verify, and install it");
        }
        return 0;
    }

    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            eprintln!("error: could not resolve current executable: {error}");
            return 1;
        }
    };
    let stage_dir = data_dir.join("update");
    println!("downloading and verifying {} ...", check.release.version);
    let staged = match selfupdate::download_and_verify(&http, &check.release, &stage_dir).await {
        Ok(path) => path,
        Err(error) => {
            eprintln!("error: download/verification failed, nothing was changed: {error}");
            return 1;
        }
    };

    // Remember how the running instance was reachable (if any) so the
    // restart after replacement uses the same bind host/port.
    let previous_info = discovery::read_server_json(&data_dir);
    let was_running = previous_info
        .as_ref()
        .map(|info| discovery::pid_is_alive(info.pid))
        .unwrap_or(false);
    if was_running {
        println!("stopping the running server before replacing its executable ...");
        let stop_code = cmd_stop(data_dir.clone()).await;
        if stop_code != 0 {
            eprintln!(
                "error: could not stop the running server; update aborted, nothing was changed"
            );
            return 1;
        }
    }

    let backup = match selfupdate::replace_exe(&exe, &staged) {
        Ok(backup) => backup,
        Err(error) => {
            eprintln!("error: could not install the new executable: {error}");
            if was_running {
                eprintln!(
                    "the previous executable is unchanged; restart it with `stt-server-next start`"
                );
            }
            return 1;
        }
    };

    let restart_flags = RunFlags {
        data_dir: Some(data_dir.clone()),
        host: previous_info.as_ref().map(|info| info.host.clone()),
        port: previous_info.as_ref().map(|info| info.port),
        network: None,
        limits: Default::default(),
        cors_origins: Vec::new(),
    };

    if !was_running {
        // Nothing was running before, so there is nothing to restart or
        // verify health of; the swap itself (already hash-verified) is the
        // whole job here.
        println!(
            "installed {} (was not running; start it with `stt-server-next start`)",
            check.release.version
        );
        return 0;
    }

    println!("starting the new version ...");
    let start_code = cmd_start(restart_flags.clone()).await;
    if start_code == 0 {
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "installed": true,
                    "from_version": check.current_version,
                    "to_version": check.release.version,
                })
            );
        } else {
            println!(
                "update installed: {} -> {}",
                check.current_version, check.release.version
            );
        }
        return 0;
    }

    eprintln!("error: new version did not become healthy; rolling back");
    // Best-effort: stop whatever the failed new instance left behind before
    // restoring the previous binary underneath it.
    let _ = cmd_stop(data_dir.clone()).await;
    if let Err(error) = selfupdate::rollback_exe(&exe, &backup) {
        eprintln!("error: automatic rollback failed: {error}");
        eprintln!(
            "the previous executable is still at {}; restore it manually",
            backup.display()
        );
        return 1;
    }
    let restore_code = cmd_start(restart_flags).await;
    if restore_code != 0 {
        eprintln!("error: rolled back the executable but could not restart the previous version");
    } else {
        eprintln!(
            "rolled back to the previous version ({})",
            check.current_version
        );
    }
    if json {
        println!(
            "{}",
            serde_json::json!({
                "installed": false,
                "reason": "rolled_back",
                "current_version": check.current_version,
                "attempted_version": check.release.version,
            })
        );
    }
    1
}

// ---------------------------------------------------------------------------
// run: foreground, single-instance, LAN-guarded server.
// ---------------------------------------------------------------------------

async fn cmd_run(flags: RunFlags) -> i32 {
    let data_dir = effective_data_dir(&flags);
    let overrides = bind_overrides(&flags);
    let network = flags.network;
    let limits = flags.limits;
    let result = stt_server_next::api::run_http_full(
        data_dir,
        limits,
        overrides,
        network,
        app::install_scope(),
        async {
            let _ = tokio::signal::ctrl_c().await;
        },
    )
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
    // Only pass `--port` on to the spawned process when the caller explicitly
    // asked for one. Passing the resolved default unconditionally would look
    // identical to an explicit `--port` once it reaches `run`, and would
    // silently disable this install's port-fallback (see
    // `api::run_http_full`): a per-user server whose port is merely
    // preferred (default or stored setting) must still be free to fall back
    // to another free port when it's taken.
    if let Some(port) = flags.port {
        args.push("--port".to_owned());
        args.push(port.to_string());
    }
    // Only pass `--host` on to the spawned process when the caller
    // explicitly asked for one -- same reasoning as `--port` above, and also
    // what lets `--network`/the stored `network_mode` setting actually take
    // effect: an unconditionally-passed `--host` would always look like an
    // explicit override to `run_http_full` and permanently force mode
    // "custom" (see `crate::network`).
    if let Some(host) = &flags.host {
        args.push("--host".to_owned());
        args.push(host.clone());
    }
    if let Some(network) = flags.network {
        args.push("--network".to_owned());
        args.push(network.as_str().to_owned());
    }
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
    std::mem::forget(child);

    // The child resolves its own effective port (explicit --port > stored
    // setting > default), and may fall back further to a free port if that
    // one is taken -- so this process cannot assume any particular port and
    // must discover it the same way every other CLI command does: through
    // `server.json`, which `run_http_full` writes with the port actually
    // bound.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(info) = discovery::read_server_json(&data_dir) {
            if health_ok(&probe_host(&info.host), info.port).await {
                println!("started: pid {} on {}:{}", info.pid, info.host, info.port);
                return 0;
            }
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
                "api_level": info.api_level,
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
        println!("version: {} (api_level {})", info.version, info.api_level);
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

// ---------------------------------------------------------------------------
// health: combines /health, /readiness, selected model and system info
// (docs/client-contract.md section 7) against the running server's
// authenticated API.
// ---------------------------------------------------------------------------

async fn cmd_health(data_dir: PathBuf, json: bool) -> i32 {
    let conn = match model_cli::connect(&data_dir).await {
        Ok(conn) => conn,
        Err(error) => return report_connect_error(&error, json),
    };
    let health = model_cli::call(&conn, reqwest::Method::GET, "/health", None).await;
    let readiness = model_cli::call(&conn, reqwest::Method::GET, "/readiness", None).await;
    let selected = model_cli::call(
        &conn,
        reqwest::Method::GET,
        "/v1/local/models/selected",
        None,
    )
    .await;
    let system = model_cli::call(&conn, reqwest::Method::GET, "/v1/local/system", None).await;

    let outcomes = [&health, &readiness, &selected, &system];
    if outcomes.iter().any(|result| result.is_err()) {
        eprintln!("error: one or more health-card requests failed");
        for (name, result) in [
            ("health", &health),
            ("readiness", &readiness),
            ("selected", &selected),
            ("system", &system),
        ] {
            if let Err(error) = result {
                eprintln!("  {name}: {error}");
            }
        }
        return 1;
    }
    let health = health.unwrap();
    let readiness = readiness.unwrap();
    let selected = selected.unwrap();
    let system = system.unwrap();

    if json {
        println!(
            "{}",
            serde_json::json!({
                "health": health.body,
                "readiness": readiness.body,
                "selected": selected.body,
                "system": system.body,
            })
        );
    } else {
        println!(
            "health: {}",
            health
                .body
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
        );
        println!(
            "readiness: {}",
            readiness
                .body
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
        );
        let version = health.body.get("version").and_then(|v| v.as_str());
        let api_level = health.body.get("api_level").and_then(|v| v.as_u64());
        if let (Some(version), Some(api_level)) = (version, api_level) {
            println!("version: {version} (api_level {api_level})");
        }
        match selected.body.get("model") {
            Some(serde_json::Value::String(id)) => println!("selected model: {id}"),
            _ => println!("selected model: none"),
        }
        if let Some(backend) = system.body.get("backend") {
            println!("backend: {backend}");
        }
        if let Some(reason) = readiness.body.get("reason").and_then(|v| v.as_str()) {
            println!("not ready: {reason}");
        }
    }
    // Non-zero exit whenever readiness itself reported failure, so scripts
    // can branch on exit code without parsing JSON.
    if readiness.is_error() {
        1
    } else {
        0
    }
}

fn report_connect_error(error: &ConnectError, json: bool) -> i32 {
    if json {
        println!(
            "{}",
            serde_json::json!({"error": {"code": "server_unavailable", "message": error.to_string()}})
        );
    } else {
        eprintln!("error: {error}");
    }
    1
}

// ---------------------------------------------------------------------------
// models: thin wrappers over the running server's model-management API
// (src/api.rs, src/import.rs, src/operations.rs). No business logic lives
// here -- every command is a direct HTTP call plus rendering.
// ---------------------------------------------------------------------------

async fn cmd_models(command: ModelsCommand) -> i32 {
    match command {
        ModelsCommand::List { json, data_dir } => {
            simple_get(data_dir, "/v1/local/models", json).await
        }
        ModelsCommand::Recommended { json, data_dir } => {
            simple_get(data_dir, "/v1/local/recommendations", json).await
        }
        ModelsCommand::Selected { json, data_dir } => {
            simple_get(data_dir, "/v1/local/models/selected", json).await
        }
        ModelsCommand::Unload { json, data_dir } => {
            simple_call(
                data_dir,
                reqwest::Method::DELETE,
                "/v1/local/models/selected",
                None,
                json,
            )
            .await
        }
        ModelsCommand::Select { id, json, data_dir } => {
            simple_call(
                data_dir,
                reqwest::Method::POST,
                &format!("/v1/local/models/{id}/select"),
                None,
                json,
            )
            .await
        }
        ModelsCommand::Remove { id, json, data_dir } => {
            simple_call(
                data_dir,
                reqwest::Method::DELETE,
                &format!("/v1/local/models/{id}"),
                None,
                json,
            )
            .await
        }
        ModelsCommand::Cancel {
            operation_id,
            data_dir,
        } => {
            simple_call(
                data_dir,
                reqwest::Method::POST,
                &format!("/v1/local/operations/{operation_id}/cancel"),
                None,
                false,
            )
            .await
        }
        ModelsCommand::Install {
            id,
            wait,
            json,
            data_dir,
        } => {
            operation_call(
                effective_data_dir_opt(&data_dir),
                reqwest::Method::POST,
                &format!("/v1/local/models/{id}/install"),
                None,
                wait,
                json,
            )
            .await
        }
        ModelsCommand::Verify {
            id,
            wait,
            json,
            data_dir,
        } => {
            operation_call(
                effective_data_dir_opt(&data_dir),
                reqwest::Method::POST,
                &format!("/v1/local/models/{id}/verify"),
                None,
                wait,
                json,
            )
            .await
        }
        ModelsCommand::Refresh {
            wait,
            json,
            data_dir,
        } => {
            let dir = effective_data_dir_opt(&data_dir);
            let conn = match model_cli::connect(&dir).await {
                Ok(conn) => conn,
                Err(error) => return report_connect_error(&error, json),
            };
            let outcome = match model_cli::call(
                &conn,
                reqwest::Method::POST,
                "/v1/local/models/refresh",
                None,
            )
            .await
            {
                Ok(outcome) => outcome,
                Err(error) => {
                    eprintln!("error: {error}");
                    return 1;
                }
            };
            if outcome.is_error() {
                return report_api_error(&outcome, json);
            }
            if !wait {
                return report_operation_started(&outcome.body, json);
            }
            let operation_id = outcome
                .body
                .get("operation_id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_owned();
            match model_cli::poll_operation(&conn, &operation_id, json).await {
                Ok(result) => report_refresh_result(&result, json),
                Err(error) => {
                    eprintln!("error: {error}");
                    1
                }
            }
        }
        ModelsCommand::Import {
            path,
            model,
            quant,
            wait,
            json,
            data_dir,
        } => {
            let dir = effective_data_dir_opt(&data_dir);
            let conn = match model_cli::connect(&dir).await {
                Ok(conn) => conn,
                Err(error) => return report_connect_error(&error, json),
            };
            let outcome =
                match model_cli::import_model(&conn, &path, &model, quant.as_deref()).await {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        eprintln!("error: {error}");
                        return 1;
                    }
                };
            if outcome.is_error() {
                return report_api_error(&outcome, json);
            }
            if !wait {
                return report_operation_started(&outcome.body, json);
            }
            let operation_id = outcome
                .body
                .get("operation_id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_owned();
            match model_cli::poll_operation(&conn, &operation_id, json).await {
                Ok(result) => report_terminal_operation(&result, json),
                Err(error) => {
                    eprintln!("error: {error}");
                    1
                }
            }
        }
        ModelsCommand::ImportUser {
            from,
            wait,
            json,
            data_dir,
        } => {
            let dir = effective_data_dir_opt(&data_dir);
            let conn = match model_cli::connect(&dir).await {
                Ok(conn) => conn,
                Err(error) => return report_connect_error(&error, json),
            };
            let body = serde_json::json!({
                "from": from.map(|path| path.display().to_string()),
            });
            let outcome = match model_cli::call(
                &conn,
                reqwest::Method::POST,
                "/v1/local/models/import-user",
                Some(body),
            )
            .await
            {
                Ok(outcome) => outcome,
                Err(error) => {
                    eprintln!("error: {error}");
                    return 1;
                }
            };
            if outcome.is_error() {
                return report_api_error(&outcome, json);
            }
            if !wait {
                return report_operation_started(&outcome.body, json);
            }
            let operation_id = outcome
                .body
                .get("operation_id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_owned();
            match model_cli::poll_operation(&conn, &operation_id, json).await {
                Ok(result) => report_import_user_result(&result, json),
                Err(error) => {
                    eprintln!("error: {error}");
                    1
                }
            }
        }
    }
}

async fn simple_get(data_dir: Option<PathBuf>, path: &str, json: bool) -> i32 {
    simple_call(data_dir, reqwest::Method::GET, path, None, json).await
}

async fn simple_call(
    data_dir: Option<PathBuf>,
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
    json: bool,
) -> i32 {
    let dir = effective_data_dir_opt(&data_dir);
    let conn = match model_cli::connect(&dir).await {
        Ok(conn) => conn,
        Err(error) => return report_connect_error(&error, json),
    };
    let outcome = match model_cli::call(&conn, method, path, body).await {
        Ok(outcome) => outcome,
        Err(error) => {
            eprintln!("error: {error}");
            return 1;
        }
    };
    if outcome.is_error() {
        return report_api_error(&outcome, json);
    }
    if json {
        println!("{}", outcome.body);
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&outcome.body).unwrap_or_default()
        );
    }
    0
}

/// Shared by `install`/`verify`: fire the mutating POST, then either report
/// the immediate `operation_id`/`state`, or poll it to a terminal state
/// under `--wait`.
async fn operation_call(
    data_dir: PathBuf,
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
    wait: bool,
    json: bool,
) -> i32 {
    let conn = match model_cli::connect(&data_dir).await {
        Ok(conn) => conn,
        Err(error) => return report_connect_error(&error, json),
    };
    let outcome = match model_cli::call(&conn, method, path, body).await {
        Ok(outcome) => outcome,
        Err(error) => {
            eprintln!("error: {error}");
            return 1;
        }
    };
    if outcome.is_error() {
        return report_api_error(&outcome, json);
    }
    if !wait {
        return report_operation_started(&outcome.body, json);
    }
    let operation_id = outcome
        .body
        .get("operation_id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned();
    match model_cli::poll_operation(&conn, &operation_id, json).await {
        Ok(result) => report_terminal_operation(&result, json),
        Err(error) => {
            eprintln!("error: {error}");
            1
        }
    }
}

fn report_api_error(outcome: &model_cli::ApiOutcome, json: bool) -> i32 {
    if json {
        println!("{}", outcome.body);
    } else {
        eprintln!("error: {}", model_cli::format_error(outcome));
    }
    1
}

fn report_operation_started(body: &serde_json::Value, json: bool) -> i32 {
    if json {
        println!("{body}");
    } else {
        let operation_id = body
            .get("operation_id")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let state = body.get("state").and_then(|v| v.as_str()).unwrap_or("?");
        println!("operation_id: {operation_id}");
        println!("state: {state}");
    }
    0
}

fn report_terminal_operation(body: &serde_json::Value, json: bool) -> i32 {
    let state = body.get("state").and_then(|v| v.as_str()).unwrap_or("?");
    if json {
        println!("{body}");
    } else {
        println!("final state: {state}");
        if let Some(error) = body.get("error").and_then(|v| v.as_str()) {
            println!("error: {error}");
        }
    }
    if state == "completed" {
        0
    } else {
        1
    }
}

fn report_refresh_result(body: &serde_json::Value, json: bool) -> i32 {
    let state = body.get("state").and_then(|v| v.as_str()).unwrap_or("?");
    if json {
        println!("{body}");
        return if state == "completed" { 0 } else { 1 };
    }
    println!("final state: {state}");
    if let Some(error) = body.get("error").and_then(|v| v.as_str()) {
        println!("error: {error}");
    }
    if let Some(result) = body.get("result") {
        for (label, key) in [
            ("registered", "registered"),
            ("duplicates", "duplicates"),
            ("removed", "removed"),
            ("changed", "changed"),
        ] {
            if let Some(items) = result.get(key).and_then(|v| v.as_array()) {
                if !items.is_empty() {
                    println!("{label}: {}", items.len());
                }
            }
        }
        if let Some(unsupported) = result.get("unsupported").and_then(|v| v.as_array()) {
            for entry in unsupported {
                let path = entry.get("path").and_then(|v| v.as_str()).unwrap_or("?");
                let reason = entry.get("reason").and_then(|v| v.as_str()).unwrap_or("?");
                let retryable = entry
                    .get("retryable")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                println!(
                    "failed: {path}: {reason} ({})",
                    if retryable {
                        "retryable"
                    } else {
                        "not retryable"
                    }
                );
            }
        }
    }
    if state == "completed" {
        0
    } else {
        1
    }
}

/// Renders `models import-user`'s terminal operation body: per-model
/// success/skip/failure, mirroring `report_refresh_result`'s shape for the
/// drop-in scan.
fn report_import_user_result(body: &serde_json::Value, json: bool) -> i32 {
    let state = body.get("state").and_then(|v| v.as_str()).unwrap_or("?");
    if json {
        println!("{body}");
        return if state == "completed" { 0 } else { 1 };
    }
    println!("final state: {state}");
    if let Some(error) = body.get("error").and_then(|v| v.as_str()) {
        println!("error: {error}");
    }
    if let Some(result) = body.get("result") {
        if let Some(imported) = result.get("imported").and_then(|v| v.as_array()) {
            for entry in imported {
                let model = entry.get("model").and_then(|v| v.as_str()).unwrap_or("?");
                println!("imported: {model}");
            }
        }
        if let Some(skipped) = result.get("skipped").and_then(|v| v.as_array()) {
            for entry in skipped {
                let model = entry.get("model").and_then(|v| v.as_str()).unwrap_or("?");
                let reason = entry.get("reason").and_then(|v| v.as_str()).unwrap_or("?");
                println!("skipped: {model} ({reason})");
            }
        }
        if let Some(unsupported) = result.get("unsupported").and_then(|v| v.as_array()) {
            for entry in unsupported {
                let what = entry
                    .get("model")
                    .and_then(|v| v.as_str())
                    .or_else(|| entry.get("path").and_then(|v| v.as_str()))
                    .unwrap_or("?");
                let reason = entry.get("reason").and_then(|v| v.as_str()).unwrap_or("?");
                println!("failed: {what}: {reason}");
            }
        }
    }
    if state == "completed" {
        0
    } else {
        1
    }
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

        let parent = std::env::temp_dir().canonicalize().unwrap();
        let path = parent.join(format!("stt-server-next-test-{}", uuid::Uuid::new_v4()));
        let real_app = app::open_app_at(path.clone()).unwrap();
        let real = axum::Router::new()
            .route("/health", axum::routing::get(stt_server_next::api::health))
            .with_state(real_app.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            axum::serve(listener, real).await.unwrap();
        });
        assert!(health_ok("127.0.0.1", port).await);
        server.abort();
        // Wait for the aborted task to actually unwind (it holds a clone of
        // `real_app`, and thus the open SQLite connection) before dropping
        // our own clone and deleting the directory -- otherwise the delete
        // can race the task's teardown and fail with a Windows file lock
        // (AGENTS.md's "drop the app and router before deleting temp dirs").
        let _ = server.await;
        drop(real_app);
        std::fs::remove_dir_all(path.canonicalize().unwrap()).unwrap();
    }
}

/// Exercises the new `models`/`health` CLI commands against a real,
/// locally-spawned server instance -- an isolated `--data-dir` and a spare
/// port (54400+), never the user's real data folder -- the same harness
/// shape `recovery_tests` above uses for `cmd_stop`. The `App` is dropped
/// (via aborting the serving task) before the temp dir is removed, per
/// `AGENTS.md`'s Windows file-lock rule.
#[cfg(test)]
mod models_cli_tests {
    use super::*;
    use stt_server_next::cli::ModelsCommand;

    struct TestServer {
        data_dir: PathBuf,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.task.abort();
            let _ = std::fs::remove_dir_all(&self.data_dir);
        }
    }

    async fn spawn_test_server(port: u16) -> TestServer {
        let data_dir = std::env::temp_dir().join(format!(
            "stt-models-cli-test-{}-{}",
            port,
            uuid::Uuid::new_v4()
        ));
        let dir_for_task = data_dir.clone();
        let task = tokio::spawn(async move {
            let overrides = app::BindOverrides {
                host: Some("127.0.0.1".to_owned()),
                port: Some(port),
                cors_origins: None,
            };
            let _ = stt_server_next::api::run_http_full(
                dir_for_task,
                Default::default(),
                overrides,
                None,
                app::InstallScope::PerUser,
                std::future::pending::<()>(),
            )
            .await;
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while discovery::read_server_json(&data_dir).is_none() {
            if std::time::Instant::now() >= deadline {
                panic!("test server did not start within 10s");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        while !health_ok("127.0.0.1", port).await {
            if std::time::Instant::now() >= deadline {
                panic!("test server never answered /health");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        TestServer { data_dir, task }
    }

    #[tokio::test]
    async fn health_command_reports_running_server() {
        let server = spawn_test_server(54410).await;
        let code = cmd_health(server.data_dir.clone(), true).await;
        // No model is loaded in a fresh data dir, so /readiness is 503 and
        // the command's exit code should reflect that honestly.
        assert_eq!(code, 1);
    }

    #[tokio::test]
    async fn health_command_reports_absent_server_clearly() {
        let dir = std::env::temp_dir().join(format!("stt-health-absent-{}", uuid::Uuid::new_v4()));
        let code = cmd_health(dir.clone(), false).await;
        assert_eq!(code, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn models_list_and_recommended_succeed_against_running_server() {
        let server = spawn_test_server(54411).await;
        let code = cmd_models(ModelsCommand::List {
            json: true,
            data_dir: Some(server.data_dir.clone()),
        })
        .await;
        assert_eq!(code, 0);
        let code = cmd_models(ModelsCommand::Recommended {
            json: true,
            data_dir: Some(server.data_dir.clone()),
        })
        .await;
        assert_eq!(code, 0);
        let code = cmd_models(ModelsCommand::Selected {
            json: true,
            data_dir: Some(server.data_dir.clone()),
        })
        .await;
        assert_eq!(code, 0);
    }

    #[tokio::test]
    async fn models_command_against_absent_server_fails_clearly_not_crash() {
        let dir = std::env::temp_dir().join(format!("stt-models-absent-{}", uuid::Uuid::new_v4()));
        let code = cmd_models(ModelsCommand::List {
            json: false,
            data_dir: Some(dir.clone()),
        })
        .await;
        assert_eq!(code, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn models_select_unknown_model_reports_api_error_not_crash() {
        let server = spawn_test_server(54412).await;
        let code = cmd_models(ModelsCommand::Select {
            id: "no-such-model".to_owned(),
            json: true,
            data_dir: Some(server.data_dir.clone()),
        })
        .await;
        assert_eq!(code, 1);
    }

    #[tokio::test]
    async fn models_cancel_unknown_operation_reports_not_found() {
        let server = spawn_test_server(54413).await;
        let code = cmd_models(ModelsCommand::Cancel {
            operation_id: "does-not-exist".to_owned(),
            data_dir: Some(server.data_dir.clone()),
        })
        .await;
        assert_eq!(code, 1);
    }

    #[tokio::test]
    async fn models_refresh_completes_against_an_empty_drop_in_folder() {
        let server = spawn_test_server(54414).await;
        let code = cmd_models(ModelsCommand::Refresh {
            wait: true,
            json: true,
            data_dir: Some(server.data_dir.clone()),
        })
        .await;
        // Empty (unconfigured) drop-in folder scans to zero results and
        // completes successfully.
        assert_eq!(code, 0);
    }

    /// `cmd_status` (like every other CLI command) must find the server
    /// through `server.json` rather than assuming any particular port: this
    /// spawns a per-user server whose preferred port is already occupied, so
    /// it falls back to an OS-assigned one (see `api::run_http_full`'s
    /// `port_fallback_tests`), then checks `status` still reports it as
    /// running by reading the port that was actually recorded.
    #[tokio::test]
    async fn status_discovers_a_fallback_port_not_the_busy_preferred_one() {
        let busy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let busy_port = busy_listener.local_addr().unwrap().port();

        let data_dir =
            std::env::temp_dir().join(format!("stt-status-fallback-test-{}", uuid::Uuid::new_v4()));
        // Persist the busy port as a stored setting (not an explicit
        // `--port`) so fallback stays eligible: `run_http_full` only falls
        // back when `bind_overrides.port` is `None`.
        {
            let app = app::open_app_at(data_dir.clone()).unwrap();
            app.db
                .lock()
                .unwrap()
                .execute(
                    "INSERT INTO settings(key,value) VALUES(?1,?2)",
                    rusqlite::params![
                        stt_server_next::store::SETTING_BIND_PORT,
                        busy_port.to_string()
                    ],
                )
                .unwrap();
            drop(app);
        }

        let dir_for_task = data_dir.clone();
        let task = tokio::spawn(async move {
            let overrides = app::BindOverrides {
                host: Some("127.0.0.1".to_owned()),
                port: None,
                cors_origins: None,
            };
            // A machine-wide install would fail clearly instead; PerUser is
            // what's eligible to fall back here.
            let _ = stt_server_next::api::run_http_full(
                dir_for_task,
                Default::default(),
                overrides,
                None,
                app::InstallScope::PerUser,
                std::future::pending::<()>(),
            )
            .await;
        });

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let info = loop {
            if let Some(info) = discovery::read_server_json(&data_dir) {
                break info;
            }
            if std::time::Instant::now() >= deadline {
                task.abort();
                let _ = std::fs::remove_dir_all(&data_dir);
                panic!("server never wrote server.json");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        assert_ne!(
            info.port, busy_port,
            "must have fallen back off the busy preferred port"
        );
        drop(busy_listener);

        let code = cmd_status(data_dir.clone(), true).await;
        assert_eq!(code, 0, "status must find the server via server.json");

        task.abort();
        let _ = std::fs::remove_dir_all(&data_dir);
    }
}
