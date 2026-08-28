use anyhow::Context;
use clap::Subcommand;
use serde_json::{json, Value};

use crate::client::Client;

#[derive(Subcommand)]
pub enum ProviderCommands {
    /// List the curated provider catalog and hardware compatibility
    List,
    /// Confirm/register a provider's artifact with the running daemon,
    /// downloading it first if it isn't already local (blocks until done)
    Install {
        provider_id: String,
        /// Which build to install; default "cpu" keeps today's instant,
        /// network-free behavior when nothing else is specified
        #[arg(long, default_value = "cpu")]
        variant: String,
    },
    /// Re-confirm/re-register (alias of install until versioned releases exist)
    Update {
        provider_id: String,
        #[arg(long, default_value = "cpu")]
        variant: String,
    },
    /// Remove a downloaded variant's cached copy, reclaiming disk space
    /// (never touches a vendored dev copy)
    RemoveVariant {
        provider_id: String,
        #[arg(long)]
        variant: String,
    },
    /// Remove a provider's install registration, stopping it first if running
    Remove { provider_id: String },
    /// Start a provider, blocking until healthy, and print its connection descriptor
    Start {
        provider_id: String,
        /// Explicit device hint (e.g. "cpu", "cuda"); default lets the runtime auto-detect
        #[arg(long)]
        device: Option<String>,
        /// Explicit compute-type hint (e.g. "int8", "float16"); default lets the runtime auto-detect
        #[arg(long = "compute-type")]
        compute_type: Option<String>,
        /// Bind the managed runtime to "0.0.0.0" instead of loopback (LAN
        /// mode). Requires --auth-token, and the daemon itself must have
        /// been started with --allow-remote.
        #[arg(long = "bind-host")]
        bind_host: Option<String>,
        /// Required alongside --bind-host: the token the managed runtime
        /// will require on every request.
        #[arg(long = "auth-token")]
        auth_token: Option<String>,
    },
    /// Stop a running provider
    Stop { provider_id: String },
    /// Print a provider's current runtime status
    Status { provider_id: String },
    /// Print a running provider's recent log lines
    Logs {
        provider_id: String,
        #[arg(long, default_value_t = 100)]
        tail: usize,
    },
}

#[derive(Subcommand)]
pub enum ModelCommands {
    /// List the curated model catalog across all providers
    List,
    /// Select which model a provider loads on its next start
    Select {
        #[arg(long)]
        provider: String,
        #[arg(long)]
        model: String,
    },
    /// Print the currently selected model for a provider
    Selected {
        #[arg(long)]
        provider: String,
    },
    /// Download a model's weights into stt-server's own structured model
    /// directory (blocks until done). Requires a provider variant to
    /// already be installed.
    Pull {
        #[arg(long)]
        provider: String,
        #[arg(long)]
        model: String,
    },
    /// Confirm a model's weights are actually present on disk
    Verify {
        #[arg(long)]
        provider: String,
        #[arg(long)]
        model: String,
    },
    /// Delete a model's cached weights, reclaiming disk space
    Remove {
        #[arg(long)]
        provider: String,
        #[arg(long)]
        model: String,
    },
}

fn print_json(value: &serde_json::Value) {
    println!("{}", serde_json::to_string_pretty(value).unwrap());
}

pub fn hardware() -> anyhow::Result<()> {
    print_json(&serde_json::to_value(stt_runtime::hardware::detect())?);
    Ok(())
}

pub fn recommend() -> anyhow::Result<()> {
    let hardware = stt_runtime::hardware::detect();
    print_json(&serde_json::to_value(stt_runtime::recommend(&hardware))?);
    Ok(())
}

