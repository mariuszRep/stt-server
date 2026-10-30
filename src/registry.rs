//! The model registry's vocabulary on this server's own rows.
//!
//! One standard list of models exists across the estate; its schema lives in `stt-sdk`
//! (`src/registry/`, `ModelEntry`). This server only *labels what it serves itself* in that
//! vocabulary: the speech models of its catalog and drop-ins, plus the two models compiled into
//! the executable (CAM++ speaker embeddings, Silero VAD). It owns no provider descriptors and
//! knows no other provider's models (see `AGENTS.md`).
//!
//! Two shapes come out of here:
//!
//! - **labels** added to the existing snake_case rows of `GET /models/manage` and `GET /v1/models`
//!   ([`label_speech_row`]), so a client reads `kind`, `provider`, `ref`, `locality`, `tasks` and
//!   `privacy` instead of deriving them; nothing existing changes;
//! - **registry entries** ([`document`], `GET /v1/registry`): the SDK's `ModelEntry` and
//!   `ModelState` shapes (camelCase, nested `local`, `editorial`), built from those same rows, so
//!   the two views cannot disagree.

use serde_json::{json, Value};

use crate::catalog::CatalogInfo;

/// The provider id of everything this server serves.
pub const PROVIDER: &str = "stt-server";
/// Version of the `ModelEntry` schema this server speaks (`MODEL_ENTRY_SCHEMA_VERSION` in the SDK).
pub const SCHEMA_VERSION: u32 = 1;

const SPEECH: &str = "speech";
const SPEECH_TASKS: [&str; 1] = ["speech-to-text"];

/// `kind/provider/id`, the one string that names a model.
fn model_ref(kind: &str, id: &str) -> String {
    format!("{kind}/{PROVIDER}/{id}")
}

/// Sets `kind`, `provider`, `ref`, `locality`, `tasks` and `privacy` on a row or entry.
fn label(row: &mut Value, kind: &str, tasks: &[&str], id: &str) {
    row["kind"] = json!(kind);
    row["provider"] = json!(PROVIDER);
    row["ref"] = json!(model_ref(kind, id));
    row["locality"] = json!("local");
    row["tasks"] = json!(tasks);
    row["privacy"] = json!({"sends": []});
}

/// Labels a speech model's row (a catalog model, a drop-in, a `/v1/models` entry) with its registry identity.
pub fn label_speech_row(row: &mut Value, id: &str) {
    label(row, SPEECH, &SPEECH_TASKS, id);
}

/// Copies the non-null `from` field of `row` to `to` on `entry`.
fn copy(entry: &mut Value, to: &str, row: &Value, from: &str) {
    if !row[from].is_null() {
        entry[to] = row[from].clone();
    }
}

/// The `local` part of an entry: what the model is made of on disk, from a `/models/manage` row.
fn local_of(row: &Value) -> Option<Value> {
    let mut local = json!({});
    if let Some(files) = row["files"].as_array() {
        local["files"] = files
            .iter()
            .map(|file| {
                json!({
                    "filename": file["filename"],
                    "quant": file["quant"],
                    "sizeBytes": file["size_bytes"],
                    "sha256": file["sha256"],
                })
            })
            .collect();
        // The size to show is the default quant's file (the first one if the default is unknown).
        let shown = files
            .iter()
            .find(|file| file["quant"] == row["default_quant"])
            .or(files.first());
        if let Some(shown) = shown {
            local["sizeBytes"] = shown["size_bytes"].clone();
        }
    }
    for (to, from) in [
        ("revision", "revision"),
        ("upstreamId", "upstream_id"),
        ("defaultQuant", "default_quant"),
        ("source", "source"),
        ("custom", "custom"),
    ] {
        copy(&mut local, to, row, from);
    }
    (!local.as_object()?.is_empty()).then_some(local)
}

/// The `editorial` part of an entry: the catalog's own curation. A drop-in has none.
fn editorial_of(row: &Value) -> Option<Value> {
    let recommended = row["recommended"].as_bool()?;
    let mut editorial = json!({"recommended": recommended, "source": "handy"});
    for (to, from) in [
        ("recommendedRank", "recommended_rank"),
        ("speedScore", "speed_score"),
        ("accuracyScore", "accuracy_score"),
    ] {
        copy(&mut editorial, to, row, from);
    }
    Some(editorial)
}

