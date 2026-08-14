use std::sync::Arc;
use std::time::Duration;

use clap::Args;

use stt_runtime::{providers::faster_whisper, ProviderId, RuntimeManager, RuntimeVariant};

#[derive(Args)]
pub struct RunArgs {
    /// Host to bind to (must be loopback in V1)
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,

    /// Port to listen on
    #[arg(short, long, default_value_t = 8080)]
    pub port: u16,

    /// Path to model directory
    #[arg(long)]
    pub model_dir: Option<String>,

    /// Default model identifier
    #[arg(long)]
    pub default_model: Option<String>,

    /// Maximum concurrent sessions
    #[arg(long, default_value_t = 16)]
    pub max_sessions: usize,

    /// Log level
    #[arg(long, default_value = "info")]
    pub log_level: String,

    /// Seconds a managed runtime may sit idle (no start/status/heartbeat)
    /// before it's stopped automatically. 0 disables idle auto-stop.
    #[arg(long, default_value_t = 600)]
    pub idle_timeout_secs: u64,

    /// Explicit opt-in to bind a non-loopback host. Loopback is the default
    /// and needs neither this nor an auth token.
    #[arg(long, default_value_t = false)]
    pub allow_remote: bool,

    /// Required alongside --allow-remote: clients must send it as
    /// `Authorization: Bearer <token>`.
    #[arg(long)]
    pub auth_token: Option<String>,
}

pub async fn execute(args: RunArgs) -> anyhow::Result<()> {
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
        allow_remote: args.allow_remote,
        auth_token: args.auth_token,
    };

    let idle_timeout =
        (args.idle_timeout_secs > 0).then(|| Duration::from_secs(args.idle_timeout_secs));
    let runtime_manager = Arc::new(RuntimeManager::new(idle_timeout));

    // Best-effort: register whichever catalog providers are actually
    // installed locally (dev source, or a previously-downloaded/cached
    // variant). A provider that isn't found yet doesn't block startup — it
    // just isn't start-able until `POST /v1/providers/:id/install` (or the
    // equivalent CLI command) succeeds. `install_local` is network-free, so
    // this never delays startup waiting on a download.
    let faster_whisper_id = ProviderId::new("faster-whisper")?;
    let preferred_variant = if runtime_manager.hardware().has_nvidia_gpu {
        RuntimeVariant::Gpu
    } else {
        RuntimeVariant::Cpu
    };
    match faster_whisper::install_local(preferred_variant) {
        Some(launch) => {
            runtime_manager
                .register_install(&faster_whisper_id, launch)
                .await;
            tracing::info!(variant = %preferred_variant, "faster-whisper runtime found and registered");
        }
        None => {
            tracing::warn!(
                "faster-whisper runtime not available locally yet (tried {preferred_variant} variant); \
                 install it via `stt provider install faster-whisper` or `POST /v1/providers/faster-whisper/install`"
            );
        }
    }

    stt_server::run_server(config, runtime_manager).await
}
