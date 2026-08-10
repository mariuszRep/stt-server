use clap::{Parser, Subcommand};
use stt_adapter::EngineAdapter;

#[derive(Parser)]
#[command(name = "stt-server")]
#[command(about = "Self-hosted STT server")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Host to bind to
    #[arg(short, long, default_value = "127.0.0.1")]
    host: String,

    /// Port to listen on
    #[arg(short, long, default_value_t = 8080)]
    port: u16,

    /// Path to model directory
    #[arg(short, long)]
    model_dir: Option<String>,

    /// Default model identifier
    #[arg(short = 'm', long)]
    default_model: Option<String>,

    /// Log level
    #[arg(long, default_value = "info")]
    log_level: String,
}

#[derive(Subcommand)]
enum Commands {
    /// Run model management commands
    Model {
        #[command(subcommand)]
        command: ModelCommands,
    },
}

#[derive(Subcommand)]
enum ModelCommands {
    /// List available models
    List,
    /// Verify a model file
    Verify { path: String },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Model { command }) => match command {
            ModelCommands::List => {
                let adapter = stt_adapter::mock::MockAdapter::new();
                let models = adapter.list_models().await?;
                if models.is_empty() {
                    println!("No models registered.");
                } else {
                    for model in &models {
                        let status = if model.loaded { "loaded" } else { "available" };
                        println!("  {} [{}] {}", model.id, status, model.name);
                    }
                }
                Ok(())
            }
            ModelCommands::Verify { path } => {
                let adapter = stt_adapter::mock::MockAdapter::new();
                let result = adapter
                    .verify_model(std::path::Path::new(&path))
                    .await?;
                if result.valid {
                    println!("Model is valid.");
                } else {
                    println!(
                        "Model verification failed: {}",
                        result.error.unwrap_or_else(|| "unknown error".into())
                    );
                }
                Ok(())
            }
        },
        None => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| {
                            tracing_subscriber::EnvFilter::new(&cli.log_level)
                        }),
                )
                .init();

            let model_dir = cli
                .model_dir
                .map(std::path::PathBuf::from)
                .unwrap_or_else(stt_common::default_model_dir);

            let config = stt_common::ServerConfig {
                host: cli.host,
                port: cli.port,
                model_dir,
                default_model: cli.default_model,
                max_sessions: 16,
                log_level: cli.log_level,
            };

            config.validate()?;

            let adapter = stt_adapter::mock::MockAdapter::new();

            // Scan model directory
            if config.model_dir.exists() {
                for entry in std::fs::read_dir(&config.model_dir)? {
                    let entry = entry?;
                    let path = entry.path();
                    if path
                        .extension()
                        .map_or(false, |e| e == "bin" || e == "gguf")
                    {
                        let name = path
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or("unknown")
                            .to_string();
                        adapter.register_model(&name, &name, path).await;
                        tracing::info!("Registered model: {name}");
                    }
                }
            }

            stt_server::run_server(config, adapter).await
        }
    }
}
