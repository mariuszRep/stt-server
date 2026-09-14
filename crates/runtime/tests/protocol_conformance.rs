//! General, engine-agnostic conformance suite for the Local Provider
//! Protocol (see `voice-typer/CONVENTIONS.md`). Parameterized over
//! `catalog::CATALOG` / `providers::registry()` via the shared
//! `stt_runtime::conformance` module -- also used by `stt verify`, so the
//! test and the CLI can never drift into checking different things. Adding
//! a new engine (a catalog entry + a `ProviderEngine` impl) means it is
//! automatically covered here, with no new test code.
//!
//! Real hardware only, and deliberately non-destructive: each provider is
//! skipped (loudly, never silently -- see `conformance::ConformanceOutcome`)
//! unless its runtime binary is already locally installable *and* its
//! catalog default model is already verified-present on disk. This suite
//! never triggers a fresh multi-hundred-megabyte download as a side effect
//! of `cargo test`.
//!
//! Engine-specific behaviour (faster-whisper's GPU variant/compute types,
//! its `/v1/admin/model` hot-swap; sherpa-onnx's request batching) lives in
//! `faster_whisper_specific.rs` / `sherpa_onnx_specific.rs`, not here.

use stt_runtime::conformance::{check_all, ConformanceOutcome};

mod support;

#[tokio::test]
async fn every_registered_provider_conforms_to_the_local_provider_protocol() {
    support::point_at_locally_built_runtimes();

    for (provider_id, outcome) in check_all().await {
        match outcome {
            ConformanceOutcome::Skipped { reason } => {
                eprintln!("SKIP {provider_id}: {reason}");
            }
            ConformanceOutcome::Ran { checks } => {
                for check in &checks {
                    let marker = if check.passed { "ok" } else { "FAIL" };
                    println!(
                        "{provider_id} :: {} [{marker}] {}",
                        check.name, check.detail
                    );
                }
                let hard_failures: Vec<_> = checks.iter().filter(|c| !c.passed).collect();
                if !hard_failures.is_empty() {
                    panic!("{provider_id}: conformance failures: {hard_failures:?}");
                }
            }
        }
    }
}
