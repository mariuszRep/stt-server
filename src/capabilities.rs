//! Capability views for models: the catalog (unloaded) view and the effective
//! (loaded, computed-from-the-live-model) view.
//!
//! See `parity-design.md` "Capabilities" for the decided design. Two views
//! exist because a catalog entry is an unverified claim about a GGUF file,
//! while the effective view is read from `transcribe_cpp::Model` once it is
//! actually loaded — the only place where "supported" is a fact rather than a
//! hope.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::catalog::{self, CatalogModel};

/// Requested (or default) timestamp granularity, independent of the
/// transcribe-cpp crate's own `TimestampKind` so this module has no compile
/// dependency on the engine for its pure, unit-tested half.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimestampGranularity {
    #[default]
    None,
    Segment,
    Word,
    Token,
}

impl TimestampGranularity {
    fn as_str(self) -> &'static str {
        match self {
            TimestampGranularity::None => "none",
            TimestampGranularity::Segment => "segment",
            TimestampGranularity::Word => "word",
            TimestampGranularity::Token => "token",
        }
    }
}

/// Everything `run_plan` needs about a *loaded* model's capabilities, decoupled
/// from `transcribe_cpp::Model` so it stays plain-data and unit-testable.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LoadedCaps {
    pub arch: String,
    pub languages: Vec<String>,
    pub translate_target_languages: Vec<String>,
    pub supports_translate: bool,
    pub supports_language_detect: bool,
    pub max_timestamp_kind: TimestampGranularity,
    /// Diagnostic-only per Handy issue #1601: never gate prompt/vocabulary
    /// support on this flag, it is unreliable. Exposed for visibility only.
    pub feature_initial_prompt_flag: bool,
    /// `Model::accepts_ext(ExtSlot::Run, TRANSCRIBE_EXT_KIND_WHISPER_RUN)`,
    /// when a public whisper run-extension kind constant exists. `None` when
    /// no such probe was available at build time.
    pub whisper_ext_accepted: Option<bool>,
    /// A known upper bound on `initial_prompt` length, in tokens, when the
    /// engine exposes one. Investigated against transcribe-cpp 0.2.3:
    /// `Model::capabilities()` (`Capabilities`) has no prompt-length field,
    /// `Session::limits()` (`SessionLimits`) exposes only `effective_n_ctx`
    /// (the whole decoder context budget, shared with generated output, not
    /// a documented prompt-specific cap) and `effective_max_audio_ms` /
    /// `max_kv_bytes`, and `WhisperRunOptions` only *sets*
    /// `max_prev_context_tokens` (a run input, not a queryable limit) — no
    /// public constant or accessor publishes a prompt-token ceiling. So this
    /// is always `None` in 0.2.3; kept as a field (rather than hard-coded
    /// `null`) so a future engine version that does expose one only needs a
    /// change in `from_model`, not in the plan/enforcement logic below.
    pub prompt_max_tokens: Option<usize>,
    /// Bug 1: set once an actual run against this loaded model rejected its
    /// own advertised `max_timestamp_kind` as unsupported at the engine
    /// level. Once set, the effective view stops claiming
    /// `timestamp_granularity` as supported for this loaded model — the
    /// claim has been observed to be wrong, even though the model itself
    /// reported it at load time.
    pub timestamp_granularity_rejected: bool,
    /// `Capabilities::supports_streaming`: the model can run through the
    /// live `GET /v1/realtime` endpoint (`Session::stream`).
    pub supports_streaming: bool,
    /// `Capabilities::supports_spec_decode`: n-gram speculative decoding.
    pub supports_spec_decode: bool,
    /// `Feature::Pnc`: the model exposes a runtime punctuation/casing toggle.
    pub supports_pnc: bool,
    /// `Feature::Itn`: the model exposes a runtime inverse-text-normalisation toggle.
    pub supports_itn: bool,
    /// `Feature::Cancellation`: the model honours the abort callback mid-run.
    pub supports_cancellation: bool,
    /// `Feature::LongForm`: audio longer than the native window in one run.
    pub supports_long_form: bool,
    /// `Capabilities::native_sample_rate` in Hz (0 when not reported).
    pub native_sample_rate: i32,
}

/// Controls the catalog has no claim for: they are `unknown` until a model is
/// loaded and the engine answers.
const ENGINE_ONLY_CONTROLS: [&str; 6] = [
    "punctuation",
    "itn",
    "cancellation",
    "long_form",
    "spec_decode",
    "native_sample_rate",
];

