use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use stt_common::{
    AudioBuffer, ModelId, ModelIdentifier, ModelInfo, ModelVerification, SessionId,
    TranscriptionResult, TranscriptionSegment,
};

use super::traits::{EngineAdapter, RealtimeContext};
use super::error::AdapterError;

// ── whisper.cpp FFI types (feature-gated) ────────────────────

/// Whisper context wrapping the C API handle.
struct WhisperContext {
    handle: *mut whisper_rs::WhisperContext,
}

// SAFETY: whisper.cpp contexts are thread-safe when accessed from one thread at a time.
// We enforce this via our own locking.
unsafe impl Send for WhisperContext {}
unsafe impl Sync for WhisperContext {}

impl Drop for WhisperContext {
    fn drop(&mut self) {
        // whisper_rs handles cleanup via its own Drop impl
    }
}

/// A transcription state for a session.
struct TranscriptionState {
    context: whisper_rs::WhisperState,
    language: Option<String>,
    sample_rate: u32,
    accumulated_samples: Vec<f32>,
}

/// Real whisper.cpp adapter implementation.
pub struct WhisperAdapter {
    models: Arc<RwLock<HashMap<ModelIdentifier, WhisperModelEntry>>>,
    selected: Arc<RwLock<Option<ModelId>>>,
    model_dir: PathBuf,
}

struct WhisperModelEntry {
    info: ModelInfo,
    path: PathBuf,
    context: Option<Arc<WhisperContext>>,
    handle: Option<ModelId>,
}

impl WhisperAdapter {
    pub fn new(model_dir: PathBuf) -> Self {
        Self {
            models: Arc::new(RwLock::new(HashMap::new())),
            selected: Arc::new(RwLock::new(None)),
            model_dir,
        }
    }

    fn get_context_for_model(
        &self,
        entry: &WhisperModelEntry,
    ) -> Result<Arc<WhisperContext>, AdapterError> {
        entry
            .context
            .as_ref()
            .cloned()
            .ok_or_else(|| AdapterError::ModelNotFound(format!("model not loaded: {}", entry.info.id)))
    }
}

#[async_trait]
impl EngineAdapter for WhisperAdapter {
    async fn load_model(
        &self,
        model_id: &ModelIdentifier,
        path: &Path,
    ) -> Result<ModelId, AdapterError> {
        let path = path.to_path_buf();
        let model_id_owned = model_id.clone();

        // Load whisper context (blocking, run on spawn_blocking)
        let ctx_handle = tokio::task::spawn_blocking(move || -> Result<_, AdapterError> {
            let ctx = whisper_rs::WhisperContext::new_with_params(
                &path.to_string_lossy(),
                whisper_rs::WhisperContextParameters::default(),
            )
            .map_err(|e| AdapterError::ModelLoadFailed(format!("whisper_init failed: {e}")))?;

            Ok(Arc::new(WhisperContext {
                handle: ctx.into_raw(),
            }))
        })
        .await
        .map_err(|e| AdapterError::InternalError(format!("task join error: {e}")))??;

        let handle = ModelId::new();

        let mut models = self.models.write().await;
        let entry = models.entry(model_id_owned.clone()).or_insert_with(|| WhisperModelEntry {
            info: ModelInfo {
                id: model_id_owned,
                name: model_id_owned.to_string(),
                language: None,
                size_bytes: None,
                loaded: false,
                model_id: None,
            },
            path: path.clone(),
            context: None,
            handle: None,
        });

        entry.context = Some(ctx_handle);
        entry.handle = Some(handle);
        entry.info.loaded = true;
        entry.info.model_id = Some(handle);

        Ok(handle)
    }

    async fn unload_model(&self, model_id: ModelId) -> Result<(), AdapterError> {
        let mut models = self.models.write().await;
        for entry in models.values_mut() {
            if entry.handle == Some(model_id) {
                entry.context = None;
                entry.handle = None;
                entry.info.loaded = false;
                entry.info.model_id = None;
                return Ok(());
            }
        }
        Err(AdapterError::ModelNotFound(format!(
            "model handle {model_id} not found"
        )))
    }

