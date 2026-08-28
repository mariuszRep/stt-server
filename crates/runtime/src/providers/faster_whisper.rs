//! Install/launch wiring for the managed faster-whisper runtime.
//!
//! "Install" here means: confirm a usable copy of the runtime is present
//! locally, and if so, register how to launch it. Two forms count as
//! usable, checked in preference order by [`detect_runtime_kind`]:
//!
//! - A **packaged executable** (a PyInstaller `--onefile` build, e.g.
//!   `voice-typer-backend.exe`) — standalone, no Python interpreter needed.
//!   This is what a real end-user install (fetched from a GitHub release,
//!   or bundled into a Tauri installer) has.
//! - **Raw Python source** (`run_sidecar.py` + `app/main.py`), launched via
//!   a system `python3`/`python` — what local development uses (this repo
//!   vendors that source directly at `runtimes/faster-whisper/`).
//!
//! Packaged is preferred when both are present, since that's what a real
//! installed environment actually has; raw source is the fallback local
//! dev relies on. `locate_runtime_dir`'s search order (override env var →
//! `<binary-dir>/runtimes/faster-whisper` → `<current-dir>/runtimes/faster-whisper`)
//! is unchanged by this — only the leaf validity check considers both forms.

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

use crate::catalog::RuntimeVariant;
use crate::error::RuntimeError;
use crate::manager::{Launch, LaunchBuilder};

/// Environment variable overriding where the runtime (packaged or raw
/// source) lives. Primarily for local development and for a Tauri sidecar
/// telling `stt` exactly where its bundled resource landed (installer
/// layouts don't preserve `externalBin`-relative paths for bundled
/// resources — see the desktop rewire plan).
pub const RUNTIME_DIR_ENV_VAR: &str = "STT_FASTER_WHISPER_RUNTIME_DIR";

/// Which form of the runtime was found in a candidate directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeKind {
    /// A standalone packaged executable at this path — spawn directly, no
    /// interpreter needed.
    Packaged(PathBuf),
    /// Raw Python source in this directory — spawn via `python3 run_sidecar.py`.
    RawSource,
}

/// The packaged runtime's expected executable name on this platform.
/// Matches `runtimes/faster-whisper/voice-typer-backend.spec`'s `name=`.
fn packaged_executable_name() -> &'static str {
    if cfg!(windows) {
        "voice-typer-backend.exe"
    } else {
        "voice-typer-backend"
    }
}

/// Find the vendored/installed faster-whisper runtime. Checked, in order:
/// the override env var, `<binary-dir>/runtimes/faster-whisper`, and
/// `<current-dir>/runtimes/faster-whisper` (the layout this workspace uses
/// during development, so `cargo run -p stt-cli` finds it without any env
/// var).
pub fn locate_runtime_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var(RUNTIME_DIR_ENV_VAR) {
        let path = PathBuf::from(dir);
        return detect_runtime_kind(&path).is_some().then_some(path);
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
        .find(|path| detect_runtime_kind(path).is_some())
}

/// Inspect a candidate directory and report which usable form it holds, if
/// any. Prefers a packaged executable over raw source when both exist.
pub fn detect_runtime_kind(dir: &Path) -> Option<RuntimeKind> {
    let packaged = dir.join(packaged_executable_name());
    if packaged.is_file() {
        return Some(RuntimeKind::Packaged(packaged));
    }
    if dir.join("run_sidecar.py").is_file() && dir.join("app").join("main.py").is_file() {
        return Some(RuntimeKind::RawSource);
    }
    None
}

/// Filename `voice-typer-backend.spec` writes next to the built exe,
/// containing the bare build-variant string ("cpu"/"gpu"). `config.py`
/// reads the exact same file for the same reason on the Python side.
const PACKAGED_VARIANT_SENTINEL_FILENAME: &str = "variant.txt";

/// Best-effort read of the sentinel next to a packaged exe. `None` covers
/// every "can't confirm" case uniformly (file absent — a pre-this-fix build
/// or a manually dropped dev/test exe; unreadable; unparseable): callers
/// treat "can't confirm" as "assume it matches," preserving the existing
/// leniency `install_local`'s doc comment already grants a vendored dev
/// copy — this only *adds* a check when there's a positive signal to check
/// against.
fn read_packaged_variant_sentinel(runtime_dir: &Path) -> Option<RuntimeVariant> {
    let contents =
        std::fs::read_to_string(runtime_dir.join(PACKAGED_VARIANT_SENTINEL_FILENAME)).ok()?;
    RuntimeVariant::parse(contents.trim())
}

/// `python -m venv` only creates `Scripts\python.exe` on Windows — no
/// `python3.exe` — but always creates both `bin/python` and `bin/python3`
/// on Unix. Trying the wrong name first can silently skip right past a
/// PATH-prepended venv and fall through to an unrelated system
/// interpreter without the venv's packages installed.
fn python_candidates() -> [&'static str; 2] {
    if cfg!(windows) {
        ["python", "python3"]
    } else {
        ["python3", "python"]
    }
}

