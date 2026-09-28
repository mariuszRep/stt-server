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
    /// Unused (user decision): we do not use Handy's `blob.handy.computer`
    /// mirror without that project's permission, so downloads are
    /// HuggingFace-only (see `download::candidate_urls`). Kept deserializable
    /// so the embedded catalog JSON stays byte-identical to upstream.
    #[serde(default)]
    pub mirrors: Vec<String>,
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

/// Find a catalog model/file whose size and SHA-256 match a candidate
/// drop-in file, across every model and quant. Used by `crate::dropin` to
/// register a drop-in file under its catalog identity when the bytes match a
/// known artifact, regardless of which quant or model happened to produce it.
pub fn catalog_match_by_hash<'a>(
    catalog: &'a [CatalogModel],
    size_bytes: u64,
    sha256: &str,
) -> Option<(&'a CatalogModel, &'a CatalogFile)> {
    for model in catalog {
        for file in &model.files {
            if file.size_bytes == size_bytes && file.sha256.eq_ignore_ascii_case(sha256) {
                return Some((model, file));
            }
        }
    }
    None
}

/// Catalog (unloaded) capability view: the *best truthful static answer*
/// derivable from architecture/catalog metadata alone, before any engine has
/// looked at the file. Unlike the effective (loaded-model) view in
/// `capabilities::EffectiveCaps`, nothing here has been verified against a
/// real run, so a claim the architecture does not rule out is reported
/// "unknown" (unverified), never "supported" outright; only a control the
/// catalog metadata or architecture itself rules out is "unsupported". This
/// mirrors how Handy trusts its per-architecture engine dispatch, not a
/// blanket claim field, to decide what is real; the catalog can only echo
/// the claim here, the selected-model endpoint (`EffectiveCaps`) gives the
/// live answer. See `docs/client-contract.md` for the documented difference
/// between the two endpoints.
pub fn capability_matrix(model: &CatalogModel) -> Value {
    let multi_language = model.languages.len() > 1;
    let has_timestamps = model.capabilities.timestamps != "none";
    // Prompt/temperature are only wired in *our own code* through the
    // whisper run-extension slot (`run_plan::to_run_options`'s
    // `RunExtension::Whisper(WhisperRunOptions)`), so today they only ever
    // work on `architecture == "whisper"` — this is a statement about our
    // implementation, not a hard architecture limit in transcribe-cpp
    // (voxtral's own `capabilities.cpp` sets `FEATURE_INITIAL_PROMPT` too,
    // via its own free-text-instruction mechanism; we just haven't wired
    // that mechanism, hence `reason: "not_implemented"` rather than
    // `"model_lacks"` below for that case).
    let is_whisper_arch = model.architecture == "whisper";
    // Timestamps: NOT architecture-restricted here. Verified against
    // transcribe-cpp-sys 0.2.3's vendored C++ (see the long comment on
    // `capabilities::EffectiveCaps::max_timestamp_granularity`) that this
    // catalog's own per-model `capabilities.timestamps` claim already
    // matches each architecture's real, engine-reported
    // `max_timestamp_kind` -- including `"token"` for gigaam-v3-*/medasr
    // and `"word"` for granite-speech-4.1-2b-plus, which an earlier,
    // Handy-copied "whisper/parakeet only" allowlist here incorrectly
    // reported as ruled out. Trust the catalog claim directly, like every
    // other control in this function.
    let claim = |claimed: bool, model_claim: Value, arch_ruled_out: bool| {
        if arch_ruled_out || !claimed {
            json!({
                "status": "unsupported",
                "reason": if !claimed { "model_lacks" } else { "not_implemented" },
                "model_claim": model_claim,
            })
        } else {
            // Claimed by the catalog and not ruled out by architecture:
            // unverified until a real model is loaded.
            json!({
                "status": "unknown",
                "reason": "unverified",
                "model_claim": model_claim,
            })
        }
    };
    json!({
        "prompt": claim(is_whisper_arch, Value::Null, !is_whisper_arch),
        "temperature": claim(is_whisper_arch, Value::Null, !is_whisper_arch),
        "language_hint": claim(
            multi_language && model.architecture != "granite",
            json!(model.languages),
            false,
        ),
        "language_detect": claim(model.capabilities.lang_detect, json!(model.capabilities.lang_detect), false),
        "translation": claim(model.capabilities.translate, json!(model.capabilities.translate), false),
        "timestamp_granularity": claim(has_timestamps, json!(model.capabilities.timestamps), false),
        "streaming": {"status": "unsupported", "reason": "not_implemented", "model_claim": model.capabilities.streaming},
        "response_formats": {"json": "supported", "text": "supported", "verbose_json": "unknown"},
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
        "downloaded": installed_quant.is_some(),
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
        assert!(matrix.get("vocabulary").is_none());
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
        // Nemotron's catalog entry claims `lang_detect: true`; the catalog
        // view cannot verify that (no engine has looked at the file yet),
        // so it must be "unknown" (unverified), not a blanket "unsupported"
        // -- that blanket answer was bug 3.
        assert_eq!(
            capability_matrix(nemotron)["language_detect"]["status"],
            "unknown"
        );
    }

    #[test]
    fn capability_matrix_for_a_whisper_model_reports_unknown_where_architecture_supports_it() {
        let catalog: Catalog =
            serde_json::from_str(include_str!("../catalog/handy-2026-08-17.json")).unwrap();
        let whisper = catalog
            .models
            .iter()
            .find(|model| model.slug.starts_with("whisper-") && model.languages.len() > 1)
            .expect("expected a multilingual whisper model in the catalog");
        let matrix = capability_matrix(whisper);
        // Whisper is the architecture that actually supports prompt and
        // multi-language hints (see `capabilities::whisper_gate`); the
        // catalog can rule out neither, so it reports "unknown" (unverified)
        // rather than a blanket "unsupported" — that was bug 3.
        assert_eq!(matrix["prompt"]["status"], "unknown");
        assert_eq!(matrix["prompt"]["reason"], "unverified");
        assert_eq!(matrix["language_hint"]["status"], "unknown");
        assert_eq!(matrix["language_hint"]["reason"], "unverified");
        assert_eq!(matrix["streaming"]["status"], "unsupported");
        assert_eq!(
            matrix["streaming"]["model_claim"],
            whisper.capabilities.streaming
        );
    }

    #[test]
    fn capability_matrix_never_reports_a_blanket_unsupported_for_every_control() {
        // Bug 3 regression: the catalog list must give a truthful static
        // answer, not hard-code "unsupported" for every capability of every
        // model regardless of what the catalog metadata itself claims.
        let catalog: Catalog =
            serde_json::from_str(include_str!("../catalog/handy-2026-08-17.json")).unwrap();
        let saw_non_unsupported = catalog.models.iter().any(|model| {
            let matrix = capability_matrix(model);
            [
                "prompt",
                "language_hint",
                "language_detect",
                "translation",
                "timestamp_granularity",
            ]
            .iter()
            .any(|control| matrix[control]["status"] != "unsupported")
        });
        assert!(
            saw_non_unsupported,
            "expected at least one model/control pair to report something other than unsupported"
        );
    }

    #[test]
    fn capability_matrix_trusts_the_catalog_timestamps_claim_for_every_architecture() {
        // Corrected understanding of Bug 1 (see the long comment on
        // `capabilities::EffectiveCaps::max_timestamp_granularity`):
        // gigaam-v3-*/medasr genuinely support token-level timestamps in
        // transcribe-cpp (family default `TRANSCRIBE_TIMESTAMPS_TOKEN`,
        // never lowered), and granite-speech-4.1-2b-plus genuinely supports
        // word-level timestamps (its GGUF sets `stt.capability.
        // word_timestamps`) -- this catalog's own `capabilities.timestamps`
        // field already reflects that per-model, verified against the
        // vendored engine source. An earlier revision of this function
        // wrongly ruled these out with a "whisper/parakeet only" allowlist
        // copied from Handy's differently-scoped engine dispatch; that was
        // itself a bug, not a fix. A claim of anything other than "none"
        // must surface as "unknown" (unverified until loaded), never
        // "unsupported".
        let catalog: Catalog =
            serde_json::from_str(include_str!("../catalog/handy-2026-08-17.json")).unwrap();
        for slug in ["gigaam-v3-ctc", "medasr", "granite-speech-4.1-2b-plus"] {
            let model = catalog
                .models
                .iter()
                .find(|m| m.slug == slug)
                .unwrap_or_else(|| panic!("expected {slug} in the catalog"));
            assert_ne!(model.capabilities.timestamps, "none", "{slug}");
            let matrix = capability_matrix(model);
            assert_eq!(
                matrix["timestamp_granularity"]["status"], "unknown",
                "{slug} claims {} timestamps and should be unknown (unverified), not unsupported",
                model.capabilities.timestamps
            );
        }
        // A model whose catalog entry genuinely claims no timestamps (e.g.
        // canary, whose transcribe-cpp family default is
        // `TRANSCRIBE_TIMESTAMPS_NONE` and is never overlaid from GGUF KV)
        // is correctly ruled out "unsupported".
        let canary = catalog
            .models
            .iter()
            .find(|m| m.architecture == "canary")
            .expect("expected a canary model in the catalog");
        assert_eq!(canary.capabilities.timestamps, "none");
        assert_eq!(
            capability_matrix(canary)["timestamp_granularity"]["status"],
            "unsupported"
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
            assert_eq!(view["downloaded"], false);
            assert!(!view["files"].as_array().unwrap().is_empty());
        }
    }
}
