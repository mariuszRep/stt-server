//! Shared helpers for the runtime crate's integration tests. Not a test
//! binary itself (Cargo only treats files placed directly under `tests/` as
//! their own binary; a subdirectory's `mod.rs` pulled in via `mod support;`
//! is not), so this can be reused by `protocol_conformance.rs` and the
//! engine-specific test files without triggering a phantom fourth test
//! binary or duplicating the discovery logic three times.

use std::path::PathBuf;

/// `cargo test`'s working directory is not reliable for the runtime
/// adapters' own cwd-relative dev-discovery candidates (they're written for
/// `cargo run`'s cwd, not `cargo test`'s) -- compute the real workspace root
/// via `CARGO_MANIFEST_DIR` instead of trusting the process cwd.
pub fn workspace_root() -> PathBuf {
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
/// Mutates process-wide env vars (`PATH`, `STT_FASTER_WHISPER_RUNTIME_DIR`,
/// `STT_SHERPA_ONNX_RUNTIME_DIR`). Every test that calls this must not run
/// concurrently, in the same test binary, with another test reading those
/// same vars for a different purpose -- each test file in `tests/` is its
/// own binary/process, so this is only a within-file concern; keep it to one
/// caller per file (or serialize callers) if that ever changes.
pub fn point_at_locally_built_runtimes() {
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