/// Add the engine-only controls to a pre-load matrix as `unknown`.
pub fn add_unverified_engine_controls(matrix: &mut Value) {
    for control in ENGINE_ONLY_CONTROLS {
        matrix[control] =
            json!({"status": "unknown", "reason": "unverified", "evidence": "catalog_claim"});
    }
}

impl LoadedCaps {
    /// Fill a `LoadedCaps` from a live, loaded model. Thin by design — this is
    /// the one function in this module that touches `transcribe_cpp::Model`
    /// and so is not unit-tested; keep any logic here to field plumbing only.
    pub fn from_model(model: &transcribe_cpp::Model) -> Self {
        use transcribe_cpp::{ExtSlot, Feature, TimestampKind};

        let caps = model.capabilities();
        let max_timestamp_kind = match caps.max_timestamp_kind {
            TimestampKind::None | TimestampKind::Auto => TimestampGranularity::None,
            TimestampKind::Segment => TimestampGranularity::Segment,
            TimestampKind::Word => TimestampGranularity::Word,
            TimestampKind::Token => TimestampGranularity::Token,
        };

        LoadedCaps {
            arch: model.arch(),
            languages: caps.languages,
            translate_target_languages: caps.translate_target_languages,
            supports_translate: caps.supports_translate,
            supports_language_detect: caps.supports_language_detect,
            max_timestamp_kind,
            feature_initial_prompt_flag: model.supports(Feature::InitialPrompt),
            whisper_ext_accepted: Some(model.accepts_ext(
                ExtSlot::Run,
                transcribe_cpp::sys::TRANSCRIBE_EXT_KIND_WHISPER_RUN,
            )),
            // See the field doc: no engine API in 0.2.3 publishes this.
            prompt_max_tokens: None,
            timestamp_granularity_rejected: false,
            supports_streaming: caps.supports_streaming,
            supports_spec_decode: caps.supports_spec_decode,
            supports_pnc: model.supports(Feature::Pnc),
            supports_itn: model.supports(Feature::Itn),
            supports_cancellation: model.supports(Feature::Cancellation),
            supports_long_form: model.supports(Feature::LongForm),
            native_sample_rate: caps.native_sample_rate,
        }
    }
}

/// The whisper gate: prompt/vocabulary/temperature require `arch() ==
/// "whisper"` AND, when the whisper run-extension kind probe is available,
/// that the model accepts it. Never gates on `Feature::InitialPrompt`
/// (Handy issue #1601 — that flag is unreliable for this purpose).
fn whisper_gate(caps: &LoadedCaps) -> bool {
    if caps.arch != "whisper" {
        return false;
    }
    caps.whisper_ext_accepted.unwrap_or(true)
}

