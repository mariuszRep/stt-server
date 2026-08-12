use std::path::PathBuf;

use crate::SttError;

const LOOPBACK_HOSTS: [&str; 3] = ["127.0.0.1", "::1", "localhost"];

/// Server configuration.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Host to bind to. Loopback by default; a non-loopback value requires
    /// `allow_remote` and `auth_token` to both be set (CONVENTIONS.md:
    /// "Loopback is default; remote binding is explicit and authenticated").
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
    /// Explicit opt-in required to bind a non-loopback host.
    pub allow_remote: bool,
    /// Required (and required to be non-empty) when binding non-loopback;
    /// callers must present it as `Authorization: Bearer <token>`.
    pub auth_token: Option<String>,
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
            allow_remote: false,
            auth_token: None,
        }
    }
}

impl ServerConfig {
    /// Validate the configuration. Non-loopback binding is rejected unless
    /// both `allow_remote` is set and a non-empty `auth_token` is present.
    pub fn validate(&self) -> Result<(), SttError> {
        let is_loopback = LOOPBACK_HOSTS.contains(&self.host.as_str());
        if !is_loopback {
            if !self.allow_remote {
                return Err(SttError::ConfigError(format!(
                    "non-loopback host '{}' requires --allow-remote to be set explicitly",
                    self.host
                )));
            }
            if self.auth_token.as_deref().unwrap_or("").is_empty() {
                return Err(SttError::ConfigError(
                    "non-loopback binding requires a non-empty --auth-token".into(),
                ));
            }
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
    dirs_or_fallback().join("stt-server").join("models")
}

fn dirs_or_fallback() -> PathBuf {
    dirs::data_local_dir().unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_binding_is_allowed_without_remote_flags() {
        let config = ServerConfig {
            host: "127.0.0.1".into(),
            ..ServerConfig::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn non_loopback_binding_rejected_without_allow_remote() {
        let config = ServerConfig {
            host: "0.0.0.0".into(),
            allow_remote: false,
            auth_token: Some("secret".into()),
            ..ServerConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn non_loopback_binding_rejected_without_auth_token() {
        let config = ServerConfig {
            host: "0.0.0.0".into(),
            allow_remote: true,
            auth_token: None,
            ..ServerConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn non_loopback_binding_rejected_with_empty_auth_token() {
        let config = ServerConfig {
            host: "0.0.0.0".into(),
            allow_remote: true,
            auth_token: Some(String::new()),
            ..ServerConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn non_loopback_binding_allowed_with_both_flags_set() {
        let config = ServerConfig {
            host: "0.0.0.0".into(),
            allow_remote: true,
            auth_token: Some("secret".into()),
            ..ServerConfig::default()
        };
        assert!(config.validate().is_ok());
    }
}
