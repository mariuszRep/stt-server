//! Minimal, header-only GGUF metadata probe used to classify drop-in models
//! copied into the user models folder (see `crate::dropin`).
//!
//! Ported from Handy (MIT licence, github.com/handy-computer/Handy, commit
//! `8f9cf53`) `src-tauri/src/managers/gguf_meta.rs` and
//! `model_capabilities.rs`, trimmed to the key/value pairs and architecture
//! list this server needs; see `README.md` for the full attribution.

use std::collections::HashMap;
use std::path::Path;

const GGUF_MAGIC: u32 = 0x4655_4747;

const T_UINT8: u32 = 0;
const T_INT8: u32 = 1;
const T_UINT16: u32 = 2;
const T_INT16: u32 = 3;
const T_UINT32: u32 = 4;
const T_INT32: u32 = 5;
const T_FLOAT32: u32 = 6;
const T_BOOL: u32 = 7;
const T_STRING: u32 = 8;
const T_ARRAY: u32 = 9;
const T_UINT64: u32 = 10;
const T_INT64: u32 = 11;
const T_FLOAT64: u32 = 12;

const MAX_STRING_LEN: usize = 64 * 1024 * 1024;
const MAX_ARRAY_LEN: u64 = 16 * 1024 * 1024;
const MAX_STORED_ARRAY_LEN: u64 = 4096;
const MAX_KV_COUNT: u64 = 1_000_000;

/// Architectures the pinned `transcribe-cpp` build accepts. Kept in sync with
/// Handy's `KNOWN_ARCHES` (same source commit as the module attribution).
pub const KNOWN_ARCHES: &[&str] = &[
    "whisper",
    "parakeet",
    "qwen3_asr",
    "voxtral",
    "voxtral_realtime",
    "cohere",
    "cohere_asr",
    "canary",
    "canary_qwen",
    "moonshine",
    "moonshine_streaming",
    "sensevoice",
    "gigaam",
    "granite",
    "granite_speech",
    "granite_nar",
    "granite_speech_nar",
    "funasr_nano",
    "medasr",
    "moss",
    "sortformer",
];

const KEY_ARCH: &str = "general.architecture";
const KEY_NAME: &str = "general.name";
const KEY_LANGUAGES: &str = "general.languages";
const KEY_CAP_STREAMING: &str = "stt.capability.streaming";
const KEY_CAP_TRANSLATE: &str = "stt.capability.translate";
const KEY_CAP_LANG_DETECT: &str = "stt.capability.lang_detect";
const PROBE_KEYS: &[&str] = &[
    KEY_ARCH,
    KEY_NAME,
    KEY_LANGUAGES,
    KEY_CAP_STREAMING,
    KEY_CAP_TRANSLATE,
    KEY_CAP_LANG_DETECT,
];

#[derive(Debug, Clone, PartialEq)]
enum GgufValue {
    U8(u8),
    I8(i8),
    U16(u16),
    I16(i16),
    U32(u32),
    I32(i32),
    U64(u64),
    I64(i64),
    F32(f32),
    F64(f64),
    Bool(bool),
    String(String),
    Array(Vec<GgufValue>),
}

impl GgufValue {
    fn as_str(&self) -> Option<&str> {
        match self {
            GgufValue::String(s) => Some(s.as_str()),
            _ => None,
        }
    }

    fn as_bool(&self) -> Option<bool> {
        match self {
            GgufValue::Bool(b) => Some(*b),
            GgufValue::U8(v) => Some(*v != 0),
            GgufValue::I8(v) => Some(*v != 0),
            GgufValue::U32(v) => Some(*v != 0),
            GgufValue::I32(v) => Some(*v != 0),
            _ => None,
        }
    }

    fn as_string_array(&self) -> Option<Vec<String>> {
        match self {
            GgufValue::Array(items) => items
                .iter()
                .map(|v| v.as_str().map(str::to_string))
                .collect(),
            _ => None,
        }
    }
}

struct GgufMetadata {
    kv: HashMap<String, GgufValue>,
}

impl GgufMetadata {
    fn get_str(&self, key: &str) -> Option<&str> {
        self.kv.get(key).and_then(GgufValue::as_str)
    }
    fn get_bool(&self, key: &str) -> Option<bool> {
        self.kv.get(key).and_then(GgufValue::as_bool)
    }
    fn get_string_array(&self, key: &str) -> Option<Vec<String>> {
        self.kv.get(key).and_then(GgufValue::as_string_array)
    }
}