/// The registry's capability matrix: every control has a `status`. The row's matrix can also
/// carry plain facts that are not controls (a loaded model's `arch`), which are left out, and
/// `response_formats`, a plain map (`{"json": "supported", ...}`), which becomes `supported` when
/// any format is and `unknown` otherwise, with the map kept in `extra.values` (as stt-sdk's
/// `normalizeMatrix` does).
fn capabilities_of(matrix: &Value) -> Value {
    let mut normalized = json!({});
    for (name, control) in matrix.as_object().into_iter().flatten() {
        if !control.is_object() {
            continue;
        }
        normalized[name] = if control["status"].is_string() {
            control.clone()
        } else {
            let any_supported = control
                .as_object()
                .is_some_and(|map| map.values().any(|value| value == "supported"));
            json!({
                "status": if any_supported { "supported" } else { "unknown" },
                "extra": {"values": control},
            })
        };
    }
    normalized
}

/// One `/models/manage` row (a catalog model or a drop-in, already labelled) as a `ModelEntry`.
fn entry_of(row: &Value) -> Value {
    let mut entry = json!({
        "ref": row["ref"],
        "kind": row["kind"],
        "provider": row["provider"],
        "id": row["id"],
        "name": row["name"],
        "tasks": row["tasks"],
        "locality": row["locality"],
        "access": "free",
        "privacy": row["privacy"],
        "capabilities": capabilities_of(&row["effective_capabilities"]),
    });
    for (to, from) in [
        ("description", "description"),
        ("family", "family"),
        ("architecture", "architecture"),
        ("parameters", "parameters"),
        ("baseModel", "base_model"),
        ("license", "license"),
        ("languages", "languages"),
    ] {
        copy(&mut entry, to, row, from);
    }
    if let Some(local) = local_of(row) {
        entry["local"] = local;
    }
    if let Some(editorial) = editorial_of(row) {
        entry["editorial"] = editorial;
    }
    entry
}

/// What a `/models/manage` row says about the model now: the `ModelState` of its entry.
fn state_of(row: &Value, loaded_id: Option<&str>) -> Value {
    let mut state = json!({
        "ref": row["ref"],
        "installed": row["downloaded"],
        "loaded": loaded_id.is_some() && loaded_id == row["id"].as_str(),
        "default": row["default"],
        "installedQuant": row["installed_quant"],
    });
    copy(&mut state, "needsVerification", row, "needs_verification");
    state
}

/// A model compiled into the executable: always installed, no download, not in the catalog.
struct Builtin {
    kind: &'static str,
    task: &'static str,
    id: &'static str,
    name: &'static str,
    description: &'static str,
    family: &'static str,
    architecture: &'static str,
    license: &'static str,
    upstream_id: &'static str,
    file: &'static str,
    quant: &'static str,
    bytes: usize,
    sha256: &'static str,
    capabilities: fn() -> Value,
}

impl Builtin {
    fn entry(&self) -> Value {
        let mut entry = json!({
            "id": self.id,
            "name": self.name,
            "description": self.description,
            "access": "free",
            "capabilities": (self.capabilities)(),
            "family": self.family,
            "architecture": self.architecture,
            "license": self.license,
            "builtin": true,
            "local": {
                "files": [{
                    "filename": self.file,
                    "quant": self.quant,
                    "sizeBytes": self.bytes,
                    "sha256": self.sha256,
                }],
                "upstreamId": self.upstream_id,
                "defaultQuant": self.quant,
                "sizeBytes": self.bytes,
            },
        });
        label(&mut entry, self.kind, &[self.task], self.id);
        entry
    }

    /// Always installed; the only model of its kind the server has, so also its default.
    fn state(&self) -> Value {
        json!({
            "ref": model_ref(self.kind, self.id),
            "installed": true,
            "default": true,
        })
    }
}

/// A capability the compiled-in code provides outright, with the numbers a client sizes things by.
fn builtin_control(extra: Value) -> Value {
    let mut control = json!({"status": "supported", "evidence": "builtin"});
    for (key, value) in extra.as_object().into_iter().flatten() {
        control[key] = value.clone();
    }
    control
}

fn speaker_capabilities() -> Value {
    json!({
        "speaker_embedding": builtin_control(json!({
            "dim": crate::speaker::DIM,
            "sample_rate": crate::speaker::SAMPLE_RATE,
        })),
        "diarization": builtin_control(json!({
            "max_speakers": crate::speaker::diarize::MAX_SPEAKERS,
        })),
    })
}

fn vad_capabilities() -> Value {
    json!({})
}

