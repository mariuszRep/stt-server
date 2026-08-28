use clap::{Parser, Subcommand};

mod client;
mod commands;
mod run;

use client::Client;
use commands::{ModelCommands, ProviderCommands};

#[derive(Parser)]
#[command(name = "stt")]
#[command(about = "Local STT control plane")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Base URL of a running `stt run` daemon, used by subcommands that
    /// need its live state (provider/runtime lifecycle, model selection).
    #[arg(long, global = true, default_value = "http://127.0.0.1:8080")]
    server_url: String,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the control-plane server
    Run(run::RunArgs),
    /// Print detected local hardware capability
    Hardware,
    /// Provider catalog, install, and lifecycle
    Provider {
        #[command(subcommand)]
        command: ProviderCommands,
    },
    /// Model catalog and per-provider selection
    Model {
        #[command(subcommand)]
        command: ModelCommands,
    },
    /// Print the recommended provider/model/device for this machine
    Recommend,
    /// Print a running provider's connection descriptor
    Descriptor { provider_id: String },
    /// Wipe every on-disk artifact stt-server manages (cached provider
    /// binaries, downloaded model weights) — a pure filesystem operation,
    /// no running daemon required
    Reset {
        /// Required to actually delete anything; omitting it prints what
        /// would be removed instead
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let client = Client::new(cli.server_url);

    match cli.command {
        Commands::Run(args) => run::execute(args).await,
        Commands::Hardware => commands::hardware(),
        Commands::Provider { command } => commands::provider(&client, command).await,
        Commands::Model { command } => commands::model(&client, command).await,
        Commands::Recommend => commands::recommend(),
        Commands::Descriptor { provider_id } => commands::descriptor(&client, &provider_id).await,
        Commands::Reset { yes } => commands::reset(yes),
    }
}
