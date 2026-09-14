//! Engine-specific behaviour for sherpa-onnx: capabilities the general,
//! engine-agnostic conformance suite deliberately never exercises because no
//! other engine has them. sherpa-onnx's `sherpad` batches concurrently
//! arriving jobs into one `decode_multiple_streams` call per worker tick
//! (`runtimes/sherpa-onnx/crates/sherpad/src/recognizer.rs::spawn_worker`) --
//! the real risk that code path introduces and a sequential test can never
//! catch is silently handing one caller's transcript back to a different
//! caller. This test proves the demux is correct under real concurrency.
//!
//! Same loud-skip discipline as the general suite: skips (via a named
//! `eprintln!`, never silently) whenever sherpa-onnx isn't locally installed
//! or its default model isn't already cached on disk.

mod support;

use stt_runtime::conformance::FIXTURE_WAV;
use stt_runtime::{catalog, ProviderId, RuntimeManager, StartOptions};

const PROVIDER_ID: &str = "sherpa-onnx";
/// Comfortably inside `recognizer.rs`'s `MAX_BATCH_SIZE` (8), so a real
/// implementation is expected to actually batch these into one decode call
/// rather than merely queueing them one at a time.
const CONCURRENT_REQUESTS: usize = 6;

fn fixture_form() -> reqwest::multipart::Form {
    reqwest::multipart::Form::new().part(
        "file",
        reqwest::multipart::Part::bytes(FIXTURE_WAV.to_vec()).file_name("sample.wav"),
    )
}

#[tokio::test]
async fn concurrent_requests_are_each_answered_with_their_own_correct_transcript() {
    support::point_at_locally_built_runtimes();

    let manager = RuntimeManager::new(None);
    manager.register_local_installs().await;
    let id = ProviderId::new(PROVIDER_ID).expect("static id is valid");

    if !manager.is_installed(&id).await {
        eprintln!("SKIP {PROVIDER_ID}: runtime not locally installed in this environment");
        return;
    }

    let entry = catalog::find_provider(&id).unwrap();
    match manager.verify_model(&id, entry.default_model) {
        Ok(Some(_)) => {}
        _ => {
            eprintln!(
                "SKIP {PROVIDER_ID}: default model '{}' not already cached in this environment",
                entry.default_model
            );
            return;
        }
    }

    manager
        .select_model(&id, entry.default_model)
        .await
        .unwrap();
    let descriptor = manager
        .start(&id, &StartOptions::default())
        .await
        .expect("sherpa-onnx should start with its default model");

    let client = reqwest::Client::new();
    let bearer = descriptor
        .auth
        .as_ref()
        .map(|a| a.value.clone())
        .unwrap_or_default();

    // Fire every request concurrently (not sequentially awaited) so they
    // actually land inside the worker's batching window together, rather
    // than trivially resolving one at a time.
    let requests = (0..CONCURRENT_REQUESTS).map(|_| {
        let client = client.clone();
        let base_url = descriptor.base_url.clone();
        let bearer = bearer.clone();
        tokio::spawn(async move {
            client
                .post(format!("{base_url}/v1/audio/transcriptions"))
                .bearer_auth(&bearer)
                .multipart(fixture_form())
                .send()
                .await
        })
    });

    let responses = futures_util::future::join_all(requests).await;

    let mut texts = Vec::with_capacity(CONCURRENT_REQUESTS);
    for joined in responses {
        let resp = joined
            .expect("request task should not panic")
            .expect("request should reach the running instance");
        assert!(
            resp.status().is_success(),
            "expected every concurrent request to succeed, got {}",
            resp.status()
        );
        let body: serde_json::Value = resp.json().await.expect("response should be valid JSON");
        let text = body
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        assert!(
            !text.trim().is_empty(),
            "expected a non-empty transcript for the known-speech fixture, got: {body}"
        );
        texts.push(text);
    }

    // Every request submitted the identical fixture clip, so a correct
    // demux must hand back the identical transcript to every caller -- any
    // divergence means responses got crossed between concurrent jobs.
    let first = &texts[0];
    assert!(
        texts.iter().all(|t| t == first),
        "expected every concurrent request (same input clip) to get the identical transcript back, got: {texts:?}"
    );

    let _ = manager.stop(&id).await;
}
