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
        /// The language the currently-running recognizer was actually built
        /// with (one of `ModelEntry::languages`, or `default_language` for a
        /// single-language model). Unlike faster-whisper, sherpa-onnx has no
        /// per-request language field -- changing it means rebuilding this
        /// recognizer (see `api::set_model_language`), so this is what lets
        /// callers tell "already serving the requested language" apart from
        /// "needs a reload".
        language: String,
    },
}

pub struct AppState {
    pub models_dir: PathBuf,
    pub tmp_dir: PathBuf,
    pub registry: RwLock<HashMap<String, ModelState>>,
    /// The model this instance serves when a transcribe request omits
    /// `model`, and reported by `GET /v1/config` -- the Local Provider
    /// Protocol's "a runtime serves exactly one model per running instance"
    /// contract. An explicit `model` field on a request still overrides this
    /// (see `api::transcribe`); sherpad's own multi-model registry is an
    /// additive capability beyond that baseline, not a replacement for it.
    /// Initially set from `VOICE_TYPER_MODEL` at launch, but mutable at
    /// runtime via `POST /v1/admin/model` (`api::admin_switch_model`) --
    /// mirrors faster-whisper's own in-process model-swap endpoint, which
    /// `stt-server`'s control plane already calls uniformly for every
    /// provider (see `RuntimeManager::switch_model`); sherpad just never
    /// implemented its side of that contract until now.
    pub default_model: RwLock<Option<String>>,
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
            default_model: RwLock::new(default_model),
            auth_token,
        }
    }
}
