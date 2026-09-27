//! Pure request planning: turn a parsed transcription/translation request plus
//! the loaded model's effective capabilities into a `Plan`, or reject it with
//! an `ApiError`. No model access — everything here is unit-testable.
//!
//! Language-matching helpers (`base_language`, `canonical_language_code`,
//! `normalize_cjk_language`) and the unmatched-hint fallback (`fallback_language`,
//! porting Handy's `effective_language`) are ported from Handy (MIT), commit
//! `8f9cf53`, `src-tauri/src/managers/model.rs:95-109,275-318` and
//! `src-tauri/src/managers/transcription.rs:1652`. See
//! `THIRD_PARTY_NOTICES.md` for Handy's MIT notice.
//!
//! The prompt is opaque to this module (CORRECTED by user 2026-09-25): the
//! caller builds it (including any vocabulary/prior-chunk context) and it is
//! passed to the engine verbatim, with no composition or trimming here.

use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::capabilities::{EffectiveCaps, TimestampGranularity};
use crate::errors::ApiError;

/// A tokenizer probe: text in, token count out (`None` when tokenization
/// isn't available). Aliased to keep clippy quiet about the otherwise
/// legal-but-unwieldy `Option<&dyn Fn(...) -> ...>` signature.
pub type Tokenizer<'a> = dyn Fn(&str) -> Option<usize> + 'a;

/// Which endpoint the request came in on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint {
    Transcriptions,
    Translations,
}

/// The task to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlannedTask {
    Transcribe,
    Translate,
}

/// Requested response format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseFormat {
    Json,
    Text,
    VerboseJson,
}

impl ResponseFormat {
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "json" => Some(ResponseFormat::Json),
            "text" => Some(ResponseFormat::Text),
            "verbose_json" => Some(ResponseFormat::VerboseJson),
            _ => None,
        }
    }
}

/// The parsed, engine-agnostic request fields this module plans against.
#[derive(Debug, Clone, Default)]
pub struct ParsedRequest {
    pub language: Option<String>,
    /// Opaque, verbatim prompt text. No vocabulary field, no composition: the
    /// client is responsible for building this string.
    pub prompt: Option<String>,
    pub temperature: Option<f32>,
    pub response_format: Option<String>,
    pub timestamp_granularities: Vec<String>,
}

/// The finished, engine-ready plan.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// The language code to send as `RunOptions.language`, or `None` to
    /// autodetect.
    pub language: Option<String>,
    pub task: PlannedTask,
    pub target_language: Option<String>,
    pub initial_prompt: Option<String>,
    pub temperature: Option<f32>,
    pub timestamps: TimestampGranularity,
    pub response_format: ResponseFormat,
    /// Whether a user-supplied language hint matched the model exactly
    /// (`true`), or the language was decided by the Handy fallback / no hint
    /// was given at all (`false`).
    pub language_hint_applied: bool,
    /// Whether the request actually included a non-empty, non-`auto`
    /// `language` hint at all (Bug 2). Distinct from `language_hint_applied`:
    /// that field says whether the hint (if any) was honored; this one says
    /// whether there was a hint to begin with, so the caller can omit
    /// `x_diagnostics.language_hint_applied` entirely when no hint was sent,
    /// rather than reporting a misleading `false`.
    pub language_hint_provided: bool,
    /// The language this plan will actually use, for diagnostics: `None`
    /// means autodetect (no hint given and the model does language
    /// detection). Always `Some` when `language` is `Some`, and also `Some`
    /// when a hint was given but unmatched and the fallback picked a
    /// concrete language.
    pub applied_language: Option<String>,
    pub language_evidence_hint: Option<String>,
    /// Whether `timestamps` was picked by an explicit
    /// `timestamp_granularities` request (`true`) or defaulted for
    /// `verbose_json` because none was given (`false`). Used by Bug 1's
    /// retry decision: only a defaulted choice may be silently downgraded
    /// when the engine rejects it; an explicit request keeps failing.
    pub timestamps_explicit: bool,
}

