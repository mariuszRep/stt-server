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
    /// Mirror information is not implemented yet; kept deserializable so the
    /// embedded catalog stays byte-identical to upstream and forward-compatible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mirrors: Option<Value>,
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

/// Pure validation: resolve the requested quant (or the model's default when
/// absent/empty) to a catalog file. Returns 400 invalid_quant when unknown.
pub fn resolve_quant<'a>(
    model: &'a CatalogModel,
    requested: Option<&str>,
) -> ApiResult<&'a CatalogFile> {
    let quant = match requested {
        Some(value) if !value.is_empty() => value,
        _ => model.default_quant.as_str(),
    };
    model
        .files
        .iter()
        .find(|file| file.quant == quant)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_quant",
                format!("Unknown quant '{quant}' for model {}", model.slug),
            )
        })
}

pub fn capability_matrix(model: &CatalogModel) -> Value {
    let multi_language = model.languages.len() > 1;
    let has_timestamps = model.capabilities.timestamps != "none";
    let unimplemented = |model_claim: Value, lacks: bool| {
        json!({
            "status": "unsupported",
            "reason": if lacks { "model_lacks" } else { "not_implemented" },
            "model_claim": model_claim,
        })
    };
    json!({
        "prompt": unimplemented(Value::Null, false),
        "vocabulary": unimplemented(Value::Null, false),
        "temperature": unimplemented(Value::Null, false),
        "language_hint": unimplemented(json!(model.languages), !multi_language),
        "language_detect": unimplemented(json!(model.capabilities.lang_detect), !model.capabilities.lang_detect),
        "translation": unimplemented(json!(model.capabilities.translate), !model.capabilities.translate),
        "timestamp_granularity": unimplemented(json!(model.capabilities.timestamps), !has_timestamps),
        "streaming": {"status": "unsupported", "model_claim": model.capabilities.streaming},
        "response_formats": {"json": "supported", "text": "unsupported", "verbose_json": "unsupported"},
    })
}

pub fn model_view(model: &CatalogModel, installed_quant: Option<&str>) -> Value {
    let files: Vec<Value> = model
        .files
        .iter()
        .map(|file| {
            json!({
                "filename": file.filename,
                "quant": file.quant,
                "size_bytes": file.size_bytes,
                "sha256": file.sha256,
            })
        })
        .collect();
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
        "files": files,
        "speed_score": model.speed_score,
        "accuracy_score": model.accuracy_score,
        "benchmark_source": "Handy catalog generated 2026-08-17; scores are derived display values, not local measurements",
        "recommended_rank": model.recommended_rank,
        "installed": installed_quant.is_some(),
        "installed_quant": installed_quant,
        "installable": true,
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

    #[test]
    fn capability_matrix_for_a_whisper_model_reports_model_lacks_where_claims_are_absent() {
        let catalog: Catalog =
            serde_json::from_str(include_str!("../catalog/handy-2026-08-17.json")).unwrap();
        let whisper = catalog
            .models
            .iter()
            .find(|model| model.slug.starts_with("whisper-") && model.languages.len() > 1)
            .expect("expected a multilingual whisper model in the catalog");
        let matrix = capability_matrix(whisper);
        assert_eq!(matrix["prompt"]["status"], "unsupported");
        assert_eq!(matrix["prompt"]["reason"], "not_implemented");
        assert_eq!(matrix["language_hint"]["status"], "unsupported");
        assert_eq!(matrix["language_hint"]["reason"], "not_implemented");
        assert_eq!(matrix["streaming"]["status"], "unsupported");
        assert_eq!(
            matrix["streaming"]["model_claim"],
            whisper.capabilities.streaming
        );
    }

    #[test]
    fn capability_matrix_for_a_single_language_model_marks_language_hint_as_model_lacks() {
        let catalog: Catalog =
            serde_json::from_str(include_str!("../catalog/handy-2026-08-17.json")).unwrap();
        let single_language = catalog
            .models
            .iter()
            .find(|model| model.languages.len() == 1)
            .expect("expected a single-language model in the catalog");
        let matrix = capability_matrix(single_language);
        assert_eq!(matrix["language_hint"]["status"], "unsupported");
        assert_eq!(matrix["language_hint"]["reason"], "model_lacks");
        if !single_language.capabilities.translate {
            assert_eq!(matrix["translation"]["reason"], "model_lacks");
        }
        if !single_language.capabilities.lang_detect {
            assert_eq!(matrix["language_detect"]["reason"], "model_lacks");
        }
    }

    #[test]
    fn resolve_quant_validates_default_explicit_and_invalid() {
        let catalog: Catalog =
            serde_json::from_str(include_str!("../catalog/handy-2026-08-17.json")).unwrap();
        let model = catalog
            .models
            .iter()
            .find(|model| model.slug == "parakeet-unified-en-0.6b")
            .unwrap();
        let default = resolve_quant(model, None).unwrap();
        assert_eq!(default.quant, model.default_quant);
        let explicit = resolve_quant(model, Some(&model.files[0].quant)).unwrap();
        assert_eq!(explicit.quant, model.files[0].quant);
        assert!(resolve_quant(model, Some("not-a-real-quant")).is_err());
    }

    #[test]
    fn every_catalog_model_is_installable_in_its_view() {
        let catalog: Catalog =
            serde_json::from_str(include_str!("../catalog/handy-2026-08-17.json")).unwrap();
        for model in &catalog.models {
            let view = model_view(model, None);
            assert_eq!(view["installable"], true, "{}", model.slug);
            assert_eq!(view["installed"], false);
            assert!(!view["files"].as_array().unwrap().is_empty());
        }
    }
}
