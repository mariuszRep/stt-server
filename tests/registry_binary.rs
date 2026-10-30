//! The model-registry labels against the real compiled binary: no model is installed, yet the
//! catalog rows of `/models/manage`, the registry listing and `/v1/local/system` carry the
//! registry vocabulary, and the fields that existed before are exactly what they were.

mod common;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// SHA-256 of the 69 `/models/manage` rows (`data`, keys sorted, compact) as the last server
/// before the registry fields (0.3.6) served them with nothing installed.
const ROWS_BEFORE_REGISTRY_FIELDS_SHA256: &str =
    "c8a5373c63f826d8372361204eae598f7c581ecb2bc4de5a00ea983e9860fdcf";

/// What the registry added to a catalog row.
const ADDED_TO_ROWS: [&str; 11] = [
    "kind",
    "provider",
    "ref",
    "locality",
    "tasks",
    "privacy",
    "description",
    "parameters",
    "base_model",
    "language_count",
    "recommended",
];

async fn get(server: &common::Server, path: &str, token: Option<&str>) -> (u16, Value) {
    let mut request = reqwest::Client::new().get(format!("{}{path}", server.base));
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let response = request.send().await.unwrap();
    (response.status().as_u16(), response.json().await.unwrap())
}

fn entry<'a>(list: &'a Value, id: &str) -> &'a Value {
    list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["id"] == id)
        .unwrap_or_else(|| panic!("no entry {id}"))
}

