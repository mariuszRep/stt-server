use serde::{Deserialize, Serialize};

// ── Model Identity ───────────────────────────────────────────

/// Model identifier (safe, validated string).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelIdentifier(String);

impl ModelIdentifier {
    /// Create a new model identifier, validating it is safe.
    pub fn new(id: impl Into<String>) -> Result<Self, crate::SttError> {
        let id = id.into();
        if id.is_empty() {
            return Err(crate::SttError::InvalidModelId(
                "model ID cannot be empty".into(),
            ));
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

/// Model verification result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelVerification {
    pub valid: bool,
    pub checksum: Option<String>,
    pub error: Option<String>,
}

// ── Runtime Connection Descriptor ───────────────────────────────
//
// Mirrors `@open-vibe-ai/stt-sdk`'s `RuntimeConnectionDescriptor` (see
// stt-sdk/src/types.ts) field-for-field. This is the sole handoff contract
// between the control plane and SDK clients: the server never proxies
// transcription traffic itself, it only issues descriptors that tell a
// client how to reach a runtime it has started.

/// Schema version of [`RuntimeConnectionDescriptor`]. Must match
/// `RUNTIME_DESCRIPTOR_SCHEMA_VERSION` in `@open-vibe-ai/stt-sdk`.
pub const RUNTIME_DESCRIPTOR_SCHEMA_VERSION: u32 = 1;

/// Mirrors the `streaming` block of a managed runtime's `GET /v1/config` response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamingCapability {
    pub enabled: bool,
    pub endpoint: String,
    pub protocol_version: u32,
    pub encodings: Vec<String>,
    pub sample_rates: Vec<u32>,
    pub resample: bool,
    pub channels: Vec<u16>,
}

/// Auth material a client must present to the managed runtime, if any.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DescriptorAuth {
    #[serde(rename = "type")]
    pub auth_type: String,
    pub value: String,
}

/// Versioned connection descriptor for a local runtime, issued by `stt-server`.
///
/// The SDK accepts these descriptors; `stt-server` never proxies audio or
/// implements a provider client itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeConnectionDescriptor {
    pub schema_version: u32,
    pub provider: String,
    pub protocol: String,
    pub transport: String,
    pub base_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub streaming: Option<StreamingCapability>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<DescriptorAuth>,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact JSON shape `stt-sdk`'s `createProvider()` requires: strict
    /// literal checks on `schemaVersion`/`protocol`/`provider`, camelCase
    /// field names, and only `streaming.endpoint` consumed today. Any drift
    /// here breaks SDK consumers silently, so this is pinned to a literal
    /// fixture rather than just round-tripped.
    #[test]
    fn runtime_connection_descriptor_matches_sdk_contract() {
        let descriptor = RuntimeConnectionDescriptor {
            schema_version: RUNTIME_DESCRIPTOR_SCHEMA_VERSION,
            provider: "faster-whisper".to_string(),
            protocol: "voice-typer-v1".to_string(),
            transport: "http".to_string(),
            base_url: "http://127.0.0.1:51234".to_string(),
            streaming: Some(StreamingCapability {
                enabled: true,
                endpoint: "/v1/audio/stream".to_string(),
                protocol_version: 1,
                encodings: vec!["pcm_s16le".to_string()],
                sample_rates: vec![16000, 44100, 48000],
                resample: true,
                channels: vec![1],
            }),
            auth: Some(DescriptorAuth {
                auth_type: "token".to_string(),
                value: "secret".to_string(),
            }),
        };

        let json = serde_json::to_value(&descriptor).unwrap();
        let expected = serde_json::json!({
            "schemaVersion": 1,
            "provider": "faster-whisper",
            "protocol": "voice-typer-v1",
            "transport": "http",
            "baseUrl": "http://127.0.0.1:51234",
            "streaming": {
                "enabled": true,
                "endpoint": "/v1/audio/stream",
                "protocolVersion": 1,
                "encodings": ["pcm_s16le"],
                "sampleRates": [16000, 44100, 48000],
                "resample": true,
                "channels": [1]
            },
            "auth": { "type": "token", "value": "secret" }
        });

        assert_eq!(json, expected);

        let round_tripped: RuntimeConnectionDescriptor = serde_json::from_value(json).unwrap();
        assert_eq!(round_tripped, descriptor);
    }

    #[test]
    fn runtime_connection_descriptor_omits_absent_optional_fields() {
        let descriptor = RuntimeConnectionDescriptor {
            schema_version: RUNTIME_DESCRIPTOR_SCHEMA_VERSION,
            provider: "faster-whisper".to_string(),
            protocol: "voice-typer-v1".to_string(),
            transport: "http".to_string(),
            base_url: "http://127.0.0.1:51234".to_string(),
            streaming: None,
            auth: None,
        };

        let json = serde_json::to_value(&descriptor).unwrap();
        assert!(json.get("streaming").is_none());
        assert!(json.get("auth").is_none());
    }
}