impl Plan {
    /// Build the transcribe-cpp `RunOptions` for this plan. Only attaches the
    /// whisper `RunExtension` when a prompt or temperature is actually set.
    pub fn to_run_options(&self) -> transcribe_cpp::RunOptions {
        use transcribe_cpp::{RunExtension, RunOptions, Task, TimestampKind, WhisperRunOptions};

        let timestamps = match self.timestamps {
            TimestampGranularity::None => TimestampKind::None,
            TimestampGranularity::Segment => TimestampKind::Segment,
            TimestampGranularity::Word => TimestampKind::Word,
            TimestampGranularity::Token => TimestampKind::Token,
        };

        let family = if self.initial_prompt.is_some() || self.temperature.is_some() {
            Some(RunExtension::Whisper(WhisperRunOptions {
                initial_prompt: self.initial_prompt.clone(),
                temperature: self.temperature,
                ..Default::default()
            }))
        } else {
            None
        };

        RunOptions {
            task: match self.task {
                PlannedTask::Transcribe => Task::Transcribe,
                PlannedTask::Translate => Task::Translate,
            },
            timestamps,
            language: self.language.clone(),
            target_language: self.target_language.clone(),
            family,
            ..Default::default()
        }
    }
}

// ---------------------------------------------------------------------------
// Language helpers — ported from Handy (MIT), commit 8f9cf53.
// ---------------------------------------------------------------------------

/// Chinese-script normalization: `zh-Hans`/`zh-Hant` both collapse to `zh` for
/// matching purposes (the model itself only ever knows the plain code).
/// Ported from Handy (MIT), commit 8f9cf53,
/// `src-tauri/src/managers/transcription.rs:1652` (`normalize_cjk_language`).
fn normalize_cjk_language(language: &str) -> &str {
    match language {
        "zh-Hans" | "zh-Hant" => "zh",
        other => other,
    }
}

/// A tag's primary language subtag, with any BCP-47 region/script suffix
/// dropped (`en-US` -> `en`). Ported from Handy (MIT), commit 8f9cf53,
/// `src-tauri/src/managers/model.rs:95-97` (`base_language`).
fn base_language(language: &str) -> &str {
    language.split(&['-', '_'][..]).next().unwrap_or(language)
}

/// The stable user intent used to compare language codes across model
/// families: `nb` <-> `no`, `fil` <-> `tl`. Ported from Handy (MIT), commit
/// 8f9cf53, `src-tauri/src/managers/model.rs:103-109`
/// (`canonical_language_code`).
fn canonical_language_code(language: &str) -> &str {
    match base_language(language) {
        "nb" => "no",
        "fil" => "tl",
        base => base,
    }
}

/// Try to match a requested language hint against the loaded model's
/// supported languages. Returns the model's own code on a match. Does not
/// itself decide what to do on a miss — see [`fallback_language`].
fn match_language_hint(requested: &str, supported: &[String]) -> Option<String> {
    let normalized = normalize_cjk_language(&requested.to_lowercase()).to_string();

    // Prefer an exact base-language match before considering an alias, so an
    // explicit `nb` selects `nb` even if the model also advertises `no`.
    let exact = supported
        .iter()
        .find(|lang| base_language(lang) == base_language(&normalized));
    let alias = || {
        supported
            .iter()
            .find(|lang| canonical_language_code(lang) == canonical_language_code(&normalized))
    };
    exact.or_else(alias).cloned()
}

/// The Handy fallback for an unresolvable (or absent) language intent: auto-
/// detect if the model supports language detection, else English if the
/// model supports English, else the model's first supported language. Ported
/// from Handy (MIT), commit 8f9cf53,
/// `src-tauri/src/managers/model.rs:275-318` (`effective_language`), adapted
/// to this module's plan shape (returns `None` for "autodetect" rather than
/// the string `"auto"`).
fn fallback_language(supported: &[String], supports_language_detection: bool) -> Option<String> {
    if supported.is_empty() {
        return None;
    }
    if supports_language_detection {
        return None;
    }
    if let Some(en) = supported
        .iter()
        .find(|language| base_language(language) == "en")
    {
        return Some(en.clone());
    }
    Some(supported[0].clone())
}

