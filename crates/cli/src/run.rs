use clap::Args;

#[derive(Args)]
pub struct RunArgs {
    /// Host to bind to (must be loopback in V1)
    #[arg(short, long, default_value = "127.0.0.1")]
    pub host: String,

    /// Port to listen on
    #[arg(short, long, default_value_t = 8080)]
    pub port: u16,

    /// Path to model directory
    #[arg(short, long)]
    pub model_dir: Option<String>,

    /// Default model identifier
    #[arg(short = 'm', long)]
    pub default_model: Option<String>,

    /// Maximum concurrent sessions
    #[arg(long, default_value_t = 16)]
    pub max_sessions: usize,

    /// Log level
    #[arg(long, default_value = "info")]
    pub log_level: String,
}

pub async fn execute(args: RunArgs) -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&args.log_level)),
        )
        .init();

    let model_dir = args
        .model_dir
        .map(std::path::PathBuf::from)
        .unwrap_or_else(stt_common::default_model_dir);

    let config = stt_common::ServerConfig {
        host: args.host,
        port: args.port,
        model_dir,
        default_model: args.default_model,
        max_sessions: args.max_sessions,
        log_level: args.log_level,
    };

    config.validate()?;

    // Use mock adapter for V1 (real adapter behind feature flag)
    let adapter = stt_adapter::mock::MockAdapter::new();

    // Scan model directory and register models
    if config.model_dir.exists() {
        for entry in std::fs::read_dir(&config.model_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().map_or(false, |e| e == "bin" || e == "gguf") {
                let name = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("unknown")
                    .to_string();
                let id = name.clone();
                adapter.register_model(&id, &name, path).await;
                tracing::info!("Registered model: {id}");
            }
        }
    }

    stt_server::run_server(config, adapter).await
}
