//! Real end-to-end test against the vendored faster-whisper Python runtime
//! (not the `python3 -m http.server` test double used elsewhere) — this is
//! the one test in the suite that actually spawns `run_sidecar.py` and
//! talks to a live FastAPI/uvicorn process.
//!
//! Requires `fastapi`/`uvicorn`/`faster-whisper`/`ctranslate2` importable by
//! the resolved `python3`. Locally that means a venv at
//! `runtimes/faster-whisper/venv` with `pip install -r requirements.txt`
//! run inside it (its `bin`/`Scripts` dir is prepended to `PATH` below so
//! `resolve_python()`'s plain `python3`/`python` lookup finds it, mirroring
//! what a developer's activated venv would do). If those deps aren't
//! present, the test explains why and exits early rather than failing the
//! whole suite over an environment precondition — real CI installs them
//! explicitly before running this test (see the goal's CI workflow).

use std::path::PathBuf;

use stt_runtime::{providers::faster_whisper, ProviderId, RuntimeManager, RuntimeStatus};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn venv_bin_dir(runtime_dir: &std::path::Path) -> PathBuf {
    if cfg!(windows) {
        runtime_dir.join("venv").join("Scripts")
    } else {
        runtime_dir.join("venv").join("bin")
    }
}

/// Prepends the vendored runtime's venv to `PATH` for the current process,
/// so `resolve_python()`'s bare `python3`/`python` lookup resolves to an
/// interpreter with the runtime's dependencies installed — the same effect
/// an activated venv has on a real shell.
///
/// # Safety contract (process-wide env mutation)
/// This mutates `PATH` for the whole test binary process, not just this
/// test. That's safe here because every other test in this crate that
/// spawns `python3` (the `python3 -m http.server` test double in
/// `supervisor.rs`/`manager.rs`) only needs the standard library, which the
/// venv's interpreter still provides in full — prepending it never breaks
/// those tests even if they run concurrently with this one.
fn activate_venv(runtime_dir: &std::path::Path) {
    let venv_bin = venv_bin_dir(runtime_dir);
    let existing = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{}:{existing}", venv_bin.display());
    // SAFETY: see module-level doc comment above.
    unsafe {
        std::env::set_var("PATH", new_path);
    }
}

fn dependencies_available(runtime_dir: &std::path::Path) -> bool {
    let python = venv_bin_dir(runtime_dir).join(if cfg!(windows) {
        "python.exe"
    } else {
        "python3"
    });
    std::process::Command::new(python)
        .args(["-c", "import fastapi, uvicorn, faster_whisper, ctranslate2"])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

#[tokio::test]
async fn real_faster_whisper_runtime_starts_and_serves_health() {
    let runtime_dir = workspace_root().join("runtimes/faster-whisper");

    if !dependencies_available(&runtime_dir) {
        eprintln!(
            "SKIP: {}/venv is missing fastapi/uvicorn/faster-whisper/ctranslate2 — \
             run `python3 -m venv venv && venv/bin/pip install -r requirements.txt` \
             inside {} to enable this test.",
            runtime_dir.display(),
            runtime_dir.display()
        );
        return;
    }

    activate_venv(&runtime_dir);
    // SAFETY: single-threaded-relevant override, see faster_whisper.rs's own
    // tests for the same pattern; no other test reads this var concurrently.
    unsafe {
        std::env::set_var(faster_whisper::RUNTIME_DIR_ENV_VAR, &runtime_dir);
    }
    let launch = faster_whisper::install().expect("vendored runtime should be found and installed");
    unsafe {
        std::env::remove_var(faster_whisper::RUNTIME_DIR_ENV_VAR);
    }

    let manager = RuntimeManager::new(None);
    let id = ProviderId::new("faster-whisper").unwrap();
    manager.register_install(&id, launch).await;

    // Use the smallest curated model so this doesn't hang waiting on a large
    // HuggingFace download; faster-whisper downloads/caches it on first
    // model load inside the Python process, not something this control
    // plane orchestrates itself.
    manager
        .select_model(&id, "Systran/faster-whisper-tiny")
        .await
        .unwrap();

    let descriptor = manager
        .start(&id)
        .await
        .expect("real faster-whisper runtime should become healthy");

    assert_eq!(descriptor.provider, "faster-whisper");
    assert_eq!(descriptor.protocol, "voice-typer-v1");
    assert_eq!(manager.status(&id).await, RuntimeStatus::Running);

    // start() already blocked on GET /health via the supervisor; re-confirm
    // directly against the descriptor's own base_url as an end-to-end check
    // that the descriptor is actually usable, not just internally consistent.
    let health_url = format!("{}/health", descriptor.base_url);
    let response = reqwest::get(&health_url)
        .await
        .expect("health endpoint should be reachable at the descriptor's base_url");
    assert!(response.status().is_success());

    manager
        .stop(&id)
        .await
        .expect("graceful stop should succeed");
    assert_eq!(manager.status(&id).await, RuntimeStatus::Stopped);
}
