//! Pure response formatting for `/v1/audio/transcriptions` and
//! `/v1/audio/translations`. Fed from a plain struct (`DiagnosticsExtra`) plus
//! the engine's own `Transcript` and the `Plan` that produced the run, so it
//! is unit-testable without a loaded model.
//!
//! See `parity-design.md` "Responses" for the decided shape.

use serde_json::{json, Value};

use crate::run_plan::{Plan, ResponseFormat};

/// The Handy-style waterfall for how the reported language was decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanguageEvidence {
    TranslatedToEnglish,
    UserSelected,
    ModelConstrained,
    ModelDetected,
    Unknown,
}

impl LanguageEvidence {
    pub fn as_str(self) -> &'static str {
        match self {
            LanguageEvidence::TranslatedToEnglish => "translated_to_english",
            LanguageEvidence::UserSelected => "user_selected",
            LanguageEvidence::ModelConstrained => "model_constrained",
            LanguageEvidence::ModelDetected => "model_detected",
            LanguageEvidence::Unknown => "unknown",
        }
    }

    /// Decide the waterfall from what the plan decided and what the engine
    /// actually reported at run time. `plan_evidence` is `Plan::language_evidence_hint`
    /// (already accounts for `user_selected` / `model_constrained` /
    /// `translated_to_english`); `engine_detected` is whether
    /// `Transcript.language` came back populated (the model detected it at
    /// run time, with no hint driving the choice).
    pub fn resolve(plan_evidence: Option<&str>, engine_detected: bool) -> Self {
        match plan_evidence {
            Some("translated_to_english") => LanguageEvidence::TranslatedToEnglish,
            Some("user_selected") => LanguageEvidence::UserSelected,
            Some("model_constrained") => LanguageEvidence::ModelConstrained,
            _ if engine_detected => LanguageEvidence::ModelDetected,
            _ => LanguageEvidence::Unknown,
        }
    }
}

/// Everything about the run that isn't already on `Transcript`/`Plan`, fed in
/// by the handler.
#[derive(Debug, Clone)]
pub struct DiagnosticsExtra {
    pub queue_wait_ms: u64,
    pub inference_ms: u64,
    pub audio_ms: u64,
    pub model: String,
    pub backend: String,
    pub fallback_reason: Option<String>,
    pub mel_ms: f32,
    pub encode_ms: f32,
    pub decode_ms: f32,
    pub truncated: bool,
    pub prompt_applied: bool,
    /// `Some(applied)` when the request included a language hint at all
    /// (Bug 2); `None` when no hint was sent, so the key is omitted from
    /// `x_diagnostics` entirely rather than reported as a misleading
    /// `false`.
    pub language_hint_applied: Option<bool>,
    pub applied_language: Option<String>,
    pub language_evidence: LanguageEvidence,
    /// Bug 1: set when a defaulted `verbose_json` timestamp request was
    /// retried without timestamps because the engine rejected the model's
    /// own advertised granularity.
    pub timestamps_unavailable: bool,
}

fn diagnostics_json(extra: &DiagnosticsExtra) -> Value {
    let mut v = json!({
        "queue_wait_ms": extra.queue_wait_ms,
        "inference_ms": extra.inference_ms,
        "audio_ms": extra.audio_ms,
        "model": extra.model,
        "backend": extra.backend,
        "fallback_reason": extra.fallback_reason,
        "language_evidence": extra.language_evidence.as_str(),
        "prompt_applied": extra.prompt_applied,
        "engine_timings": {
            "mel_ms": extra.mel_ms,
            "encode_ms": extra.encode_ms,
            "decode_ms": extra.decode_ms,
        },
    });
    if let Some(applied) = extra.language_hint_applied {
        v["language_hint_applied"] = json!(applied);
    }
    if let Some(language) = &extra.applied_language {
        v["applied_language"] = json!(language);
    }
    if extra.truncated {
        v["truncated"] = json!(true);
    }
    if extra.timestamps_unavailable {
        v["timestamps_unavailable"] = json!(true);
    }
    v
}

/// A formatted response body, still content-type-agnostic (the caller wires
/// this to the right `Content-Type` / status).
#[derive(Debug, Clone, PartialEq)]
pub enum Formatted {
    Json(Value),
    PlainText(String),
}

