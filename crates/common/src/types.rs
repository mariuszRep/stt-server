use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ── Model Types ──────────────────────────────────────────────

/// Unique identifier for a loaded model instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelId(pub Uuid);

impl ModelId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ModelId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for ModelId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Model identifier (safe, validated string).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelIdentifier(String);

impl ModelIdentifier {
    /// Create a new model identifier, validating it is safe.
    pub fn new(id: impl Into<String>) -> Result<Self, crate::SttError> {
        let id = id.into();
        if id.is_empty() {
            return Err(crate::SttError::InvalidModelId("model ID cannot be empty".into()));
        }
        if id.len() > 256 {
            return Err(crate::SttError::InvalidModelId("model ID too long".into()));
        }
        // Reject path separators and other dangerous characters
        if id.contains('/') || id.contains('\\') || id.contains("..") || id.contains('\0') {
            return Err(crate::SttError::InvalidModelId(format!(
                "model ID contains invalid characters: {id}"
            )));
        }
        Ok(Self(id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ModelIdentifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl AsRef<str> for ModelIdentifier {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Information about a model (available or loaded).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: ModelIdentifier,
    pub name: String,
    pub language: Option<String>,
    pub size_bytes: Option<u64>,
    pub loaded: bool,
    pub model_id: Option<ModelId>,
}

/// Model verification result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelVerification {
    pub valid: bool,
    pub checksum: Option<String>,
    pub error: Option<String>,
}

// ── Audio Types ──────────────────────────────────────────────

/// Audio format for batch transcription input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioFormat {
    WavPcm,
}

/// Sample format for realtime PCM input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SampleFormat {
    Signed16BitLittleEndian,
}

/// Audio buffer for batch transcription.
#[derive(Debug, Clone)]
pub struct AudioBuffer {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub channels: u16,
    pub format: AudioFormat,
}

impl AudioBuffer {
    /// Create from raw WAV bytes (PCM s16le).
    pub fn from_wav_bytes(data: &[u8]) -> Result<Self, crate::SttError> {
        let mut reader = hound::WavReader::new(std::io::Cursor::new(data))
            .map_err(|e| crate::SttError::AudioError(format!("failed to parse WAV: {e}")))?;

        let spec = reader.spec();
        if spec.channels != 1 {
            return Err(crate::SttError::AudioError(format!(
                "expected mono audio, got {} channels",
                spec.channels
            )));
        }
        if spec.sample_rate != 16000 {
            return Err(crate::SttError::AudioError(format!(
                "expected 16kHz sample rate, got {}",
                spec.sample_rate
            )));
        }
        if spec.sample_format != hound::SampleFormat::Int {
            return Err(crate::SttError::AudioError(
                "expected integer sample format".into(),
            ));
        }
        if spec.bits_per_sample != 16 {
            return Err(crate::SttError::AudioError(format!(
                "expected 16-bit samples, got {}-bit",
                spec.bits_per_sample
            )));
        }

        let samples: Vec<f32> = reader
            .samples::<i16>()
            .map(|s| s.map(|v| v as f32 / 32768.0))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| crate::SttError::AudioError(format!("failed to read samples: {e}")))?;

        Ok(Self {
            samples,
            sample_rate: spec.sample_rate,
            channels: spec.channels,
            format: AudioFormat::WavPcm,
        })
    }

    /// Duration in seconds.
    pub fn duration_secs(&self) -> f64 {
        self.samples.len() as f64 / self.sample_rate as f64
    }
}

// ── Transcription Types ──────────────────────────────────────

/// Request for batch transcription.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptionRequest {
    pub model: Option<ModelIdentifier>,
    pub language: Option<String>,
    pub prompt: Option<String>,
    pub temperature: Option<f32>,
}

/// A segment of transcribed text with timing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptionSegment {
    pub text: String,
    pub start_ms: i64,
    pub end_ms: i64,
    pub probability: f32,
}

/// Result of a batch transcription.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptionResult {
    pub id: String,
    pub text: String,
    pub language: String,
    pub duration_secs: f64,
    pub segments: Vec<TranscriptionSegment>,
}

// ── Realtime Types ───────────────────────────────────────────

/// Session identifier for realtime transcription.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub Uuid);

impl SessionId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Configuration for a realtime transcription session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RealtimeConfig {
    pub model: Option<ModelIdentifier>,
    pub language: Option<String>,
    pub sample_rate: u32,
    pub channels: u16,
    pub sample_format: SampleFormat,
}

/// A partial transcription result (intermediate).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartialResult {
    pub text: String,
    pub is_final: bool,
}

/// Messages sent from client to server over WebSocket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum RealtimeClientMessage {
    #[serde(rename = "start")]
    Start { config: RealtimeConfig },
    #[serde(rename = "binary")]
    Binary { data: Vec<u8> },
    #[serde(rename = "complete")]
    Complete,
    #[serde(rename = "cancel")]
    Cancel,
}

/// Messages sent from server to client over WebSocket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum RealtimeServerMessage {
    #[serde(rename = "started")]
    Started { session_id: SessionId },
    #[serde(rename = "partial")]
    Partial { text: String },
    #[serde(rename = "final")]
    Final {
        text: String,
        segments: Vec<TranscriptionSegment>,
    },
    #[serde(rename = "completed")]
    Completed { session_id: SessionId },
    #[serde(rename = "error")]
    Error { code: String, message: String },
}

// ── Health Types ─────────────────────────────────────────────

/// Health check response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
}

/// Readiness check response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadinessResponse {
    pub ready: bool,
    pub reason: Option<String>,
}