#[derive(Debug)]
enum GgufError {
    NotGguf,
    UnsupportedVersion(u32),
    Truncated { needed: usize },
    Malformed(&'static str),
}

impl std::fmt::Display for GgufError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GgufError::NotGguf => write!(f, "not a GGUF file"),
            GgufError::UnsupportedVersion(v) => write!(f, "unsupported GGUF version {v}"),
            GgufError::Truncated { needed } => {
                write!(f, "buffer truncated, need at least {needed} bytes")
            }
            GgufError::Malformed(why) => write!(f, "malformed GGUF: {why}"),
        }
    }
}

struct ByteCursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> ByteCursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        ByteCursor { buf, pos: 0 }
    }

    fn truncated(&self, more: usize) -> GgufError {
        GgufError::Truncated {
            needed: self.pos.saturating_add(more),
        }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], GgufError> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or(GgufError::Malformed("length overflow"))?;
        if end > self.buf.len() {
            return Err(self.truncated(n));
        }
        let slice = &self.buf[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn u32(&mut self) -> Result<u32, GgufError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self) -> Result<u64, GgufError> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    fn string_len(&mut self) -> Result<usize, GgufError> {
        let len = usize::try_from(self.u64()?)
            .map_err(|_| GgufError::Malformed("string length too large"))?;
        if len > MAX_STRING_LEN {
            return Err(GgufError::Malformed("string length too large"));
        }
        Ok(len)
    }

    fn string(&mut self) -> Result<String, GgufError> {
        let len = self.string_len()?;
        let bytes = self.take(len)?;
        Ok(String::from_utf8_lossy(bytes).into_owned())
    }

    fn skip_string(&mut self) -> Result<(), GgufError> {
        let len = self.string_len()?;
        self.take(len)?;
        Ok(())
    }
}

fn read_value(cur: &mut ByteCursor, value_type: u32) -> Result<GgufValue, GgufError> {
    Ok(match value_type {
        T_UINT8 => GgufValue::U8(cur.take(1)?[0]),
        T_INT8 => GgufValue::I8(cur.take(1)?[0] as i8),
        T_UINT16 => {
            let b = cur.take(2)?;
            GgufValue::U16(u16::from_le_bytes([b[0], b[1]]))
        }
        T_INT16 => {
            let b = cur.take(2)?;
            GgufValue::I16(i16::from_le_bytes([b[0], b[1]]))
        }
        T_UINT32 => GgufValue::U32(cur.u32()?),
        T_INT32 => GgufValue::I32(cur.u32()? as i32),
        T_FLOAT32 => GgufValue::F32(f32::from_bits(cur.u32()?)),
        T_BOOL => GgufValue::Bool(cur.take(1)?[0] != 0),
        T_STRING => GgufValue::String(cur.string()?),
        T_UINT64 => GgufValue::U64(cur.u64()?),
        T_INT64 => GgufValue::I64(cur.u64()? as i64),
        T_FLOAT64 => GgufValue::F64(f64::from_bits(cur.u64()?)),
        T_ARRAY => {
            let elem_type = cur.u32()?;
            if elem_type == T_ARRAY {
                return Err(GgufError::Malformed("nested arrays are not allowed"));
            }
            let len = cur.u64()?;
            if len > MAX_ARRAY_LEN {
                return Err(GgufError::Malformed("array length too large"));
            }
            if len > MAX_STORED_ARRAY_LEN {
                return Err(GgufError::Malformed("stored array length too large"));
            }
            let mut items = Vec::with_capacity(len.min(1024) as usize);
            for _ in 0..len {
                items.push(read_value(cur, elem_type)?);
            }
            GgufValue::Array(items)
        }
        _ => return Err(GgufError::Malformed("unknown value type")),
    })
}

fn scalar_size(value_type: u32) -> Option<usize> {
    match value_type {
        T_UINT8 | T_INT8 | T_BOOL => Some(1),
        T_UINT16 | T_INT16 => Some(2),
        T_UINT32 | T_INT32 | T_FLOAT32 => Some(4),
        T_UINT64 | T_INT64 | T_FLOAT64 => Some(8),
        _ => None,
    }
}

fn skip_value(cur: &mut ByteCursor, value_type: u32) -> Result<(), GgufError> {
    if let Some(size) = scalar_size(value_type) {
        cur.take(size)?;
        return Ok(());
    }
    match value_type {
        T_STRING => cur.skip_string(),
        T_ARRAY => {
            let elem_type = cur.u32()?;
            if elem_type == T_ARRAY {
                return Err(GgufError::Malformed("nested arrays are not allowed"));
            }
            let len = cur.u64()?;
            if len > MAX_ARRAY_LEN {
                return Err(GgufError::Malformed("array length too large"));
            }
            if let Some(size) = scalar_size(elem_type) {
                let bytes = usize::try_from(len)
                    .ok()
                    .and_then(|len| len.checked_mul(size))
                    .ok_or(GgufError::Malformed("length overflow"))?;
                cur.take(bytes)?;
            } else if elem_type == T_STRING {
                for _ in 0..len {
                    cur.skip_string()?;
                }
            } else {
                return Err(GgufError::Malformed("unknown array element type"));
            }
            Ok(())
        }
        _ => Err(GgufError::Malformed("unknown value type")),
    }
}

