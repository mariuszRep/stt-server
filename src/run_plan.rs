//! Pure request planning: turn a parsed transcription/translation request plus
//! the loaded model's effective capabilities into a `Plan`, or reject it with
//! an `ApiError`. No model access — everything here is unit-testable.
//!
//! Language-matching helpers (`base_language`, `canonical_language_code`,
//! `normalize_cjk_language`) are ported from Handy (MIT), commit `8f9cf53`,
//! `src-tauri/src/managers/model.rs:95-109` and
//! `src-tauri/src/managers/transcription.rs:1652`. See
//! `THIRD_PARTY_NOTICES.md` for Handy's MIT notice.

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
    pub prompt: Option<String>,
    pub vocabulary: Vec<String>,
    pub temperature: Option<f32>,
    pub response_format: Option<String>,
    pub timestamp_granularities: Vec<String>,
}

/// The finished, engine-ready plan.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub language: Option<String>,
    pub task: PlannedTask,
    pub target_language: Option<String>,
    pub initial_prompt: Option<String>,
    pub temperature: Option<f32>,
    pub timestamps: TimestampGranularity,
    pub response_format: ResponseFormat,
    pub prompt_truncated: bool,
    pub language_evidence_hint: Option<String>,
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

/// Resolve a requested language hint against the loaded model's supported
/// languages. Returns the model's own code on a match, `None` for
/// absent/empty/"auto", or an `ApiError` (422 `unsupported_language`) when the
/// hint cannot be matched to anything the model supports.
fn resolve_language_hint(
    requested: &str,
    supported: &[String],
) -> Result<Option<String>, ApiError> {
    let trimmed = requested.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("auto") {
        return Ok(None);
    }
    let normalized = normalize_cjk_language(&trimmed.to_lowercase()).to_string();

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

    if let Some(code) = exact.or_else(alias) {
        return Ok(Some(code.clone()));
    }

    Err(ApiError::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        "unsupported_language",
        format!("Language '{requested}' is not supported by this model"),
    )
    .with_details(json!({
        "requested": requested,
        "supported": supported,
    })))
}

// ---------------------------------------------------------------------------
// Prompt / vocabulary budget.
// ---------------------------------------------------------------------------

const MAX_VOCABULARY_ITEMS: usize = 100;
const MAX_VOCABULARY_TERM_LEN: usize = 64;
const PROMPT_TOKEN_BUDGET: usize = 223;
const PROMPT_FALLBACK_CHAR_BUDGET: usize = 900;

fn validate_vocabulary(vocabulary: &[String]) -> Result<Vec<String>, ApiError> {
    if vocabulary.len() > MAX_VOCABULARY_ITEMS {
        return Err(invalid_vocabulary(format!(
            "at most {MAX_VOCABULARY_ITEMS} vocabulary terms are allowed, got {}",
            vocabulary.len()
        )));
    }
    let mut cleaned = Vec::with_capacity(vocabulary.len());
    for term in vocabulary {
        let trimmed = term.trim();
        if trimmed.is_empty() {
            return Err(invalid_vocabulary("vocabulary terms must not be empty"));
        }
        if trimmed.chars().count() > MAX_VOCABULARY_TERM_LEN {
            return Err(invalid_vocabulary(format!(
                "vocabulary term '{trimmed}' exceeds {MAX_VOCABULARY_TERM_LEN} characters"
            )));
        }
        if trimmed.chars().any(|c| c.is_control()) {
            return Err(invalid_vocabulary(
                "vocabulary terms must not contain control characters",
            ));
        }
        cleaned.push(trimmed.to_string());
    }
    Ok(cleaned)
}

fn invalid_vocabulary(message: impl Into<String>) -> ApiError {
    ApiError::new(
        StatusCode::BAD_REQUEST,
        "invalid_vocabulary",
        message.into(),
    )
}

/// Join vocabulary terms and an optional prompt into the whisper initial
/// prompt string: `vocab, terms, here` then, if a prompt is also present,
/// `. <prompt>` appended.
fn join_prompt(vocabulary: &[String], prompt: Option<&str>) -> Option<String> {
    let vocab_part = if vocabulary.is_empty() {
        None
    } else {
        Some(vocabulary.join(", "))
    };
    match (vocab_part, prompt) {
        (Some(vocab), Some(p)) if !p.is_empty() => Some(format!("{vocab}. {p}")),
        (Some(vocab), _) => Some(vocab),
        (None, Some(p)) if !p.is_empty() => Some(p.to_string()),
        (None, _) => None,
    }
}

/// Count "tokens" via the supplied tokenizer closure when it succeeds,
/// otherwise fall back to a character-budget proxy (900 chars ~ 223 tokens).
fn fits_budget(text: &str, tokenize: Option<&Tokenizer>) -> bool {
    if let Some(tokenize) = tokenize {
        if let Some(count) = tokenize(text) {
            return count <= PROMPT_TOKEN_BUDGET;
        }
    }
    text.chars().count() <= PROMPT_FALLBACK_CHAR_BUDGET
}

