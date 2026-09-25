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
