//! Curated provider catalog and compatibility evaluation.
//!
//! Real artifact fetch/install wiring for each catalog entry lands when the
//! managed runtime is actually packaged (faster-whisper first); this module
//! defines the shape every provider entry has and the control-plane's view
//! of "what's available," independent of any one provider's install state.

use crate::error::RuntimeError;
use crate::hardware::HardwareReport;

/// Validated provider identifier — never a raw filesystem path.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ProviderId(String);

impl ProviderId {
    pub fn new(id: impl Into<String>) -> Result<Self, RuntimeError> {
        let id = id.into();
        if id.is_empty() {
            return Err(RuntimeError::InvalidProviderId(
                "provider ID cannot be empty".into(),
            ));
        }
        if id.len() > 128 {
            return Err(RuntimeError::InvalidProviderId(
                "provider ID too long".into(),
            ));
        }
        if !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(RuntimeError::InvalidProviderId(format!(
                "provider ID contains invalid characters: {id}"
            )));
        }
        Ok(Self(id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ProviderId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A build flavor of a provider's managed runtime — same provider, same
/// wire protocol, different binary (and, for `Gpu`, a much larger one with
/// bundled CUDA/cuDNN). Not a plugin system: this project has one provider
/// today with exactly two flavors, so a closed enum is the right amount of
/// structure — not a registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeVariant {
    Cpu,
    Gpu,
}

impl RuntimeVariant {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Gpu => "gpu",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "cpu" => Some(Self::Cpu),
            "gpu" => Some(Self::Gpu),
            _ => None,
        }
    }
}

impl std::fmt::Display for RuntimeVariant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// A provider variant's availability on this machine, reported over the
/// API so a caller can decide whether to offer/recommend installing it
/// *before* asking for a (possibly large) download.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VariantInfo {
    pub variant: String,
    pub compatible: bool,
    pub recommended: bool,
    pub reason: Option<String>,
}

/// GPU is only ever compatible/recommended when an NVIDIA GPU was
/// detected; CPU is always compatible and recommended only in the absence
/// of one. Listed either way (never hidden) so a disabled option in a UI
/// can still show *why* — consistent with "explicit, observable" installs.
fn evaluate_variant(variant: RuntimeVariant, hardware: &HardwareReport) -> VariantInfo {
    match variant {
        RuntimeVariant::Cpu => VariantInfo {
            variant: variant.as_str().to_string(),
            compatible: true,
            recommended: !hardware.has_nvidia_gpu,
            reason: None,
        },
        RuntimeVariant::Gpu => VariantInfo {
            variant: variant.as_str().to_string(),
            compatible: hardware.has_nvidia_gpu,
            recommended: hardware.has_nvidia_gpu,
            reason: (!hardware.has_nvidia_gpu).then(|| "no NVIDIA GPU detected".to_string()),
        },
    }
}

/// A curated model choice for a provider. For faster-whisper specifically,
/// this is just the HuggingFace model name the managed runtime is told to
/// load (`VOICE_TYPER_MODEL`) — the runtime downloads/caches it itself on
/// first use via `ctranslate2`'s HuggingFace integration, so there is no
/// separate file for the control plane to fetch, verify, or store.
#[derive(Debug, Clone, Copy)]
pub struct ModelEntry {
    pub id: &'static str,
    pub display_name: &'static str,
}

/// A single curated provider's static metadata.
#[derive(Debug, Clone, Copy)]
pub struct CatalogEntry {
    pub id: &'static str,
    pub display_name: &'static str,
    /// Value written into `RuntimeConnectionDescriptor.protocol`.
    pub protocol: &'static str,
    /// Value written into `RuntimeConnectionDescriptor.transport`.
    pub transport: &'static str,
    /// Path polled on the runtime's own port to confirm it's healthy.
    pub health_path: &'static str,
    pub models: &'static [ModelEntry],
    pub default_model: &'static str,
    pub variants: &'static [RuntimeVariant],
}

pub const CATALOG: &[CatalogEntry] = &[CatalogEntry {
    id: "faster-whisper",
    display_name: "Faster Whisper",
    protocol: "voice-typer-v1",
    transport: "http",
    health_path: "/health",
    default_model: "Systran/faster-whisper-small",
    variants: &[RuntimeVariant::Cpu, RuntimeVariant::Gpu],
    models: &[
        ModelEntry {
            id: "Systran/faster-whisper-tiny",
            display_name: "Tiny",
        },
        ModelEntry {
            id: "Systran/faster-whisper-base",
            display_name: "Base",
        },
        ModelEntry {
            id: "Systran/faster-whisper-small",
            display_name: "Small",
        },
        ModelEntry {
            id: "Systran/faster-whisper-medium",
            display_name: "Medium",
        },
        ModelEntry {
            id: "Systran/faster-whisper-large-v3",
            display_name: "Large v3",
        },
    ],
}];

