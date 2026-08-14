/// Errors from provider/model catalog, install, and process-supervision
/// operations. HTTP-layer mapping to structured error responses happens
/// where these are consumed (the control-plane routes), not here.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("invalid provider ID: {0}")]
    InvalidProviderId(String),

    #[error("provider not found: {0}")]
    ProviderNotFound(String),

    #[error("provider not installed: {0}")]
    ProviderNotInstalled(String),

    #[error("model not found for provider: {0}")]
    ModelNotFound(String),

    #[error("runtime not running: {0}")]
    RuntimeNotRunning(String),

    #[error("runtime failed to start: {0}")]
    RuntimeStartFailed(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("download failed: {0}")]
    DownloadFailed(String),

    #[error("unsupported variant: {0}")]
    UnsupportedVariant(String),

    #[error("install operation not found: {0}")]
    InstallOperationNotFound(String),

    #[error("invalid start options: {0}")]
    InvalidStartOptions(String),

    #[error("internal error: {0}")]
    Internal(String),
}
