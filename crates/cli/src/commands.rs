use clap::Subcommand;
use serde_json::json;

use crate::client::Client;

#[derive(Subcommand)]
pub enum ProviderCommands {
    /// List the curated provider catalog and hardware compatibility
    List,
    /// Confirm/register a provider's artifact with the running daemon
    Install { provider_id: String },
    /// Re-confirm/re-register (alias of install until versioned releases exist)
    Update { provider_id: String },
    /// Remove a provider's install registration, stopping it first if running
    Remove { provider_id: String },
    /// Start a provider, blocking until healthy, and print its connection descriptor
    Start { provider_id: String },
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

pub async fn provider(client: &Client, cmd: ProviderCommands) -> anyhow::Result<()> {
    match cmd {
        ProviderCommands::List => provider_list()?,
        ProviderCommands::Install { provider_id } => print_json(
            &client
                .post(&format!("/v1/providers/{provider_id}/install"), None)
                .await?,
        ),
        ProviderCommands::Update { provider_id } => print_json(
            &client
                .post(&format!("/v1/providers/{provider_id}/update"), None)
                .await?,
        ),
        ProviderCommands::Remove { provider_id } => {
            client
                .delete(&format!("/v1/providers/{provider_id}"))
                .await?;
            println!("removed {provider_id}");
        }
        ProviderCommands::Start { provider_id } => print_json(
            &client
                .post(&format!("/v1/providers/{provider_id}/start"), None)
                .await?,
        ),
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
