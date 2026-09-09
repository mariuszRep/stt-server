//! General, engine-agnostic Local Provider Protocol conformance checks,
//! parameterized over `catalog::CATALOG` / `providers::registry()`. Shared
//! by `crates/runtime/tests/protocol_conformance.rs` (asserts these results
//! in CI/`cargo test`) and `stt verify` (prints them as a table on real
//! hardware) so the two never drift into checking different things.
//!
//! Never triggers a fresh download as a side effect: a provider is skipped
//! (visibly, via [`ConformanceOutcome::Skipped`], never silently) unless
//! its runtime is already locally installable *and* its catalog default
//! model is already verified-present on disk.

use std::sync::Arc;

use crate::catalog::{self, CatalogEntry};
use crate::manager::StartOptions;
use crate::{ProviderId, RuntimeManager};

#[derive(Debug)]
pub struct CheckResult {
    pub name: &'static str,
    pub passed: bool,
    pub detail: String,
}

pub enum ConformanceOutcome {
    Skipped { reason: String },
    Ran { checks: Vec<CheckResult> },
}

impl ConformanceOutcome {
    pub fn all_passed(&self) -> bool {
        match self {
            ConformanceOutcome::Skipped { .. } => true,
            ConformanceOutcome::Ran { checks } => checks.iter().all(|c| c.passed),
        }
    }
}

fn ok(name: &'static str, detail: impl Into<String>) -> CheckResult {
    CheckResult {
        name,
        passed: true,
        detail: detail.into(),
    }
}

fn fail(name: &'static str, detail: impl Into<String>) -> CheckResult {
    CheckResult {
        name,
        passed: false,
        detail: detail.into(),
    }
}

/// Runs every conformance check for one catalog entry against a freshly
/// registered `RuntimeManager`, using `fixture_wav` as the known-speech
/// clip. Starts and stops the provider itself; the caller doesn't need a
/// pre-existing running instance.
pub async fn check_provider(
    entry: &'static CatalogEntry,
    fixture_wav: &[u8],
) -> ConformanceOutcome {
    let manager = Arc::new(RuntimeManager::new(None));
    manager.register_local_installs().await;

    let id = match ProviderId::new(entry.id) {
        Ok(id) => id,
        Err(e) => {
            return ConformanceOutcome::Skipped {
                reason: format!("invalid provider id: {e}"),
            }
        }
    };

    if !manager.is_installed(&id).await {
        return ConformanceOutcome::Skipped {
            reason: "runtime not locally installed in this environment".to_string(),
        };
    }

    match manager.verify_model(&id, entry.default_model) {
        Ok(Some(_)) => {}
        _ => {
            return ConformanceOutcome::Skipped {
                reason: format!(
                    "default model '{}' not already cached in this environment",
                    entry.default_model
                ),
            }
        }
    }

    if let Err(e) = manager.select_model(&id, entry.default_model).await {
        return ConformanceOutcome::Skipped {
            reason: format!("select_model failed: {e}"),
        };
    }

    let mut checks = Vec::new();

    let descriptor = match manager.start(&id, &StartOptions::default()).await {
        Ok(d) => d,
        Err(e) => {
            checks.push(fail("start", e.to_string()));
            return ConformanceOutcome::Ran { checks };
        }
    };

    checks.push(if descriptor.schema_version == 1 {
        ok("descriptor.schemaVersion", "1")
    } else {
        fail(
            "descriptor.schemaVersion",
            format!("expected 1, got {}", descriptor.schema_version),
        )
    });
    checks.push(if descriptor.protocol == "voice-typer-v1" {
        ok("descriptor.protocol", &descriptor.protocol)
    } else {
        fail("descriptor.protocol", &descriptor.protocol)
    });
    checks.push(if !descriptor.base_url.is_empty() {
        ok("descriptor.baseUrl", &descriptor.base_url)
    } else {
        fail("descriptor.baseUrl", "empty")
    });

    let client = reqwest::Client::new();
    let bearer = descriptor
        .auth
        .as_ref()
        .map(|a| a.value.clone())
        .unwrap_or_default();

    match client
        .get(format!("{}/health", descriptor.base_url))
        .bearer_auth(&bearer)
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            checks.push(ok("GET /health", resp.status().to_string()))
        }
        Ok(resp) => checks.push(fail("GET /health", resp.status().to_string())),
        Err(e) => checks.push(fail("GET /health", e.to_string())),
    }

    match client
        .get(format!("{}/v1/config", descriptor.base_url))
        .bearer_auth(&bearer)
        .send()
        .await
    {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(v) if v.get("model").is_some() => checks.push(ok("GET /v1/config", "has 'model'")),
            Ok(_) => checks.push(fail("GET /v1/config", "missing 'model' field")),
            Err(e) => checks.push(fail("GET /v1/config", e.to_string())),
        },
        Err(e) => checks.push(fail("GET /v1/config", e.to_string())),
    }

    match client
        .post(format!("{}/v1/audio/transcriptions", descriptor.base_url))
        .multipart(fixture_form(fixture_wav))
        .send()
        .await
    {
        Ok(resp) if resp.status() == reqwest::StatusCode::UNAUTHORIZED => {
            checks.push(ok("auth enforcement", "401 without token"));
        }
        Ok(resp) => checks.push(fail(
            "auth enforcement",
            format!("expected 401, got {} (token not enforced)", resp.status()),
        )),
        Err(e) => checks.push(fail("auth enforcement", e.to_string())),
    }

    match client
        .post(format!("{}/v1/audio/transcriptions", descriptor.base_url))
        .bearer_auth(&bearer)
        .multipart(fixture_form(fixture_wav))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => match resp.json::<serde_json::Value>().await {
            Ok(body) => {
                let text = body.get("text").and_then(|v| v.as_str()).unwrap_or("");
                if text.trim().is_empty() {
                    checks.push(fail("transcribe (no model field)", "'text' was empty"));
                } else {
                    checks.push(ok(
                        "transcribe (no model field)",
                        format!("{}...", &text.chars().take(40).collect::<String>()),
                    ));
                }
            }
            Err(e) => checks.push(fail("transcribe (no model field)", e.to_string())),
        },
        Ok(resp) => checks.push(fail(
            "transcribe (no model field)",
            resp.status().to_string(),
        )),
        Err(e) => checks.push(fail("transcribe (no model field)", e.to_string())),
    }

    let _ = manager.stop(&id).await;

    ConformanceOutcome::Ran { checks }
}

fn fixture_form(bytes: &[u8]) -> reqwest::multipart::Form {
    reqwest::multipart::Form::new().part(
        "file",
        reqwest::multipart::Part::bytes(bytes.to_vec()).file_name("sample.wav"),
    )
}

/// The shared conformance suite's own known-speech fixture. Owned by
/// `stt-runtime` (not `stt-cli`) so both the library-level suite and any
/// CLI/test caller use the identical clip. Provenance/license documented in
/// `tests/fixtures/README.md`; this is the same file, just re-embedded here
/// so `stt verify` doesn't need to know about the test tree layout.
pub const FIXTURE_WAV: &[u8] = include_bytes!("../tests/fixtures/sample.wav");

pub async fn check_all() -> Vec<(&'static str, ConformanceOutcome)> {
    let mut results = Vec::new();
    for entry in catalog::CATALOG {
        let outcome = check_provider(entry, FIXTURE_WAV).await;
        results.push((entry.id, outcome));
    }
    results
}