/// Resolve which Python interpreter to use, preferring whichever name a
/// `venv` on this platform actually provides (see [`python_candidates`]).
fn resolve_python() -> Result<&'static str, RuntimeError> {
    for candidate in python_candidates() {
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

/// Confirm a usable runtime (packaged or raw source) is available locally
/// and build its launch spec. Returns [`RuntimeError::ProviderNotInstalled`],
/// not a panic or a silent no-op, when it isn't — explicit and typed,
/// matching how `@open-vibe-ai/stt-sdk` handles not-yet-implemented adapters.
pub fn install() -> Result<LaunchBuilder, RuntimeError> {
    let runtime_dir = locate_runtime_dir().ok_or_else(|| {
        RuntimeError::ProviderNotInstalled(format!(
            "faster-whisper runtime not found locally (set {RUNTIME_DIR_ENV_VAR}, \
             or place it at <binary-dir>/runtimes/faster-whisper)"
        ))
    })?;

    match detect_runtime_kind(&runtime_dir) {
        Some(RuntimeKind::Packaged(exe_path)) => Ok(packaged_launch_builder(exe_path, runtime_dir)),
        Some(RuntimeKind::RawSource) => {
            let python = resolve_python()?;
            Ok(raw_source_launch_builder(runtime_dir, python.to_string()))
        }
        None => Err(RuntimeError::ProviderNotInstalled(format!(
            "{} contains neither a packaged executable nor raw Python source",
            runtime_dir.display()
        ))),
    }
}

/// GitHub repo this runtime's packaged release assets are published to —
/// `stt-server`'s own releases, not the App's `OpenVibeAI-data`, since this
/// is `stt-server`'s own build artifact.
const RELEASE_REPO: &str = "mariuszRep/stt-server";

/// Overrides the computed release-asset base URL entirely. Primarily for
/// tests (point it at a local fake server) and for anyone who mirrors
/// releases elsewhere; unset in normal operation.
pub const RELEASE_BASE_URL_ENV_VAR: &str = "STT_FASTER_WHISPER_RELEASE_BASE_URL";

/// The base URL release assets are downloaded from, i.e. everything up to
/// (not including) the `/<asset-name>` suffix. Versioned by this crate's
/// *own* version — a caller picks provider/model/variant, but which
/// `stt-server` build produced/speaks a compatible wire protocol isn't a
/// caller-tunable axis.
fn release_base_url() -> String {
    std::env::var(RELEASE_BASE_URL_ENV_VAR).unwrap_or_else(|_| {
        format!(
            "https://github.com/{RELEASE_REPO}/releases/download/v{}",
            env!("CARGO_PKG_VERSION")
        )
    })
}

/// Release-asset filename convention: `faster-whisper-runtime-{os}-{variant}{ext}`.
fn asset_name(variant: RuntimeVariant) -> String {
    let os = if cfg!(windows) { "windows" } else { "linux" };
    let ext = if cfg!(windows) { ".exe" } else { "" };
    format!("faster-whisper-runtime-{os}-{}{ext}", variant.as_str())
}

/// Overrides the runtime cache root (`stt_common::default_runtime_cache_dir()`
/// otherwise). Mainly for tests, so a download test doesn't write into the
/// real user's data-local directory.
pub const RUNTIME_CACHE_DIR_ENV_VAR: &str = "STT_FASTER_WHISPER_CACHE_DIR";

/// On-disk cache directory for a downloaded variant. Each variant gets its
/// own subdirectory so installing one never evicts another — switching
/// device preference back and forth doesn't force a re-download.
fn cached_variant_dir(variant: RuntimeVariant) -> PathBuf {
    let root = std::env::var(RUNTIME_CACHE_DIR_ENV_VAR)
        .map(PathBuf::from)
        .unwrap_or_else(|_| stt_common::default_runtime_cache_dir());
    root.join("faster-whisper").join(variant.as_str())
}

fn cached_variant_exe_path(variant: RuntimeVariant) -> PathBuf {
    cached_variant_dir(variant).join(asset_name(variant))
}

/// Local-only lookup: a vendored/dev copy (variant-agnostic — whatever a
/// developer has locally is used regardless of which variant was asked
/// for) or a previously-downloaded packaged copy of exactly `variant`. No
/// network access, always fast. Returns `None` (not an error) when nothing
/// local is found, so the caller can decide whether to fall back to a
/// network download.
pub fn install_local(variant: RuntimeVariant) -> Option<LaunchBuilder> {
    if let Some(runtime_dir) = locate_runtime_dir() {
        match detect_runtime_kind(&runtime_dir) {
            Some(RuntimeKind::Packaged(exe_path)) => {
                match read_packaged_variant_sentinel(&runtime_dir) {
                    Some(found) if found != variant => {
                        // Labeled, and it's the other flavor (e.g. a desktop
                        // install's fixed cpu-only bundled resource) — never
                        // silently substitute it for what was asked for. Fall
                        // through to the variant-scoped cache check below.
                    }
                    _ => return Some(packaged_launch_builder(exe_path, runtime_dir)),
                }
            }
            Some(RuntimeKind::RawSource) => {
                if let Ok(python) = resolve_python() {
                    return Some(raw_source_launch_builder(runtime_dir, python.to_string()));
                }
            }
            None => {}
        }
    }

    let cached = cached_variant_exe_path(variant);
    if cached.is_file() {
        let dir = cached
            .parent()
            .expect("cached_variant_exe_path always has a parent")
            .to_path_buf();
        return Some(packaged_launch_builder(cached, dir));
    }

    None
}

/// Delete a previously-downloaded variant's cached directory. A no-op
/// (`Ok`) if it was never downloaded — matches `uninstall`'s existing
/// "idempotent, not an error to remove something already absent" spirit
/// for the parts of this that are genuinely safe to no-op; never touches a
/// vendored dev copy (that's `locate_runtime_dir()`'s domain, not the
/// cache dir this function operates on).
pub fn remove_cached_variant(variant: RuntimeVariant) -> Result<(), RuntimeError> {
    let dir = cached_variant_dir(variant);
    if dir.is_dir() {
        std::fs::remove_dir_all(&dir).map_err(RuntimeError::Io)?;
    }
    Ok(())
}

/// Overrides where downloaded model weights are cached
/// (`stt_common::default_model_dir()` otherwise). Mainly for tests, so a
/// download/verify/remove test never touches the real user's data-local
/// directory.
pub const MODEL_CACHE_DIR_ENV_VAR: &str = "STT_FASTER_WHISPER_MODEL_DIR";

/// On-disk directory a given model's weights live in — a flat, predictable
/// `<model-root>/faster-whisper/<model-id>/` layout (deliberately *not*
/// HuggingFace's own hashed `hub/models--org--name/snapshots/<hash>/` cache
/// scheme), passed straight through as `download_root` so `verify`/`remove`
/// can operate on it directly without knowing anything about HF's internal
/// layout. `model_id` values like `"Systran/faster-whisper-small"` contain
/// their own `/`, which `PathBuf::join` treats as another path component —
/// harmless here since ids only ever come from the curated catalog, never
/// caller-supplied paths.
pub fn cached_model_dir(model_id: &str) -> PathBuf {
    let root = std::env::var(MODEL_CACHE_DIR_ENV_VAR)
        .map(PathBuf::from)
        .unwrap_or_else(|_| stt_common::default_model_dir());
    root.join("faster-whisper").join(model_id)
}

/// The one file whose presence/size actually proves a model finished
/// downloading — always part of a CTranslate2-converted faster-whisper
/// model's HF repo, regardless of model size or which repo it came from.
const MODEL_WEIGHTS_FILENAME: &str = "model.bin";

/// Whether `model_id`'s weights are present on disk, and their size if so.
/// A pure filesystem check — no subprocess, no network — mirroring how
/// `remove_cached_variant` needs neither to clean up.
pub fn verify_cached_model(model_id: &str) -> Result<Option<u64>, RuntimeError> {
    let weights = cached_model_dir(model_id).join(MODEL_WEIGHTS_FILENAME);
    match std::fs::metadata(&weights) {
        Ok(meta) if meta.len() > 0 => Ok(Some(meta.len())),
        Ok(_) => Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(RuntimeError::Io(e)),
    }
}

/// Delete a previously-downloaded model's cached directory. Idempotent —
/// `Ok` if it was never downloaded, matching `remove_cached_variant`'s
/// spirit.
pub fn remove_cached_model(model_id: &str) -> Result<(), RuntimeError> {
    let dir = cached_model_dir(model_id);
    if dir.is_dir() {
        std::fs::remove_dir_all(&dir).map_err(RuntimeError::Io)?;
    }
    Ok(())
}

/// Locate any usable local copy of the runtime, packaged or raw source,
/// regardless of which hardware variant it is. Unlike [`install_local`],
/// variant doesn't matter here: a plain model-weight download exercises
/// none of CTranslate2/CUDA, so whichever copy happens to be present
/// (preferring a vendored dev copy, then a cached CPU build, then GPU) can
/// run it. Returns the program, base args, and working directory needed to
/// spawn it.
fn resolve_download_runtime() -> Option<(PathBuf, Vec<String>, PathBuf)> {
    if let Some(runtime_dir) = locate_runtime_dir() {
        match detect_runtime_kind(&runtime_dir) {
            Some(RuntimeKind::Packaged(exe_path)) => return Some((exe_path, vec![], runtime_dir)),
            Some(RuntimeKind::RawSource) => {
                if let Ok(python) = resolve_python() {
                    return Some((
                        PathBuf::from(python),
                        vec!["run_sidecar.py".to_string()],
                        runtime_dir,
                    ));
                }
            }
            None => {}
        }
    }

    for variant in [RuntimeVariant::Cpu, RuntimeVariant::Gpu] {
        let cached = cached_variant_exe_path(variant);
        if cached.is_file() {
            let dir = cached
                .parent()
                .expect("cached_variant_exe_path always has a parent")
                .to_path_buf();
            return Some((cached, vec![], dir));
        }
    }

    None
}

/// Download `model_id`'s weights into `output_dir` by spawning the local
/// runtime in its `download-model` mode (see `run_sidecar.py`) and waiting
/// for it to exit — no HTTP server is started, no CUDA/CTranslate2 model is
/// constructed, just the underlying HuggingFace fetch. Requires a provider
/// variant to already be installed locally: there's no other way to get a
/// Python + faster-whisper environment capable of running the fetch, so a
/// clean error is returned rather than attempting to auto-install one
/// (auto-installing a variant just to pull a model would be a surprising
/// side effect of what's meant to be an explicit, curated action).
pub async fn download_model(model_id: &str, output_dir: &Path) -> Result<(), RuntimeError> {
    let (program, mut args, cwd) = resolve_download_runtime().ok_or_else(|| {
        RuntimeError::ProviderNotInstalled(
            "no local faster-whisper runtime found — install a provider variant first \
             (e.g. `stt provider install faster-whisper`) before pulling a model"
                .to_string(),
        )
    })?;
    args.push("download-model".to_string());
    args.push(model_id.to_string());
    args.push(output_dir.to_string_lossy().to_string());

    std::fs::create_dir_all(output_dir).map_err(RuntimeError::Io)?;

    let output = tokio::process::Command::new(&program)
        .args(&args)
        .current_dir(&cwd)
        .output()
        .await
        .map_err(RuntimeError::Io)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(RuntimeError::DownloadFailed(format!(
            "model download for {model_id} failed: {stderr}"
        )));
    }
    Ok(())
}

