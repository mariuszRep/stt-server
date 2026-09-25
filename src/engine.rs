use std::path::Path;

use serde::Serialize;
use transcribe_cpp::{backend_available, Backend, CancelToken, Model, ModelOptions};

#[derive(Clone, Serialize)]
pub struct BackendDiagnostic {
    pub observed_backend: String,
    pub fallback_reason: Option<String>,
}

pub struct LoadedModel {
    pub id: String,
    pub model: Model,
    pub diagnostic: BackendDiagnostic,
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
