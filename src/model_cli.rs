//! HTTP-calling logic backing the `models`/`health` CLI commands
//! (`src/cli.rs`'s `ModelsCommand`, dispatched from `src/bin/server.rs`).
//!
//! This module owns *no* business logic: every call here talks to the
//! already-running server's existing authenticated API (`src/api.rs`,
//! `src/import.rs`, `src/operations.rs`) exactly the way `cmd_stop` in
//! `src/bin/server.rs` already does (discover via `server.json`, confirm
//! liveness with `/health`, read the bearer token from `auth.token`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use crate::discovery;

/// Interval between `GET /v1/local/operations/{id}` polls under `--wait`.
pub const POLL_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug)]
pub enum ConnectError {
    NotRunning,
    TokenUnreadable(std::io::Error),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::NotRunning => write!(
                f,
                "server is not running for this data directory (start it with `stt-server-next start`)"
            ),
            ConnectError::TokenUnreadable(error) => {
                write!(f, "could not read the auth token: {error}")
            }
        }
    }
}

pub struct Connection {
    pub base_url: String,
    token: String,
    client: reqwest::Client,
}

/// A wildcard bind is not itself connectable; probe loopback instead (the
/// server always also accepts loopback connections). Mirrors
/// `server.rs::probe_host`; kept independent here so this module has no
/// dependency on the binary crate.
fn probe_host(host: &str) -> String {
    match host {
        "0.0.0.0" | "::" => "127.0.0.1".to_owned(),
        other => other.to_owned(),
    }
}

/// Reads this data dir's bearer token, preferring the admin token
/// (`auth.token`) and falling back to the user token (`user.token`) if the
/// admin token can't be read -- e.g. an ordinary local user on a
/// machine-wide install, whose ACL (see `service::install`) grants
/// `auth.token` only to SYSTEM/Administrators. A command that needs admin
/// access still runs, just with a user-level token that the server then
/// rejects with `403 admin_required` (see `format_error`'s hint for that
/// code) rather than the CLI failing before ever reaching the server.
fn read_token(data_dir: &Path) -> Result<String, ConnectError> {
    match std::fs::read_to_string(data_dir.join("auth.token")) {
        Ok(token) => Ok(token),
        Err(admin_error) => std::fs::read_to_string(data_dir.join("user.token"))
            .map_err(|_| ConnectError::TokenUnreadable(admin_error)),
    }
}

/// Discovers the running server, confirms it answers `/health`, and reads
/// its bearer token -- the same three steps `cmd_stop`/`cmd_status` perform
/// before touching the authenticated API.
pub async fn connect(data_dir: &Path) -> Result<Connection, ConnectError> {
    let info = discovery::read_server_json(data_dir).ok_or(ConnectError::NotRunning)?;
    if !discovery::pid_is_alive(info.pid) {
        return Err(ConnectError::NotRunning);
    }
    let host = probe_host(&info.host);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|_| ConnectError::NotRunning)?;
    let health_url = format!("http://{host}:{}/health", info.port);
    let healthy = client
        .get(&health_url)
        .timeout(Duration::from_secs(3))
        .send()
        .await
        .ok()
        .map(|response| response.status().is_success())
        .unwrap_or(false);
    if !healthy {
        return Err(ConnectError::NotRunning);
    }
    let token = read_token(data_dir)?;
    Ok(Connection {
        base_url: format!("http://{host}:{}", info.port),
        token: token.trim().to_owned(),
        client,
    })
}

/// The result of one HTTP call: status code plus parsed JSON body (`Value::Null`
/// when the body was empty or not JSON).
pub struct ApiOutcome {
    pub status: u16,
    pub body: Value,
}

impl ApiOutcome {
    pub fn is_error(&self) -> bool {
        self.status >= 400
    }
}

pub async fn call(
    conn: &Connection,
    method: reqwest::Method,
    path: &str,
    json_body: Option<Value>,
) -> Result<ApiOutcome, String> {
    let mut builder = conn
        .client
        .request(method, format!("{}{path}", conn.base_url))
        .bearer_auth(&conn.token);
    if let Some(body) = json_body {
        builder = builder.json(&body);
    }
    let response = builder
        .send()
        .await
        .map_err(|error| format!("request to {path} failed: {error}"))?;
    let status = response.status().as_u16();
    let body = response.json::<Value>().await.unwrap_or(Value::Null);
    Ok(ApiOutcome { status, body })
}