/// Wipe every on-disk artifact stt-server manages (cached provider
/// binaries and downloaded model weights) in one pure filesystem
/// operation — deliberately independent of a running `stt run` daemon
/// (calls `stt_common::purge_all_local_state` directly, never `Client`),
/// so an installer's uninstall hook or a "clean-slate test" workflow can
/// use it without spinning one up first. Destructive, so it requires
/// `--yes` rather than acting on a bare `stt reset`.
pub fn reset(yes: bool) -> anyhow::Result<()> {
    let root = stt_common::resolved_data_root();
    if !yes {
        anyhow::bail!(
            "this deletes {} and everything under it (cached provider binaries and \
             downloaded model weights) — re-run with --yes to confirm",
            root.display()
        );
    }
    let removed = stt_common::purge_all_local_state()?;
    println!("removed {}", removed.display());
    Ok(())
}

pub fn model_list() -> anyhow::Result<()> {
    #[derive(serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    struct ModelInfo {
        id: &'static str,
        display_name: &'static str,
        provider_id: &'static str,
    }
    let models: Vec<ModelInfo> = stt_runtime::CATALOG
        .iter()
        .flat_map(|entry| {
            entry.models.iter().map(move |m| ModelInfo {
                id: m.id,
                display_name: m.display_name,
                provider_id: entry.id,
            })
        })
        .collect();
    print_json(&serde_json::to_value(models)?);
    Ok(())
}

pub fn provider_list() -> anyhow::Result<()> {
    let hardware = stt_runtime::hardware::detect();
    print_json(&serde_json::to_value(
        stt_runtime::catalog::list_providers(&hardware),
    )?);
    Ok(())
}