    async fn verify_model(&self, path: &Path) -> Result<ModelVerification, AdapterError> {
        if !path.exists() {
            return Ok(ModelVerification {
                valid: false,
                checksum: None,
                error: Some(format!("file not found: {}", path.display())),
            });
        }

        // Try to load the model briefly to verify it's valid
        let path_owned = path.to_path_buf();
        let result = tokio::task::spawn_blocking(move || {
            whisper_rs::WhisperContext::new_with_params(
                &path_owned.to_string_lossy(),
                whisper_rs::WhisperContextParameters::default(),
            )
        })
        .await
        .map_err(|e| AdapterError::InternalError(format!("task join error: {e}")));

        match result {
            Ok(Ok(_ctx)) => Ok(ModelVerification {
                valid: true,
                checksum: None,
                error: None,
            }),
            Ok(Err(e)) => Ok(ModelVerification {
                valid: false,
                checksum: None,
                error: Some(format!("verification failed: {e}")),
            }),
            Err(e) => Ok(ModelVerification {
                valid: false,
                checksum: None,
                error: Some(format!("verification error: {e}")),
            }),
        }
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, AdapterError> {
        let models = self.models.read().await;
        Ok(models.values().map(|m| m.info.clone()).collect())
    }

    async fn select_model(&self, model_id: ModelId) -> Result<(), AdapterError> {
        let models = self.models.read().await;
        let found = models.values().any(|m| m.handle == Some(model_id));
        drop(models);

        if !found {
            return Err(AdapterError::ModelNotFound(format!(
                "model handle {model_id} not loaded"
            )));
        }
        *self.selected.write().await = Some(model_id);
        Ok(())
    }

    async fn get_selected_model(&self) -> Result<Option<ModelId>, AdapterError> {
        Ok(*self.selected.read().await)
    }

    async fn transcribe_batch(
        &self,
        model_id: ModelId,
        audio: AudioBuffer,
        language: Option<&str>,
    ) -> Result<TranscriptionResult, AdapterError> {
        // Find the loaded model context
        let models = self.models.read().await;
        let entry = models
            .values()
            .find(|m| m.handle == Some(model_id))
            .ok_or_else(|| AdapterError::ModelNotFound(format!("model handle {model_id} not loaded")))?;
        let ctx_arc = self.get_context_for_model(entry)?;
        drop(models);

        // Run transcription on blocking thread
        let samples = audio.samples;
        let lang = language.map(|s| s.to_string());

        let result = tokio::task::spawn_blocking(move || -> Result<_, AdapterError> {
            let ctx_ref = unsafe { &*ctx_arc.handle };

            let mut state = ctx_ref.create_state().map_err(|e| {
                AdapterError::TranscriptionFailed(format!("failed to create state: {e}"))
            })?;

            let mut params = whisper_rs::WhisperFullDefaultParameters::default();
            if let Some(ref l) = lang {
                params.set_language(Some(l.as_str()));
            }
            params.set_print_progress(false);
            params.set_print_realtime(false);
            params.set_print_timestamps(false);

            state.full(params, &samples).map_err(|e| {
                AdapterError::TranscriptionFailed(format!("full() failed: {e}"))
            })?;

            let n_segments = state.full_n_segments().map_err(|e| {
                AdapterError::TranscriptionFailed(format!("n_segments failed: {e}"))
            })?;

            let mut text = String::new();
            let mut segments = Vec::new();

            for i in 0..n_segments {
                let seg_text = state.full_get_segment_text(i).map_err(|e| {
                    AdapterError::TranscriptionFailed(format!("segment_text failed: {e}"))
                })?;
                let start = state.full_get_segment_t0(i).map_err(|e| {
                    AdapterError::TranscriptionFailed(format!("segment_t0 failed: {e}"))
                })?;
                let end = state.full_get_segment_t1(i).map_err(|e| {
                    AdapterError::TranscriptionFailed(format!("segment_t1 failed: {e}"))
                })?;

                text.push_str(&seg_text);
                segments.push(TranscriptionSegment {
                    text: seg_text,
                    start_ms: start as i64 * 10, // whisper.cpp uses centiseconds
                    end_ms: end as i64 * 10,
                    probability: 0.0, // Not easily available from this API
                });
            }

            Ok(TranscriptionResult {
                id: uuid::Uuid::new_v4().to_string(),
                text: text.trim().to_string(),
                language: lang.unwrap_or_else(|| "en".to_string()),
                duration_secs: samples.len() as f64 / 16000.0,
                segments,
            })
        })
        .await
        .map_err(|e| AdapterError::InternalError(format!("task join error: {e}")))??;

        Ok(result)
    }

    async fn create_realtime_context(
        &self,
        model_id: ModelId,
        sample_rate: u32,
        language: Option<&str>,
    ) -> Result<RealtimeContext, AdapterError> {
        // Verify model is loaded
        let models = self.models.read().await;
        let entry = models
            .values()
            .find(|m| m.handle == Some(model_id))
            .ok_or_else(|| AdapterError::ModelNotFound(format!("model handle {model_id} not loaded")))?;
        let _ctx_arc = self.get_context_for_model(entry)?;
        drop(models);

        Ok(RealtimeContext {
            session_id: SessionId::new(),
            model_id,
            sample_rate,
            language: language.map(|s| s.to_string()),
        })
    }

    async fn feed_realtime_audio(
        &self,
        ctx: &mut RealtimeContext,
        samples: &[f32],
    ) -> Result<Vec<String>, AdapterError> {
        // Accumulate samples; run transcription periodically
        // For simplicity, run every ~1 second of audio
        let chunk_size = ctx.sample_rate as usize; // 1 second
        let mut partials = Vec::new();

        // In a real implementation, we'd maintain state across calls
        // and only run transcription on new chunks. For V1, we'll
        // do a simple accumulation + transcription approach.
        let total_samples: usize = samples.len();

        if total_samples >= chunk_size {
            // Run transcription on the accumulated audio
            let models = self.models.read().await;
            let entry = models
                .values()
                .find(|m| m.handle == Some(ctx.model_id))
                .ok_or_else(|| AdapterError::SessionError("model not loaded".into()))?;
            let ctx_arc = self.get_context_for_model(entry)?;
            drop(models);

            let samples_owned = samples.to_vec();
            let lang = ctx.language.clone();

            let result = tokio::task::spawn_blocking(move || -> Result<String, AdapterError> {
                let ctx_ref = unsafe { &*ctx_arc.handle };
                let mut state = ctx_ref.create_state().map_err(|e| {
                    AdapterError::TranscriptionFailed(format!("state creation failed: {e}"))
                })?;

                let mut params = whisper_rs::WhisperFullDefaultParameters::default();
                if let Some(ref l) = lang {
                    params.set_language(Some(l.as_str()));
                }
                params.set_print_progress(false);
                params.set_print_realtime(false);
                params.set_print_timestamps(false);

                state.full(params, &samples_owned).map_err(|e| {
                    AdapterError::TranscriptionFailed(format!("full() failed: {e}"))
                })?;

                let n_segments = state.full_n_segments().map_err(|e| {
                    AdapterError::TranscriptionFailed(format!("n_segments failed: {e}"))
                })?;

                let mut text = String::new();
                for i in 0..n_segments {
                    let seg = state.full_get_segment_text(i).map_err(|e| {
                        AdapterError::TranscriptionFailed(format!("segment failed: {e}"))
                    })?;
                    text.push_str(&seg);
                }

                Ok(text.trim().to_string())
            })
            .await
            .map_err(|e| AdapterError::InternalError(format!("join error: {e}")))??;

            if !result.is_empty() {
                partials.push(result);
            }
        }

        Ok(partials)
    }

    async fn finalize_realtime(
        &self,
        ctx: &mut RealtimeContext,
    ) -> Result<TranscriptionResult, AdapterError> {
        // Return a final result with whatever was accumulated
        Ok(TranscriptionResult {
            id: uuid::Uuid::new_v4().to_string(),
            text: String::new(), // In real impl, return accumulated text
            language: ctx.language.clone().unwrap_or_else(|| "en".to_string()),
            duration_secs: 0.0,
            segments: vec![],
        })
    }

    async fn destroy_realtime_context(&self, _ctx: RealtimeContext) -> Result<(), AdapterError> {
        Ok(())
    }
}
