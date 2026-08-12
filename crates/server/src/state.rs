use std::sync::Arc;

use stt_common::ServerConfig;
use stt_runtime::RuntimeManager;

/// Shared application state.
#[derive(Clone)]
pub struct AppState {
    pub config: ServerConfig,
    pub runtime_manager: Arc<RuntimeManager>,
}

impl AppState {
    pub fn new(config: ServerConfig, runtime_manager: Arc<RuntimeManager>) -> Self {
        Self {
            config,
            runtime_manager,
        }
    }
}