fn token_count_or_chars(text: &str, tokenize: Option<&Tokenizer>) -> usize {
    if let Some(tokenize) = tokenize {
        if let Some(count) = tokenize(text) {
            return count;
        }
    }
    text.chars().count()
}

fn budget_limit(tokenize: Option<&Tokenizer>, text_has_tokenizer_result: bool) -> usize {
    // Only used to decide which limit governs left-trimming; kept for clarity.
    let _ = text_has_tokenizer_result;
    if tokenize.is_some() {
        PROMPT_TOKEN_BUDGET
    } else {
        PROMPT_FALLBACK_CHAR_BUDGET
    }
}

/// Build the final initial-prompt string within budget, left-trimming the
/// oldest part of `prompt` (never the vocabulary) at a word boundary until it
/// fits. Returns `(final_prompt, truncated)`.
fn build_initial_prompt(
    vocabulary: &[String],
    prompt: Option<&str>,
    tokenize: Option<&Tokenizer>,
) -> Result<(Option<String>, bool), ApiError> {
    let vocab_joined = if vocabulary.is_empty() {
        None
    } else {
        Some(vocabulary.join(", "))
    };

    let combined = join_prompt(vocabulary, prompt);
    let combined = match combined {
        None => return Ok((None, false)),
        Some(c) => c,
    };

    if fits_budget(&combined, tokenize) {
        return Ok((Some(combined), false));
    }

    // Vocabulary alone (with no prompt to trim) over budget -> 422.
    if prompt.map(str::is_empty).unwrap_or(true) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "prompt_too_long",
            "Vocabulary alone exceeds the prompt budget",
        ));
    }
    if let Some(vocab) = &vocab_joined {
        if !fits_budget(vocab, tokenize) {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "prompt_too_long",
                "Vocabulary alone exceeds the prompt budget",
            ));
        }
    }

    // Left-trim the oldest part of `prompt` at a word boundary until the
    // combined string fits.
    let prompt_text = prompt.unwrap_or_default();
    let prefix = vocab_joined
        .as_ref()
        .map(|v| format!("{v}. "))
        .unwrap_or_default();
    let limit = budget_limit(tokenize, false);
    let prefix_cost = token_count_or_chars(&prefix, tokenize);
    if prefix_cost >= limit {
        // Even the vocabulary+separator alone doesn't fit with any prompt
        // headroom; treat as vocabulary-alone-too-long.
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "prompt_too_long",
            "Vocabulary alone exceeds the prompt budget",
        ));
    }

    let words: Vec<&str> = prompt_text.split_whitespace().collect();
    let mut start = 0usize;
    loop {
        let candidate_words = &words[start..];
        let candidate_prompt = candidate_words.join(" ");
        let candidate = format!("{prefix}{candidate_prompt}");
        if fits_budget(&candidate, tokenize) || candidate_words.is_empty() {
            let final_text = if candidate_words.is_empty() {
                prefix.trim_end_matches(". ").to_string()
            } else {
                candidate
            };
            return Ok((Some(final_text), true));
        }
        start += 1;
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
    let mut language_evidence_hint: Option<String> = None;
    let language_hint = match &request.language {
        Some(raw) if !raw.trim().is_empty() => {
            let resolved = resolve_language_hint(raw, &caps.loaded.languages)?;
            if resolved.is_some() {
                language_evidence_hint = Some("user_selected".to_string());
            }
            resolved
        }
        _ => None,
    };

    // --- prompt / vocabulary -------------------------------------------
    let vocabulary = validate_vocabulary(&request.vocabulary)?;
    let has_prompt_input = request
        .prompt
        .as_deref()
        .map(|p| !p.is_empty())
        .unwrap_or(false)
        || !vocabulary.is_empty();
    if has_prompt_input && !caps.is_whisper() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unsupported_capability",
            if request
                .prompt
                .as_deref()
                .map(|p| !p.is_empty())
                .unwrap_or(false)
            {
                "This model does not support 'prompt'"
            } else {
                "This model does not support 'vocabulary'"
            },
        ));
    }
    let (initial_prompt, prompt_truncated) = if has_prompt_input {
        build_initial_prompt(&vocabulary, request.prompt.as_deref(), tokenize)?
    } else {
        (None, false)
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
    let timestamps = match response_format {
        ResponseFormat::VerboseJson => {
            if word_requested {
                if !caps.supports_word_timestamps() {
                    return Err(ApiError::new(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "unsupported_capability",
                        "This model does not support word-level timestamps",
                    ));
                }
                TimestampGranularity::Word
            } else {
                TimestampGranularity::Segment
            }
        }
        ResponseFormat::Json | ResponseFormat::Text => TimestampGranularity::None,
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
            let source_is_english = language_hint.as_deref() == Some("en")
                || (caps.loaded.languages.len() == 1 && caps.loaded.languages[0] == "en");
            if source_is_english {
                language_evidence_hint = Some("translated_to_english".to_string());
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
        prompt_truncated,
        language_evidence_hint,
    })
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
    }

    #[test]
    fn alias_nb_matches_no() {
        let mut r = req();
        r.language = Some("nb".to_string());
        let p = plan(&r, &whisper_caps(), Endpoint::Transcriptions, None).unwrap();
        assert_eq!(p.language, Some("no".to_string()));
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
    fn unsupported_language_is_422_with_details() {
        let mut r = req();
        r.language = Some("xx".to_string());
        let err = plan(&r, &whisper_caps(), Endpoint::Transcriptions, None).unwrap_err();
        assert_eq!(err.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(err.code, "unsupported_language");
        let details = err.details.unwrap();
        assert_eq!(details["requested"], "xx");
    }

    #[test]
    fn auto_and_empty_mean_no_hint() {
        let mut r = req();
        r.language = Some("auto".to_string());
        let p = plan(&r, &whisper_caps(), Endpoint::Transcriptions, None).unwrap();
        assert_eq!(p.language, None);

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
    }

    // --- prompt / vocabulary -----------------------------------------------

    #[test]
    fn prompt_supported_on_whisper() {
        let mut r = req();
        r.prompt = Some("hello world".to_string());
        let p = plan(&r, &whisper_caps(), Endpoint::Transcriptions, None).unwrap();
        assert_eq!(p.initial_prompt.as_deref(), Some("hello world"));
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
    fn vocabulary_over_101_items_is_400() {
        let mut r = req();
        r.vocabulary = (0..101).map(|i| format!("term{i}")).collect();
        let err = plan(&r, &whisper_caps(), Endpoint::Transcriptions, None).unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert_eq!(err.code, "invalid_vocabulary");
    }

    #[test]
    fn vocabulary_term_65_chars_is_400() {
        let mut r = req();
        r.vocabulary = vec!["a".repeat(65)];
        let err = plan(&r, &whisper_caps(), Endpoint::Transcriptions, None).unwrap_err();
        assert_eq!(err.code, "invalid_vocabulary");
    }

    #[test]
    fn vocabulary_empty_term_is_400() {
        let mut r = req();
        r.vocabulary = vec!["   ".to_string()];
        let err = plan(&r, &whisper_caps(), Endpoint::Transcriptions, None).unwrap_err();
        assert_eq!(err.code, "invalid_vocabulary");
    }

    #[test]
    fn vocabulary_control_char_is_400() {
        let mut r = req();
        r.vocabulary = vec!["bad\u{0007}word".to_string()];
        let err = plan(&r, &whisper_caps(), Endpoint::Transcriptions, None).unwrap_err();
        assert_eq!(err.code, "invalid_vocabulary");
    }

    #[test]
    fn vocabulary_and_prompt_join_format() {
        let mut r = req();
        r.vocabulary = vec!["Alice".to_string(), "Bob".to_string()];
        r.prompt = Some("They spoke.".to_string());
        let p = plan(&r, &whisper_caps(), Endpoint::Transcriptions, None).unwrap();
        assert_eq!(p.initial_prompt.as_deref(), Some("Alice, Bob. They spoke."));
    }

    #[test]
    fn budget_left_trim_with_fake_tokenizer() {
        // Fake tokenizer: 1 token per word.
        let tokenize = |s: &str| Some(s.split_whitespace().count());
        let mut r = req();
        r.prompt = Some(
            (0..300)
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
        assert!(p.prompt_truncated);
        let final_prompt = p.initial_prompt.unwrap();
        assert!(final_prompt.split_whitespace().count() <= PROMPT_TOKEN_BUDGET);
        // Left-trim means the OLDEST part (the start) is dropped, so the tail
        // word should survive.
        assert!(final_prompt.contains("w299"));
    }

    #[test]
    fn budget_left_trim_with_900_char_fallback() {
        let mut r = req();
        r.prompt = Some("word ".repeat(400));
        let p = plan(&r, &whisper_caps(), Endpoint::Transcriptions, None).unwrap();
        assert!(p.prompt_truncated);
        assert!(p.initial_prompt.unwrap().chars().count() <= PROMPT_FALLBACK_CHAR_BUDGET);
    }

    #[test]
    fn vocabulary_alone_over_budget_is_422_prompt_too_long() {
        let mut r = req();
        r.vocabulary = (0..100)
            .map(|i| format!("term-number-{i}-{}", "x".repeat(30)))
            .collect();
        let err = plan(&r, &whisper_caps(), Endpoint::Transcriptions, None).unwrap_err();
        assert_eq!(err.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(err.code, "prompt_too_long");
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
        assert_eq!(
            p.language_evidence_hint.as_deref(),
            Some("translated_to_english")
        );
    }

    #[test]
    fn translation_single_language_en_model_transcribes() {
        let r = req();
        let p = plan(&r, &single_lang_en_caps(), Endpoint::Translations, None).unwrap();
        assert_eq!(p.task, PlannedTask::Transcribe);
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
