use async_trait::async_trait;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;

use stt_common::{
    AudioBuffer, ModelId, ModelIdentifier, ModelInfo, ModelVerification, SessionId,
    TranscriptionResult, TranscriptionSegment,
};

use super::traits::{EngineAdapter, RealtimeContext};
use super::error::AdapterError;

/// Mock adapter for testing. Returns deterministic results.
pub struct MockAdapter {
    models: Arc<RwLock<HashMap<ModelIdentifier, MockModel>>>,
    loaded: Arc<RwLock<HashMap<ModelId, ModelIdentifier>>>,
    selected: Arc<RwLock<Option<ModelId>>>,
}

struct MockModel {
    info: ModelInfo,
    path: PathBuf,
}

impl MockAdapter {
    pub fn new() -> Self {
        Self {
            models: Arc::new(RwLock::new(HashMap::new())),
            loaded: Arc::new(RwLock::new(HashMap::new())),
            selected: Arc::new(RwLock::new(None)),
        }
    }

    /// Register a mock model for testing.
    pub async fn register_model(&self, id: &str, name: &str, path: PathBuf) {
        let model_id = ModelIdentifier::new(id).unwrap();
        let info = ModelInfo {
            id: model_id.clone(),
            name: name.to_string(),
            language: Some("en".to_string()),
            size_bytes: Some(1024 * 1024),
            loaded: false,
            model_id: None,
        };
        self.models.write().await.insert(
            model_id,
            MockModel { info, path },
        );
    }
}

impl Default for MockAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl EngineAdapter for MockAdapter {
    async fn load_model(
        &self,
        model_id: &ModelIdentifier,
        _path: &Path,
    ) -> Result<ModelId, AdapterError> {
        let models = self.models.read().await;
        if !models.contains_key(model_id) {
            return Err(AdapterError::ModelNotFound(model_id.to_string()));
        }
        drop(models);

        let handle = ModelId::new();
        self.loaded
            .write()
            .await
            .insert(handle, model_id.clone());

        // Update loaded status
        let mut models = self.models.write().await;
        if let Some(m) = models.get_mut(model_id) {
            m.info.loaded = true;
            m.info.model_id = Some(handle);
        }

        Ok(handle)
    }

    async fn unload_model(&self, model_id: ModelId) -> Result<(), AdapterError> {
        let mut loaded = self.loaded.write().await;
        let model_name = loaded.remove(&model_id).ok_or_else(|| {
            AdapterError::ModelNotFound(format!("model handle {model_id} not loaded"))
        })?;

        let mut models = self.models.write().await;
        if let Some(m) = models.get_mut(&model_name) {
            m.info.loaded = false;
            m.info.model_id = None;
        }
        Ok(())
    }

