use std::fmt;

/// Canonical error type for stt-server.
#[derive(Debug, thiserror::Error)]
pub enum SttError {
    #[error("invalid model ID: {0}")]
    InvalidModelId(String),

    #[error("model not found: {0}")]
    ModelNotFound(String),

    #[error("model already loaded: {0}")]
    ModelAlreadyLoaded(String),

    #[error("model verification failed: {0}")]
    ModelVerificationFailed(String),

    #[error("adapter error: {0}")]
    AdapterError(String),

    #[error("audio error: {0}")]
    AudioError(String),

    #[error("transcription error: {0}")]
    TranscriptionError(String),

    #[error("session error: {0}")]
    SessionError(String),

    #[error("configuration error: {0}")]
    ConfigError(String),

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    SerializationError(#[from] serde_json::Error),

    #[error("internal error: {0}")]
    InternalError(String),
}

impl SttError {
    /// Get the error code for structured error responses.
    pub fn code(&self) -> &'static str {
        match self {
            SttError::InvalidModelId(_) => "INVALID_MODEL_ID",
            SttError::ModelNotFound(_) => "MODEL_NOT_FOUND",
            SttError::ModelAlreadyLoaded(_) => "MODEL_ALREADY_LOADED",
            SttError::ModelVerificationFailed(_) => "MODEL_VERIFICATION_FAILED",
            SttError::AdapterError(_) => "ADAPTER_ERROR",
            SttError::AudioError(_) => "AUDIO_ERROR",
            SttError::TranscriptionError(_) => "TRANSCRIPTION_ERROR",
            SttError::SessionError(_) => "SESSION_ERROR",
            SttError::ConfigError(_) => "CONFIG_ERROR",
            SttError::IoError(_) => "IO_ERROR",
            SttError::SerializationError(_) => "SERIALIZATION_ERROR",
            SttError::InternalError(_) => "INTERNAL_ERROR",
        }
    }
}

/// Structured error response for API endpoints.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct ErrorResponse {
    pub code: String,
    pub message: String,
}

impl From<&SttError> for ErrorResponse {
    fn from(err: &SttError) -> Self {
        Self {
            code: err.code().to_string(),
            message: err.to_string(),
        }
    }
}

impl From<SttError> for ErrorResponse {
    fn from(err: SttError) -> Self {
        Self::from(&err)
    }
}

impl fmt::Display for ErrorResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}
