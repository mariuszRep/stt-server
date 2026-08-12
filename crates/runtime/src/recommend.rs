//! Hardware-driven recommendations, factored out of the HTTP route so the
//! CLI's `stt recommend` can produce the same answer without a running
//! daemon.

use crate::hardware::HardwareReport;

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRecommendation {
    pub provider_id: String,
    pub model_id: String,
    pub device: String,
    pub reason: String,
}

/// Coarse hardware-driven recommendation. Real CUDA runtime viability is
/// only known once the runtime itself has tried and reported back; this
/// only decides which curated model/device combo to suggest requesting.
pub fn recommend(hardware: &HardwareReport) -> Vec<ModelRecommendation> {
    let (model_id, device, reason) = if hardware.has_nvidia_gpu {
        (
            "Systran/faster-whisper-small",
            "cuda",
            "NVIDIA GPU detected; small model balances speed and accuracy on GPU.",
        )
    } else {
        (
            "Systran/faster-whisper-tiny",
            "cpu",
            "No NVIDIA GPU detected; tiny model keeps CPU-only transcription responsive.",
        )
    };

    vec![ModelRecommendation {
        provider_id: "faster-whisper".to_string(),
        model_id: model_id.to_string(),
        device: device.to_string(),
        reason: reason.to_string(),
    }]
}
