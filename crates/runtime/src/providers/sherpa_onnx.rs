//! Install/launch wiring for the managed sherpa-onnx runtime (`sherpad`).
//!
//! A genuine hybrid, unlike faster-whisper (fully self-hosted) or a
//! hypothetical upstream-native engine (fully upstream): the **binary** is
//! self-hosted (`stt-server` builds and releases `sherpad` itself, because
//! `k2-fsa/sherpa-onnx` ships no server conforming to this project's local
//! provider protocol — verified directly against its releases, see
//! `generalize-provider-engine-installation`'s goal notes), while **models**
//! are fetched directly from sherpa-onnx's own upstream `asr-models`
//! release tag, per `CONVENTIONS.md`'s "minimize self-hosted binaries" rule
//! (there is no reason to re-host multi-hundred-megabyte model archives
//! `k2-fsa` already hosts).
//!
//! `sherpad` serves multiple models from one running instance — a real
//! capability beyond the Local Provider Protocol's "one model per instance"
//! baseline (see `make-sherpad-protocol-conformant`) — but this adapter
//! only ever asks it to run the single model `RuntimeManager` selected,
//! via `VOICE_TYPER_MODEL`; `sherpad`'s extra flexibility isn't exercised
//! here.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::error::RuntimeError;
use crate::manager::{Launch, LaunchBuilder, StartOptions};
use crate::providers::{cache, ProgressCallback, ProviderEngine};

pub struct SherpaOnnx;

/// Override for where a locally built/installed `sherpad` binary lives.
/// Primarily for local development (pointing at
/// `runtimes/sherpa-onnx/target/release/sherpad` without a real release
/// download) — mirrors faster-whisper's `STT_FASTER_WHISPER_RUNTIME_DIR`.
pub const RUNTIME_DIR_ENV_VAR: &str = "STT_SHERPA_ONNX_RUNTIME_DIR";

fn binary_name() -> &'static str {
    if cfg!(windows) {
        "sherpad.exe"
    } else {
        "sherpad"
    }
}

/// Candidate directories that might hold a locally built `sherpad`, checked
/// before falling back to a downloaded cached copy. Mirrors
/// `faster_whisper::locate_runtime_dir`'s search order and rationale (env
/// override, then binary-relative, then cwd-relative dev layout) — but
/// there is no "raw source + interpreter" form here, only ever a binary.
fn locate_dev_binary() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var(RUNTIME_DIR_ENV_VAR) {
        let candidate = PathBuf::from(dir).join(binary_name());
        return candidate.is_file().then_some(candidate);
    }

    let candidates = [
        std::env::current_exe().ok().and_then(|exe| {
            exe.parent()
                .map(|dir| dir.join("runtimes/sherpa-onnx").join(binary_name()))
        }),
        std::env::current_dir().ok().map(|dir| {
            dir.join("runtimes/sherpa-onnx/target/release")
                .join(binary_name())
        }),
    ];

    candidates.into_iter().flatten().find(|path| path.is_file())
}

/// Overrides the runtime cache root (mirrors
/// `faster_whisper::RUNTIME_CACHE_DIR_ENV_VAR`) — mainly for tests.
pub const RUNTIME_CACHE_DIR_ENV_VAR: &str = "STT_SHERPA_ONNX_CACHE_DIR";

fn cached_variant_dir() -> PathBuf {
    // Only one variant exists today (`cpu`) -- `sherpad` links a CPU-only
    // onnxruntime build; GPU is out of scope here, its own future goal.
    cache::variant_dir("sherpa-onnx", "cpu")
}

fn cached_binary_path() -> PathBuf {
    cached_variant_dir().join(binary_name())
}

/// Overrides where downloaded model weights are cached (mirrors
/// `faster_whisper::MODEL_CACHE_DIR_ENV_VAR`) — mainly for tests.
pub const MODEL_CACHE_DIR_ENV_VAR: &str = "STT_SHERPA_ONNX_MODEL_DIR";

fn cached_model_dir(model_id: &str) -> PathBuf {
    cache::model_dir("sherpa-onnx", model_id)
}