/// Format a finished (possibly truncated) transcript per the plan's
/// `response_format`. `samples` is the input PCM sample count, at the fixed
/// 16 kHz engine rate, used for `duration = samples / 16000`.
pub fn format_response(
    transcript: &transcribe_cpp::Transcript,
    plan: &Plan,
    samples: usize,
    extra: &DiagnosticsExtra,
) -> Formatted {
    let diagnostics = diagnostics_json(extra);
    match plan.response_format {
        ResponseFormat::Json => Formatted::Json(json!({
            "text": transcript.text,
            "x_diagnostics": diagnostics,
        })),
        ResponseFormat::Text => Formatted::PlainText(transcript.text.clone()),
        ResponseFormat::VerboseJson => {
            let duration = samples as f64 / 16_000.0;
            let language = extra
                .applied_language
                .clone()
                .or_else(|| transcript.language.clone());

            let segments: Vec<Value> = transcript
                .segments
                .iter()
                .enumerate()
                .map(|(id, seg)| {
                    json!({
                        "id": id,
                        "start": seg.t0_ms as f64 / 1000.0,
                        "end": seg.t1_ms as f64 / 1000.0,
                        "text": seg.text,
                    })
                })
                .collect();

            let mut body = json!({
                "task": match plan.task {
                    crate::run_plan::PlannedTask::Transcribe => "transcribe",
                    crate::run_plan::PlannedTask::Translate => "translate",
                },
                "duration": duration,
                "text": transcript.text,
                "segments": segments,
                "x_diagnostics": diagnostics,
            });
            if let Some(language) = language {
                body["language"] = json!(language);
            }
            if plan.timestamps == crate::capabilities::TimestampGranularity::Word {
                let words: Vec<Value> = transcript
                    .words
                    .iter()
                    .map(|w| {
                        json!({
                            "word": w.text,
                            "start": w.t0_ms as f64 / 1000.0,
                            "end": w.t1_ms as f64 / 1000.0,
                        })
                    })
                    .collect();
                body["words"] = json!(words);
            }
            Formatted::Json(body)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::TimestampGranularity;
    use crate::run_plan::PlannedTask;
    use transcribe_cpp::{Segment, Transcript, Word};

    fn extra() -> DiagnosticsExtra {
        DiagnosticsExtra {
            queue_wait_ms: 1,
            inference_ms: 2,
            audio_ms: 3,
            model: "m".to_string(),
            backend: "cpu".to_string(),
            fallback_reason: None,
            mel_ms: 1.0,
            encode_ms: 2.0,
            decode_ms: 3.0,
            truncated: false,
            prompt_applied: false,
            language_hint_applied: None,
            applied_language: None,
            language_evidence: LanguageEvidence::Unknown,
            timestamps_unavailable: false,
        }
    }

    fn plan(response_format: ResponseFormat, timestamps: TimestampGranularity) -> Plan {
        Plan {
            language: None,
            task: PlannedTask::Transcribe,
            target_language: None,
            initial_prompt: None,
            temperature: None,
            timestamps,
            response_format,
            language_hint_applied: false,
            language_hint_provided: false,
            applied_language: None,
            language_evidence_hint: None,
            timestamps_explicit: false,
        }
    }

    #[test]
    fn json_format_has_text_and_diagnostics_only() {
        let t = Transcript {
            text: "hello".to_string(),
            ..Default::default()
        };
        let p = plan(ResponseFormat::Json, TimestampGranularity::None);
        let out = format_response(&t, &p, 16_000, &extra());
        match out {
            Formatted::Json(v) => {
                assert_eq!(v["text"], "hello");
                assert!(v.get("segments").is_none());
                assert!(v["x_diagnostics"].is_object());
            }
            _ => panic!("expected json"),
        }
    }

    #[test]
    fn text_format_is_plain_text_only() {
        let t = Transcript {
            text: "hello".to_string(),
            ..Default::default()
        };
        let p = plan(ResponseFormat::Text, TimestampGranularity::None);
        let out = format_response(&t, &p, 16_000, &extra());
        assert_eq!(out, Formatted::PlainText("hello".to_string()));
    }

    #[test]
    fn verbose_json_has_segments_duration_and_omits_words_when_not_planned() {
        let t = Transcript {
            text: "hi there".to_string(),
            segments: vec![Segment {
                t0_ms: 0,
                t1_ms: 1500,
                text: "hi there".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let p = plan(ResponseFormat::VerboseJson, TimestampGranularity::Segment);
        let out = format_response(&t, &p, 32_000, &extra());
        match out {
            Formatted::Json(v) => {
                assert_eq!(v["task"], "transcribe");
                assert_eq!(v["duration"], 2.0);
                assert_eq!(v["segments"][0]["id"], 0);
                assert_eq!(v["segments"][0]["start"], 0.0);
                assert_eq!(v["segments"][0]["end"], 1.5);
                assert!(v.get("words").is_none());
                assert!(v.get("language").is_none());
            }
            _ => panic!("expected json"),
        }
    }

    #[test]
    fn verbose_json_includes_words_only_when_word_timestamps_planned() {
        let t = Transcript {
            words: vec![Word {
                t0_ms: 0,
                t1_ms: 500,
                text: "hi".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let p = plan(ResponseFormat::VerboseJson, TimestampGranularity::Word);
        let out = format_response(&t, &p, 8_000, &extra());
        match out {
            Formatted::Json(v) => {
                assert_eq!(v["words"][0]["word"], "hi");
                assert_eq!(v["words"][0]["start"], 0.0);
                assert_eq!(v["words"][0]["end"], 0.5);
            }
            _ => panic!("expected json"),
        }
    }

    #[test]
    fn verbose_json_language_prefers_applied_language_then_transcript_language() {
        let t = Transcript {
            language: Some("es".to_string()),
            ..Default::default()
        };
        let p = plan(ResponseFormat::VerboseJson, TimestampGranularity::None);

        let mut e = extra();
        e.applied_language = Some("fr".to_string());
        let out = format_response(&t, &p, 16_000, &e);
        match out {
            Formatted::Json(v) => assert_eq!(v["language"], "fr"),
            _ => panic!("expected json"),
        }

        let out2 = format_response(&t, &p, 16_000, &extra());
        match out2 {
            Formatted::Json(v) => assert_eq!(v["language"], "es"),
            _ => panic!("expected json"),
        }
    }

    #[test]
    fn truncated_flag_only_present_when_true() {
        let t = Transcript::default();
        let p = plan(ResponseFormat::Json, TimestampGranularity::None);
        let out = format_response(&t, &p, 16_000, &extra());
        match out {
            Formatted::Json(v) => assert!(v["x_diagnostics"].get("truncated").is_none()),
            _ => panic!("expected json"),
        }

        let mut e = extra();
        e.truncated = true;
        let out2 = format_response(&t, &p, 16_000, &e);
        match out2 {
            Formatted::Json(v) => assert_eq!(v["x_diagnostics"]["truncated"], true),
            _ => panic!("expected json"),
        }
    }

    #[test]
    fn language_hint_applied_key_omitted_when_no_hint_was_sent() {
        let t = Transcript::default();
        let p = plan(ResponseFormat::Json, TimestampGranularity::None);
        let out = format_response(&t, &p, 16_000, &extra());
        match out {
            Formatted::Json(v) => {
                assert!(v["x_diagnostics"].get("language_hint_applied").is_none())
            }
            _ => panic!("expected json"),
        }
    }

    #[test]
    fn language_hint_applied_key_present_true_or_false_when_hint_was_sent() {
        let t = Transcript::default();
        let p = plan(ResponseFormat::Json, TimestampGranularity::None);

        let mut e_false = extra();
        e_false.language_hint_applied = Some(false);
        let out = format_response(&t, &p, 16_000, &e_false);
        match out {
            Formatted::Json(v) => assert_eq!(v["x_diagnostics"]["language_hint_applied"], false),
            _ => panic!("expected json"),
        }

        let mut e_true = extra();
        e_true.language_hint_applied = Some(true);
        let out2 = format_response(&t, &p, 16_000, &e_true);
        match out2 {
            Formatted::Json(v) => assert_eq!(v["x_diagnostics"]["language_hint_applied"], true),
            _ => panic!("expected json"),
        }
    }

    #[test]
    fn timestamps_unavailable_flag_only_present_when_true() {
        let t = Transcript::default();
        let p = plan(ResponseFormat::VerboseJson, TimestampGranularity::None);
        let out = format_response(&t, &p, 16_000, &extra());
        match out {
            Formatted::Json(v) => {
                assert!(v["x_diagnostics"].get("timestamps_unavailable").is_none())
            }
            _ => panic!("expected json"),
        }

        let mut e = extra();
        e.timestamps_unavailable = true;
        let out2 = format_response(&t, &p, 16_000, &e);
        match out2 {
            Formatted::Json(v) => assert_eq!(v["x_diagnostics"]["timestamps_unavailable"], true),
            _ => panic!("expected json"),
        }
    }

    #[test]
    fn language_evidence_waterfall() {
        assert_eq!(
            LanguageEvidence::resolve(Some("translated_to_english"), true),
            LanguageEvidence::TranslatedToEnglish
        );
        assert_eq!(
            LanguageEvidence::resolve(Some("user_selected"), false),
            LanguageEvidence::UserSelected
        );
        assert_eq!(
            LanguageEvidence::resolve(Some("model_constrained"), false),
            LanguageEvidence::ModelConstrained
        );
        assert_eq!(
            LanguageEvidence::resolve(None, true),
            LanguageEvidence::ModelDetected
        );
        assert_eq!(
            LanguageEvidence::resolve(None, false),
            LanguageEvidence::Unknown
        );
    }
}