/// Resolve the request's language hint into `(language_for_run,
/// hint_applied, applied_language, evidence)`.
///
/// - Absent/empty/`auto`: no hint. `language_for_run` is `None` (autodetect
///   when the model supports it) unless the model can't detect language, in
///   which case the Handy fallback still picks a concrete default so the
///   engine is never handed nothing to work with on a non-detecting model.
/// - Present and matches (base language or alias) a model-supported
///   language: use the model's own code, `hint_applied = true`.
/// - Present but unmatched: NOT an error (CORRECTED by user 2026-09-25) —
///   fall back the same way as "absent", but `hint_applied` stays `false` so
///   the caller can record that the hint was not honored.
fn resolve_language(
    requested: Option<&str>,
    caps: &EffectiveCaps,
) -> (Option<String>, bool, bool, Option<String>, Option<String>) {
    let supported = &caps.loaded.languages;
    let has_hint = requested
        .map(|raw| {
            let trimmed = raw.trim();
            !trimmed.is_empty() && !trimmed.eq_ignore_ascii_case("auto")
        })
        .unwrap_or(false);

    if has_hint {
        let raw = requested.unwrap().trim();
        if let Some(matched) = match_language_hint(raw, supported) {
            return (
                Some(matched.clone()),
                true,
                true,
                Some(matched),
                Some("user_selected".to_string()),
            );
        }
    }

    // No hint, or a hint that didn't match anything the model supports.
    match fallback_language(supported, caps.loaded.supports_language_detect) {
        None => (None, false, has_hint, None, None),
        Some(lang) => (
            Some(lang.clone()),
            false,
            has_hint,
            Some(lang),
            Some("model_constrained".to_string()),
        ),
    }
}

// ---------------------------------------------------------------------------
// Planning entry point.
// ---------------------------------------------------------------------------

