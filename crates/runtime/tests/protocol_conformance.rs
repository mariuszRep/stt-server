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
//! its `/v1/admin/model` hot-swap; sherpa-onnx's request batching) belongs
//! in a separate engine-specific test file, not here.

use std::path::PathBuf;

use stt_runtime::conformance::{check_all, ConformanceOutcome};

/// `cargo test`'s working directory is not reliable for the runtime
/// adapters' own cwd-relative dev-discovery candidates (they're written for
/// `cargo run`'s cwd, not `cargo test`'s) -- mirrors
/// `faster_whisper_integration.rs::workspace_root`'s existing fix for the
/// same issue: compute the real workspace root via `CARGO_MANIFEST_DIR`
/// instead of trusting the process cwd.
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

/// Points each engine's dev-binary env-var override at its real location so
/// `register_local_installs()` can find what's already built in this
/// environment, regardless of `cargo test`'s actual cwd.
///
/// # Safety contract (process-wide env mutation)
/// Mutates process-wide env vars. Safe here: this is the only test in this
/// binary that reads these two vars, and this file has exactly one test
/// function (no other test in this file to race with).
fn point_at_locally_built_runtimes() {
    let root = workspace_root();
    let faster_whisper_venv_bin = if cfg!(windows) {
        root.join("runtimes/faster-whisper/venv/Scripts")
    } else {
        root.join("runtimes/faster-whisper/venv/bin")
    };
    let existing = std::env::var_os("PATH").unwrap_or_default();
    let new_path = std::env::join_paths(
        std::iter::once(faster_whisper_venv_bin).chain(std::env::split_paths(&existing)),
    )
    .expect("venv and existing PATH entries should form a valid PATH");

    let sherpa_onnx_bin_dir = root.join("runtimes/sherpa-onnx/target/release");
    let faster_whisper_dir = root.join("runtimes/faster-whisper");

    // SAFETY: see function doc comment above.
    unsafe {
        std::env::set_var("PATH", new_path);
        std::env::set_var("STT_FASTER_WHISPER_RUNTIME_DIR", &faster_whisper_dir);
        std::env::set_var("STT_SHERPA_ONNX_RUNTIME_DIR", &sherpa_onnx_bin_dir);
    }
}

#[tokio::test]
async fn every_registered_provider_conforms_to_the_local_provider_protocol() {
    point_at_locally_built_runtimes();

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
