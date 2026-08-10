use async_trait::async_trait;
use std::path::Path;

use stt_common::{
    AudioBuffer, ModelId, ModelIdentifier, ModelInfo, ModelVerification, TranscriptionResult,
};

use crate::AdapterError;

/// Canonical engine adapter trait.
///
/// All local engines and external STT providers implement this trait.
/// The server never accesses whisper.cpp or any engine directly;
/// it only goes through this interface.
#[async_trait]
pub trait EngineAdapter: Send + Sync {
    /// Load a model from disk and return a handle for subsequent operations.
    async fn load_model(&self, model_id: &ModelIdentifier, path: &Path) -> Result<ModelId, AdapterError>;

    /// Unload a previously loaded model.
    async fn unload_model(&self, model_id: ModelId) -> Result<(), AdapterError>;

    /// Verify a model file is valid without loading it.
    async fn verify_model(&self, path: &Path) -> Result<ModelVerification, AdapterError>;

    /// List all known models (loaded and available).
    async fn list_models(&self) -> Result<Vec<ModelInfo>, AdapterError>;

    /// Select a loaded model as the default for subsequent transcriptions.
    async fn select_model(&self, model_id: ModelId) -> Result<(), AdapterError>;

    /// Get the currently selected default model.
    async fn get_selected_model(&self) -> Result<Option<ModelId>, AdapterError>;

    /// Run batch transcription on a complete audio buffer.
    async fn transcribe_batch(
        &self,
        model_id: ModelId,
        audio: AudioBuffer,
        language: Option<&str>,
    ) -> Result<TranscriptionResult, AdapterError>;

    /// Create a new realtime transcription context.
    /// Returns a context handle that must be used for subsequent audio chunks.
    async fn create_realtime_context(
        &self,
        model_id: ModelId,
        sample_rate: u32,
        language: Option<&str>,
    ) -> Result<RealtimeContext, AdapterError>;

    /// Feed audio samples into a realtime context and get partial results.
    async fn feed_realtime_audio(
        &self,
        ctx: &mut RealtimeContext,
        samples: &[f32],
    ) -> Result<Vec<String>, AdapterError>;

    /// Finalize a realtime transcription and get the complete result.
    async fn finalize_realtime(
        &self,
        ctx: &mut RealtimeContext,
    ) -> Result<TranscriptionResult, AdapterError>;

    /// Destroy a realtime context without finalizing.
    async fn destroy_realtime_context(&self, ctx: RealtimeContext) -> Result<(), AdapterError>;
}

/// Handle for a realtime transcription session.
/// The adapter owns the underlying context; this is just an opaque identifier.
pub struct RealtimeContext {
    pub session_id: stt_common::SessionId,
    pub model_id: ModelId,
    pub sample_rate: u32,
    pub language: Option<String>,
}