/// Renders the `{"error": {"code", "message", "details"}}` envelope
/// (`docs/client-contract.md` section 6) into a single readable line, with a
/// short actionable hint for the codes an operator hits most often.
pub fn format_error(outcome: &ApiOutcome) -> String {
    let Some(error) = outcome.body.get("error") else {
        return format!("HTTP {}: {}", outcome.status, outcome.body);
    };
    let code = error.get("code").and_then(Value::as_str).unwrap_or("");
    let message = error.get("message").and_then(Value::as_str).unwrap_or("");
    let hint: &str = match code {
        "needs_verification" => {
            " -- run `models verify <id>` (or `models refresh` for a drop-in model), then select again"
        }
        "already_installed" => " -- the model is already installed",
        "operation_conflict" => " -- an operation for this model is already running; use `models cancel <operation_id>` or wait",
        "model_not_installed" => " -- install it first with `models install <id>`",
        "model_in_use" => " -- unload it first with `models unload`",
        "model_not_found" | "operation_not_found" => " -- check the id",
        "server_not_ready" => " -- no model is loaded yet; select one, or check for an in-progress install/verify",
        "admin_required" => " -- admin access required (run as administrator)",
        _ => "",
    };
    if code.is_empty() {
        format!("HTTP {}: {message}", outcome.status)
    } else {
        format!("{code}: {message}{hint}")
    }
}

/// Polls `GET /v1/local/operations/{id}` on [`POLL_INTERVAL`] until it
/// reaches a terminal state (`completed`/`failed`/`cancelled`), printing
/// progress lines unless `json` (in which case only the final body matters
/// to the caller). Returns the last-seen operation body.
pub async fn poll_operation(
    conn: &Connection,
    operation_id: &str,
    json: bool,
) -> Result<Value, String> {
    loop {
        let path = format!("/v1/local/operations/{operation_id}");
        let outcome = call(conn, reqwest::Method::GET, &path, None).await?;
        if outcome.is_error() {
            return Err(format_error(&outcome));
        }
        let state = outcome
            .body
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("");
        if !json {
            print_progress_line(&outcome.body, state);
        }
        if matches!(state, "completed" | "failed" | "cancelled") {
            return Ok(outcome.body);
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

fn print_progress_line(body: &Value, state: &str) {
    let progress_bytes = body.get("progress_bytes").and_then(Value::as_u64);
    let total_bytes = body.get("total_bytes").and_then(Value::as_u64);
    let progress_items = body.get("progress_items").and_then(Value::as_u64);
    let total_items = body.get("total_items").and_then(Value::as_u64);
    match (progress_bytes, total_bytes, progress_items, total_items) {
        (Some(pb), Some(tb), _, _) if tb > 0 => {
            println!("{state}: {pb}/{tb} bytes");
        }
        (_, _, Some(pi), Some(ti)) if ti > 0 => {
            println!("{state}: {pi}/{ti} items");
        }
        _ => println!("{state}"),
    }
}

/// Streams a local file into `POST /v1/local/models/import` as
/// `multipart/form-data` with the exact field shape `src/import.rs` expects:
/// a `model` text field, an optional `quant` text field, then a `file` part
/// -- `model` (and `quant`, if present) must arrive before `file` since the
/// server reads the stream field-by-field and needs the id to validate the
/// catalog entry and size cap before it starts writing bytes.
pub async fn import_model(
    conn: &Connection,
    path: &PathBuf,
    model: &str,
    quant: Option<&str>,
) -> Result<ApiOutcome, String> {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "model.gguf".to_owned());
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let mut form = reqwest::multipart::Form::new().text("model", model.to_owned());
    if let Some(quant) = quant {
        form = form.text("quant", quant.to_owned());
    }
    form = form.part(
        "file",
        reqwest::multipart::Part::bytes(bytes).file_name(file_name),
    );
    let response = conn
        .client
        .post(format!("{}/v1/local/models/import", conn.base_url))
        .bearer_auth(&conn.token)
        .multipart(form)
        .send()
        .await
        .map_err(|error| format!("import request failed: {error}"))?;
    let status = response.status().as_u16();
    let body = response.json::<Value>().await.unwrap_or(Value::Null);
    Ok(ApiOutcome { status, body })
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("stt-cli-token-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The CLI must prefer `auth.token` (the admin token) when it can be
    /// read, per "CLI: prefer auth.token if readable, else fall back to
    /// user.token" -- otherwise an admin's own CLI on a machine-wide
    /// install would silently run at user level.
    #[test]
    fn read_token_prefers_admin_token_when_both_exist() {
        let dir = temp_dir();
        std::fs::write(dir.join("auth.token"), "a".repeat(64)).unwrap();
        std::fs::write(dir.join("user.token"), "u".repeat(64)).unwrap();
        assert_eq!(read_token(&dir).unwrap(), "a".repeat(64));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Falls back to `user.token` when `auth.token` can't be read at all
    /// (missing here; on a real machine-wide install this is instead an
    /// ACL-denied read for a non-admin local user).
    #[test]
    fn read_token_falls_back_to_user_token_when_admin_token_is_unreadable() {
        let dir = temp_dir();
        std::fs::write(dir.join("user.token"), "u".repeat(64)).unwrap();
        assert_eq!(read_token(&dir).unwrap(), "u".repeat(64));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Neither token present/readable: surfaces the admin-token read error,
    /// not a generic failure, so the CLI's error message stays useful.
    #[test]
    fn read_token_errors_when_neither_token_is_readable() {
        let dir = temp_dir();
        let result = read_token(&dir);
        assert!(matches!(result, Err(ConnectError::TokenUnreadable(_))));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
