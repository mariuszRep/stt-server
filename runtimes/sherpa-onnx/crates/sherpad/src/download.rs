use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sherpa_manifest::ModelEntry;

/// Download `entry`'s archive and extract it under `models_dir`, normalizing
/// the extracted folder to `models_dir/<entry.id>` regardless of what the
/// upstream archive's internal directory is named. Blocking; run this on a
/// blocking thread pool from async code.
pub fn download_and_extract(entry: &ModelEntry, models_dir: &Path) -> Result<PathBuf> {
    let install_dir = models_dir.join(entry.id);
    if install_dir.exists() {
        return Ok(install_dir);
    }
    std::fs::create_dir_all(models_dir)?;

    tracing::info!(
        model = entry.id,
        url = entry.download_url,
        "downloading model"
    );
    let mut response = ureq::get(entry.download_url)
        .call()
        .with_context(|| format!("GET {}", entry.download_url))?;

    let reader = response.body_mut().as_reader();
    let bz = bzip2::read::BzDecoder::new(reader);
    let mut archive = tar::Archive::new(bz);
    archive
        .unpack(models_dir)
        .with_context(|| format!("extracting archive for {}", entry.id))?;

    let extracted = models_dir.join(entry.archive_root);
    if extracted != install_dir {
        std::fs::rename(&extracted, &install_dir).with_context(|| {
            format!(
                "renaming {} to {}",
                extracted.display(),
                install_dir.display()
            )
        })?;
    }

    Ok(install_dir)
}

pub fn remove_install(models_dir: &Path, id: &str) -> Result<()> {
    let dir = models_dir.join(id);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
    }
    Ok(())
}
