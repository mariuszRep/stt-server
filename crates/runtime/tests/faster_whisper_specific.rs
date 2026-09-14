//! Engine-specific behaviour for faster-whisper: capabilities the general,
//! engine-agnostic conformance suite (`protocol_conformance.rs` /
//! `stt_runtime::conformance`) deliberately never exercises because no other
//! engine has them. Each test here targets the `"faster-whisper"` catalog
//! entry by name -- that's what makes this file engine-specific rather than
//! general, and it's why none of it lives in `conformance.rs`.
//!
//! Same loud-skip discipline as the general suite: a test skips (via a named
//! `eprintln!`, never silently) whenever the runtime isn't locally installed,
//! its model isn't already cached, or (for the GPU test only) no NVIDIA GPU
//! is present -- never triggers a fresh download or requires hardware this
//! environment doesn't have.

mod support;

use stt_runtime::conformance::FIXTURE_WAV;
use stt_runtime::{catalog, hardware, ProviderId, RuntimeManager, StartOptions};

const PROVIDER_ID: &str = "faster-whisper";
/// A second curated model distinct from the catalog default, used only to
/// prove the hot-swap endpoint actually swaps -- not a claim about which
/// model is "better."
const SECONDARY_MODEL: &str = "Systran/faster-whisper-tiny.en";

/// Skips (returning `None`) unless faster-whisper is locally installed *and*
/// `model_id` is already cached on disk -- mirrors
/// `conformance::check_provider`'s own skip conditions so this file never
/// triggers a download as a side effect of `cargo test`.
async fn installed_manager_with_cached_model(
    model_id: &str,
) -> Option<(RuntimeManager, ProviderId)> {
    support::point_at_locally_built_runtimes();

    let manager = RuntimeManager::new(None);
    manager.register_local_installs().await;
    let id = ProviderId::new(PROVIDER_ID).expect("static id is valid");

    if !manager.is_installed(&id).await {
        eprintln!("SKIP {PROVIDER_ID}: runtime not locally installed in this environment");
        return None;
    }

    match manager.verify_model(&id, model_id) {
        Ok(Some(_)) => {}
        _ => {
            eprintln!(
                "SKIP {PROVIDER_ID}: model '{model_id}' not already cached in this environment"
            );
            return None;
        }
    }

    Some((manager, id))
}

fn fixture_form() -> reqwest::multipart::Form {
    reqwest::multipart::Form::new().part(
        "file",
        reqwest::multipart::Part::bytes(FIXTURE_WAV.to_vec()).file_name("sample.wav"),
    )
}

/// Starting with a non-default CPU compute type is honored end to end: the
/// running instance's own `/v1/config` reports back the compute type this
/// test actually requested, not the catalog default.
#[tokio::test]
async fn non_default_cpu_compute_type_is_honored() {
    let entry = catalog::find_provider(&ProviderId::new(PROVIDER_ID).unwrap()).unwrap();
    let Some((manager, id)) = installed_manager_with_cached_model(entry.default_model).await else {
        return;
    };

    manager
        .select_model(&id, entry.default_model)
        .await
        .unwrap();

    let options = StartOptions {
        device: Some("cpu".to_string()),
        compute_type: Some("float32".to_string()),
        ..StartOptions::default()
    };
    let descriptor = manager
        .start(&id, &options)
        .await
        .expect("faster-whisper should start with an explicit float32 compute type");

    let client = reqwest::Client::new();
    let bearer = descriptor
        .auth
        .as_ref()
        .map(|a| a.value.clone())
        .unwrap_or_default();
    let config: serde_json::Value = client
        .get(format!("{}/v1/config", descriptor.base_url))
        .bearer_auth(&bearer)
        .send()
        .await
        .expect("GET /v1/config should succeed")
        .json()
        .await
        .expect("config response should be valid JSON");

    assert_eq!(
        config.get("compute_type").and_then(|v| v.as_str()),
        Some("float32"),
        "expected the runtime to report back the requested compute type, got: {config}"
    );

    let _ = manager.stop(&id).await;
}

/// GPU device selection actually starts a healthy instance on hardware that
/// has an NVIDIA GPU. Skips loudly (not a failure) on this environment's own
/// hardware report when there is none, since this suite must never require
/// hardware it doesn't have.
#[tokio::test]
async fn gpu_device_variant_starts_and_serves_health() {
    if !hardware::detect().has_nvidia_gpu {
        eprintln!("SKIP {PROVIDER_ID} GPU variant: no NVIDIA GPU detected on this machine");
        return;
    }

    let entry = catalog::find_provider(&ProviderId::new(PROVIDER_ID).unwrap()).unwrap();
    let Some((manager, id)) = installed_manager_with_cached_model(entry.default_model).await else {
        return;
    };

    manager
        .select_model(&id, entry.default_model)
        .await
        .unwrap();

    let options = StartOptions {
        device: Some("cuda".to_string()),
        compute_type: Some("float16".to_string()),
        ..StartOptions::default()
    };
    let descriptor = manager
        .start(&id, &options)
        .await
        .expect("faster-whisper should start on the detected NVIDIA GPU");

    let health_url = format!("{}/health", descriptor.base_url);
    let response = reqwest::get(&health_url)
        .await
        .expect("health endpoint should be reachable");
    assert!(response.status().is_success());

    let _ = manager.stop(&id).await;
}

/// `/v1/admin/model` hot-swap: exists only on faster-whisper's own runtime
/// (`runtimes/faster-whisper/app/main.py`'s `admin_switch_model`) -- sherpa-onnx
/// has no such route, so this test only ever targets this one catalog entry
/// by name, deliberately, rather than being folded into the general suite.
#[tokio::test]
async fn admin_model_hot_swap_switches_the_running_instance() {
    let entry = catalog::find_provider(&ProviderId::new(PROVIDER_ID).unwrap()).unwrap();
    let Some((manager, id)) = installed_manager_with_cached_model(entry.default_model).await else {
        return;
    };
    if manager
        .verify_model(&id, SECONDARY_MODEL)
        .ok()
        .flatten()
        .is_none()
    {
        eprintln!(
            "SKIP {PROVIDER_ID} hot-swap: secondary model '{SECONDARY_MODEL}' not already cached in this environment"
        );
        return;
    }

    manager
        .select_model(&id, entry.default_model)
        .await
        .unwrap();
    let descriptor = manager
        .start(&id, &StartOptions::default())
        .await
        .expect("faster-whisper should start with its default model");

    manager
        .switch_model(&id, SECONDARY_MODEL)
        .await
        .expect("hot-swap to the secondary model should succeed on a running instance");

    let client = reqwest::Client::new();
    let bearer = descriptor
        .auth
        .as_ref()
        .map(|a| a.value.clone())
        .unwrap_or_default();
    let config: serde_json::Value = client
        .get(format!("{}/v1/config", descriptor.base_url))
        .bearer_auth(&bearer)
        .send()
        .await
        .expect("GET /v1/config should succeed")
        .json()
        .await
        .expect("config response should be valid JSON");
    assert_eq!(
        config.get("model").and_then(|v| v.as_str()),
        Some(SECONDARY_MODEL),
        "expected the live instance to report the hot-swapped model, got: {config}"
    );

    // The swapped-in model should also actually serve transcriptions, not
    // just report its own name back.
    let resp = client
        .post(format!("{}/v1/audio/transcriptions", descriptor.base_url))
        .bearer_auth(&bearer)
        .multipart(fixture_form())
        .send()
        .await
        .expect("transcription request should succeed after the hot-swap");
    assert!(resp.status().is_success());

    let _ = manager.stop(&id).await;
}