/// Poll `/v1/install-operations/:id` to completion. Shared by provider
/// variant installs and model pulls — both are tracked through the same
/// `InstallOperationState` table server-side, so one polling loop works for
/// either kind; only the initial-response handling and `label` printed
/// while waiting differ between callers.
async fn wait_for_operation(
    client: &Client,
    response: Value,
    label: &str,
) -> anyhow::Result<Value> {
    if response.get("status").and_then(Value::as_str) != Some("downloading") {
        return Ok(response);
    }

    let operation_id = response
        .get("operationId")
        .and_then(Value::as_str)
        .context("server response was missing operationId for a downloading operation")?
        .to_string();

    eprintln!("downloading {label}...");
    loop {
        let state = client
            .get(&format!("/v1/install-operations/{operation_id}"))
            .await?;
        match state.get("status").and_then(Value::as_str).unwrap_or("") {
            "downloading" => {
                let downloaded = state
                    .get("downloadedBytes")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                match state.get("totalBytes").and_then(Value::as_u64) {
                    Some(total) if total > 0 => eprint!("\r  {downloaded}/{total} bytes"),
                    _ => eprint!("\r  {downloaded} bytes"),
                }
                use std::io::Write;
                std::io::stderr().flush().ok();
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            "complete" => {
                eprintln!();
                return Ok(state);
            }
            "failed" => {
                eprintln!();
                let error = state
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("operation failed");
                anyhow::bail!("operation failed: {error}");
            }
            other => anyhow::bail!("unexpected operation status: {other}"),
        }
    }
}

/// Post to an install/update endpoint and, if the server kicked off a
/// background download (`202 Accepted`, `status: "downloading"`), poll it
/// to completion — the old synchronous "block until done" UX layered on top
/// of the new non-blocking HTTP API.
async fn install_and_wait(client: &Client, path: &str, variant: &str) -> anyhow::Result<Value> {
    let response = client
        .post(path, Some(json!({ "variant": variant })))
        .await?;
    wait_for_operation(client, response, &format!("{variant} build")).await
}

/// `POST /v1/models/pull?provider=&model=` and, if the server kicked off a
/// background download (`202 Accepted`, `status: "downloading"`), poll it to
/// completion — mirrors `install_and_wait` for models.
async fn pull_model_and_wait(
    client: &Client,
    provider: &str,
    model: &str,
) -> anyhow::Result<Value> {
    let path = format!(
        "/v1/models/pull?provider={}&model={}",
        urlencoding_component(provider),
        urlencoding_component(model)
    );
    let response = client.post(&path, None).await?;
    wait_for_operation(client, response, model).await
}

/// Minimal query-string component encoder (no external crate dependency):
/// percent-encodes everything outside the RFC 3986 "unreserved" set. Model
/// ids like `"Systran/faster-whisper-small"` contain `/`, which must be
/// encoded here since it's a query *value*, not a path segment — an
/// unencoded `/` in a query value is harmless to most servers but encoding
/// it is the only fully spec-correct behavior, and provider ids are cheap
/// to run through the same helper for consistency.
fn urlencoding_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

pub async fn provider(client: &Client, cmd: ProviderCommands) -> anyhow::Result<()> {
    match cmd {
        ProviderCommands::List => provider_list()?,
        ProviderCommands::Install {
            provider_id,
            variant,
        } => print_json(
            &install_and_wait(
                client,
                &format!("/v1/providers/{provider_id}/install"),
                &variant,
            )
            .await?,
        ),
        ProviderCommands::Update {
            provider_id,
            variant,
        } => print_json(
            &install_and_wait(
                client,
                &format!("/v1/providers/{provider_id}/update"),
                &variant,
            )
            .await?,
        ),
        ProviderCommands::RemoveVariant {
            provider_id,
            variant,
        } => {
            client
                .delete(&format!("/v1/providers/{provider_id}/install/{variant}"))
                .await?;
            println!("removed {variant} variant of {provider_id}");
        }
        ProviderCommands::Remove { provider_id } => {
            client
                .delete(&format!("/v1/providers/{provider_id}"))
                .await?;
            println!("removed {provider_id}");
        }
        ProviderCommands::Start {
            provider_id,
            device,
            compute_type,
            bind_host,
            auth_token,
        } => {
            let body = if device.is_some()
                || compute_type.is_some()
                || bind_host.is_some()
                || auth_token.is_some()
            {
                Some(json!({
                    "device": device,
                    "computeType": compute_type,
                    "bindHost": bind_host,
                    "authToken": auth_token,
                }))
            } else {
                None
            };
            print_json(
                &client
                    .post(&format!("/v1/providers/{provider_id}/start"), body)
                    .await?,
            )
        }
        ProviderCommands::Stop { provider_id } => {
            client
                .post(&format!("/v1/providers/{provider_id}/stop"), None)
                .await?;
            println!("stopped {provider_id}");
        }
        ProviderCommands::Status { provider_id } => print_json(
            &client
                .get(&format!("/v1/providers/{provider_id}/status"))
                .await?,
        ),
        ProviderCommands::Logs { provider_id, tail } => print_json(
            &client
                .get(&format!("/v1/providers/{provider_id}/logs?tail={tail}"))
                .await?,
        ),
    }
    Ok(())
}

pub async fn model(client: &Client, cmd: ModelCommands) -> anyhow::Result<()> {
    match cmd {
        ModelCommands::List => model_list()?,
        ModelCommands::Select { provider, model } => {
            client
                .post(
                    "/v1/models/select",
                    Some(json!({ "providerId": provider, "modelId": model })),
                )
                .await?;
            println!("selected {model} for {provider}");
        }
        ModelCommands::Selected { provider } => print_json(
            &client
                .get(&format!("/v1/models/selected?provider={provider}"))
                .await?,
        ),
        ModelCommands::Pull { provider, model } => {
            print_json(&pull_model_and_wait(client, &provider, &model).await?)
        }
        ModelCommands::Verify { provider, model } => print_json(
            &client
                .post(
                    &format!(
                        "/v1/models/verify?provider={}&model={}",
                        urlencoding_component(&provider),
                        urlencoding_component(&model)
                    ),
                    None,
                )
                .await?,
        ),
        ModelCommands::Remove { provider, model } => {
            client
                .delete(&format!(
                    "/v1/models/remove?provider={}&model={}",
                    urlencoding_component(&provider),
                    urlencoding_component(&model)
                ))
                .await?;
            println!("removed {model} for {provider}");
        }
    }
    Ok(())
}

pub async fn descriptor(client: &Client, provider_id: &str) -> anyhow::Result<()> {
    print_json(
        &client
            .get(&format!("/v1/providers/{provider_id}/descriptor"))
            .await?,
    );
    Ok(())
}
