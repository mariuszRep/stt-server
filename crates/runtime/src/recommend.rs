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
    if hardware.has_nvidia_gpu {
        vec![
            ModelRecommendation {
                provider_id: "faster-whisper".to_string(),
                model_id: "Systran/faster-distil-whisper-small.en".to_string(),
                device: "cuda".to_string(),
                reason: "NVIDIA GPU detected; distilled small model matches small's accuracy at about half the latency."
                    .to_string(),
            },
            ModelRecommendation {
                provider_id: "sherpa-onnx".to_string(),
                model_id: "parakeet-tdt-0.6b-v2".to_string(),
                device: "cpu".to_string(),
                reason: "Fast CPU fallback when the CUDA runtime is unavailable.".to_string(),
            },
        ]
    } else {
        vec![
            ModelRecommendation {
                provider_id: "sherpa-onnx".to_string(),
                model_id: "parakeet-tdt-0.6b-v2".to_string(),
                device: "cpu".to_string(),
                reason: "No NVIDIA GPU detected; Parakeet provides the fastest curated CPU transcription option."
                    .to_string(),
            },
            ModelRecommendation {
                provider_id: "faster-whisper".to_string(),
                model_id: "Systran/faster-whisper-tiny.en".to_string(),
                device: "cpu".to_string(),
                reason: "CPU fallback for users who need a Whisper model.".to_string(),
            },
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hardware(has_nvidia_gpu: bool) -> HardwareReport {
        HardwareReport {
            has_nvidia_gpu,
            gpu_name: has_nvidia_gpu.then(|| "Test GPU".to_string()),
            driver_version: None,
            vram_bytes: None,
            cpu_cores: 8,
            cpu_architecture: "x86_64".to_string(),
            total_ram_bytes: 16 * 1024 * 1024 * 1024,
        }
    }

    #[test]
    fn recommends_faster_whisper_first_with_nvidia_gpu() {
        let recommendations = recommend(&hardware(true));

        assert_eq!(recommendations.len(), 2);
        assert_eq!(recommendations[0].provider_id, "faster-whisper");
        assert_eq!(recommendations[0].device, "cuda");
        assert_eq!(recommendations[1].provider_id, "sherpa-onnx");
    }

    #[test]
    fn recommends_parakeet_first_without_nvidia_gpu() {
        let recommendations = recommend(&hardware(false));

        assert_eq!(recommendations.len(), 2);
        assert_eq!(recommendations[0].provider_id, "sherpa-onnx");
        assert_eq!(recommendations[0].model_id, "parakeet-tdt-0.6b-v2");
        assert_eq!(recommendations[1].provider_id, "faster-whisper");
    }
}
