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

/// Post to an install/update endpoint and, if the server kicked off a
/// background download (`202 Accepted`, `status: "downloading"`), poll
/// `/v1/install-operations/:id` to completion — the old synchronous "block
/// until done" UX layered on top of the new non-blocking HTTP API.
async fn install_and_wait(client: &Client, path: &str, variant: &str) -> anyhow::Result<Value> {
    let response = client
        .post(path, Some(json!({ "variant": variant })))
        .await?;

    if response.get("status").and_then(Value::as_str) != Some("downloading") {
        return Ok(response);
    }

    let operation_id = response
        .get("operationId")
        .and_then(Value::as_str)
        .context("server response was missing operationId for a downloading install")?
        .to_string();

    eprintln!("downloading {variant} build...");
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
                    .unwrap_or("download failed");
                anyhow::bail!("install failed: {error}");
            }
            other => anyhow::bail!("unexpected install operation status: {other}"),
        }
    }
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
