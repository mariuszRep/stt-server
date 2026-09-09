use std::collections::HashMap;
use std::path::PathBuf;

use tokio::sync::{mpsc, RwLock};

use crate::recognizer::Job;

pub enum ModelState {
    Installed {
        dir: PathBuf,
    },
    Loaded {
        dir: PathBuf,
        jobs: mpsc::Sender<Job>,
    },
}

pub struct AppState {
    pub models_dir: PathBuf,
    pub tmp_dir: PathBuf,
    pub registry: RwLock<HashMap<String, ModelState>>,
    /// The model this instance was launched to serve (`VOICE_TYPER_MODEL`).
    /// Used when a transcribe request omits `model`, and reported by
    /// `GET /v1/config` -- the Local Provider Protocol's "a runtime serves
    /// exactly one model per running instance" contract. An explicit `model`
    /// field on a request still overrides this (see `api::transcribe`);
    /// sherpad's own multi-model registry is an additive capability beyond
    /// that baseline, not a replacement for it.
    pub default_model: Option<String>,
    /// Enforced on every route via `require_auth` when `Some` (matches
    /// `stt-server`'s own control-plane auth: no route is exempt).
    pub auth_token: Option<String>,
}

impl AppState {
    pub fn new(
        models_dir: PathBuf,
        tmp_dir: PathBuf,
        default_model: Option<String>,
        auth_token: Option<String>,
    ) -> Self {
        Self {
            models_dir,
            tmp_dir,
            registry: RwLock::new(HashMap::new()),
            default_model,
            auth_token,
        }
    }
}
