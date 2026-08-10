
/// Adapter-specific error type.
#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    #[error("model not found: {0}")]
    ModelNotFound(String),

    #[error("model load failed: {0}")]
    ModelLoadFailed(String),

    #[error("model verification failed: {0}")]
    ModelVerificationFailed(String),

    #[error("transcription failed: {0}")]
    TranscriptionFailed(String),

    #[error("session error: {0}")]
    SessionError(String),

    #[error("adapter internal error: {0}")]
    InternalError(String),

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("not supported: {0}")]
    NotSupported(String),
}

impl From<AdapterError> for stt_common::SttError {
    fn from(err: AdapterError) -> Self {
        match err {
            AdapterError::ModelNotFound(msg) => stt_common::SttError::ModelNotFound(msg),
            AdapterError::ModelLoadFailed(msg) => stt_common::SttError::AdapterError(msg),
            AdapterError::ModelVerificationFailed(msg) => {
                stt_common::SttError::ModelVerificationFailed(msg)
            }
            AdapterError::TranscriptionFailed(msg) => {
                stt_common::SttError::TranscriptionError(msg)
            }
            AdapterError::SessionError(msg) => stt_common::SttError::SessionError(msg),
            AdapterError::InternalError(msg) => stt_common::SttError::InternalError(msg),
            AdapterError::IoError(e) => stt_common::SttError::IoError(e),
            AdapterError::NotSupported(msg) => stt_common::SttError::AdapterError(msg),
        }
    }
}
