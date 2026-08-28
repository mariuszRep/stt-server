use std::path::PathBuf;

use crate::SttError;

const LOOPBACK_HOSTS: [&str; 3] = ["127.0.0.1", "::1", "localhost"];

/// Whether `host` is one of the recognized loopback spellings. Shared by
/// `ServerConfig::validate` (the control plane's own bind host) and
/// `stt-runtime`'s managed-runtime `bind_host` guardrails (CONVENTIONS.md:
/// "Loopback is default; remote binding is explicit and authenticated"),
/// so both layers agree on exactly what counts as "still local."
pub fn is_loopback_host(host: &str) -> bool {
    LOOPBACK_HOSTS.contains(&host)
}

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
        let is_loopback = is_loopback_host(&self.host);
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

/// Root of every on-disk artifact `stt-server` manages for itself —
/// downloaded model weights and cached provider runtime binaries alike.
/// This is the single confirmed root an installer's uninstall hook (or any
/// other daemon-independent cleanup) can safely wipe wholesale, since
/// nothing else is ever written here.
pub fn default_data_root() -> PathBuf {
    dirs_or_fallback().join("stt-server")
}

/// Default model directory based on platform.
pub fn default_model_dir() -> PathBuf {
    default_data_root().join("models")
}

/// Where downloaded managed-runtime artifacts (e.g. a packaged
/// faster-whisper build fetched for a given variant) are cached on disk.
/// Same `dirs::data_local_dir()` convention as [`default_model_dir`].
pub fn default_runtime_cache_dir() -> PathBuf {
    default_data_root().join("runtimes")
}

fn dirs_or_fallback() -> PathBuf {
    dirs::data_local_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// Overrides [`default_data_root`] for tests, so a purge test never touches
/// the real user's `%LOCALAPPDATA%\stt-server\`.
pub const DATA_ROOT_ENV_VAR: &str = "STT_DATA_ROOT";

/// The data root actually in effect — [`DATA_ROOT_ENV_VAR`] if set,
/// [`default_data_root`] otherwise. `pub` (not just used internally by
/// [`purge_all_local_state`]) specifically so a caller that wants to *tell
/// the user* what's about to be deleted — e.g. `stt reset`'s confirmation
/// prompt — shows the real target, not [`default_data_root`]'s unconditional
/// answer, which would silently disagree with the override during tests and
/// confuse whoever's reading the message.
pub fn resolved_data_root() -> PathBuf {
    std::env::var(DATA_ROOT_ENV_VAR)
        .map(PathBuf::from)
        .unwrap_or_else(|_| default_data_root())
}

/// Wipe every on-disk artifact `stt-server` manages — model weights and
/// cached provider runtime binaries alike — in one pure filesystem
/// operation. Deliberately independent of `RuntimeManager`/`AppState`/HTTP:
/// an installer's uninstall hook (or a user running `stt reset`) must be
/// able to call this without first spinning up a whole daemon. Idempotent:
/// `Ok` if the root was already absent. Returns the root it operated on, so
/// a caller can report exactly what was (or would have been) removed.
pub fn purge_all_local_state() -> std::io::Result<PathBuf> {
    let root = resolved_data_root();
    if root.is_dir() {
        std::fs::remove_dir_all(&root)?;
    }
    Ok(root)
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

    #[test]
    fn is_loopback_host_recognizes_all_three_spellings_and_nothing_else() {
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("::1"));
        assert!(is_loopback_host("localhost"));
        assert!(!is_loopback_host("0.0.0.0"));
        assert!(!is_loopback_host("192.168.1.5"));
    }

    // Serializes tests that mutate the process-wide DATA_ROOT_ENV_VAR — same
    // rationale as stt-runtime's ENV_TEST_LOCK (crates/runtime's default
    // test harness runs #[test] functions concurrently on separate threads
    // within the same process).
    static ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn purge_all_local_state_removes_the_whole_data_root_and_is_idempotent() {
        let _guard = ENV_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let root =
            std::env::temp_dir().join(format!("stt-purge-test-{}-{}", std::process::id(), line!()));
        std::fs::create_dir_all(root.join("models").join("faster-whisper")).unwrap();
        std::fs::create_dir_all(root.join("runtimes").join("faster-whisper")).unwrap();
        std::fs::write(root.join("models/faster-whisper/marker"), b"x").unwrap();

        // SAFETY: test-only env var mutation, scoped to this single test.
        unsafe {
            std::env::set_var(DATA_ROOT_ENV_VAR, &root);
        }

        purge_all_local_state().unwrap();
        assert!(!root.exists());

        // Idempotent: purging an already-absent root is not an error.
        purge_all_local_state().unwrap();

        unsafe {
            std::env::remove_var(DATA_ROOT_ENV_VAR);
        }
    }

    #[test]
    fn default_model_dir_and_runtime_cache_dir_nest_under_the_same_data_root() {
        let root = default_data_root();
        assert_eq!(default_model_dir(), root.join("models"));
        assert_eq!(default_runtime_cache_dir(), root.join("runtimes"));
    }
}
