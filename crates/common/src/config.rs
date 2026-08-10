use std::path::PathBuf;

use crate::SttError;

/// Server configuration.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Host to bind to (must be loopback in V1).
    pub host: String,
    /// Port to listen on.
    pub port: u16,
    /// Path to model directory.
    pub model_dir: PathBuf,
    /// Default model identifier.
    pub default_model: Option<String>,
    /// Maximum concurrent transcription sessions.
    pub max_sessions: usize,
    /// Log level.
    pub log_level: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 8080,
            model_dir: default_model_dir(),
            default_model: None,
            max_sessions: 16,
            log_level: "info".into(),
        }
    }
}

impl ServerConfig {
    /// Validate the configuration. V1 rejects non-loopback binding.
    pub fn validate(&self) -> Result<(), SttError> {
        let allowed_hosts = ["127.0.0.1", "::1", "localhost"];
        if !allowed_hosts.contains(&self.host.as_str()) {
            return Err(SttError::ConfigError(format!(
                "V1 only allows loopback binding (127.0.0.1, ::1, localhost); got: {}",
                self.host
            )));
        }
        if self.port == 0 {
            return Err(SttError::ConfigError("port cannot be 0".into()));
        }
        if self.max_sessions == 0 {
            return Err(SttError::ConfigError("max_sessions cannot be 0".into()));
        }
        Ok(())
    }

    /// Build the listen address string.
    pub fn listen_addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// Default model directory based on platform.
pub fn default_model_dir() -> PathBuf {
    dirs_or_fallback()
        .join("stt-server")
        .join("models")
}

fn dirs_or_fallback() -> PathBuf {
    dirs::data_local_dir().unwrap_or_else(|| PathBuf::from("."))
}