pub fn plan(
    request: &ParsedRequest,
    caps: &EffectiveCaps,
    endpoint: Endpoint,
    tokenize: Option<&Tokenizer>,
) -> Result<Plan, ApiError> {
    // --- language -----------------------------------------------------
    let (
        language_hint,
        language_hint_applied,
        language_hint_provided,
        applied_language,
        language_evidence_hint,
    ) = resolve_language(request.language.as_deref(), caps);

    // --- prompt ---------------------------------------------------------
    let has_prompt = request
        .prompt
        .as_deref()
        .map(|p| !p.is_empty())
        .unwrap_or(false);
    if has_prompt && !caps.is_whisper() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unsupported_capability",
            "This model does not support 'prompt'",
        ));
    }
    let initial_prompt = if has_prompt {
        let prompt = request.prompt.as_deref().unwrap();
        if let Some(limit) = caps.loaded.prompt_max_tokens {
            if let Some(tokenize) = tokenize {
                if let Some(actual) = tokenize(prompt) {
                    if actual > limit {
                        return Err(ApiError::new(
                            StatusCode::UNPROCESSABLE_ENTITY,
                            "prompt_too_long",
                            "The prompt exceeds this model's prompt token limit",
                        )
                        .with_details(json!({
                            "limit": limit,
                            "actual": actual,
                            "unit": "tokens",
                        })));
                    }
                }
            }
        }
        // Passed verbatim: no composition, no trimming.
        Some(prompt.to_string())
    } else {
        None
    };

    // --- temperature -----------------------------------------------------
    let temperature = match request.temperature {
        Some(t) => {
            if !(0.0..=1.0).contains(&t) {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "invalid_temperature",
                    "temperature must be between 0.0 and 1.0",
                ));
            }
            if !caps.is_whisper() {
                return Err(ApiError::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "unsupported_capability",
                    "This model does not support 'temperature'",
                ));
            }
            Some(t)
        }
        None => None,
    };

    // --- response_format ---------------------------------------------------
    let response_format = match &request.response_format {
        None => ResponseFormat::Json,
        Some(raw) => ResponseFormat::parse(raw).ok_or_else(|| {
            ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "unsupported_capability",
                format!("response_format '{raw}' is not supported"),
            )
        })?,
    };

    // --- timestamps ---------------------------------------------------------
    let word_requested = request.timestamp_granularities.iter().any(|g| g == "word");
    let segment_requested = request
        .timestamp_granularities
        .iter()
        .any(|g| g == "segment");
    let (timestamps, timestamps_explicit) = match response_format {
        ResponseFormat::VerboseJson => {
            if word_requested {
                if !caps.supports_word_timestamps() {
                    return Err(ApiError::new(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "unsupported_capability",
                        "This model does not support word-level timestamps",
                    ));
                }
                (TimestampGranularity::Word, true)
            } else if segment_requested {
                if !caps.supports_segment_timestamps() {
                    return Err(ApiError::new(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "unsupported_capability",
                        "This model does not support timestamps",
                    ));
                }
                (TimestampGranularity::Segment, true)
            } else if caps.supports_segment_timestamps() {
                // Not explicitly requested: defaulted for verbose_json. Bug
                // 1's retry may still silently downgrade this one.
                (TimestampGranularity::Segment, false)
            } else {
                // Model has no working timestamp path at all (see
                // `EffectiveCaps::supports_segment_timestamps`): fall back to
                // no timestamps rather than planning a run the engine is
                // known to reject. `verbose_json` still returns 200 with an
                // empty `segments` array, honestly reflecting that this
                // model cannot produce them.
                (TimestampGranularity::None, false)
            }
        }
        ResponseFormat::Json | ResponseFormat::Text => (TimestampGranularity::None, false),
    };

    // --- task / translation --------------------------------------------------
    let (task, target_language) = match endpoint {
        Endpoint::Transcriptions => (PlannedTask::Transcribe, None),
        Endpoint::Translations => {
            if !caps.supports_translation() {
                return Err(ApiError::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "unsupported_capability",
                    "This model does not support translation",
                ));
            }
            let source_is_english = applied_language.as_deref() == Some("en")
                || (caps.loaded.languages.len() == 1 && caps.loaded.languages[0] == "en");
            if source_is_english {
                // Transcribing instead of translating is correct here (the
                // source is already English), but the *evidence* for why is
                // whatever `resolve_language` already determined: an explicit
                // `en` hint is `user_selected`, and a single-language `en`
                // model with no hint is `model_constrained`. Neither case is
                // actually "translated to English" — nothing was translated.
                (PlannedTask::Transcribe, None)
            } else {
                (PlannedTask::Translate, Some("en".to_string()))
            }
        }
    };

    Ok(Plan {
        language: language_hint,
        task,
        target_language,
        initial_prompt,
        temperature,
        timestamps,
        response_format,
        language_hint_applied,
        language_hint_provided,
        applied_language,
        language_evidence_hint,
        timestamps_explicit,
    })
}