/// Progress of an in-flight release-asset download.
#[derive(Debug, Clone, Copy)]
pub struct DownloadProgress {
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
}

/// Download `variant`'s packaged runtime from this crate's GitHub release,
/// persist it under the runtime cache dir, and return a launch spec for
/// it. Streams to a `.part` file and atomically renames into place on
/// success, so a failed/interrupted download never leaves a corrupt file
/// where [`install_local`] would find it. `on_progress` is called after
/// each chunk; it's synchronous (not async) so callers can back it with a
/// plain `std::sync::Mutex`-guarded state update with no `.await` needed.
pub async fn download_variant(
    variant: RuntimeVariant,
    on_progress: impl Fn(DownloadProgress),
) -> Result<LaunchBuilder, RuntimeError> {
    let name = asset_name(variant);
    let url = format!("{}/{name}", release_base_url());
    let dest_dir = cached_variant_dir(variant);
    std::fs::create_dir_all(&dest_dir).map_err(RuntimeError::Io)?;
    let dest = dest_dir.join(&name);
    let tmp = dest_dir.join(format!("{name}.part"));

    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| RuntimeError::DownloadFailed(format!("request to {url} failed: {e}")))?;
    if !resp.status().is_success() {
        return Err(RuntimeError::DownloadFailed(format!(
            "{url} returned {}",
            resp.status()
        )));
    }
    let total_bytes = resp.content_length();

    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;

    let mut downloaded: u64 = 0;
    let mut file = tokio::fs::File::create(&tmp)
        .await
        .map_err(RuntimeError::Io)?;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk
            .map_err(|e| RuntimeError::DownloadFailed(format!("download interrupted: {e}")))?;
        downloaded += chunk.len() as u64;
        file.write_all(&chunk).await.map_err(RuntimeError::Io)?;
        on_progress(DownloadProgress {
            downloaded_bytes: downloaded,
            total_bytes,
        });
    }
    file.flush().await.map_err(RuntimeError::Io)?;
    drop(file);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&tmp)
            .map_err(RuntimeError::Io)?
            .permissions();
        perms.set_mode(perms.mode() | 0o111);
        std::fs::set_permissions(&tmp, perms).map_err(RuntimeError::Io)?;
    }

    tokio::fs::rename(&tmp, &dest)
        .await
        .map_err(RuntimeError::Io)?;

    Ok(packaged_launch_builder(dest, dest_dir))
}

