//! Curated catalog of sherpa-onnx ASR models for Stage 1 (voice-to-text only).
//!
//! Upstream ships ~500 ASR release assets with no official "recommended" subset
//! (verified against https://github.com/k2-fsa/sherpa-onnx/releases/tag/asr-models).
//! We hand-pick a small, known-good set instead of surfacing all of them; each
//! entry here has been downloaded and smoke-tested. Extend this list as new
//! entries are verified — don't add one from the release page without testing it.

use serde::Serialize;

/// Which model family an entry belongs to, and the file layout `sherpad` needs
/// to build the corresponding `sherpa_onnx::Offline*ModelConfig`.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "family", rename_all = "snake_case")]
pub enum ModelFiles {
    SenseVoice {
        /// Relative to the extracted archive root.
        model: &'static str,
        tokens: &'static str,
    },
    Whisper {
        encoder: &'static str,
        decoder: &'static str,
        tokens: &'static str,
    },
    /// NeMo-exported transducer models (e.g. NVIDIA Parakeet TDT): separate
    /// encoder/decoder/joiner ONNX graphs, no combined encoder-decoder step.
    Transducer {
        encoder: &'static str,
        decoder: &'static str,
        joiner: &'static str,
        tokens: &'static str,
    },
}

#[derive(Clone, Debug, Serialize)]
pub struct ModelEntry {
    /// Stable id used in the API and CLI, e.g. "sense-voice-multi".
    pub id: &'static str,
    pub description: &'static str,
    /// BCP-47-ish language tags this model covers, or ["auto"] for
    /// language-agnostic/auto-detecting models.
    pub languages: &'static [&'static str],
    pub download_url: &'static str,
    /// Size of the .tar.bz2 download, for CLI/API display before pulling.
    pub download_bytes: u64,
    /// Top-level directory name produced by extracting the archive.
    pub archive_root: &'static str,
    pub files: ModelFiles,
    /// Passed as the model's `language` config field when the request doesn't
    /// override it.
    pub default_language: &'static str,
}

pub const MODELS: &[ModelEntry] = &[
    ModelEntry {
        id: "sense-voice-multi",
        description: "SenseVoice, multilingual (zh/en/ja/ko/yue), int8 quantized. Default general-purpose pick.",
        languages: &["auto", "zh", "en", "ja", "ko", "yue"],
        download_url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2024-07-17.tar.bz2",
        download_bytes: 163_002_883,
        archive_root: "sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2024-07-17",
        files: ModelFiles::SenseVoice {
            model: "model.int8.onnx",
            tokens: "tokens.txt",
        },
        default_language: "auto",
    },
    ModelEntry {
        id: "whisper-tiny-en",
        description: "Whisper tiny.en, int8 quantized. Smallest/fastest English-only option.",
        languages: &["en"],
        download_url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-whisper-tiny.en.tar.bz2",
        download_bytes: 118_071_777,
        archive_root: "sherpa-onnx-whisper-tiny.en",
        files: ModelFiles::Whisper {
            encoder: "tiny.en-encoder.int8.onnx",
            decoder: "tiny.en-decoder.int8.onnx",
            tokens: "tiny.en-tokens.txt",
        },
        default_language: "en",
    },
    ModelEntry {
        id: "whisper-base-en",
        description: "Whisper base.en, int8 quantized. Better accuracy than tiny.en, still English-only.",
        languages: &["en"],
        download_url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-whisper-base.en.tar.bz2",
        download_bytes: 208_576_005,
        archive_root: "sherpa-onnx-whisper-base.en",
        files: ModelFiles::Whisper {
            encoder: "base.en-encoder.int8.onnx",
            decoder: "base.en-decoder.int8.onnx",
            tokens: "base.en-tokens.txt",
        },
        default_language: "en",
    },
    ModelEntry {
        id: "parakeet-tdt-0.6b-v2",
        description: "NVIDIA Parakeet TDT 0.6B v2, int8 quantized. Transducer decoding (no autoregressive decoder loop) — the speed candidate under validate-parakeet-performance. English only.",
        languages: &["en"],
        download_url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8.tar.bz2",
        download_bytes: 482_468_385,
        archive_root: "sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8",
        files: ModelFiles::Transducer {
            encoder: "encoder.int8.onnx",
            decoder: "decoder.int8.onnx",
            joiner: "joiner.int8.onnx",
            tokens: "tokens.txt",
        },
        default_language: "en",
    },
];

pub fn find(id: &str) -> Option<&'static ModelEntry> {
    MODELS.iter().find(|m| m.id == id)
}
