use std::collections::HashMap;
use std::path::Path;

use async_trait::async_trait;

use crate::error::RuntimeError;
use crate::manager::LaunchBuilder;

pub mod cache;
pub mod faster_whisper;
pub mod sherpa_onnx;

pub use cache::{DownloadProgress, ProgressCallback};

#[async_trait]
pub trait ProviderEngine: Send + Sync {
    fn provider_id(&self) -> &'static str;
    fn install_local(&self, variant: &str) -> Option<LaunchBuilder>;
    async fn download_variant(
        &self,
        variant: &str,
        on_progress: ProgressCallback,
    ) -> Result<LaunchBuilder, RuntimeError>;
    async fn download_model(
        &self,
        model_id: &str,
        output_dir: &Path,
        on_progress: ProgressCallback,
    ) -> Result<(), RuntimeError>;
    fn verify_cached_model(&self, model_id: &str) -> Result<Option<u64>, RuntimeError>;
}

pub fn registry() -> HashMap<String, Box<dyn ProviderEngine>> {
    let engines: Vec<Box<dyn ProviderEngine>> = vec![
        Box::new(faster_whisper::FasterWhisper),
        Box::new(sherpa_onnx::SherpaOnnx),
    ];
    engines
        .into_iter()
        .map(|engine| (engine.provider_id().to_string(), engine))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_catalog_provider_has_an_engine() {
        let engines = registry();
        assert!(crate::catalog::CATALOG
            .iter()
            .all(|entry| engines.contains_key(entry.id)));
    }
}