/// Env vars every launch form shares: host/port/auth/model/model
/// dir/device/compute_type, the `VOICE_TYPER_*` contract both the packaged
/// exe and `run_sidecar.py` read identically. `VOICE_TYPER_MODEL_DIR` is
/// `cached_model_dir(model)` — an explicit, stt-server-owned download
/// location passed to `WhisperModel(download_root=...)` instead of letting
/// weights land wherever the OS-default HuggingFace cache happens to be,
/// per CONVENTIONS.md's "no invisible model download" rule: the location is
/// now explicit and inspectable even though the lazy download-on-first-use
/// behavior itself is unchanged. `VOICE_TYPER_HOST` follows
/// `options.bind_host` (default loopback) so LAN mode actually binds where
/// the caller asked —
/// `RuntimeManager::start` has already validated it's either loopback or the
/// wildcard `"0.0.0.0"` before this runs.
fn build_env(
    auth_token: &str,
    selected_model: Option<&str>,
    options: &crate::manager::StartOptions,
) -> Vec<(String, String)> {
    let host = options.bind_host.as_deref().unwrap_or("127.0.0.1");
    let mut env = vec![
        ("VOICE_TYPER_HOST".to_string(), host.to_string()),
        ("VOICE_TYPER_AUTH_TOKEN".to_string(), auth_token.to_string()),
    ];
    if let Some(model) = selected_model {
        env.push(("VOICE_TYPER_MODEL".to_string(), model.to_string()));
        env.push((
            "VOICE_TYPER_MODEL_DIR".to_string(),
            cached_model_dir(model).to_string_lossy().to_string(),
        ));
    }
    if let Some(device) = &options.device {
        env.push(("VOICE_TYPER_DEVICE".to_string(), device.clone()));
    }
    if let Some(compute_type) = &options.compute_type {
        env.push(("VOICE_TYPER_COMPUTE_TYPE".to_string(), compute_type.clone()));
    }
    env
}

/// Standalone packaged executable: no interpreter, no script argument.
fn packaged_launch_builder(exe_path: PathBuf, runtime_dir: PathBuf) -> LaunchBuilder {
    Box::new(move |port, auth_token, selected_model, options| {
        let mut env = build_env(auth_token, selected_model, options);
        env.push(("VOICE_TYPER_PORT".to_string(), port.to_string()));
        Launch {
            program: exe_path.clone(),
            args: vec![],
            env,
            cwd: Some(runtime_dir.clone()),
        }
    })
}

/// Raw Python source: spawn the resolved interpreter against `run_sidecar.py`.
fn raw_source_launch_builder(runtime_dir: PathBuf, python: String) -> LaunchBuilder {
    Box::new(move |port, auth_token, selected_model, options| {
        let mut env = build_env(auth_token, selected_model, options);
        env.push(("VOICE_TYPER_PORT".to_string(), port.to_string()));
        Launch {
            program: PathBuf::from(&python),
            args: vec!["run_sidecar.py".to_string()],
            env,
            cwd: Some(runtime_dir.clone()),
        }
    })
}

