//! Install/launch wiring for the managed faster-whisper runtime.
//!
//! "Install" here means: confirm the vendored Python runtime source is
//! present locally, and if so, register how to launch it. This repo vendors
//! that source directly (see `runtimes/faster-whisper/` at the workspace
//! root) rather than depending on `whisper-vibes` at build time. Fetching a
//! packaged (PyInstaller) build from a GitHub release when nothing is
//! present locally is the production distribution path described in the
//! goal's CI/release phase — this module's `locate_runtime_dir` is exactly
//! the seam that path would populate before calling `install()`; today, in
//! the absence of a published release, it only finds a locally vendored or
//! developer-provided copy.

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

use crate::error::RuntimeError;
use crate::manager::{Launch, LaunchBuilder};

/// Environment variable overriding where the vendored runtime source lives.
/// Primarily for local development, where there's no installed binary
/// layout yet (running `cargo run`/`cargo test` from the workspace root).
pub const RUNTIME_DIR_ENV_VAR: &str = "STT_FASTER_WHISPER_RUNTIME_DIR";

/// Find the vendored faster-whisper runtime source. Checked, in order: the
/// override env var, `<binary-dir>/runtimes/faster-whisper`, and
/// `<current-dir>/runtimes/faster-whisper` (the layout this workspace uses
/// during development, so `cargo run -p stt-cli` finds it without any env
/// var).
pub fn locate_runtime_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var(RUNTIME_DIR_ENV_VAR) {
        let path = PathBuf::from(dir);
        return is_valid_runtime_dir(&path).then_some(path);
    }

    let candidates = [
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join("runtimes/faster-whisper"))),
        std::env::current_dir()
            .ok()
            .map(|dir| dir.join("runtimes/faster-whisper")),
    ];

    candidates
        .into_iter()
        .flatten()
        .find(|path| is_valid_runtime_dir(path))
}

fn is_valid_runtime_dir(path: &Path) -> bool {
    path.join("run_sidecar.py").is_file() && path.join("app").join("main.py").is_file()
}

/// Resolve which Python interpreter to use: `python3` (Mac/Linux) first,
/// falling back to `python` (common on Windows).
fn resolve_python() -> Result<&'static str, RuntimeError> {
    for candidate in ["python3", "python"] {
        let found = StdCommand::new(candidate)
            .arg("--version")
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false);
        if found {
            return Ok(candidate);
        }
    }
    Err(RuntimeError::ProviderNotInstalled(
        "no python3/python interpreter found on PATH".into(),
    ))
}

/// Confirm the vendored runtime is available locally and build its launch
/// spec. Returns [`RuntimeError::ProviderNotInstalled`], not a panic or a
/// silent no-op, when it isn't — explicit and typed, matching how
/// `@voice-typer/stt-sdk` handles not-yet-implemented adapters.
pub fn install() -> Result<LaunchBuilder, RuntimeError> {
    let runtime_dir = locate_runtime_dir().ok_or_else(|| {
        RuntimeError::ProviderNotInstalled(format!(
            "vendored faster-whisper runtime not found locally (set {RUNTIME_DIR_ENV_VAR}, \
             or place it at <binary-dir>/runtimes/faster-whisper)"
        ))
    })?;
    let python = resolve_python()?;

    Ok(launch_builder(runtime_dir, python.to_string()))
}

fn launch_builder(runtime_dir: PathBuf, python: String) -> LaunchBuilder {
    Box::new(move |port, auth_token, selected_model| {
        let mut env = vec![
            ("VOICE_TYPER_HOST".to_string(), "127.0.0.1".to_string()),
            ("VOICE_TYPER_PORT".to_string(), port.to_string()),
            ("VOICE_TYPER_AUTH_TOKEN".to_string(), auth_token.to_string()),
        ];
        if let Some(model) = selected_model {
            env.push(("VOICE_TYPER_MODEL".to_string(), model.to_string()));
        }

        Launch {
            program: PathBuf::from(&python),
            args: vec!["run_sidecar.py".to_string()],
            env,
            cwd: Some(runtime_dir.clone()),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Combined into one test, run sequentially: `RUNTIME_DIR_ENV_VAR` is a
    // process-wide env var, and Rust's default test harness runs `#[test]`
    // functions concurrently on separate threads within the same process —
    // two separate tests each mutating it raced against each other and
    // flaked. A single test owns the mutation start-to-finish instead.
    #[test]
    fn locate_runtime_dir_honors_the_env_var_override() {
        // The vendored source at the workspace root is a valid runtime dir;
        // point the env var at it directly rather than relying on the
        // cwd-relative fallback (test binaries don't run from the repo root).
        let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let runtime_dir = workspace_root.join("runtimes/faster-whisper");
        assert!(
            is_valid_runtime_dir(&runtime_dir),
            "expected vendored runtime at {runtime_dir:?}"
        );

        // SAFETY: test-only env var mutation, kept within this single test
        // function so no other test can interleave a conflicting value.
        unsafe {
            std::env::set_var(RUNTIME_DIR_ENV_VAR, &runtime_dir);
        }
        assert_eq!(locate_runtime_dir(), Some(runtime_dir));

        unsafe {
            std::env::set_var(RUNTIME_DIR_ENV_VAR, "/nonexistent/path/for/sure");
        }
        assert_eq!(locate_runtime_dir(), None);

        unsafe {
            std::env::remove_var(RUNTIME_DIR_ENV_VAR);
        }
    }

    #[test]
    fn launch_builder_passes_port_auth_and_model_through_env() {
        let builder = launch_builder(PathBuf::from("/runtime"), "python3".to_string());
        let launch = builder(5123, "tok-abc", Some("Systran/faster-whisper-tiny"));

        assert_eq!(launch.program, PathBuf::from("python3"));
        assert_eq!(launch.args, vec!["run_sidecar.py".to_string()]);
        assert_eq!(launch.cwd, Some(PathBuf::from("/runtime")));
        assert!(launch
            .env
            .contains(&("VOICE_TYPER_PORT".to_string(), "5123".to_string())));
        assert!(launch
            .env
            .contains(&("VOICE_TYPER_AUTH_TOKEN".to_string(), "tok-abc".to_string())));
        assert!(launch.env.contains(&(
            "VOICE_TYPER_MODEL".to_string(),
            "Systran/faster-whisper-tiny".to_string()
        )));
    }

    #[test]
    fn launch_builder_omits_model_env_var_when_none_selected() {
        let builder = launch_builder(PathBuf::from("/runtime"), "python3".to_string());
        let launch = builder(5123, "tok-abc", None);
        assert!(!launch.env.iter().any(|(k, _)| k == "VOICE_TYPER_MODEL"));
    }
}