    async fn verify_model(&self, path: &Path) -> Result<ModelVerification, AdapterError> {
        if path.exists() {
            Ok(ModelVerification {
                valid: true,
                checksum: Some("mock-checksum".to_string()),
                error: None,
            })
        } else {
            Ok(ModelVerification {
                valid: false,
                checksum: None,
                error: Some(format!("file not found: {}", path.display())),
            })
        }
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, AdapterError> {
        let models = self.models.read().await;
        Ok(models.values().map(|m| m.info.clone()).collect())
    }

    async fn select_model(&self, model_id: ModelId) -> Result<(), AdapterError> {
        let loaded = self.loaded.read().await;
        if !loaded.contains_key(&model_id) {
            return Err(AdapterError::ModelNotFound(format!(
                "model handle {model_id} is not loaded"
            )));
        }
        drop(loaded);
        *self.selected.write().await = Some(model_id);
        Ok(())
    }

    async fn get_selected_model(&self) -> Result<Option<ModelId>, AdapterError> {
        Ok(*self.selected.read().await)
    }

    async fn transcribe_batch(
        &self,
        _model_id: ModelId,
        audio: AudioBuffer,
        _language: Option<&str>,
    ) -> Result<TranscriptionResult, AdapterError> {
        let duration = audio.duration_secs();
        Ok(TranscriptionResult {
            id: uuid::Uuid::new_v4().to_string(),
            text: format!("Mock transcription of {:.1}s audio", duration),
            language: "en".to_string(),
            duration_secs: duration,
            segments: vec![TranscriptionSegment {
                text: format!("Mock transcription of {:.1}s audio", duration),
                start_ms: 0,
                end_ms: (duration * 1000.0) as i64,
                probability: 0.95,
            }],
        })
    }

    async fn create_realtime_context(
        &self,
        model_id: ModelId,
        sample_rate: u32,
        language: Option<&str>,
    ) -> Result<RealtimeContext, AdapterError> {
        Ok(RealtimeContext {
            session_id: SessionId::new(),
            model_id,
            sample_rate,
            language: language.map(|s| s.to_string()),
        })
    }

    async fn feed_realtime_audio(
        &self,
        _ctx: &mut RealtimeContext,
        samples: &[f32],
    ) -> Result<Vec<String>, AdapterError> {
        // Mock: return partial text based on accumulated samples
        let duration = samples.len() as f64 / 16000.0;
        if duration > 0.5 {
            Ok(vec![format!("partial... {:.1}s", duration)])
        } else {
            Ok(vec![])
        }
    }

    async fn finalize_realtime(
        &self,
        ctx: &mut RealtimeContext,
    ) -> Result<TranscriptionResult, AdapterError> {
        Ok(TranscriptionResult {
            id: uuid::Uuid::new_v4().to_string(),
            text: "Mock realtime transcription complete".to_string(),
            language: ctx.language.clone().unwrap_or_else(|| "en".to_string()),
            duration_secs: 0.0,
            segments: vec![TranscriptionSegment {
                text: "Mock realtime transcription complete".to_string(),
                start_ms: 0,
                end_ms: 0,
                probability: 0.95,
            }],
        })
    }

    async fn destroy_realtime_context(&self, _ctx: RealtimeContext) -> Result<(), AdapterError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_mock_adapter_load_unload() {
        let adapter = MockAdapter::new();
        let id = ModelIdentifier::new("test-model").unwrap();

        adapter
            .register_model("test-model", "Test Model", PathBuf::from("/tmp/test.bin"))
            .await;

        let handle = adapter.load_model(&id, Path::new("/tmp/test.bin")).await.unwrap();

        let models = adapter.list_models().await.unwrap();
        assert_eq!(models.len(), 1);
        assert!(models[0].loaded);

        adapter.unload_model(handle).await.unwrap();

        let models = adapter.list_models().await.unwrap();
        assert!(!models[0].loaded);
    }

    #[tokio::test]
    async fn test_mock_adapter_transcribe() {
        let adapter = MockAdapter::new();
        let id = ModelIdentifier::new("test-model").unwrap();

        adapter
            .register_model("test-model", "Test Model", PathBuf::from("/tmp/test.bin"))
            .await;

        let handle = adapter.load_model(&id, Path::new("/tmp/test.bin")).await.unwrap();

        let audio = AudioBuffer {
            samples: vec![0.0; 16000], // 1 second
            sample_rate: 16000,
            channels: 1,
            format: stt_common::AudioFormat::WavPcm,
        };

        let result = adapter
            .transcribe_batch(handle, audio, None)
            .await
            .unwrap();
        assert!(!result.text.is_empty());
        assert!(result.duration_secs > 0.0);
    }

    #[tokio::test]
    async fn test_mock_adapter_select() {
        let adapter = MockAdapter::new();
        let id = ModelIdentifier::new("test-model").unwrap();

        adapter
            .register_model("test-model", "Test Model", PathBuf::from("/tmp/test.bin"))
            .await;

        let handle = adapter.load_model(&id, Path::new("/tmp/test.bin")).await.unwrap();
        adapter.select_model(handle).await.unwrap();

        let selected = adapter.get_selected_model().await.unwrap();
        assert_eq!(selected, Some(handle));
    }
}
