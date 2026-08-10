use clap::{Parser, Subcommand};
use stt_adapter::EngineAdapter;

mod run;

#[derive(Parser)]
#[command(name = "stt")]
#[command(about = "stt-server CLI - Speech-to-Text server management")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the STT server
    Run(run::RunArgs),
    /// Manage models
    Model {
        #[command(subcommand)]
        command: ModelCommands,
    },
}

#[derive(Subcommand)]
enum ModelCommands {
    /// Pull/download a model
    Pull {
        /// Model identifier to pull
        model_id: String,
        /// Optional output directory
        #[arg(short, long)]
        dir: Option<String>,
    },
    /// List available models
    List {
        /// Show only loaded models
        #[arg(short, long)]
        loaded: bool,
    },
    /// Remove a model
    Remove {
        /// Model identifier to remove
        model_id: String,
    },
    /// Select a loaded model as default
    Select {
        /// Model identifier to select
        model_id: String,
    },
    /// Verify a model file
    Verify {
        /// Path to model file
        path: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Run(args) => run::execute(args).await,
        Commands::Model { command } => match command {
            ModelCommands::Pull { model_id, dir } => {
                model_pull(&model_id, dir.as_deref()).await
            }
            ModelCommands::List { loaded } => model_list(loaded).await,
            ModelCommands::Remove { model_id } => model_remove(&model_id).await,
            ModelCommands::Select { model_id } => model_select(&model_id).await,
            ModelCommands::Verify { path } => model_verify(&path).await,
        },
    }
}

async fn model_pull(model_id: &str, _dir: Option<&str>) -> anyhow::Result<()> {
    println!("Pulling model: {model_id}");
    println!("Note: Model download not yet implemented. Place model files in the model directory.");
    Ok(())
}

async fn model_list(loaded_only: bool) -> anyhow::Result<()> {
    let adapter = stt_adapter::mock::MockAdapter::new();
    let models = adapter.list_models().await?;

    if models.is_empty() {
        println!("No models registered.");
        return Ok(());
    }

    for model in &models {
        if loaded_only && !model.loaded {
            continue;
        }
        let status = if model.loaded { "loaded" } else { "available" };
        println!(
            "  {} [{}] {} - {}",
            model.id,
            status,
            model.name,
            model
                .size_bytes
                .map(|s| format!("{} bytes", s))
                .unwrap_or_else(|| "unknown size".into())
        );
    }

    Ok(())
}

async fn model_remove(model_id: &str) -> anyhow::Result<()> {
    println!("Removing model: {model_id}");
    println!("Note: Model removal from disk not yet implemented.");
    Ok(())
}

async fn model_select(model_id: &str) -> anyhow::Result<()> {
    println!("Selecting model: {model_id}");
    println!("Note: Model selection not yet implemented for standalone CLI.");
    Ok(())
}

async fn model_verify(path: &str) -> anyhow::Result<()> {
    let path = std::path::Path::new(path);
    let adapter = stt_adapter::mock::MockAdapter::new();
    let result = adapter.verify_model(path).await?;

    if result.valid {
        println!("Model is valid.");
        if let Some(checksum) = &result.checksum {
            println!("Checksum: {checksum}");
        }
    } else {
        println!(
            "Model verification failed: {}",
            result.error.unwrap_or_else(|| "unknown error".into())
        );
    }

    Ok(())
}