/// Provider catalog entry as reported over the control-plane API, with
/// compatibility evaluated against the current machine's hardware.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderInfo {
    pub id: String,
    pub display_name: String,
    pub protocol: String,
    pub transport: String,
    pub compatible: bool,
    pub compatibility_reason: Option<String>,
    pub variants: Vec<VariantInfo>,
}

pub fn list_providers(hardware: &HardwareReport) -> Vec<ProviderInfo> {
    CATALOG
        .iter()
        .map(|entry| {
            let (compatible, reason) = evaluate_compatibility(entry, hardware);
            ProviderInfo {
                id: entry.id.to_string(),
                display_name: entry.display_name.to_string(),
                protocol: entry.protocol.to_string(),
                transport: entry.transport.to_string(),
                compatible,
                compatibility_reason: reason,
                variants: entry
                    .variants
                    .iter()
                    .map(|v| evaluate_variant(*v, hardware))
                    .collect(),
            }
        })
        .collect()
}

pub fn find_provider(id: &ProviderId) -> Result<&'static CatalogEntry, RuntimeError> {
    CATALOG
        .iter()
        .find(|entry| entry.id == id.as_str())
        .ok_or_else(|| RuntimeError::ProviderNotFound(id.to_string()))
}

pub fn find_model(entry: &CatalogEntry, model_id: &str) -> Option<&'static ModelEntry> {
    entry.models.iter().find(|m| m.id == model_id)
}

/// faster-whisper always runs (CPU fallback is unconditional); hardware only
/// changes which install variant / `device` env var gets used at start, not
/// eligibility. This function exists as the seam later providers (e.g.
/// whisper.cpp, which may have real minimum requirements) will hook into.
fn evaluate_compatibility(
    _entry: &CatalogEntry,
    _hardware: &HardwareReport,
) -> (bool, Option<String>) {
    (true, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_path_traversal_provider_ids() {
        assert!(ProviderId::new("../etc/passwd").is_err());
        assert!(ProviderId::new("foo/bar").is_err());
        assert!(ProviderId::new("").is_err());
        assert!(ProviderId::new("faster-whisper").is_ok());
    }

    #[test]
    fn find_provider_rejects_unknown_id() {
        let id = ProviderId::new("does-not-exist").unwrap();
        assert!(matches!(
            find_provider(&id),
            Err(RuntimeError::ProviderNotFound(_))
        ));
    }

    #[test]
    fn find_provider_returns_faster_whisper() {
        let id = ProviderId::new("faster-whisper").unwrap();
        assert_eq!(find_provider(&id).unwrap().id, "faster-whisper");
    }

    #[test]
    fn gpu_variant_incompatible_without_nvidia_gpu() {
        let hardware = HardwareReport {
            has_nvidia_gpu: false,
            gpu_name: None,
            driver_version: None,
            cpu_cores: 4,
            total_ram_bytes: 0,
        };
        let info = evaluate_variant(RuntimeVariant::Gpu, &hardware);
        assert!(!info.compatible);
        assert!(!info.recommended);
        assert!(info.reason.is_some());

        let cpu_info = evaluate_variant(RuntimeVariant::Cpu, &hardware);
        assert!(cpu_info.compatible);
        assert!(
            cpu_info.recommended,
            "cpu should be recommended absent a GPU"
        );
    }

    #[test]
    fn gpu_variant_compatible_and_recommended_with_nvidia_gpu() {
        let hardware = HardwareReport {
            has_nvidia_gpu: true,
            gpu_name: Some("Test GPU".to_string()),
            driver_version: None,
            cpu_cores: 4,
            total_ram_bytes: 0,
        };
        let info = evaluate_variant(RuntimeVariant::Gpu, &hardware);
        assert!(info.compatible);
        assert!(info.recommended);
        assert!(info.reason.is_none());

        let cpu_info = evaluate_variant(RuntimeVariant::Cpu, &hardware);
        assert!(cpu_info.compatible);
        assert!(
            !cpu_info.recommended,
            "gpu should be recommended over cpu when present"
        );
    }

    #[test]
    fn runtime_variant_parse_round_trips_as_str() {
        assert_eq!(RuntimeVariant::parse("cpu"), Some(RuntimeVariant::Cpu));
        assert_eq!(RuntimeVariant::parse("gpu"), Some(RuntimeVariant::Gpu));
        assert_eq!(RuntimeVariant::parse("bogus"), None);
        assert_eq!(RuntimeVariant::Cpu.as_str(), "cpu");
        assert_eq!(RuntimeVariant::Gpu.as_str(), "gpu");
    }

    #[test]
    fn list_providers_includes_both_variants_for_faster_whisper() {
        let hardware = HardwareReport {
            has_nvidia_gpu: false,
            gpu_name: None,
            driver_version: None,
            cpu_cores: 4,
            total_ram_bytes: 0,
        };
        let providers = list_providers(&hardware);
        let fw = providers.iter().find(|p| p.id == "faster-whisper").unwrap();
        assert_eq!(fw.variants.len(), 2);
        assert!(fw.variants.iter().any(|v| v.variant == "cpu"));
        assert!(fw.variants.iter().any(|v| v.variant == "gpu"));
    }
}