fn parse_header(bytes: &[u8], wanted_keys: &[&str]) -> Result<GgufMetadata, GgufError> {
    let mut cur = ByteCursor::new(bytes);
    let magic = cur.u32()?;
    if magic != GGUF_MAGIC {
        return Err(GgufError::NotGguf);
    }
    let version = cur.u32()?;
    if version != 2 && version != 3 {
        return Err(GgufError::UnsupportedVersion(version));
    }
    cur.u64()?; // tensor_count
    let kv_count = cur.u64()?;
    if kv_count > MAX_KV_COUNT {
        return Err(GgufError::Malformed("absurd metadata kv count"));
    }
    let mut kv = HashMap::with_capacity(wanted_keys.len());
    for _ in 0..kv_count {
        let key = cur.string()?;
        let value_type = cur.u32()?;
        if wanted_keys.contains(&key.as_str()) {
            let value = read_value(&mut cur, value_type)?;
            kv.insert(key, value);
            if kv.len() == wanted_keys.len() {
                break;
            }
        } else {
            skip_value(&mut cur, value_type)?;
        }
    }
    Ok(GgufMetadata { kv })
}

/// Read up to `size` bytes from the start of `path`, tolerating short reads.
fn read_prefix(path: &Path, size: usize) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut buf = vec![0u8; size];
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    buf.truncate(filled);
    Ok(buf)
}