#[test]
fn the_binary_serves_the_registry_vocabulary_and_keeps_every_old_field() {
    let server = common::Server::start("stt-registry-test", 54425);
    let token = server.token.as_str();
    let (manage, registry, system) = tokio::runtime::Runtime::new().unwrap().block_on(async {
        for path in ["/v1/registry", "/models/manage"] {
            for bad in [None, Some("wrong")] {
                assert_eq!(get(&server, path, bad).await.0, 401, "{path} {bad:?}");
            }
        }
        let mut bodies = Vec::new();
        for path in ["/models/manage", "/v1/registry", "/v1/local/system"] {
            let (status, body) = get(&server, path, Some(token)).await;
            assert_eq!(status, 200, "{path}");
            bodies.push(body);
        }
        let system = bodies.pop().unwrap();
        let registry = bodies.pop().unwrap();
        (bodies.pop().unwrap(), registry, system)
    });

    // The catalog's own version, read from the embedded file, on every route that reports it.
    for (name, body) in [("manage", &manage), ("registry", &registry)] {
        assert_eq!(body["catalog_version"], 2, "{name}");
        assert_eq!(body["generated_at"], "2026-08-17T11:27:14+00:00", "{name}");
    }
    assert_eq!(
        system["catalog"],
        json!({"catalog_version": 2, "generated_at": "2026-08-17T11:27:14+00:00"})
    );

    // Every catalog row is labelled, and with the labels removed it is what 0.3.6 served.
    let rows = manage["data"].as_array().unwrap();
    assert_eq!(rows.len(), 69);
    let mut old_rows = rows.clone();
    for row in &mut old_rows {
        let id = row["id"].as_str().unwrap().to_owned();
        assert_eq!(row["kind"], "speech");
        assert_eq!(row["provider"], "stt-server");
        assert_eq!(row["ref"], format!("speech/stt-server/{id}"));
        assert_eq!(row["locality"], "local");
        assert_eq!(row["tasks"], json!(["speech-to-text"]));
        assert_eq!(row["privacy"], json!({"sends": []}));
        for field in ["description", "parameters", "base_model", "language_count"] {
            assert!(!row[field].is_null(), "{id} has no {field}");
        }
        let object = row.as_object_mut().unwrap();
        for field in ADDED_TO_ROWS {
            object.remove(field);
        }
    }
    let old = serde_json::to_string(&old_rows).unwrap();
    assert_eq!(
        format!("{:x}", Sha256::digest(old.as_bytes())),
        ROWS_BEFORE_REGISTRY_FIELDS_SHA256
    );
    let parakeet = rows
        .iter()
        .find(|row| row["id"] == "parakeet-unified-en-0.6b")
        .unwrap();
    assert_eq!(
        parakeet["description"],
        "Fast, accurate live English transcription"
    );
    assert_eq!(parakeet["parameters"], "0.6B");
    assert_eq!(parakeet["base_model"], "nvidia/parakeet-unified-en-0.6b");
    assert_eq!(parakeet["language_count"], 1);
    assert_eq!(parakeet["recommended"], true);

    // The registry lists the 69 speech models and the two compiled-in ones, entry for state.
    assert_eq!(registry["object"], "list");
    assert_eq!(registry["schema_version"], 1);
    let entries = registry["data"].as_array().unwrap();
    let states = registry["states"].as_array().unwrap();
    assert_eq!(entries.len(), 71);
    assert_eq!(states.len(), 71);
    for (entry, state) in entries.iter().zip(states) {
        let (kind, provider, id) = (&entry["kind"], &entry["provider"], &entry["id"]);
        assert_eq!(
            entry["ref"],
            format!(
                "{}/{}/{}",
                kind.as_str().unwrap(),
                provider.as_str().unwrap(),
                id.as_str().unwrap()
            )
        );
        assert_eq!(state["ref"], entry["ref"]);
        assert_eq!(provider, "stt-server");
        assert_eq!(entry["locality"], "local");
        assert_eq!(entry["access"], "free");
        assert_eq!(entry["privacy"], json!({"sends": []}));
        for (control, capability) in entry["capabilities"].as_object().unwrap() {
            assert!(
                capability["status"].is_string(),
                "{id}: {control} has no status"
            );
        }
        assert!(!entry["name"].as_str().unwrap().is_empty());
        assert_eq!(state["installed"], entry["builtin"] == true);
    }

    let parakeet = entry(&registry, "parakeet-unified-en-0.6b");
    assert_eq!(
        parakeet["ref"],
        "speech/stt-server/parakeet-unified-en-0.6b"
    );
    assert_eq!(parakeet["tasks"], json!(["speech-to-text"]));
    assert_eq!(parakeet["baseModel"], "nvidia/parakeet-unified-en-0.6b");
    assert_eq!(parakeet["parameters"], "0.6B");
    assert_eq!(parakeet["languages"], json!(["en"]));
    assert_eq!(parakeet["local"]["defaultQuant"], "Q8_0");
    assert_eq!(parakeet["local"]["sizeBytes"], 731_357_568);
    assert_eq!(parakeet["local"]["files"].as_array().unwrap().len(), 6);
    assert_eq!(
        parakeet["editorial"],
        json!({
            "recommended": true, "recommendedRank": 1, "source": "handy",
            "speedScore": 79, "accuracyScore": 90,
        })
    );
    assert!(parakeet.get("builtin").is_none());

    // A model the catalog ranks without recommending it is not "recommended".
    let ranked_only = entry(&registry, "parakeet-tdt-0.6b-v3");
    assert_eq!(ranked_only["editorial"]["recommended"], false);
    assert_eq!(ranked_only["editorial"]["recommendedRank"], 7);

    let speaker = entry(&registry, "campplus-voxceleb-v1");
    assert_eq!(speaker["ref"], "speaker/stt-server/campplus-voxceleb-v1");
    assert_eq!(speaker["kind"], "speaker");
    assert_eq!(speaker["tasks"], json!(["speaker-embedding"]));
    assert_eq!(speaker["builtin"], true);
    assert_eq!(
        speaker["local"]["files"][0]["sha256"],
        "11122726706b2cfe164966cdafb93cc7727176e1db9d45952ef999d90b53e65b"
    );
    assert_eq!(
        speaker["capabilities"]["speaker_embedding"]["status"],
        "supported"
    );
    assert_eq!(speaker["capabilities"]["speaker_embedding"]["dim"], 512);

    let vad = entry(&registry, "silero-v5");
    assert_eq!(vad["ref"], "vad/stt-server/silero-v5");
    assert_eq!(vad["kind"], "vad");
    assert_eq!(vad["tasks"], json!(["voice-activity-detection"]));
    assert_eq!(vad["builtin"], true);
    assert_eq!(
        vad["local"]["files"][0]["sha256"],
        "b8df2e6e32753b7aa47ab59571b0d9d0b490a223f8dc9118bb388efeaec6f8e3"
    );

    let state = |reference: &str| {
        states
            .iter()
            .find(|state| state["ref"] == reference)
            .unwrap()
    };
    assert_eq!(
        state("vad/stt-server/silero-v5"),
        &json!({"ref": "vad/stt-server/silero-v5", "installed": true, "default": true})
    );
    assert_eq!(
        state("speech/stt-server/parakeet-unified-en-0.6b"),
        &json!({
            "ref": "speech/stt-server/parakeet-unified-en-0.6b",
            "installed": false, "loaded": false, "default": false, "installedQuant": null,
        })
    );
}