// Bug 1's *actual* root cause (verified by reading transcribe-cpp-sys
// 0.2.3's vendored C++ directly, not by copying Handy's engine dispatch —
// see the correction below):
//
// `transcribe_cpp::Model::capabilities().max_timestamp_kind` is NOT an
// optimistic self-report. Per architecture:
// - `arch/{canary,cohere,moonshine,moonshine_streaming,qwen3_asr,voxtral,
//   sensevoice,funasr_nano}/capabilities.cpp` all hard-code
//   `TRANSCRIBE_TIMESTAMPS_NONE` as the family default, and
//   `transcribe-meta.cpp`'s `read_capability_kv` explicitly does NOT
//   overlay `max_timestamp_kind` from any GGUF KV ("max_timestamp_kind is
//   NOT read here: converters emit timestamp-capability KVs ... but the
//   ceiling is variant-specific and applied in each family's load()") for
//   any of those families, so it stays NONE for the lifetime of the
//   loaded model. That matches the catalog's own per-model
//   `capabilities.timestamps` claim for every one of those slugs
//   (`catalog/handy-2026-08-17.json`: `"none"`).
// - `arch/granite/model.cpp` (~line 236-250) reads the GGUF's own
//   `stt.capability.word_timestamps` bool and lowers the family default
//   `WORD` to `NONE` per-variant *before* `capabilities()` is ever read by
//   us — so by the time our Rust code sees it, it is already correct
//   (`granite-speech-4.1-2b-plus` claims `"word"` in the catalog; the
//   other granite variants claim `"none"`).
// - `arch/{gigaam,medasr}/capabilities.cpp` set the family default to
//   `TOKEN` and never lower it — matching the catalog's `"token"` claim
//   for `gigaam-v3-*`/`medasr`. `transcribe.cpp`'s shared
//   `validate_run_params_common` (the ranked `NONE(0) < SEGMENT(1) <
//   WORD(2) < TOKEN(3)` ceiling check every family goes through before
//   `run()`) genuinely allows a segment/word request there — the
//   architecture-specific `run()` for these two families just never fills
//   `transcript.segments`/`words`, only `transcript.tokens` (see
//   `format::format_response`'s token-fallback synthesis, added
//   alongside this fix, for the client-visible consequence).
//
// **What was actually wrong was entirely on our side**, in
// `EffectiveCaps::to_json`'s `timestamp_granularity` branch below: it
// reported `Status::Supported` unconditionally whenever a run hadn't yet
// been observed to reject it (`timestamp_granularity_rejected == false`)
// — it never even looked at `self.loaded.max_timestamp_kind`. So a model
// whose own, accurate, engine-reported ceiling was `None` (canary, cohere,
// moonshine, voxtral, qwen3_asr, sensevoice, funasr_nano) was still
// advertised as "supported" until a real request came back
// `engine_unsupported` and got remembered. A prior revision of this fix
// papered over that by hard-coding an arch allowlist copied from Handy's
// own (differently-scoped) engine dispatch — but Handy has no
// granite/gigaam/medasr code paths at all to copy from, and that allowlist
// incorrectly reported `gigaam-v3-*`/`medasr`/`granite-*-plus` as
// timestamp-incapable even though both the engine and the catalog's own
// claim say they are capable. The correct fix is simply to *use*
// `self.loaded.max_timestamp_kind`, which is already accurate — see
// `max_timestamp_granularity` below.

/// One control's status in either view.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Supported,
    Unsupported,
    Unknown,
}

/// A single control's reported capability, shared shape for both views.
#[derive(Debug, Clone, Serialize)]
pub struct ControlCapability {
    pub status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
    pub evidence: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mechanism: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<&'static str>,
    #[serde(skip_serializing_if = "Value::is_null")]
    pub extra: Value,
}

impl ControlCapability {
    fn to_json(&self) -> Value {
        let mut v = json!({
            "status": match self.status {
                Status::Supported => "supported",
                Status::Unsupported => "unsupported",
                Status::Unknown => "unknown",
            },
            "evidence": self.evidence,
        });
        if let Some(reason) = self.reason {
            v["reason"] = json!(reason);
        }
        if let Some(mechanism) = self.mechanism {
            v["mechanism"] = json!(mechanism);
        }
        if let Some(scope) = self.scope {
            v["scope"] = json!(scope);
        }
        if let Value::Object(extra) = &self.extra {
            for (k, val) in extra {
                v[k] = val.clone();
            }
        }
        v
    }
}

/// Build the catalog (unloaded-model) capability view for a `CatalogModel`.
/// Delegates to `catalog::capability_matrix` for backward compatibility (the
/// unloaded-model JSON shape it already produces matches this view).
pub fn catalog_view(model: &CatalogModel) -> Value {
    catalog::capability_matrix(model)
}

/// The effective (loaded-model) capability view, computed once from
/// `LoadedCaps`.
#[derive(Debug, Clone)]
pub struct EffectiveCaps {
    pub loaded: LoadedCaps,
}

impl EffectiveCaps {
    pub fn new(loaded: LoadedCaps) -> Self {
        EffectiveCaps { loaded }
    }

    pub fn arch(&self) -> &str {
        &self.loaded.arch
    }

    pub fn is_whisper(&self) -> bool {
        whisper_gate(&self.loaded)
    }

    pub fn supports_language_hint(&self) -> bool {
        // Granite's run() never reads the language parameter; its language list is informational only.
        self.loaded.arch != "granite" && !self.loaded.languages.is_empty()
    }

    /// Whether the model translates at all. An empty target list means the
    /// engine does not advertise its targets, not that there are none: such a
    /// model is assumed to translate into English only (see
    /// [`Self::offers_translation_target`]).
    pub fn supports_translation(&self) -> bool {
        self.loaded.supports_translate
    }

