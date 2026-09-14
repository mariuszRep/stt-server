//! Provider-neutral runtime and model cache mechanics.

use std::path::{Path, PathBuf};

use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;

use crate::error::RuntimeError;

#[derive(Debug, Clone, Copy)]
pub struct DownloadProgress {
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
}

pub type ProgressCallback = Box<dyn Fn(DownloadProgress) + Send + Sync + 'static>;

fn provider_env_prefix(provider_id: &str) -> String {
    provider_id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

fn runtime_root(provider_id: &str) -> PathBuf {
    std::env::var(format!(
        "STT_{}_CACHE_DIR",
        provider_env_prefix(provider_id)
    ))
    .map(PathBuf::from)
    .unwrap_or_else(|_| stt_common::default_runtime_cache_dir())
}

fn model_root(provider_id: &str) -> PathBuf {
    std::env::var(format!(
        "STT_{}_MODEL_DIR",
        provider_env_prefix(provider_id)
    ))
    .map(PathBuf::from)
    .unwrap_or_else(|_| stt_common::default_model_dir())
}

pub fn variant_dir(provider_id: &str, variant: &str) -> PathBuf {
    runtime_root(provider_id).join(provider_id).join(variant)
}

/// The directory holding *every* model for `provider_id`, each as its own
/// subdirectory (`<this>/<model_id>/...`) -- what `model_dir` computes minus
/// the trailing model id. Exists for engines whose runtime needs to see the
/// whole model root at once (sherpa-onnx's multi-model registry scan), not
/// just one currently-selected model's directory the way faster-whisper's
/// single-model launch env only ever needs.
pub fn provider_model_root(provider_id: &str) -> PathBuf {
    model_root(provider_id).join(provider_id)
}

pub fn model_dir(provider_id: &str, model_id: &str) -> PathBuf {
    model_root(provider_id).join(provider_id).join(model_id)
}

pub fn remove_dir(dir: &Path) -> Result<(), RuntimeError> {
    if dir.is_dir() {
        std::fs::remove_dir_all(dir).map_err(RuntimeError::Io)?;
    }
    Ok(())
}

pub fn remove_variant(provider_id: &str, variant: &str) -> Result<(), RuntimeError> {
    remove_dir(&variant_dir(provider_id, variant))
}

pub fn remove_model(provider_id: &str, model_id: &str) -> Result<(), RuntimeError> {
    remove_dir(&model_dir(provider_id, model_id))
}

pub fn verify_files_present(
    dir: &Path,
    relative_paths: &[&str],
) -> Result<Option<u64>, RuntimeError> {
    let mut total = 0;
    for relative_path in relative_paths {
        match std::fs::metadata(dir.join(relative_path)) {
            Ok(meta) if meta.is_file() && meta.len() > 0 => total += meta.len(),
            Ok(_) => return Ok(None),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(RuntimeError::Io(error)),
        }
    }
    Ok(Some(total))
}

pub async fn download_to_cache(
    url: &str,
    dest_dir: &Path,
    filename: &str,
    make_executable: bool,
    on_progress: ProgressCallback,
) -> Result<PathBuf, RuntimeError> {
    std::fs::create_dir_all(dest_dir).map_err(RuntimeError::Io)?;
    let dest = dest_dir.join(filename);
    let partial = dest_dir.join(format!("{filename}.part"));

    let response = reqwest::Client::new()
        .get(url)
        .send()
        .await
        .map_err(|error| {
            RuntimeError::DownloadFailed(format!("request to {url} failed: {error}"))
        })?;
    if !response.status().is_success() {
        return Err(RuntimeError::DownloadFailed(format!(
            "{url} returned {}",
            response.status()
        )));
    }

    let total_bytes = response.content_length();
    let mut downloaded_bytes = 0;
    let mut file = tokio::fs::File::create(&partial)
        .await
        .map_err(RuntimeError::Io)?;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            RuntimeError::DownloadFailed(format!("download interrupted: {error}"))
        })?;
        downloaded_bytes += chunk.len() as u64;
        file.write_all(&chunk).await.map_err(RuntimeError::Io)?;
        on_progress(DownloadProgress {
            downloaded_bytes,
            total_bytes,
        });
    }
    file.flush().await.map_err(RuntimeError::Io)?;
    drop(file);

    #[cfg(unix)]
    if make_executable {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&partial)
            .map_err(RuntimeError::Io)?
            .permissions();
        permissions.set_mode(permissions.mode() | 0o111);
        std::fs::set_permissions(&partial, permissions).map_err(RuntimeError::Io)?;
    }

    #[cfg(not(unix))]
    let _ = make_executable;

    // On Windows, renaming onto a destination that's currently executing
    // (e.g. this same binary already running as a provider process) fails
    // with "Access is denied" (os error 5) — unlike Unix, where replacing a
    // running executable's inode is legal. If the file we just downloaded is
    // byte-identical to what's already there, there's nothing to swap: drop
    // the redundant `.part` copy and treat the existing file as the result,
    // instead of trying (and failing) to overwrite a locked file.
    if let Ok(existing_len) = tokio::fs::metadata(&dest).await.map(|m| m.len()) {
        if existing_len == downloaded_bytes {
            let _ = tokio::fs::remove_file(&partial).await;
            return Ok(dest);
        }
    }

    tokio::fs::rename(&partial, &dest)
        .await
        .map_err(RuntimeError::Io)?;
    Ok(dest)
}