fn builtins() -> [Builtin; 2] {
    [
        Builtin {
            kind: "speaker",
            task: "speaker-embedding",
            id: crate::speaker::MODEL_ID,
            name: "CAM++ speaker embeddings (VoxCeleb)",
            description:
                "Voice fingerprints and diarization of finished audio, compiled into the server",
            family: "campplus",
            architecture: "campplus",
            license: "apache-2.0",
            upstream_id: "iic/speech_campplus_sv_en_voxceleb_16k",
            file: crate::speaker::WEIGHTS_FILE,
            quant: "F16",
            bytes: crate::speaker::WEIGHTS_BYTES,
            sha256: crate::speaker::WEIGHTS_SHA256,
            capabilities: speaker_capabilities,
        },
        Builtin {
            kind: "vad",
            task: "voice-activity-detection",
            id: crate::vad::MODEL_ID,
            name: "Silero VAD v5",
            description: "Offline speech detection on finished audio, compiled into the server",
            family: "silero",
            architecture: "silero",
            license: "mit",
            upstream_id: "snakers4/silero-vad",
            file: crate::vad::WEIGHTS_FILE,
            quant: "F32",
            bytes: crate::vad::WEIGHTS_BYTES,
            sha256: crate::vad::WEIGHTS_SHA256,
            capabilities: vad_capabilities,
        },
    ]
}