    /// Whether the model can translate into `target`: any of its advertised
    /// target languages, or English when it advertises none.
    pub fn offers_translation_target(&self, target: &str) -> bool {
        let targets = &self.loaded.translate_target_languages;
        if targets.is_empty() {
            target == "en"
        } else {
            targets.iter().any(|offered| offered == target)
        }
    }

    pub fn max_timestamp_granularity(&self) -> TimestampGranularity {
        self.loaded.max_timestamp_kind
    }

    /// Whether even the cheapest granularity (segment) is honoured. Bug 1:
    /// this must be checked before planning `Segment` timestamps too, not
    /// just `Word` — an unchecked segment request was exactly how the
    /// catalog sweep got a 422 from canary/cohere/Voxtral/Qwen3-ASR/Fun-ASR/
    /// granite/moonshine/SenseVoice: nothing rejected the request before it
    /// reached the engine.
    pub fn supports_segment_timestamps(&self) -> bool {
        self.max_timestamp_granularity() != TimestampGranularity::None
    }

    pub fn supports_word_timestamps(&self) -> bool {
        matches!(
            self.max_timestamp_granularity(),
            TimestampGranularity::Word | TimestampGranularity::Token
        )
    }

    /// Bug 1: record that a real run rejected this loaded model's advertised
    /// timestamp granularity. Update the cached `EffectiveCaps` under the
    /// same brief lock that guards `App::loaded`, so later requests against
    /// this same loaded model stop being told `timestamp_granularity` is
    /// supported.
    pub fn mark_timestamp_granularity_rejected(&mut self) {
        self.loaded.timestamp_granularity_rejected = true;
    }