/// Env vars `sherpad` reads (see `make-sherpad-protocol-conformant`).
/// `VOICE_TYPER_MODEL_DIR` here is deliberately the *provider's whole model
/// root* (the directory holding every model as a subdirectory), not one
/// model's own directory the way faster-whisper's identically-named env var
/// works — `sherpad`'s multi-model registry needs to see every installed
/// model, not just the one currently selected, documented on both sides
/// (see `sherpad`'s own `main.rs`).
fn build_env(
    auth_token: &str,
    selected_model: Option<&str>,
    options: &StartOptions,
) -> Vec<(String, String)> {
    let host = options.bind_host.as_deref().unwrap_or("127.0.0.1");
    let mut env = vec![
        ("VOICE_TYPER_HOST".to_string(), host.to_string()),
        ("VOICE_TYPER_AUTH_TOKEN".to_string(), auth_token.to_string()),
        (
            "VOICE_TYPER_MODEL_DIR".to_string(),
            model_root_dir().to_string_lossy().to_string(),
        ),
    ];
    if let Some(model) = selected_model {
        env.push(("VOICE_TYPER_MODEL".to_string(), model.to_string()));
    }
    env
}

/// The provider-scoped root every sherpa-onnx model lives under — what
/// `sherpad`'s own startup registry scan expects as `VOICE_TYPER_MODEL_DIR`.
fn model_root_dir() -> PathBuf {
    cache::provider_model_root("sherpa-onnx")
}

fn launch_builder(binary: PathBuf) -> LaunchBuilder {
    Box::new(move |port, auth_token, selected_model, options| {
        let mut env = build_env(auth_token, selected_model, options);
        env.push(("VOICE_TYPER_PORT".to_string(), port.to_string()));
        Launch {
            program: binary.clone(),
            args: vec![],
            env,
            cwd: None,
        }
    })
}

/// Local-only lookup: a locally built dev binary, or a previously downloaded
/// cached copy. No network access. Mirrors
/// `faster_whisper::install_local`'s contract exactly (`None`, not an
/// error, when nothing is found).
pub fn install_local() -> Option<LaunchBuilder> {
    if let Some(binary) = locate_dev_binary() {
        return Some(launch_builder(binary));
    }
    let cached = cached_binary_path();
    cached.is_file().then(|| launch_builder(cached))
}

/// GitHub repo `sherpad`'s self-hosted release binary is published to —
/// `stt-server`'s own releases (see `release.yml`'s `build-sherpad` job),
/// the same self-hosted scheme faster-whisper uses, for the same reason:
/// no protocol-conformant binary exists upstream to fetch instead.
const RELEASE_REPO: &str = "mariuszRep/stt-server";

pub const RELEASE_BASE_URL_ENV_VAR: &str = "STT_SHERPA_ONNX_RELEASE_BASE_URL";

fn release_base_url() -> String {
    std::env::var(RELEASE_BASE_URL_ENV_VAR).unwrap_or_else(|_| {
        format!(
            "https://github.com/{RELEASE_REPO}/releases/download/v{}",
            env!("CARGO_PKG_VERSION")
        )
    })
}

/// Matches `release.yml`'s `build-sherpad` job's rename step exactly.
fn asset_name() -> String {
    let os = if cfg!(windows) { "windows" } else { "linux" };
    let ext = if cfg!(windows) { ".exe" } else { "" };
    format!("sherpad-{os}-cpu{ext}")
}

/// Download the `sherpad` binary from `stt-server`'s own release, cache it,
/// and return a launch spec. Mirrors
/// `faster_whisper::download_variant`'s streaming/atomic-rename contract via
/// the same shared `cache::download_to_cache` helper.
pub async fn download_variant(
    on_progress: impl Fn(cache::DownloadProgress) + Send + Sync + 'static,
) -> Result<LaunchBuilder, RuntimeError> {
    let name = asset_name();
    let url = format!("{}/{name}", release_base_url());
    let dest_dir = cached_variant_dir();
    let dest =
        cache::download_to_cache(&url, &dest_dir, &name, true, Box::new(on_progress)).await?;
    Ok(launch_builder(dest))
}