/// Serializes tests (in this module and in `manager.rs`) that mutate the
/// process-wide env vars this module reads to redirect where it looks for a
/// runtime / where it caches downloads / where it downloads from
/// (`RUNTIME_DIR_ENV_VAR`, `RUNTIME_CACHE_DIR_ENV_VAR`,
/// `RELEASE_BASE_URL_ENV_VAR`). Rust's default test harness runs `#[test]`/
/// `#[tokio::test]` functions concurrently on separate threads within the
/// same process, so without this lock two such tests can interleave and
/// clobber each other's env var value mid-critical-section. `pub(crate)` so
/// `manager.rs`'s own env-var-mutating tests can share it.
#[cfg(test)]
pub(crate) static ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
pub(crate) fn lock_env_test() -> std::sync::MutexGuard<'static, ()> {
    ENV_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::StartOptions;

    // Combined into one test, run sequentially: `RUNTIME_DIR_ENV_VAR` is a
    // process-wide env var, and Rust's default test harness runs `#[test]`
    // functions concurrently on separate threads within the same process —
    // two separate tests each mutating it raced against each other and
    // flaked. A single test owns the mutation start-to-finish instead.
    #[test]
    fn locate_runtime_dir_honors_the_env_var_override() {
        let _guard = lock_env_test();
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
            detect_runtime_kind(&runtime_dir).is_some(),
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
    fn raw_source_launch_builder_passes_port_auth_and_model_through_env() {
        let builder = raw_source_launch_builder(PathBuf::from("/runtime"), "python3".to_string());
        let launch = builder(
            5123,
            "tok-abc",
            Some("Systran/faster-whisper-tiny"),
            &StartOptions::default(),
        );

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
        assert!(
            launch.env.iter().any(|(k, _)| k == "VOICE_TYPER_MODEL_DIR"),
            "expected an explicit VOICE_TYPER_MODEL_DIR alongside VOICE_TYPER_MODEL"
        );
    }

    #[test]
    fn packaged_launch_builder_spawns_the_exe_directly_with_no_args() {
        let builder = packaged_launch_builder(
            PathBuf::from("/runtime/voice-typer-backend.exe"),
            PathBuf::from("/runtime"),
        );
        let launch = builder(5123, "tok-abc", None, &StartOptions::default());

        assert_eq!(
            launch.program,
            PathBuf::from("/runtime/voice-typer-backend.exe")
        );
        assert!(
            launch.args.is_empty(),
            "packaged exe needs no script argument"
        );
        assert_eq!(launch.cwd, Some(PathBuf::from("/runtime")));
        assert!(launch
            .env
            .contains(&("VOICE_TYPER_PORT".to_string(), "5123".to_string())));
        assert!(launch
            .env
            .contains(&("VOICE_TYPER_AUTH_TOKEN".to_string(), "tok-abc".to_string())));
    }

    #[test]
    fn launch_builder_passes_device_and_compute_type_through_env_when_set() {
        let builder = raw_source_launch_builder(PathBuf::from("/runtime"), "python3".to_string());
        let options = StartOptions {
            device: Some("cpu".to_string()),
            compute_type: Some("int8".to_string()),
            ..StartOptions::default()
        };
        let launch = builder(5123, "tok-abc", None, &options);

        assert!(launch
            .env
            .contains(&("VOICE_TYPER_DEVICE".to_string(), "cpu".to_string())));
        assert!(launch
            .env
            .contains(&("VOICE_TYPER_COMPUTE_TYPE".to_string(), "int8".to_string())));
    }

    #[test]
    fn launch_builder_binds_loopback_by_default_and_the_requested_host_when_set() {
        let builder = raw_source_launch_builder(PathBuf::from("/runtime"), "python3".to_string());

        let default_launch = builder(5123, "tok-abc", None, &StartOptions::default());
        assert!(default_launch
            .env
            .contains(&("VOICE_TYPER_HOST".to_string(), "127.0.0.1".to_string())));

        let lan_options = StartOptions {
            bind_host: Some("0.0.0.0".to_string()),
            auth_token: Some("shared-secret".to_string()),
            ..StartOptions::default()
        };
        let lan_launch = builder(5123, "tok-abc", None, &lan_options);
        assert!(lan_launch
            .env
            .contains(&("VOICE_TYPER_HOST".to_string(), "0.0.0.0".to_string())));
    }

    #[test]
    fn launch_builder_omits_device_and_compute_type_when_not_set() {
        let builder = raw_source_launch_builder(PathBuf::from("/runtime"), "python3".to_string());
        let launch = builder(5123, "tok-abc", None, &StartOptions::default());

        assert!(!launch.env.iter().any(|(k, _)| k == "VOICE_TYPER_DEVICE"));
        assert!(!launch
            .env
            .iter()
            .any(|(k, _)| k == "VOICE_TYPER_COMPUTE_TYPE"));
    }

    #[test]
    fn detect_runtime_kind_prefers_packaged_over_raw_source_when_both_present() {
        let dir =
            std::env::temp_dir().join(format!("stt-runtime-detect-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("app")).unwrap();
        std::fs::write(dir.join("run_sidecar.py"), b"").unwrap();
        std::fs::write(dir.join("app").join("main.py"), b"").unwrap();
        std::fs::write(dir.join(packaged_executable_name()), b"").unwrap();

        assert_eq!(
            detect_runtime_kind(&dir),
            Some(RuntimeKind::Packaged(dir.join(packaged_executable_name())))
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn detect_runtime_kind_falls_back_to_raw_source_when_no_packaged_exe() {
        let dir = std::env::temp_dir().join(format!(
            "stt-runtime-detect-test-rawonly-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(dir.join("app")).unwrap();
        std::fs::write(dir.join("run_sidecar.py"), b"").unwrap();
        std::fs::write(dir.join("app").join("main.py"), b"").unwrap();

        assert_eq!(detect_runtime_kind(&dir), Some(RuntimeKind::RawSource));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn detect_runtime_kind_none_when_neither_form_present() {
        let dir = std::env::temp_dir().join(format!(
            "stt-runtime-detect-test-empty-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        assert_eq!(detect_runtime_kind(&dir), None);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn python_candidate_order_matches_what_a_stdlib_venv_actually_provides() {
        let candidates = python_candidates();
        if cfg!(windows) {
            assert_eq!(candidates, ["python", "python3"]);
        } else {
            assert_eq!(candidates, ["python3", "python"]);
        }
    }

    #[test]
    fn launch_builder_omits_model_env_var_when_none_selected() {
        let builder = raw_source_launch_builder(PathBuf::from("/runtime"), "python3".to_string());
        let launch = builder(5123, "tok-abc", None, &StartOptions::default());
        assert!(!launch.env.iter().any(|(k, _)| k == "VOICE_TYPER_MODEL"));
    }

    #[test]
    fn asset_name_follows_the_os_variant_naming_convention() {
        let name = asset_name(RuntimeVariant::Cpu);
        if cfg!(windows) {
            assert_eq!(name, "faster-whisper-runtime-windows-cpu.exe");
        } else {
            assert_eq!(name, "faster-whisper-runtime-linux-cpu");
        }
        assert!(asset_name(RuntimeVariant::Gpu).contains("-gpu"));
    }

    #[test]
    fn release_base_url_honors_the_env_var_override() {
        let _guard = lock_env_test();
        // SAFETY: test-only env var mutation, scoped to this single test.
        unsafe {
            std::env::set_var(RELEASE_BASE_URL_ENV_VAR, "http://example.invalid/releases");
        }
        assert_eq!(release_base_url(), "http://example.invalid/releases");
        unsafe {
            std::env::remove_var(RELEASE_BASE_URL_ENV_VAR);
        }
        assert!(release_base_url().starts_with(&format!(
            "https://github.com/{RELEASE_REPO}/releases/download/v"
        )));
    }

    #[test]
    fn install_local_finds_a_previously_cached_variant_when_no_dev_source_present() {
        let _guard = lock_env_test();
        let cache_root =
            std::env::temp_dir().join(format!("stt-cache-test-{}-{}", std::process::id(), line!()));
        let variant_dir = cache_root.join("faster-whisper").join("gpu");
        std::fs::create_dir_all(&variant_dir).unwrap();
        std::fs::write(
            variant_dir.join(asset_name(RuntimeVariant::Gpu)),
            b"fake exe",
        )
        .unwrap();

        // SAFETY: test-only env var mutation, scoped to this single test.
        // Also override RUNTIME_DIR_ENV_VAR to a nonexistent path so
        // `locate_runtime_dir()`'s dev-source lookup can't find the real
        // vendored copy and mask what this test is actually checking.
        unsafe {
            std::env::set_var(RUNTIME_CACHE_DIR_ENV_VAR, &cache_root);
            std::env::set_var(RUNTIME_DIR_ENV_VAR, "/nonexistent/for/sure");
        }

        let builder = install_local(RuntimeVariant::Gpu);

        unsafe {
            std::env::remove_var(RUNTIME_CACHE_DIR_ENV_VAR);
            std::env::remove_var(RUNTIME_DIR_ENV_VAR);
        }
        std::fs::remove_dir_all(&cache_root).ok();

        let builder = builder.expect("expected the cached gpu variant to be found");
        let launch = builder(5123, "tok-abc", None, &StartOptions::default());
        assert_eq!(launch.args, Vec::<String>::new());
        assert!(launch
            .program
            .to_string_lossy()
            .contains(&asset_name(RuntimeVariant::Gpu)));
    }

    #[test]
    fn install_local_finds_nothing_when_cache_and_dev_source_are_both_absent() {
        let _guard = lock_env_test();
        let cache_root = std::env::temp_dir().join(format!(
            "stt-cache-test-empty-{}-{}",
            std::process::id(),
            line!()
        ));
        // SAFETY: test-only env var mutation, scoped to this single test.
        unsafe {
            std::env::set_var(RUNTIME_CACHE_DIR_ENV_VAR, &cache_root);
            std::env::set_var(RUNTIME_DIR_ENV_VAR, "/nonexistent/for/sure");
        }

        let result = install_local(RuntimeVariant::Cpu);

        unsafe {
            std::env::remove_var(RUNTIME_CACHE_DIR_ENV_VAR);
            std::env::remove_var(RUNTIME_DIR_ENV_VAR);
        }

        assert!(result.is_none());
    }

    /// Sets up a fake "packaged" runtime directory (via `RUNTIME_DIR_ENV_VAR`)
    /// containing a dummy exe and, if `sentinel` is `Some`, a `variant.txt`
    /// next to it. Returns the directory so the caller can clean it up.
    fn write_fake_packaged_runtime_dir(name_suffix: &str, sentinel: Option<&str>) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "stt-packaged-test-{name_suffix}-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(packaged_executable_name()), b"fake exe").unwrap();
        if let Some(variant) = sentinel {
            std::fs::write(dir.join(PACKAGED_VARIANT_SENTINEL_FILENAME), variant).unwrap();
        }
        dir
    }

    #[test]
    fn install_local_falls_through_to_a_cached_variant_when_the_packaged_exe_sentinel_says_the_other_variant(
    ) {
        let _guard = lock_env_test();
        let packaged_dir = write_fake_packaged_runtime_dir("mismatch", Some("cpu"));

        let cache_root = std::env::temp_dir().join(format!(
            "stt-cache-test-sentinel-mismatch-{}-{}",
            std::process::id(),
            line!()
        ));
        let variant_dir = cache_root.join("faster-whisper").join("gpu");
        std::fs::create_dir_all(&variant_dir).unwrap();
        std::fs::write(
            variant_dir.join(asset_name(RuntimeVariant::Gpu)),
            b"fake gpu exe",
        )
        .unwrap();

        // SAFETY: test-only env var mutation, scoped to this single test.
        unsafe {
            std::env::set_var(RUNTIME_DIR_ENV_VAR, &packaged_dir);
            std::env::set_var(RUNTIME_CACHE_DIR_ENV_VAR, &cache_root);
        }

        let builder = install_local(RuntimeVariant::Gpu);

        unsafe {
            std::env::remove_var(RUNTIME_DIR_ENV_VAR);
            std::env::remove_var(RUNTIME_CACHE_DIR_ENV_VAR);
        }
        std::fs::remove_dir_all(&packaged_dir).ok();
        std::fs::remove_dir_all(&cache_root).ok();

        let builder = builder.expect(
            "expected the cached gpu variant to be found, not the packaged cpu-sentinel exe",
        );
        let launch = builder(5123, "tok-abc", None, &StartOptions::default());
        assert!(
            launch
                .program
                .to_string_lossy()
                .contains(&asset_name(RuntimeVariant::Gpu)),
            "expected the gpu cache asset, got {:?}",
            launch.program
        );
    }

    #[test]
    fn install_local_returns_none_when_packaged_sentinel_mismatches_and_no_cache_exists() {
        let _guard = lock_env_test();
        let packaged_dir = write_fake_packaged_runtime_dir("no-cache", Some("cpu"));
        let cache_root = std::env::temp_dir().join(format!(
            "stt-cache-test-no-cache-{}-{}",
            std::process::id(),
            line!()
        ));

        // SAFETY: test-only env var mutation, scoped to this single test.
        unsafe {
            std::env::set_var(RUNTIME_DIR_ENV_VAR, &packaged_dir);
            std::env::set_var(RUNTIME_CACHE_DIR_ENV_VAR, &cache_root);
        }

        let result = install_local(RuntimeVariant::Gpu);

        unsafe {
            std::env::remove_var(RUNTIME_DIR_ENV_VAR);
            std::env::remove_var(RUNTIME_CACHE_DIR_ENV_VAR);
        }
        std::fs::remove_dir_all(&packaged_dir).ok();

        assert!(result.is_none());
    }

    #[test]
    fn install_local_still_returns_the_packaged_exe_when_its_sentinel_matches() {
        let _guard = lock_env_test();
        let packaged_dir = write_fake_packaged_runtime_dir("match", Some("gpu"));

        // SAFETY: test-only env var mutation, scoped to this single test.
        unsafe {
            std::env::set_var(RUNTIME_DIR_ENV_VAR, &packaged_dir);
        }

        let builder = install_local(RuntimeVariant::Gpu);

        unsafe {
            std::env::remove_var(RUNTIME_DIR_ENV_VAR);
        }
        std::fs::remove_dir_all(&packaged_dir).ok();

        let builder = builder.expect("expected the packaged exe to still be returned");
        let launch = builder(5123, "tok-abc", None, &StartOptions::default());
        assert!(launch
            .program
            .to_string_lossy()
            .contains(packaged_executable_name()));
    }

    #[test]
    fn install_local_still_returns_the_packaged_exe_when_no_sentinel_is_present() {
        let _guard = lock_env_test();
        let packaged_dir = write_fake_packaged_runtime_dir("no-sentinel", None);

        // SAFETY: test-only env var mutation, scoped to this single test.
        unsafe {
            std::env::set_var(RUNTIME_DIR_ENV_VAR, &packaged_dir);
        }

        let builder = install_local(RuntimeVariant::Gpu);

        unsafe {
            std::env::remove_var(RUNTIME_DIR_ENV_VAR);
        }
        std::fs::remove_dir_all(&packaged_dir).ok();

        let builder = builder.expect(
            "a packaged exe with no sentinel at all (pre-fix build / dev copy) should still be leniently accepted",
        );
        let launch = builder(5123, "tok-abc", None, &StartOptions::default());
        assert!(launch
            .program
            .to_string_lossy()
            .contains(packaged_executable_name()));
    }

    /// Real end-to-end download test: serves a small fake asset over a
    /// local `python3 -m http.server` (the same test-double pattern used
    /// throughout this crate), points `download_variant` at it via the env
    /// override, and verifies the file actually lands in the cache dir
    /// with the right content, progress was reported, and the returned
    /// launch spec points at the downloaded file.
    #[tokio::test]
    // Held across `.await` deliberately: this guard serializes tests
    // that mutate process-wide env vars, and each `#[tokio::test]` here runs
    // on its own single-threaded (current_thread) runtime, so holding it
    // through the awaits below just serializes those tests, it can't starve
    // unrelated async work on another task.
    #[allow(clippy::await_holding_lock)]
    async fn download_variant_fetches_persists_and_builds_a_working_launch_spec() {
        let _guard = lock_env_test();
        let serve_dir =
            std::env::temp_dir().join(format!("stt-download-test-serve-{}", std::process::id()));
        std::fs::create_dir_all(&serve_dir).unwrap();
        let fake_asset_contents = b"pretend this is a packaged exe";
        std::fs::write(
            serve_dir.join(asset_name(RuntimeVariant::Cpu)),
            fake_asset_contents,
        )
        .unwrap();

        let port = crate::supervisor::allocate_loopback_port().unwrap();
        // `python_candidates()[0]` (not a literal "python3"): a plain
        // Windows venv only ever provides `python.exe`, and a literal
        // "python3" resolves to Windows's "app execution alias" stub
        // instead of erroring outright, which silently exits without
        // binding the port rather than failing loudly at spawn time.
        let mut server = tokio::process::Command::new(python_candidates()[0])
            .args([
                "-m",
                "http.server",
                &port.to_string(),
                "--bind",
                "127.0.0.1",
                "--directory",
                serve_dir.to_string_lossy().as_ref(),
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();

        // Give the server a moment to bind; poll rather than a fixed sleep.
        let base_url = format!("http://127.0.0.1:{port}");
        let client = reqwest::Client::new();
        for _ in 0..50 {
            if client.get(&base_url).send().await.is_ok() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        let cache_root =
            std::env::temp_dir().join(format!("stt-cache-test-download-{}", std::process::id()));
        // SAFETY: test-only env var mutation. This test doesn't run
        // concurrently with others that set these same two vars because
        // each such test scopes and clears them before returning, and the
        // default test harness only interleaves at `.await` points, not
        // mid-critical-section — consistent with this file's existing
        // convention for `RUNTIME_DIR_ENV_VAR` mutation.
        unsafe {
            std::env::set_var(RELEASE_BASE_URL_ENV_VAR, &base_url);
            std::env::set_var(RUNTIME_CACHE_DIR_ENV_VAR, &cache_root);
        }

        let progress_calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let progress_calls_clone = progress_calls.clone();
        let result = download_variant(RuntimeVariant::Cpu, move |p| {
            progress_calls_clone.lock().unwrap().push(p);
        })
        .await;

        unsafe {
            std::env::remove_var(RELEASE_BASE_URL_ENV_VAR);
            std::env::remove_var(RUNTIME_CACHE_DIR_ENV_VAR);
        }
        let _ = server.kill().await;

        let builder = result.expect("download should succeed against the local fake server");
        let launch = builder(5123, "tok-abc", None, &StartOptions::default());
        let downloaded_path = launch.program.clone();
        assert!(downloaded_path.is_file());
        assert_eq!(
            std::fs::read(&downloaded_path).unwrap(),
            fake_asset_contents
        );
        assert!(
            !progress_calls.lock().unwrap().is_empty(),
            "expected at least one progress callback"
        );
        assert_eq!(
            progress_calls
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .downloaded_bytes,
            fake_asset_contents.len() as u64
        );

        std::fs::remove_dir_all(&serve_dir).ok();
        std::fs::remove_dir_all(&cache_root).ok();
    }

    #[test]
    fn cached_model_dir_nests_under_the_model_root_and_honors_the_env_override() {
        let _guard = lock_env_test();
        let root = std::env::temp_dir().join(format!(
            "stt-model-dir-test-{}-{}",
            std::process::id(),
            line!()
        ));
        // SAFETY: test-only env var mutation, scoped to this single test.
        unsafe {
            std::env::set_var(MODEL_CACHE_DIR_ENV_VAR, &root);
        }
        let dir = cached_model_dir("Systran/faster-whisper-tiny");
        unsafe {
            std::env::remove_var(MODEL_CACHE_DIR_ENV_VAR);
        }
        assert_eq!(
            dir,
            root.join("faster-whisper")
                .join("Systran")
                .join("faster-whisper-tiny")
        );
    }

    #[test]
    fn verify_and_remove_cached_model_round_trip_against_real_files() {
        let _guard = lock_env_test();
        let root = std::env::temp_dir().join(format!(
            "stt-model-verify-test-{}-{}",
            std::process::id(),
            line!()
        ));
        // SAFETY: test-only env var mutation, scoped to this single test.
        unsafe {
            std::env::set_var(MODEL_CACHE_DIR_ENV_VAR, &root);
        }

        assert_eq!(verify_cached_model("some/model").unwrap(), None);
        remove_cached_model("some/model")
            .expect("removing an absent model is a no-op, not an error");

        let dir = cached_model_dir("some/model");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(MODEL_WEIGHTS_FILENAME), b"fake weights").unwrap();

        assert_eq!(
            verify_cached_model("some/model").unwrap(),
            Some(b"fake weights".len() as u64)
        );

        remove_cached_model("some/model").unwrap();
        assert_eq!(verify_cached_model("some/model").unwrap(), None);
        assert!(!dir.exists());

        unsafe {
            std::env::remove_var(MODEL_CACHE_DIR_ENV_VAR);
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn verify_cached_model_treats_an_empty_weights_file_as_not_verified() {
        let _guard = lock_env_test();
        let root = std::env::temp_dir().join(format!(
            "stt-model-verify-empty-test-{}-{}",
            std::process::id(),
            line!()
        ));
        // SAFETY: test-only env var mutation, scoped to this single test.
        unsafe {
            std::env::set_var(MODEL_CACHE_DIR_ENV_VAR, &root);
        }
        let dir = cached_model_dir("some/model");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(MODEL_WEIGHTS_FILENAME), b"").unwrap();

        assert_eq!(verify_cached_model("some/model").unwrap(), None);

        unsafe {
            std::env::remove_var(MODEL_CACHE_DIR_ENV_VAR);
        }
        std::fs::remove_dir_all(&root).ok();
    }
}