    /// Serialize the full effective capability matrix, in the same shape
    /// family as `catalog_view`, plus `catalog_mismatch` against a catalog
    /// entry when one is supplied by the caller (see [`catalog_mismatch`]).
    pub fn to_json(&self) -> Value {
        let whisper_ok = self.is_whisper();
        let prompt_vocab = ControlCapability {
            status: if whisper_ok {
                Status::Supported
            } else {
                Status::Unsupported
            },
            reason: if whisper_ok {
                None
            } else {
                Some("model_lacks")
            },
            evidence: "loaded_model",
            mechanism: Some("whisper_initial_prompt"),
            scope: None,
            extra: json!({ "max_tokens": self.loaded.prompt_max_tokens }),
        };
        let temperature = ControlCapability {
            status: if whisper_ok {
                Status::Supported
            } else {
                Status::Unsupported
            },
            reason: if whisper_ok {
                None
            } else {
                Some("model_lacks")
            },
            evidence: "loaded_model",
            mechanism: Some("whisper_initial_prompt"),
            scope: None,
            extra: json!({}),
        };
        let language_hint_ok = self.supports_language_hint();
        let language_hint = ControlCapability {
            status: if language_hint_ok {
                Status::Supported
            } else {
                Status::Unsupported
            },
            reason: if language_hint_ok {
                None
            } else {
                Some("model_lacks")
            },
            evidence: "loaded_model",
            mechanism: Some("run_option"),
            scope: Some("request"),
            extra: json!({ "languages": self.loaded.languages }),
        };
        let language_detect = ControlCapability {
            status: if self.loaded.supports_language_detect {
                Status::Supported
            } else {
                Status::Unsupported
            },
            reason: if self.loaded.supports_language_detect {
                None
            } else {
                Some("model_lacks")
            },
            evidence: "loaded_model",
            mechanism: None,
            scope: None,
            extra: json!({}),
        };
        let translation_ok = self.supports_translation();
        let translation = ControlCapability {
            status: if translation_ok {
                Status::Supported
            } else {
                Status::Unsupported
            },
            reason: if translation_ok {
                None
            } else {
                Some("model_lacks")
            },
            evidence: "loaded_model",
            mechanism: Some("task_translate"),
            scope: None,
            // An empty list on a translating model means "not advertised":
            // reported as null (unknown), distinct from `[]` (none).
            extra: json!({
                "target_languages": if translation_ok && self.loaded.translate_target_languages.is_empty() {
                    Value::Null
                } else {
                    json!(self.loaded.translate_target_languages)
                }
            }),
        };
        // Trust `self.loaded.max_timestamp_kind` directly: it is the
        // engine's own accurate, per-architecture (and, for granite,
        // per-variant-via-GGUF-KV) ceiling — see the long comment above
        // `EffectiveCaps::max_timestamp_granularity` for the transcribe-cpp
        // source evidence. No architecture allowlist needed or wanted here.
        let timestamp_granularity = if self.loaded.timestamp_granularity_rejected {
            ControlCapability {
                status: Status::Unsupported,
                reason: Some("run_rejected"),
                evidence: "run_rejected",
                mechanism: None,
                scope: None,
                extra: json!({ "max": self.max_timestamp_granularity().as_str() }),
            }
        } else if self.max_timestamp_granularity() == TimestampGranularity::None {
            ControlCapability {
                status: Status::Unsupported,
                reason: Some("model_lacks"),
                evidence: "loaded_model",
                mechanism: None,
                scope: None,
                extra: json!({ "max": TimestampGranularity::None.as_str() }),
            }
        } else {
            ControlCapability {
                status: Status::Supported,
                reason: None,
                evidence: "loaded_model",
                mechanism: None,
                scope: None,
                extra: json!({ "max": self.max_timestamp_granularity().as_str() }),
            }
        };
        let streaming = ControlCapability {
            status: if self.loaded.supports_streaming {
                Status::Supported
            } else {
                Status::Unsupported
            },
            reason: (!self.loaded.supports_streaming).then_some("model_lacks"),
            evidence: "loaded_model",
            mechanism: None,
            scope: None,
            extra: json!({ "model_claim": self.loaded.supports_streaming }),
        };

        let feature = |supported: bool| ControlCapability {
            status: if supported {
                Status::Supported
            } else {
                Status::Unsupported
            },
            reason: (!supported).then_some("model_lacks"),
            evidence: "loaded_model",
            mechanism: None,
            scope: None,
            extra: Value::Null,
        };
        let native_sample_rate = if self.loaded.native_sample_rate > 0 {
            ControlCapability {
                status: Status::Supported,
                reason: None,
                evidence: "loaded_model",
                mechanism: None,
                scope: None,
                extra: json!({ "hz": self.loaded.native_sample_rate }),
            }
        } else {
            ControlCapability {
                status: Status::Unknown,
                reason: Some("not_reported"),
                evidence: "loaded_model",
                mechanism: None,
                scope: None,
                extra: Value::Null,
            }
        };

        json!({
            "prompt": prompt_vocab.to_json(),
            "temperature": temperature.to_json(),
            "language_hint": language_hint.to_json(),
            "language_detect": language_detect.to_json(),
            "translation": translation.to_json(),
            "timestamp_granularity": timestamp_granularity.to_json(),
            "streaming": streaming.to_json(),
            "punctuation": feature(self.loaded.supports_pnc).to_json(),
            "itn": feature(self.loaded.supports_itn).to_json(),
            "cancellation": feature(self.loaded.supports_cancellation).to_json(),
            "long_form": feature(self.loaded.supports_long_form).to_json(),
            "spec_decode": feature(self.loaded.supports_spec_decode).to_json(),
            "native_sample_rate": native_sample_rate.to_json(),
            "response_formats": {"json": "supported", "text": "supported", "verbose_json": "supported", "srt": "unsupported", "vtt": "unsupported"},
            "feature_initial_prompt_flag": self.loaded.feature_initial_prompt_flag,
            "whisper_ext_accepted": self.loaded.whisper_ext_accepted,
            "arch": self.loaded.arch,
        })
    }
}

