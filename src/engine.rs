use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::Serialize;
use transcribe_cpp::{backend_available, Backend, CancelToken, Model, ModelOptions};

use crate::capabilities::{EffectiveCaps, LoadedCaps};

#[derive(Clone, Serialize)]
pub struct BackendDiagnostic {
    pub observed_backend: String,
    pub fallback_reason: Option<String>,
}

pub struct LoadedModel {
    pub id: String,
    pub model: Model,
    pub diagnostic: BackendDiagnostic,
    /// Computed once at load time from the live `Model` (see
    /// `capabilities.rs`). Reused for every request against this model
    /// instead of recomputing per-request.
    pub caps: EffectiveCaps,
}

impl LoadedModel {
    pub fn new(id: String, model: Model, diagnostic: BackendDiagnostic) -> Self {
        let caps = EffectiveCaps::new(LoadedCaps::from_model(&model));
        LoadedModel {
            id,
            model,
            diagnostic,
            caps,
        }
    }
}

/// The live (loaded-model) capability view for `model`, the same computation
/// `LoadedModel::new` does internally. Exposed separately so callers that
/// load a model on demand (a per-request swap in `api::transcribe_or_translate`)
/// can cache it per model id (`App::live_caps`) for `GET /v1/models` to reuse
/// even after that model is no longer resident, without re-deriving it from a
/// `LoadedModel` they may not otherwise construct.
pub fn caps_for(model: &Model) -> EffectiveCaps {
    EffectiveCaps::new(LoadedCaps::from_model(model))
}

/// Reported by `/readiness` (and the transcription handlers) while a model
/// is loading in the background: see `spawn_tracked_load`. `started_at` is
/// process-local monotonic time, never serialized directly -- callers read
/// [`LoadingStatus::elapsed_ms`] instead.
#[derive(Clone)]
pub struct LoadingStatus {
    pub model_id: String,
    started_at: Instant,
}

impl LoadingStatus {
    pub fn new(model_id: String) -> Self {
        LoadingStatus {
            model_id,
            started_at: Instant::now(),
        }
    }

    pub fn elapsed_ms(&self) -> u64 {
        self.started_at.elapsed().as_millis() as u64
    }
}

/// Load `loader` on a detached OS thread (not `tokio::spawn_blocking`, so
/// this works from a plain synchronous caller with no tokio runtime, such as
/// `App::open_app_at_full` when it is exercised directly by a non-`tokio`
/// `#[test]`) and swap the result into `slot` when it finishes. `status` is
/// set to `Some(LoadingStatus::new(model_id))` before the thread starts and
/// unconditionally cleared back to `None` when it finishes -- on success
/// *and* on failure -- so a concurrent reader (`/readiness`, a transcription
/// request) can report "still loading" for exactly the load's real duration.
/// Never blocks the caller: this is the mechanism that lets the HTTP server
/// and `server.json` become available immediately at startup even when the
/// selected model takes minutes to load (see the "instant startup, model
/// loads in background" fix).
///
/// On a failed load, `slot` is left untouched (`None`) and the error is
/// logged; there is no channel back to the caller because startup has
/// already moved on by the time this could complete.
pub fn spawn_tracked_load<T, E>(
    status: Arc<Mutex<Option<LoadingStatus>>>,
    slot: Arc<Mutex<Option<T>>>,
    model_id: String,
    loader: impl FnOnce() -> Result<T, E> + Send + 'static,
) where
    T: Send + 'static,
    E: Send + std::fmt::Display + 'static,
{
    *status.lock().expect("loading status poisoned") = Some(LoadingStatus::new(model_id));
    let status_for_thread = status.clone();
    let spawned = std::thread::Builder::new()
        .name("model-startup-load".to_owned())
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            match loader() {
                Ok(value) => *slot.lock().expect("swap slot poisoned") = Some(value),
                Err(error) => eprintln!("selected model could not load: {error}"),
            }
            *status_for_thread.lock().expect("loading status poisoned") = None;
        });
    if spawned.is_err() {
        eprintln!("could not spawn model-startup-load thread; model stays unloaded");
        *status.lock().expect("loading status poisoned") = None;
    }
}

pub struct CancelWhenDropped(pub CancelToken);

