use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::app::App;
use crate::errors::{ApiError, ApiResult};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CatalogFile {
    pub filename: String,
    pub quant: String,
    pub size_bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModelClaims {
    pub streaming: bool,
    pub translate: bool,
    pub lang_detect: bool,
    pub timestamps: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CatalogModel {
    pub id: String,
    pub revision: String,
    pub slug: String,
    pub name: String,
    pub architecture: String,
    pub family: String,
    pub license: String,
    pub languages: Vec<String>,
    pub capabilities: ModelClaims,
    pub speed_score: Option<u32>,
    pub accuracy_score: Option<u32>,
    pub files: Vec<CatalogFile>,
    pub default_quant: String,
    pub recommended: bool,
    pub recommended_rank: Option<u32>,
}

#[derive(Deserialize)]
pub struct Catalog {
    pub models: Vec<CatalogModel>,
}

pub fn catalog_model<'a>(app: &'a App, id: &str) -> ApiResult<&'a CatalogModel> {
    app.catalog
        .iter()
        .find(|model| model.slug == id)
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "model_not_found", "Unknown model ID"))
}

pub fn capability_matrix(model: &CatalogModel) -> Value {
    let english_fixed = model.slug == "parakeet-unified-en-0.6b";
    json!({
        "prompt": {"status": if english_fixed { "unsupported" } else { "unknown" }, "mechanism": null},
        "vocabulary": {"status": if english_fixed { "unsupported" } else { "unknown" }, "mechanism": null},
        "language_hint": {"status": if english_fixed { "unsupported" } else { "unknown" }, "scope": "request"},
        "language_detect": {"status": "unsupported", "model_claim": model.capabilities.lang_detect},
        "translation": {"status": "unsupported", "model_claim": model.capabilities.translate},
        "temperature": {"status": "unsupported"},
        "response_formats": {"json": "supported", "text": "unsupported", "verbose_json": "unsupported"},
        "timestamp_granularity": {"status": "unknown", "model_claim": model.capabilities.timestamps},
        "streaming": {"status": "unsupported", "model_claim": model.capabilities.streaming}
    })
}

pub fn model_view(model: &CatalogModel, installed: bool) -> Value {
    let file = model
        .files
        .iter()
        .find(|file| file.quant == model.default_quant);
    json!({
        "id": model.slug,
        "name": model.name,
        "upstream_id": model.id,
        "revision": model.revision,
        "architecture": model.architecture,
        "family": model.family,
        "license": model.license,
        "languages": model.languages,
        "model_capabilities": model.capabilities,
        "effective_capabilities": capability_matrix(model),
        "default_quant": model.default_quant,
        "size_bytes": file.map(|f| f.size_bytes),
        "speed_score": model.speed_score,
        "accuracy_score": model.accuracy_score,
        "benchmark_source": "Handy catalog generated 2026-08-17; scores are derived display values, not local measurements",
        "recommended_rank": model.recommended_rank,
        "installed": installed,
        "installable": model.slug == "parakeet-unified-en-0.6b" && file.is_some(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recommendations_are_curated_and_capabilities_are_conservative() {
        let catalog: Catalog =
            serde_json::from_str(include_str!("../catalog/handy-2026-08-17.json")).unwrap();
        let mut recommended: Vec<_> = catalog
            .models
            .iter()
            .filter(|model| model.recommended)
            .collect();
        recommended.sort_by_key(|model| model.recommended_rank.unwrap_or(u32::MAX));
        assert_eq!(
            recommended.first().unwrap().slug,
            "parakeet-unified-en-0.6b"
        );
        let parakeet = recommended.first().unwrap();
        let matrix = capability_matrix(parakeet);
        assert_eq!(matrix["prompt"]["status"], "unsupported");
        assert_eq!(matrix["vocabulary"]["status"], "unsupported");
        assert_eq!(matrix["streaming"]["status"], "unsupported");
        let nemotron = catalog
            .models
            .iter()
            .find(|model| model.slug == "nemotron-3.5-asr-streaming-0.6b")
            .unwrap();
        assert_eq!(
            capability_matrix(nemotron)["streaming"]["status"],
            "unsupported"
        );
        assert_eq!(
            capability_matrix(nemotron)["language_detect"]["status"],
            "unsupported"
        );
    }
}