/// Informational list of controls where the catalog's claims disagree with
/// the effective (loaded-model) view. Each entry names the control and both
/// views' status strings.
pub fn catalog_mismatch(catalog: &CatalogModel, effective: &EffectiveCaps) -> Vec<Value> {
    let catalog_json = catalog_view(catalog);
    let effective_json = effective.to_json();
    let mut mismatches = Vec::new();

    let controls = [
        "prompt",
        "temperature",
        "language_hint",
        "language_detect",
        "translation",
        "timestamp_granularity",
        "streaming",
    ];
    for control in controls {
        let catalog_status = catalog_json
            .get(control)
            .and_then(|v| v.get("status"))
            .and_then(|v| v.as_str());
        let effective_status = effective_json
            .get(control)
            .and_then(|v| v.get("status"))
            .and_then(|v| v.as_str());
        // "unknown" in the catalog view never counts as a mismatch on its own
        // — it means "unverified", not "disagrees".
        if let (Some(cat), Some(eff)) = (catalog_status, effective_status) {
            if cat != "unknown" && cat != eff {
                mismatches.push(json!({
                    "control": control,
                    "catalog": cat,
                    "effective": eff,
                }));
            }
        }
    }
    mismatches
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loaded_whisper() -> LoadedCaps {
        LoadedCaps {
            arch: "whisper".to_string(),
            languages: vec!["en".to_string(), "es".to_string()],
            translate_target_languages: vec!["en".to_string()],
            supports_translate: true,
            supports_language_detect: true,
            max_timestamp_kind: TimestampGranularity::Word,
            feature_initial_prompt_flag: true,
            whisper_ext_accepted: Some(true),
            prompt_max_tokens: None,
            timestamp_granularity_rejected: false,
            supports_streaming: false,
            ..Default::default()
        }
    }

    fn loaded_non_whisper() -> LoadedCaps {
        LoadedCaps {
            arch: "parakeet".to_string(),
            languages: vec!["en".to_string()],
            translate_target_languages: vec![],
            supports_translate: false,
            supports_language_detect: false,
            max_timestamp_kind: TimestampGranularity::Segment,
            feature_initial_prompt_flag: false,
            whisper_ext_accepted: None,
            prompt_max_tokens: None,
            timestamp_granularity_rejected: false,
            supports_streaming: false,
            ..Default::default()
        }
    }

    #[test]
    fn whisper_arch_is_whisper_gated_supported() {
        let caps = EffectiveCaps::new(loaded_whisper());
        assert!(caps.is_whisper());
        let json = caps.to_json();
        assert_eq!(json["prompt"]["status"], "supported");
        assert_eq!(json["prompt"]["max_tokens"], Value::Null);
        assert_eq!(json["temperature"]["status"], "supported");
    }

    #[test]
    fn non_whisper_arch_is_unsupported_regardless_of_feature_flag() {
        let mut loaded = loaded_non_whisper();
        // Simulate Handy issue #1601: flag says yes, arch says no -> still no.
        loaded.feature_initial_prompt_flag = true;
        let caps = EffectiveCaps::new(loaded);
        assert!(!caps.is_whisper());
        let json = caps.to_json();
        assert_eq!(json["prompt"]["status"], "unsupported");
        assert_eq!(json["temperature"]["status"], "unsupported");
        // Flag is exposed diagnostically but never used to gate.
        assert_eq!(json["feature_initial_prompt_flag"], true);
    }

    #[test]
    fn whisper_ext_not_accepted_denies_the_gate_even_on_whisper_arch() {
        let mut loaded = loaded_whisper();
        loaded.whisper_ext_accepted = Some(false);
        let caps = EffectiveCaps::new(loaded);
        assert!(!caps.is_whisper());
    }

    #[test]
    fn translation_targets_are_the_models_own_list_or_english_when_unadvertised() {
        let caps = EffectiveCaps::new(loaded_whisper());
        assert!(caps.supports_translation());
        assert!(caps.offers_translation_target("en"));
        assert!(!caps.offers_translation_target("fr"));

        let mut loaded = loaded_whisper();
        loaded.translate_target_languages = vec!["en".to_string(), "de".to_string()];
        let caps = EffectiveCaps::new(loaded);
        assert!(caps.offers_translation_target("de"));
        assert!(!caps.offers_translation_target("fr"));

        // No advertised list: still translates, to English only.
        let mut loaded = loaded_whisper();
        loaded.translate_target_languages = vec![];
        let caps = EffectiveCaps::new(loaded);
        assert!(caps.supports_translation());
        assert!(caps.offers_translation_target("en"));
        assert!(!caps.offers_translation_target("de"));
        assert_eq!(
            caps.to_json()["translation"]["target_languages"],
            Value::Null
        );
    }

    #[test]
    fn translation_unsupported_when_model_does_not_advertise_it() {
        let caps = EffectiveCaps::new(loaded_non_whisper());
        assert!(!caps.supports_translation());
    }

    #[test]
    fn granite_never_supports_language_hint() {
        let mut loaded = loaded_non_whisper();
        loaded.arch = "granite".to_string();
        loaded.languages = vec!["en".to_string(), "fr".to_string()];
        assert!(!EffectiveCaps::new(loaded).supports_language_hint());
    }

    #[test]
    fn single_language_model_still_supports_language_hint() {
        let caps = EffectiveCaps::new(loaded_non_whisper());
        assert!(caps.supports_language_hint());
    }

    #[test]
    fn loaded_translation_reports_the_models_own_targets() {
        let whisper = EffectiveCaps::new(loaded_whisper()).to_json();
        assert_eq!(whisper["translation"]["status"], "supported");
        assert_eq!(whisper["translation"]["evidence"], "loaded_model");
        assert_eq!(whisper["translation"]["target_languages"], json!(["en"]));
        let parakeet = EffectiveCaps::new(loaded_non_whisper()).to_json();
        assert_eq!(parakeet["translation"]["status"], "unsupported");
        assert_eq!(parakeet["translation"]["reason"], "model_lacks");
        assert_eq!(parakeet["translation"]["target_languages"], json!([]));
    }

    #[test]
    fn loaded_engine_features_follow_the_model() {
        let controls = [
            "punctuation",
            "itn",
            "cancellation",
            "long_form",
            "spec_decode",
        ];
        let none = EffectiveCaps::new(loaded_non_whisper()).to_json();
        for control in controls {
            assert_eq!(none[control]["status"], "unsupported", "{control}");
            assert_eq!(none[control]["reason"], "model_lacks", "{control}");
            assert_eq!(none[control]["evidence"], "loaded_model", "{control}");
        }
        let all = EffectiveCaps::new(LoadedCaps {
            supports_pnc: true,
            supports_itn: true,
            supports_cancellation: true,
            supports_long_form: true,
            supports_spec_decode: true,
            ..loaded_non_whisper()
        })
        .to_json();
        for control in controls {
            assert_eq!(all[control]["status"], "supported", "{control}");
            assert!(all[control].get("reason").is_none(), "{control}");
        }
    }

    #[test]
    fn loaded_each_feature_is_reported_independently() {
        let json = EffectiveCaps::new(LoadedCaps {
            supports_itn: true,
            ..loaded_non_whisper()
        })
        .to_json();
        assert_eq!(json["itn"]["status"], "supported");
        assert_eq!(json["punctuation"]["status"], "unsupported");
        assert_eq!(json["spec_decode"]["status"], "unsupported");
    }

    #[test]
    fn native_sample_rate_is_reported_in_hz_or_unknown() {
        let known = EffectiveCaps::new(LoadedCaps {
            native_sample_rate: 16000,
            ..loaded_non_whisper()
        })
        .to_json();
        assert_eq!(known["native_sample_rate"]["status"], "supported");
        assert_eq!(known["native_sample_rate"]["hz"], 16000);
        let unreported = EffectiveCaps::new(loaded_non_whisper()).to_json();
        assert_eq!(unreported["native_sample_rate"]["status"], "unknown");
        assert_eq!(unreported["native_sample_rate"]["reason"], "not_reported");
    }

    #[test]
    fn catalog_mismatch_lists_disagreements_and_ignores_unknown() {
        let catalog_json = serde_json::json!({
            "id": "m1",
            "revision": "r1",
            "slug": "m1",
            "name": "Model 1",
            "architecture": "whisper",
            "family": "whisper",
            "license": "MIT",
            "languages": ["en", "es"],
            "capabilities": {
                "streaming": false,
                "translate": false,
                "lang_detect": false,
                "timestamps": "segment"
            },
            "speed_score": null,
            "accuracy_score": null,
            "files": [],
            "default_quant": "q4",
            "recommended": false,
            "recommended_rank": null
        });
        let catalog: CatalogModel = serde_json::from_value(catalog_json).unwrap();
        let effective = EffectiveCaps::new(loaded_whisper());
        let mismatches = catalog_mismatch(&catalog, &effective);
        // Catalog claims translate=false (unsupported/model_lacks); effective
        // says translate is supported (en target present) -> mismatch.
        assert!(mismatches.iter().any(|m| m["control"] == "translation"
            && m["catalog"] == "unsupported"
            && m["effective"] == "supported"));
        // Catalog's `prompt` for a whisper-architecture model is "unknown"
        // (unverified -- our own code only wires prompt through the
        // whisper run extension, so the catalog can't rule it out, but
        // hasn't verified it either; see `catalog::capability_matrix`).
        // "unknown" never counts as a mismatch against the effective view's
        // "supported", by design (an unverified claim isn't a disagreement).
        assert!(!mismatches.iter().any(|m| m["control"] == "prompt"));
    }
}