/// Fetch `model_id`'s `.tar.bz2` archive directly from
/// `k2-fsa/sherpa-onnx`'s own `asr-models` release tag and extract it into
/// `output_dir`, normalizing the extracted directory name the same way
/// `sherpad`'s own `download.rs::download_and_extract` does. Unlike
/// faster-whisper's `download_model`, this needs no locally installed
/// runtime as a precondition -- it's a plain HTTP GET, proving the trait
/// genuinely doesn't presuppose faster-whisper's "spawn the runtime itself
/// to fetch its own model" shape.
pub async fn download_model(model_id: &str, output_dir: &Path) -> Result<(), RuntimeError> {
    let entry = sherpa_manifest::find(model_id).ok_or_else(|| {
        RuntimeError::DownloadFailed(format!("unknown sherpa-onnx model id: {model_id}"))
    })?;

    let parent = output_dir
        .parent()
        .ok_or_else(|| RuntimeError::DownloadFailed("model output dir has no parent".into()))?
        .to_path_buf();
    std::fs::create_dir_all(&parent).map_err(RuntimeError::Io)?;

    // Reuse the shared streaming/atomic-completion primitive for the
    // archive fetch itself (same one `download_variant` and
    // faster-whisper's `download_variant` use) -- extraction, being
    // per-family bzip2+tar rather than bytes-agnostic, happens after.
    let archive_name = format!("{}.tar.bz2", entry.id);
    let archive_path = cache::download_to_cache(
        entry.download_url,
        &parent,
        &archive_name,
        false,
        Box::new(|_progress| {}),
    )
    .await?;

    let extract_dir = parent.clone();
    let output_dir = output_dir.to_path_buf();
    let archive_root = entry.archive_root;
    let model_id_owned = entry.id;
    tokio::task::spawn_blocking(move || {
        let file = std::fs::File::open(&archive_path).map_err(RuntimeError::Io)?;
        let bz = bzip2::read::BzDecoder::new(file);
        let mut archive = tar::Archive::new(bz);
        archive.unpack(&extract_dir).map_err(|e| {
            RuntimeError::DownloadFailed(format!("extracting {model_id_owned}: {e}"))
        })?;
        let _ = std::fs::remove_file(&archive_path);

        let extracted = extract_dir.join(archive_root);
        if extracted != output_dir {
            std::fs::rename(&extracted, &output_dir).map_err(RuntimeError::Io)?;
        }
        Ok::<(), RuntimeError>(())
    })
    .await
    .map_err(|e| RuntimeError::DownloadFailed(e.to_string()))??;

    Ok(())
}

/// The relative file paths that must all be present for `model_id` to count
/// as fully downloaded, per its `sherpa_manifest::ModelFiles` family.
fn required_relative_paths(entry: &sherpa_manifest::ModelEntry) -> Vec<&'static str> {
    match &entry.files {
        sherpa_manifest::ModelFiles::SenseVoice { model, tokens } => vec![model, tokens],
        sherpa_manifest::ModelFiles::Whisper {
            encoder,
            decoder,
            tokens,
        } => vec![encoder, decoder, tokens],
        sherpa_manifest::ModelFiles::Transducer {
            encoder,
            decoder,
            joiner,
            tokens,
        } => vec![encoder, decoder, joiner, tokens],
    }
}

pub fn verify_cached_model(model_id: &str) -> Result<Option<u64>, RuntimeError> {
    let entry = sherpa_manifest::find(model_id).ok_or_else(|| {
        RuntimeError::DownloadFailed(format!("unknown sherpa-onnx model id: {model_id}"))
    })?;
    cache::verify_files_present(&cached_model_dir(model_id), &required_relative_paths(entry))
}

#[async_trait]
impl ProviderEngine for SherpaOnnx {
    fn provider_id(&self) -> &'static str {
        "sherpa-onnx"
    }

    fn install_local(&self, _variant: &str) -> Option<LaunchBuilder> {
        // Only one variant exists (`cpu`); the string is accepted (never
        // rejected) so the generic registry dispatch never needs to know
        // that, matching the trait's engine-agnostic contract.
        install_local()
    }

    async fn download_variant(
        &self,
        _variant: &str,
        on_progress: ProgressCallback,
    ) -> Result<LaunchBuilder, RuntimeError> {
        download_variant(on_progress).await
    }

    async fn download_model(&self, model_id: &str, output_dir: &Path) -> Result<(), RuntimeError> {
        download_model(model_id, output_dir).await
    }

    fn verify_cached_model(&self, model_id: &str) -> Result<Option<u64>, RuntimeError> {
        verify_cached_model(model_id)
    }
}