/// Bug 1's retry decision, kept pure and unit-tested on its own: should a
/// failed run be retried once with `TimestampKind::None`? Only when all of
/// these hold:
/// - the response format is `verbose_json` (only it ever asks for segment
///   timestamps),
/// - the timestamp kind actually run was `Segment`,
/// - that choice was defaulted, not explicitly requested via
///   `timestamp_granularities` (an explicit request keeps failing — no
///   silent downgrade of an explicit ask), and
/// - the engine's failure was `engine_unsupported` (an `Unsupported` run
///   error), not some other failure this retry wouldn't fix.
pub fn should_retry_without_timestamps(
    response_format: ResponseFormat,
    timestamps: TimestampGranularity,
    timestamps_explicit: bool,
    error_code: &str,
) -> bool {
    response_format == ResponseFormat::VerboseJson
        && timestamps == TimestampGranularity::Segment
        && !timestamps_explicit
        && error_code == "engine_unsupported"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::LoadedCaps;

    fn whisper_caps() -> EffectiveCaps {
        EffectiveCaps::new(LoadedCaps {
            arch: "whisper".to_string(),
            languages: vec!["en".to_string(), "es".to_string(), "no".to_string()],
            translate_target_languages: vec!["en".to_string()],
            supports_translate: true,
            supports_language_detect: true,
            max_timestamp_kind: TimestampGranularity::Word,
            feature_initial_prompt_flag: true,
            whisper_ext_accepted: Some(true),
            prompt_max_tokens: None,
            timestamp_granularity_rejected: false,
        })
    }

    fn non_whisper_caps() -> EffectiveCaps {
        EffectiveCaps::new(LoadedCaps {
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
        })
    }

    fn single_lang_en_caps() -> EffectiveCaps {
        EffectiveCaps::new(LoadedCaps {
            arch: "parakeet".to_string(),
            languages: vec!["en".to_string()],
            translate_target_languages: vec!["en".to_string()],
            supports_translate: true,
            supports_language_detect: false,
            max_timestamp_kind: TimestampGranularity::Segment,
            feature_initial_prompt_flag: false,
            whisper_ext_accepted: None,
            prompt_max_tokens: None,
            timestamp_granularity_rejected: false,
        })
    }

    fn req() -> ParsedRequest {
        ParsedRequest::default()
    }

    // --- language ---------------------------------------------------------

    #[test]
    fn base_match_en_us_to_en() {
        let mut r = req();
        r.language = Some("en-US".to_string());
        let p = plan(&r, &whisper_caps(), Endpoint::Transcriptions, None).unwrap();
        assert_eq!(p.language, Some("en".to_string()));
        assert!(p.language_hint_applied);
        assert_eq!(p.applied_language, Some("en".to_string()));
    }

    #[test]
    fn alias_nb_matches_no() {
        let mut r = req();
        r.language = Some("nb".to_string());
        let p = plan(&r, &whisper_caps(), Endpoint::Transcriptions, None).unwrap();
        assert_eq!(p.language, Some("no".to_string()));
        assert!(p.language_hint_applied);
    }

    #[test]
    fn alias_fil_matches_tl() {
        let mut caps = whisper_caps();
        caps.loaded.languages.push("tl".to_string());
        let mut r = req();
        r.language = Some("fil".to_string());
        let p = plan(&r, &caps, Endpoint::Transcriptions, None).unwrap();
        assert_eq!(p.language, Some("tl".to_string()));
    }

    #[test]
    fn cjk_script_normalization() {
        let mut caps = whisper_caps();
        caps.loaded.languages.push("zh".to_string());
        let mut r = req();
        r.language = Some("zh-Hans".to_string());
        let p = plan(&r, &caps, Endpoint::Transcriptions, None).unwrap();
        assert_eq!(p.language, Some("zh".to_string()));
    }

    #[test]
    fn unmatched_language_falls_back_to_autodetect_when_supported() {
        let mut r = req();
        r.language = Some("xx".to_string());
        let p = plan(&r, &whisper_caps(), Endpoint::Transcriptions, None).unwrap();
        // whisper_caps() supports language detection -> fallback is autodetect.
        assert_eq!(p.language, None);
        assert!(!p.language_hint_applied);
        assert_eq!(p.applied_language, None);
    }

    #[test]
    fn unmatched_language_falls_back_to_english_when_no_detection() {
        let mut caps = non_whisper_caps();
        caps.loaded.languages = vec!["en".to_string(), "es".to_string()];
        let mut r = req();
        r.language = Some("xx".to_string());
        let p = plan(&r, &caps, Endpoint::Transcriptions, None).unwrap();
        assert_eq!(p.language, Some("en".to_string()));
        assert!(!p.language_hint_applied);
        assert_eq!(p.applied_language, Some("en".to_string()));
        assert_eq!(
            p.language_evidence_hint.as_deref(),
            Some("model_constrained")
        );
    }

    #[test]
    fn unmatched_language_falls_back_to_first_language_when_no_english() {
        let mut caps = non_whisper_caps();
        caps.loaded.languages = vec!["es".to_string(), "fr".to_string()];
        let mut r = req();
        r.language = Some("xx".to_string());
        let p = plan(&r, &caps, Endpoint::Transcriptions, None).unwrap();
        assert_eq!(p.language, Some("es".to_string()));
        assert!(!p.language_hint_applied);
    }

    #[test]
    fn auto_and_empty_mean_no_hint() {
        let mut r = req();
        r.language = Some("auto".to_string());
        let p = plan(&r, &whisper_caps(), Endpoint::Transcriptions, None).unwrap();
        assert_eq!(p.language, None);
        assert!(!p.language_hint_applied);

        let mut r2 = req();
        r2.language = Some("".to_string());
        let p2 = plan(&r2, &whisper_caps(), Endpoint::Transcriptions, None).unwrap();
        assert_eq!(p2.language, None);
    }

    #[test]
    fn single_language_model_hint_is_no_op_accept() {
        let mut r = req();
        r.language = Some("en".to_string());
        let p = plan(&r, &single_lang_en_caps(), Endpoint::Transcriptions, None).unwrap();
        assert_eq!(p.language, Some("en".to_string()));
        assert!(p.language_hint_applied);
    }

    #[test]
    fn single_language_model_unrelated_hint_falls_back_to_the_model_language() {
        let mut r = req();
        r.language = Some("fr".to_string());
        let p = plan(&r, &single_lang_en_caps(), Endpoint::Transcriptions, None).unwrap();
        assert_eq!(p.language, Some("en".to_string()));
        assert!(!p.language_hint_applied);
    }

    // --- prompt -----------------------------------------------------------

    #[test]
    fn prompt_supported_on_whisper_passed_verbatim() {
        let mut r = req();
        r.prompt = Some("hello   world  with  odd   spacing".to_string());
        let p = plan(&r, &whisper_caps(), Endpoint::Transcriptions, None).unwrap();
        assert_eq!(
            p.initial_prompt.as_deref(),
            Some("hello   world  with  odd   spacing")
        );
    }

    #[test]
    fn prompt_unsupported_on_non_whisper_even_with_feature_flag_true() {
        let mut caps = non_whisper_caps();
        caps.loaded.feature_initial_prompt_flag = true;
        let mut r = req();
        r.prompt = Some("hello".to_string());
        let err = plan(&r, &caps, Endpoint::Transcriptions, None).unwrap_err();
        assert_eq!(err.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(err.code, "unsupported_capability");
        assert!(err.message.contains("prompt"));
    }

    #[test]
    fn prompt_limit_enforced_only_when_known() {
        // No limit published (0.2.3 default) -> never rejected regardless of
        // length, even with a tokenizer available.
        let tokenize = |s: &str| Some(s.split_whitespace().count());
        let mut r = req();
        r.prompt = Some(
            (0..500)
                .map(|i| format!("w{i}"))
                .collect::<Vec<_>>()
                .join(" "),
        );
        let p = plan(
            &r,
            &whisper_caps(),
            Endpoint::Transcriptions,
            Some(&tokenize as &Tokenizer),
        )
        .unwrap();
        assert!(p.initial_prompt.unwrap().split_whitespace().count() == 500);

        // With a published limit, an over-limit prompt is rejected using the
        // tokenizer closure.
        let mut caps = whisper_caps();
        caps.loaded.prompt_max_tokens = Some(10);
        let err = plan(
            &r,
            &caps,
            Endpoint::Transcriptions,
            Some(&tokenize as &Tokenizer),
        )
        .unwrap_err();
        assert_eq!(err.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(err.code, "prompt_too_long");
        let details = err.details.unwrap();
        assert_eq!(details["limit"], 10);
        assert_eq!(details["actual"], 500);
        assert_eq!(details["unit"], "tokens");

        // Under the limit is fine.
        let mut r2 = req();
        r2.prompt = Some("short prompt".to_string());
        let p2 = plan(
            &r2,
            &caps,
            Endpoint::Transcriptions,
            Some(&tokenize as &Tokenizer),
        )
        .unwrap();
        assert_eq!(p2.initial_prompt.as_deref(), Some("short prompt"));
    }

    // --- temperature --------------------------------------------------------

    #[test]
    fn temperature_bounds_rejected() {
        let mut r = req();
        r.temperature = Some(1.5);
        let err = plan(&r, &whisper_caps(), Endpoint::Transcriptions, None).unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert_eq!(err.code, "invalid_temperature");

        let mut r2 = req();
        r2.temperature = Some(-0.1);
        let err2 = plan(&r2, &whisper_caps(), Endpoint::Transcriptions, None).unwrap_err();
        assert_eq!(err2.code, "invalid_temperature");
    }

    #[test]
    fn temperature_unsupported_on_non_whisper() {
        let mut r = req();
        r.temperature = Some(0.5);
        let err = plan(&r, &non_whisper_caps(), Endpoint::Transcriptions, None).unwrap_err();
        assert_eq!(err.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(err.code, "unsupported_capability");
    }

    #[test]
    fn temperature_in_range_accepted_on_whisper() {
        let mut r = req();
        r.temperature = Some(0.3);
        let p = plan(&r, &whisper_caps(), Endpoint::Transcriptions, None).unwrap();
        assert_eq!(p.temperature, Some(0.3));
    }

    // --- translation ---------------------------------------------------------

    #[test]
    fn translation_sets_translate_task_and_en_target() {
        let mut r = req();
        r.language = Some("es".to_string());
        let p = plan(&r, &whisper_caps(), Endpoint::Translations, None).unwrap();
        assert_eq!(p.task, PlannedTask::Translate);
        assert_eq!(p.target_language.as_deref(), Some("en"));
    }

    #[test]
    fn translation_english_hint_transcribes_instead() {
        let mut r = req();
        r.language = Some("en".to_string());
        let p = plan(&r, &whisper_caps(), Endpoint::Translations, None).unwrap();
        assert_eq!(p.task, PlannedTask::Transcribe);
        assert_eq!(p.target_language, None);
        // The source is English so transcribing (not translating) is
        // correct, but the hint was explicit -- the evidence must say so,
        // not the stale "translated_to_english" (nothing was translated).
        assert_eq!(p.language_evidence_hint.as_deref(), Some("user_selected"));
    }

    #[test]
    fn translation_single_language_en_model_transcribes() {
        let r = req();
        let p = plan(&r, &single_lang_en_caps(), Endpoint::Translations, None).unwrap();
        assert_eq!(p.task, PlannedTask::Transcribe);
        // No hint was given; a single-language `en` model forced English --
        // that's model_constrained, not translated_to_english.
        assert_eq!(
            p.language_evidence_hint.as_deref(),
            Some("model_constrained")
        );
    }

    #[test]
    fn translation_unsupported_is_422() {
        let r = req();
        let err = plan(&r, &non_whisper_caps(), Endpoint::Translations, None).unwrap_err();
        assert_eq!(err.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(err.code, "unsupported_capability");
    }

    // --- timestamps -----------------------------------------------------------

    #[test]
    fn verbose_json_defaults_to_segment_timestamps() {
        let mut r = req();
        r.response_format = Some("verbose_json".to_string());
        let p = plan(&r, &whisper_caps(), Endpoint::Transcriptions, None).unwrap();
        assert_eq!(p.timestamps, TimestampGranularity::Segment);
    }

    #[test]
    fn word_timestamps_supported_when_model_supports_it() {
        let mut r = req();
        r.response_format = Some("verbose_json".to_string());
        r.timestamp_granularities = vec!["word".to_string()];
        let p = plan(&r, &whisper_caps(), Endpoint::Transcriptions, None).unwrap();
        assert_eq!(p.timestamps, TimestampGranularity::Word);
    }

    #[test]
    fn word_timestamps_unsupported_is_422() {
        let mut r = req();
        r.response_format = Some("verbose_json".to_string());
        r.timestamp_granularities = vec!["word".to_string()];
        let err = plan(&r, &non_whisper_caps(), Endpoint::Transcriptions, None).unwrap_err();
        assert_eq!(err.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(err.code, "unsupported_capability");
    }

    #[test]
    fn json_format_means_no_timestamps() {
        let r = req();
        let p = plan(&r, &whisper_caps(), Endpoint::Transcriptions, None).unwrap();
        assert_eq!(p.timestamps, TimestampGranularity::None);
    }

    // --- language_hint_provided (Bug 2) ---------------------------------------

    #[test]
    fn language_hint_provided_false_when_no_language_sent() {
        let r = req();
        let p = plan(&r, &whisper_caps(), Endpoint::Transcriptions, None).unwrap();
        assert!(!p.language_hint_provided);
    }

    #[test]
    fn language_hint_provided_false_for_auto_or_empty() {
        let mut r = req();
        r.language = Some("auto".to_string());
        let p = plan(&r, &whisper_caps(), Endpoint::Transcriptions, None).unwrap();
        assert!(!p.language_hint_provided);

        let mut r2 = req();
        r2.language = Some("".to_string());
        let p2 = plan(&r2, &whisper_caps(), Endpoint::Transcriptions, None).unwrap();
        assert!(!p2.language_hint_provided);
    }

    #[test]
    fn language_hint_provided_true_when_matched() {
        let mut r = req();
        r.language = Some("en".to_string());
        let p = plan(&r, &whisper_caps(), Endpoint::Transcriptions, None).unwrap();
        assert!(p.language_hint_provided);
        assert!(p.language_hint_applied);
    }

    #[test]
    fn language_hint_provided_true_but_applied_false_when_unmatched() {
        let mut r = req();
        r.language = Some("xx".to_string());
        let p = plan(&r, &whisper_caps(), Endpoint::Transcriptions, None).unwrap();
        assert!(p.language_hint_provided);
        assert!(!p.language_hint_applied);
    }

    // --- should_retry_without_timestamps (Bug 1) ------------------------------

    #[test]
    fn retries_defaulted_segment_timestamps_on_engine_unsupported() {
        assert!(should_retry_without_timestamps(
            ResponseFormat::VerboseJson,
            TimestampGranularity::Segment,
            false,
            "engine_unsupported",
        ));
    }

    #[test]
    fn does_not_retry_when_timestamps_were_explicit() {
        assert!(!should_retry_without_timestamps(
            ResponseFormat::VerboseJson,
            TimestampGranularity::Segment,
            true,
            "engine_unsupported",
        ));
    }

    #[test]
    fn does_not_retry_for_non_verbose_json() {
        assert!(!should_retry_without_timestamps(
            ResponseFormat::Json,
            TimestampGranularity::Segment,
            false,
            "engine_unsupported",
        ));
    }

    #[test]
    fn does_not_retry_for_word_timestamps() {
        assert!(!should_retry_without_timestamps(
            ResponseFormat::VerboseJson,
            TimestampGranularity::Word,
            false,
            "engine_unsupported",
        ));
    }

    #[test]
    fn does_not_retry_for_other_error_codes() {
        assert!(!should_retry_without_timestamps(
            ResponseFormat::VerboseJson,
            TimestampGranularity::Segment,
            false,
            "engine_rejected_option",
        ));
    }

    // --- response_format --------------------------------------------------

    #[test]
    fn srt_response_format_is_422() {
        let mut r = req();
        r.response_format = Some("srt".to_string());
        let err = plan(&r, &whisper_caps(), Endpoint::Transcriptions, None).unwrap_err();
        assert_eq!(err.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(err.code, "unsupported_capability");
    }

    // --- to_run_options -----------------------------------------------------

    #[test]
    fn to_run_options_sets_whisper_ext_only_with_prompt_or_temperature() {
        let r = req();
        let p = plan(&r, &whisper_caps(), Endpoint::Transcriptions, None).unwrap();
        let opts = p.to_run_options();
        assert!(opts.family.is_none());

        let mut r2 = req();
        r2.prompt = Some("hi".to_string());
        let p2 = plan(&r2, &whisper_caps(), Endpoint::Transcriptions, None).unwrap();
        let opts2 = p2.to_run_options();
        assert!(matches!(
            opts2.family,
            Some(transcribe_cpp::RunExtension::Whisper(_))
        ));

        let mut r3 = req();
        r3.temperature = Some(0.2);
        let p3 = plan(&r3, &whisper_caps(), Endpoint::Transcriptions, None).unwrap();
        let opts3 = p3.to_run_options();
        assert!(matches!(
            opts3.family,
            Some(transcribe_cpp::RunExtension::Whisper(_))
        ));
    }
}