fn read_header_metadata(path: &Path) -> Result<GgufMetadata, GgufError> {
    const INITIAL_PREFIX: usize = 64 << 10;
    const MAX_PREFIX: usize = 16 << 20;
    let mut size = INITIAL_PREFIX;
    loop {
        let buf = read_prefix(path, size).map_err(|_| GgufError::Malformed("cannot read file"))?;
        let read_len = buf.len();
        match parse_header(&buf, PROBE_KEYS) {
            Ok(meta) => return Ok(meta),
            Err(GgufError::Truncated { needed }) => {
                if read_len < size {
                    return Err(GgufError::Malformed("file shorter than its header"));
                }
                let next = needed.max(size.saturating_mul(2)).min(MAX_PREFIX);
                if next <= size {
                    return Err(GgufError::Truncated { needed });
                }
                size = next;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Result of probing a candidate drop-in file's GGUF header.
#[derive(Debug, Clone)]
pub struct GgufProbe {
    pub architecture: String,
    pub display_name: Option<String>,
    pub languages: Vec<String>,
    pub supports_streaming: bool,
    pub supports_translation: bool,
    pub supports_language_detect: bool,
}

impl GgufProbe {
    pub fn is_supported(&self) -> bool {
        KNOWN_ARCHES.contains(&self.architecture.as_str())
    }
}

/// Probe `path`'s GGUF header. Returns a plain string reason on any failure
/// (not a GGUF, truncated/malformed, or unreadable) suitable for a refresh
/// result's `unsupported` list.
pub fn probe_gguf_file(path: &Path) -> Result<GgufProbe, String> {
    let meta = read_header_metadata(path).map_err(|error| error.to_string())?;
    let architecture = meta
        .get_str(KEY_ARCH)
        .map(str::to_string)
        .ok_or_else(|| "missing general.architecture".to_string())?;
    Ok(GgufProbe {
        architecture,
        display_name: meta.get_str(KEY_NAME).map(str::to_string),
        languages: meta.get_string_array(KEY_LANGUAGES).unwrap_or_default(),
        supports_streaming: meta.get_bool(KEY_CAP_STREAMING).unwrap_or(false),
        supports_translation: meta.get_bool(KEY_CAP_TRANSLATE).unwrap_or(false),
        supports_language_detect: meta.get_bool(KEY_CAP_LANG_DETECT).unwrap_or(false),
    })
}

/// Derive a collision-safe custom model ID from the probe's display name (or
/// a fallback file stem) and the file's SHA-256:
/// `custom-<slug>-<first 8 hex chars of sha256>`. The slug is lowercased and
/// restricted to `[a-z0-9-]`, with runs of other characters collapsed to a
/// single `-` and leading/trailing `-` trimmed; an empty result falls back to
/// `model`.
pub fn custom_model_id(name_or_stem: &str, sha256: &str) -> String {
    let mut slug = String::new();
    let mut last_was_dash = false;
    for ch in name_or_stem.to_ascii_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            last_was_dash = false;
        } else if !last_was_dash && !slug.is_empty() {
            slug.push('-');
            last_was_dash = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        slug.push_str("model");
    }
    let short_hash = &sha256[..sha256.len().min(8)];
    format!("custom-{slug}-{short_hash}")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn push_str(out: &mut Vec<u8>, s: &str) {
        out.extend_from_slice(&(s.len() as u64).to_le_bytes());
        out.extend_from_slice(s.as_bytes());
    }

    /// Builds a minimal valid GGUF header with the given string/bool KV
    /// pairs, for tests only (mirrors `gguf_meta`'s own test builder).
    pub fn build_test_gguf(
        architecture: &str,
        name: Option<&str>,
        languages: &[&str],
        streaming: Option<bool>,
        translate: Option<bool>,
        lang_detect: Option<bool>,
    ) -> Vec<u8> {
        let mut kvs: Vec<(&str, GgufValue)> =
            vec![(KEY_ARCH, GgufValue::String(architecture.to_string()))];
        if let Some(name) = name {
            kvs.push((KEY_NAME, GgufValue::String(name.to_string())));
        }
        if !languages.is_empty() {
            kvs.push((
                KEY_LANGUAGES,
                GgufValue::Array(
                    languages
                        .iter()
                        .map(|l| GgufValue::String(l.to_string()))
                        .collect(),
                ),
            ));
        }
        if let Some(v) = streaming {
            kvs.push((KEY_CAP_STREAMING, GgufValue::Bool(v)));
        }
        if let Some(v) = translate {
            kvs.push((KEY_CAP_TRANSLATE, GgufValue::Bool(v)));
        }
        if let Some(v) = lang_detect {
            kvs.push((KEY_CAP_LANG_DETECT, GgufValue::Bool(v)));
        }

        let mut out = Vec::new();
        out.extend_from_slice(&GGUF_MAGIC.to_le_bytes());
        out.extend_from_slice(&3u32.to_le_bytes());
        out.extend_from_slice(&0u64.to_le_bytes());
        out.extend_from_slice(&(kvs.len() as u64).to_le_bytes());
        for (k, v) in kvs {
            push_str(&mut out, k);
            match v {
                GgufValue::Bool(b) => {
                    out.extend_from_slice(&T_BOOL.to_le_bytes());
                    out.push(b as u8);
                }
                GgufValue::String(s) => {
                    out.extend_from_slice(&T_STRING.to_le_bytes());
                    push_str(&mut out, &s);
                }
                GgufValue::Array(items) => {
                    out.extend_from_slice(&T_ARRAY.to_le_bytes());
                    out.extend_from_slice(&T_STRING.to_le_bytes());
                    out.extend_from_slice(&(items.len() as u64).to_le_bytes());
                    for item in items {
                        if let GgufValue::String(s) = item {
                            push_str(&mut out, &s);
                        }
                    }
                }
                _ => unreachable!("test builder only emits bool/string/string-array"),
            }
        }
        out
    }

    #[test]
    fn probes_a_known_architecture() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("gguf-probe-test-{}.gguf", uuid::Uuid::new_v4()));
        std::fs::write(
            &path,
            build_test_gguf(
                "parakeet",
                Some("Test Parakeet"),
                &["en"],
                Some(true),
                Some(false),
                Some(false),
            ),
        )
        .unwrap();
        let probe = probe_gguf_file(&path).unwrap();
        assert!(probe.is_supported());
        assert_eq!(probe.architecture, "parakeet");
        assert_eq!(probe.display_name.as_deref(), Some("Test Parakeet"));
        assert_eq!(probe.languages, vec!["en".to_string()]);
        assert!(probe.supports_streaming);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn unknown_architecture_is_not_supported() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("gguf-probe-test-{}.gguf", uuid::Uuid::new_v4()));
        std::fs::write(&path, build_test_gguf("llama", None, &[], None, None, None)).unwrap();
        let probe = probe_gguf_file(&path).unwrap();
        assert!(!probe.is_supported());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn non_gguf_file_is_an_error() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("gguf-probe-test-{}.gguf", uuid::Uuid::new_v4()));
        std::fs::write(&path, b"not a gguf at all").unwrap();
        assert!(probe_gguf_file(&path).is_err());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn custom_model_id_slugifies_name_and_appends_short_hash() {
        assert_eq!(
            custom_model_id("My Cool Model! v2", "abcdef0123456789"),
            "custom-my-cool-model-v2-abcdef01"
        );
        assert_eq!(
            custom_model_id("", "abcdef0123456789"),
            "custom-model-abcdef01"
        );
        assert_eq!(
            custom_model_id("___", "abcdef0123456789"),
            "custom-model-abcdef01"
        );
    }
}
