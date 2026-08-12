//! Thin HTTP client for the subcommands that need to talk to an already
//! running `stt run` daemon (provider/runtime lifecycle, model selection).
//!
//! This exists because that state — which providers are registered as
//! installed, which are running, which model is selected — lives in the
//! daemon's own in-memory `RuntimeManager` and isn't shared across separate
//! CLI invocations any other way; a fresh `stt provider status` process has
//! no way to know what a previous `stt provider start` did except by asking
//! the daemon that's still holding that state. Read-only/local-only
//! subcommands (`hardware`, `provider list`, `model list`, `recommend`)
//! don't need this — they call `stt-runtime` directly.

use anyhow::{bail, Context};
use reqwest::{Method, StatusCode};
use serde_json::Value;

pub struct Client {
    base_url: String,
    http: reqwest::Client,
}

impl Client {
    pub fn new(base_url: String) -> Self {
        Self {
            base_url,
            http: reqwest::Client::new(),
        }
    }

    pub async fn get(&self, path: &str) -> anyhow::Result<Value> {
        self.request(Method::GET, path, None).await
    }

    pub async fn post(&self, path: &str, body: Option<Value>) -> anyhow::Result<Value> {
        self.request(Method::POST, path, body).await
    }

    pub async fn delete(&self, path: &str) -> anyhow::Result<Value> {
        self.request(Method::DELETE, path, None).await
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> anyhow::Result<Value> {
        let url = format!("{}{path}", self.base_url);
        let mut req = self.http.request(method, &url);
        if let Some(body) = body {
            req = req.json(&body);
        }

        let response = req.send().await.with_context(|| {
            format!("could not reach stt-server at {url} — is `stt run` running?")
        })?;

        let status = response.status();
        if status == StatusCode::NO_CONTENT {
            return Ok(Value::Null);
        }

        let body: Value = response
            .json()
            .await
            .unwrap_or(Value::String("<non-JSON response>".to_string()));

        if !status.is_success() {
            let message = body
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("request failed");
            let code = body.get("code").and_then(Value::as_str).unwrap_or("ERROR");
            bail!("[{code}] {message} ({status})");
        }

        Ok(body)
    }
}