impl Drop for CancelWhenDropped {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// Load a new value with `loader` on a blocking thread, then swap it into
/// `slot` under a brief lock. `slot` holds its old value (still readable and
/// clonable by anyone who locked it before this call) for the whole load,
/// and is only ever locked for the instant of the swap. On a failed load,
/// `slot` is left untouched and the error is returned. This is the generic
/// shape behind `select_model`'s non-blocking model switch, factored out so
/// it can be unit tested with a fake loader instead of a real `Model`.
pub async fn load_and_swap<T, E>(
    slot: &std::sync::Mutex<Option<T>>,
    loader: impl FnOnce() -> Result<T, E> + Send + 'static,
) -> Result<(), E>
where
    T: Send + 'static,
    E: Send + 'static,
{
    let new_value = tokio::task::spawn_blocking(loader)
        .await
        .expect("loader task panicked")?;
    *slot.lock().expect("swap slot poisoned") = Some(new_value);
    Ok(())
}

pub fn load_engine(path: &Path, preference: &str) -> Result<(Model, BackendDiagnostic), String> {
    if preference != "cpu" && backend_available(Backend::Vulkan) {
        match Model::load_with(
            path,
            &ModelOptions {
                backend: Backend::Vulkan,
                ..Default::default()
            },
        ) {
            Ok(model) => {
                let observed_backend = model.backend().to_string();
                return Ok((
                    model,
                    BackendDiagnostic {
                        observed_backend,
                        fallback_reason: None,
                    },
                ));
            }
            Err(vulkan_error) => {
                let model = Model::load_with(
                    path,
                    &ModelOptions {
                        backend: Backend::Cpu,
                        ..Default::default()
                    },
                )
                .map_err(|cpu_error| format!("Vulkan: {vulkan_error}; CPU: {cpu_error}"))?;
                return Ok((
                    model,
                    BackendDiagnostic {
                        observed_backend: "CPU".to_owned(),
                        fallback_reason: Some(vulkan_error.to_string()),
                    },
                ));
            }
        }
    }
    let model = Model::load_with(
        path,
        &ModelOptions {
            backend: Backend::Cpu,
            ..Default::default()
        },
    )
    .map_err(|error| error.to_string())?;
    Ok((
        model,
        BackendDiagnostic {
            observed_backend: "CPU".to_owned(),
            fallback_reason: if preference == "cpu" {
                None
            } else {
                Some("Vulkan backend unavailable".to_owned())
            },
        },
    ))
}

#[cfg(test)]
mod swap_tests {
    use super::*;
    use std::sync::Mutex;

    #[tokio::test]
    async fn old_value_is_served_until_swap_completes() {
        let slot: Mutex<Option<i32>> = Mutex::new(Some(1));
        // Snapshot the old value the way a request would, before the swap.
        let old = *slot.lock().unwrap().as_ref().unwrap();
        assert_eq!(old, 1);
        load_and_swap::<i32, String>(&slot, || Ok(2)).await.unwrap();
        assert_eq!(*slot.lock().unwrap(), Some(2));
    }

    #[tokio::test]
    async fn failed_load_keeps_the_old_selection() {
        let slot: Mutex<Option<i32>> = Mutex::new(Some(1));
        let result = load_and_swap::<i32, String>(&slot, || Err("boom".to_owned())).await;
        assert_eq!(result, Err("boom".to_owned()));
        assert_eq!(*slot.lock().unwrap(), Some(1));
    }
}

/// `spawn_tracked_load` is the mechanism behind "instant startup, model
/// loads in background": these tests simulate a slow model load with a fake
/// loader (a sleeping closure, no real `Model`) the same way `swap_tests`
/// above simulates a slow/failing load with `i32`, and are the seam the
/// server-level "server.json present and /health ok while a slow load is in
/// progress" tests build on.
#[cfg(test)]
mod tracked_load_tests {
    use super::*;
    use std::time::Duration;

    fn wait_until<F: Fn() -> bool>(condition: F, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while !condition() {
            assert!(Instant::now() < deadline, "condition never became true");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn status_is_visible_while_loading_and_clears_on_success() {
        let status: Arc<Mutex<Option<LoadingStatus>>> = Arc::new(Mutex::new(None));
        let slot: Arc<Mutex<Option<i32>>> = Arc::new(Mutex::new(None));
        spawn_tracked_load(
            status.clone(),
            slot.clone(),
            "slow-model".to_owned(),
            || {
                std::thread::sleep(Duration::from_millis(100));
                Ok::<i32, String>(42)
            },
        );
        // The status must be observable immediately, before the loader
        // thread has had a chance to finish -- this is exactly what lets
        // `/readiness` report "loading" instead of blocking or 404ing.
        {
            let guard = status.lock().unwrap();
            let observed = guard.as_ref().expect("status must be Some while loading");
            assert_eq!(observed.model_id, "slow-model");
        }
        assert!(slot.lock().unwrap().is_none());

        wait_until(|| slot.lock().unwrap().is_some(), Duration::from_secs(5));
        assert_eq!(*slot.lock().unwrap(), Some(42));
        wait_until(|| status.lock().unwrap().is_none(), Duration::from_secs(5));
    }

    #[test]
    fn status_clears_and_slot_stays_empty_on_failure() {
        let status: Arc<Mutex<Option<LoadingStatus>>> = Arc::new(Mutex::new(None));
        let slot: Arc<Mutex<Option<i32>>> = Arc::new(Mutex::new(None));
        spawn_tracked_load(status.clone(), slot.clone(), "bad-model".to_owned(), || {
            std::thread::sleep(Duration::from_millis(30));
            Err::<i32, String>("boom".to_owned())
        });
        assert!(status.lock().unwrap().is_some());
        wait_until(|| status.lock().unwrap().is_none(), Duration::from_secs(5));
        assert!(slot.lock().unwrap().is_none());
    }

    #[test]
    fn elapsed_ms_increases_while_loading() {
        let status = LoadingStatus::new("m".to_owned());
        std::thread::sleep(Duration::from_millis(20));
        assert!(status.elapsed_ms() >= 15);
    }
}