/// The `GET /v1/registry` body: every model this server serves as a `ModelEntry` with its
/// `ModelState`, from the `/models/manage` rows (speech) and the compiled-in models.
pub fn document(rows: &[Value], loaded_id: Option<&str>, catalog: &CatalogInfo) -> Value {
    let builtins = builtins();
    let entries: Vec<Value> = rows
        .iter()
        .map(entry_of)
        .chain(builtins.iter().map(Builtin::entry))
        .collect();
    let states: Vec<Value> = rows
        .iter()
        .map(|row| state_of(row, loaded_id))
        .chain(builtins.iter().map(Builtin::state))
        .collect();
    let mut body = json!({
        "object": "list",
        "schema_version": SCHEMA_VERSION,
        "data": entries,
        "states": states,
    });
    catalog.stamp(&mut body);
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    fn info() -> CatalogInfo {
        CatalogInfo {
            catalog_version: 2,
            generated_at: "2026-08-17T11:27:14+00:00".to_owned(),
        }
    }

    /// The `model-entry.v1.json` rules a builtin entry must obey (stt-sdk `src/registry/schema`).
    fn assert_entry_shape(entry: &Value) {
        for field in [
            "ref",
            "kind",
            "provider",
            "id",
            "name",
            "tasks",
            "locality",
            "access",
            "privacy",
            "capabilities",
        ] {
            assert!(!entry[field].is_null(), "{} lacks {field}", entry["id"]);
        }
        let (kind, provider, id) = (
            entry["kind"].as_str().unwrap(),
            entry["provider"].as_str().unwrap(),
            entry["id"].as_str().unwrap(),
        );
        assert_eq!(entry["ref"], format!("{kind}/{provider}/{id}"));
        assert!(!entry["tasks"].as_array().unwrap().is_empty());
        assert_eq!(entry["locality"], "local");
        assert_eq!(entry["privacy"]["sends"], json!([]));
        assert!(entry["capabilities"].is_object());
    }

    #[test]
    fn labels_name_a_speech_row_by_its_reference() {
        let mut row = json!({"id": "custom-whisper-ab12cd34"});
        label_speech_row(&mut row, "custom-whisper-ab12cd34");
        assert_eq!(
            row,
            json!({
                "id": "custom-whisper-ab12cd34",
                "kind": "speech",
                "provider": "stt-server",
                "ref": "speech/stt-server/custom-whisper-ab12cd34",
                "locality": "local",
                "tasks": ["speech-to-text"],
                "privacy": {"sends": []},
            })
        );
    }

    #[test]
    fn builtin_entries_are_the_two_compiled_in_models() {
        let document = document(&[], None, &info());
        let entries = document["data"].as_array().unwrap();
        let refs: Vec<_> = entries.iter().map(|e| e["ref"].as_str().unwrap()).collect();
        assert_eq!(
            refs,
            [
                "speaker/stt-server/campplus-voxceleb-v1",
                "vad/stt-server/silero-v5"
            ]
        );
        for entry in entries {
            assert_entry_shape(entry);
            assert_eq!(entry["builtin"], true);
            assert_eq!(entry["access"], "free");
            let file = &entry["local"]["files"][0];
            assert_eq!(entry["local"]["sizeBytes"], file["sizeBytes"]);
            assert_eq!(file["sha256"].as_str().unwrap().len(), 64);
        }
        assert_eq!(entries[0]["tasks"], json!(["speaker-embedding"]));
        assert_eq!(entries[1]["tasks"], json!(["voice-activity-detection"]));
        assert_eq!(document["states"].as_array().unwrap().len(), 2);
    }

    /// The reported file is the blob compiled into this executable, not a copy of its description.
    #[test]
    fn builtin_files_are_the_embedded_weights() {
        let weights: [(&str, &[u8]); 2] = [
            (
                crate::speaker::WEIGHTS_SHA256,
                include_bytes!("../assets/campplus_vox_16k.f16"),
            ),
            (
                crate::vad::WEIGHTS_SHA256,
                include_bytes!("../assets/silero_v5_16k.f32"),
            ),
        ];
        for (builtin, (sha256, bytes)) in builtins().iter().zip(weights) {
            assert_eq!(builtin.sha256, sha256);
            assert_eq!(builtin.bytes, bytes.len());
            assert_eq!(format!("{:x}", Sha256::digest(bytes)), builtin.sha256);
        }
    }

    #[test]
    fn speaker_capabilities_carry_the_numbers_a_client_sizes_things_by() {
        let capabilities = speaker_capabilities();
        assert_eq!(capabilities["speaker_embedding"]["status"], "supported");
        assert_eq!(capabilities["speaker_embedding"]["evidence"], "builtin");
        assert_eq!(capabilities["speaker_embedding"]["dim"], 512);
        assert_eq!(capabilities["speaker_embedding"]["sample_rate"], 16_000);
        assert_eq!(capabilities["diarization"]["max_speakers"], 26);
    }

    #[test]
    fn an_entry_is_built_from_the_row_it_describes() {
        let row = json!({
            "id": "m", "name": "M", "ref": "speech/stt-server/m", "kind": "speech",
            "provider": "stt-server", "locality": "local", "tasks": ["speech-to-text"],
            "privacy": {"sends": []}, "effective_capabilities": {"prompt": {"status": "unknown"}},
            "base_model": "org/m", "description": null, "languages": ["en"],
            "default_quant": "Q8_0", "upstream_id": "org/m-gguf", "revision": "abc",
            "files": [
                {"filename": "m-Q4.gguf", "quant": "Q4_K_M", "size_bytes": 1, "sha256": "a"},
                {"filename": "m-Q8.gguf", "quant": "Q8_0", "size_bytes": 2, "sha256": "b"},
            ],
            "recommended": false, "recommended_rank": 9, "speed_score": 70, "accuracy_score": null,
            "downloaded": true, "installed_quant": "Q4_K_M", "needs_verification": true,
            "default": false,
        });
        let entry = entry_of(&row);
        assert_eq!(entry["baseModel"], "org/m");
        assert!(
            entry.get("description").is_none(),
            "a null field is left out"
        );
        assert_eq!(entry["local"]["sizeBytes"], 2);
        assert_eq!(entry["local"]["upstreamId"], "org/m-gguf");
        assert_eq!(entry["local"]["files"][1]["sizeBytes"], 2);
        assert_eq!(
            entry["editorial"],
            json!({"recommended": false, "recommendedRank": 9, "speedScore": 70, "source": "handy"})
        );
        assert_eq!(
            state_of(&row, Some("m")),
            json!({
                "ref": "speech/stt-server/m", "installed": true, "loaded": true, "default": false,
                "installedQuant": "Q4_K_M", "needsVerification": true,
            })
        );
        assert_eq!(state_of(&row, Some("other"))["loaded"], false);
    }

    #[test]
    fn capabilities_give_every_control_a_status() {
        let matrix = capabilities_of(&json!({
            "prompt": {"status": "unsupported", "reason": "model_lacks"},
            "response_formats": {"json": "supported", "srt": "unknown"},
            "subtitles_only": {"srt": "unknown", "vtt": "unsupported"},
            "arch": "whisper",
            "whisper_ext_accepted": true,
        }));
        assert_eq!(
            matrix,
            json!({
                "prompt": {"status": "unsupported", "reason": "model_lacks"},
                "response_formats": {
                    "status": "supported",
                    "extra": {"values": {"json": "supported", "srt": "unknown"}},
                },
                "subtitles_only": {
                    "status": "unknown",
                    "extra": {"values": {"srt": "unknown", "vtt": "unsupported"}},
                },
            })
        );
    }

    #[test]
    fn the_document_reports_the_catalog_it_was_built_from() {
        let document = document(&[], None, &info());
        assert_eq!(document["schema_version"], 1);
        assert_eq!(document["catalog_version"], 2);
        assert_eq!(document["generated_at"], "2026-08-17T11:27:14+00:00");
    }
}
