//! Top-level coordination: hardware, catalog, the running-instance table,
//! and idle lifecycle — exposed as one handle the control-plane routes and
//! CLI share.
//!
//! Runtimes are never started proactively: `start()` only runs a provider
//! when a caller asks for it, and an idle-timeout sweep (`sweep_idle`) stops
//! ones nobody has touched recently, so a provider that isn't in active use
//! doesn't sit around holding memory/GPU/CPU for no reason.

use std::collections::HashMap;
use std::time::Duration;

use tokio::sync::Mutex;
use tokio::time::Instant;
use tracing::info;
use uuid::Uuid;

use stt_common::{DescriptorAuth, RuntimeConnectionDescriptor, RUNTIME_DESCRIPTOR_SCHEMA_VERSION};

use crate::catalog::{self, CatalogEntry, ProviderId, ProviderInfo};
use crate::error::RuntimeError;
use crate::hardware::{self, HardwareReport};
use crate::supervisor::{self, ManagedInstance, RuntimeStatus, SpawnSpec};

/// What to run for a provider: program, args, env, and an optional working
/// directory (e.g. a vendored runtime's source root, so its relative
/// imports resolve).
pub struct Launch {
    pub program: std::path::PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<std::path::PathBuf>,
}

/// Builds a [`Launch`] for a provider given the port it must bind, the auth
/// token it must accept, and the currently selected model id (`None` means
/// "use the provider's own default"). Registered per-provider via
/// `register_install` once that provider's artifact is actually available
/// locally (see `crates/runtime/src/providers`); `RuntimeManager` itself
/// stays ignorant of any one provider's launch details.
pub type LaunchBuilder = Box<dyn Fn(u16, &str, Option<&str>) -> Launch + Send + Sync>;

struct RunningEntry {
    instance: ManagedInstance,
    last_activity: Instant,
}

pub struct RuntimeManager {
    hardware: HardwareReport,
    instances: Mutex<HashMap<String, RunningEntry>>,
    installed: Mutex<HashMap<String, LaunchBuilder>>,
    selected_models: Mutex<HashMap<String, String>>,
    /// `None` disables idle auto-stop entirely (always-on).
    idle_timeout: Option<Duration>,
}

impl RuntimeManager {
    pub fn new(idle_timeout: Option<Duration>) -> Self {
        Self {
            hardware: hardware::detect(),
            instances: Mutex::new(HashMap::new()),
            installed: Mutex::new(HashMap::new()),
            selected_models: Mutex::new(HashMap::new()),
            idle_timeout,
        }
    }

    pub fn hardware(&self) -> &HardwareReport {
        &self.hardware
    }

    pub fn list_providers(&self) -> Vec<ProviderInfo> {
        catalog::list_providers(&self.hardware)
    }

    /// Register how to launch a provider once its artifact is available
    /// locally. Overwrites any previous registration for the same id.
    pub async fn register_install(&self, id: &ProviderId, launch: LaunchBuilder) {
        self.installed
            .lock()
            .await
            .insert(id.as_str().to_string(), launch);
    }

    pub async fn is_installed(&self, id: &ProviderId) -> bool {
        self.installed.lock().await.contains_key(id.as_str())
    }

    /// Remove a provider's install registration, stopping it first if running.
    pub async fn uninstall(&self, id: &ProviderId) -> Result<(), RuntimeError> {
        let _ = self.stop(id).await; // fine if it wasn't running
        let mut installed = self.installed.lock().await;
        if installed.remove(id.as_str()).is_none() {
            return Err(RuntimeError::ProviderNotInstalled(id.to_string()));
        }
        Ok(())
    }

    /// Select which curated model a provider should load on its next
    /// `start()`. Does not affect an already-running instance — restart to
    /// pick up the change, consistent with "installation/selection is
    /// explicit and observable," not silently hot-swapped underneath a
    /// caller mid-session.
    pub async fn select_model(&self, id: &ProviderId, model_id: &str) -> Result<(), RuntimeError> {
        let entry = catalog::find_provider(id)?;
        catalog::find_model(entry, model_id)
            .ok_or_else(|| RuntimeError::ModelNotFound(model_id.to_string()))?;
        self.selected_models
            .lock()
            .await
            .insert(id.as_str().to_string(), model_id.to_string());
        Ok(())
    }

    pub async fn selected_model(&self, id: &ProviderId) -> Option<String> {
        self.selected_models.lock().await.get(id.as_str()).cloned()
    }

    /// Start (or return the descriptor of an already-running instance of) a
    /// managed provider. Blocks until the runtime reports healthy.
    pub async fn start(
        &self,
        id: &ProviderId,
    ) -> Result<RuntimeConnectionDescriptor, RuntimeError> {
        let entry = catalog::find_provider(id)?;

        {
            let mut instances = self.instances.lock().await;
            if let Some(existing) = instances.get_mut(id.as_str()) {
                if existing.instance.status() == RuntimeStatus::Running {
                    existing.last_activity = Instant::now();
                    return Ok(descriptor_for(entry, &existing.instance));
                }
                // Not running (stopped/crashed): fall through and replace it.
                instances.remove(id.as_str());
            }
        }

        let port = supervisor::allocate_loopback_port()?;
        let auth_token = Uuid::new_v4().to_string();
        let selected_model = self.selected_model(id).await;
        let launch = {
            let installed = self.installed.lock().await;
            let builder = installed
                .get(id.as_str())
                .ok_or_else(|| RuntimeError::ProviderNotInstalled(id.to_string()))?;
            builder(port, &auth_token, selected_model.as_deref())
        };

        let instance = supervisor::spawn(
            SpawnSpec {
                program: launch.program,
                args: launch.args,
                env: launch.env,
                cwd: launch.cwd,
                health_path: entry.health_path.to_string(),
                auth_token,
            },
            port,
        )
        .await?;

        let descriptor = descriptor_for(entry, &instance);

        let mut instances = self.instances.lock().await;
        instances.insert(
            id.as_str().to_string(),
            RunningEntry {
                instance,
                last_activity: Instant::now(),
            },
        );
        Ok(descriptor)
    }

    pub async fn stop(&self, id: &ProviderId) -> Result<(), RuntimeError> {
        let mut instances = self.instances.lock().await;
        match instances.remove(id.as_str()) {
            Some(mut entry) => entry.instance.stop().await,
            None => Err(RuntimeError::RuntimeNotRunning(id.to_string())),
        }
    }

    pub async fn status(&self, id: &ProviderId) -> RuntimeStatus {
        let mut instances = self.instances.lock().await;
        instances
            .get_mut(id.as_str())
            .map(|e| e.instance.status())
            .unwrap_or(RuntimeStatus::Stopped)
    }

    pub async fn logs(&self, id: &ProviderId, tail: usize) -> Vec<String> {
        let instances = self.instances.lock().await;
        instances
            .get(id.as_str())
            .map(|e| e.instance.logs(tail))
            .unwrap_or_default()
    }

    /// Re-fetch the descriptor for an already-running instance without
    /// restarting it. Counts as activity, same as `start`.
    pub async fn descriptor(
        &self,
        id: &ProviderId,
    ) -> Result<RuntimeConnectionDescriptor, RuntimeError> {
        let entry = catalog::find_provider(id)?;
        let mut instances = self.instances.lock().await;
        let running = instances
            .get_mut(id.as_str())
            .ok_or_else(|| RuntimeError::RuntimeNotRunning(id.to_string()))?;
        if running.instance.status() != RuntimeStatus::Running {
            return Err(RuntimeError::RuntimeNotRunning(id.to_string()));
        }
        running.last_activity = Instant::now();
        Ok(descriptor_for(entry, &running.instance))
    }

    /// Record that a provider is actively being used, resetting its idle
    /// clock. Intended for a lightweight heartbeat a client sends while a
    /// session is ongoing; `start`/`descriptor` already imply activity on
    /// their own, so this only matters for long-lived sessions in between.
    pub async fn touch(&self, id: &ProviderId) -> Result<(), RuntimeError> {
        let mut instances = self.instances.lock().await;
        let running = instances
            .get_mut(id.as_str())
            .ok_or_else(|| RuntimeError::RuntimeNotRunning(id.to_string()))?;
        running.last_activity = Instant::now();
        Ok(())
    }

    /// Stop any running instance that hasn't been started, queried, or
    /// heartbeated within the configured idle timeout. Returns the ids of
    /// providers that were stopped, for logging by the caller's sweep loop.
    /// A no-op (returns empty) if idle auto-stop is disabled.
    pub async fn sweep_idle(&self) -> Vec<String> {
        let Some(idle_timeout) = self.idle_timeout else {
            return Vec::new();
        };
        let now = Instant::now();

        let expired: Vec<(String, ManagedInstance)> = {
            let mut instances = self.instances.lock().await;
            let expired_ids: Vec<String> = instances
                .iter()
                .filter(|(_, entry)| now.duration_since(entry.last_activity) >= idle_timeout)
                .map(|(id, _)| id.clone())
                .collect();
            expired_ids
                .into_iter()
                .filter_map(|id| instances.remove(&id).map(|entry| (id, entry.instance)))
                .collect()
        };

        let mut stopped = Vec::new();
        for (id, mut instance) in expired {
            info!(provider = %id, "stopping idle managed runtime (no activity within idle timeout)");
            let _ = instance.stop().await;
            stopped.push(id);
        }
        stopped
    }
}

fn descriptor_for(entry: &CatalogEntry, instance: &ManagedInstance) -> RuntimeConnectionDescriptor {
    RuntimeConnectionDescriptor {
        schema_version: RUNTIME_DESCRIPTOR_SCHEMA_VERSION,
        provider: entry.id.to_string(),
        protocol: entry.protocol.to_string(),
        transport: entry.transport.to_string(),
        base_url: format!("http://127.0.0.1:{}", instance.port),
        // Populated once the real faster-whisper runtime's `GET /v1/config`
        // streaming block is proxied through; a running instance without it
        // is still a valid descriptor for batch-only use.
        streaming: None,
        auth: Some(DescriptorAuth {
            auth_type: "token".to_string(),
            value: instance.auth_token.clone(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `faster-whisper` catalog entry's health path is `/health`, so the
    /// fake runtime needs an actual file there for `http.server` to serve
    /// (unlike supervisor.rs's tests, which use `/` directly).
    fn fake_launch() -> LaunchBuilder {
        Box::new(|port, _auth_token, _selected_model| {
            let dir = std::env::temp_dir().join(format!("stt-runtime-test-{port}"));
            std::fs::create_dir_all(&dir).expect("create fake runtime temp dir");
            std::fs::write(dir.join("health"), b"ok").expect("write fake health file");
            Launch {
                program: "python3".into(),
                args: vec![
                    "-m".into(),
                    "http.server".into(),
                    port.to_string(),
                    "--bind".into(),
                    "127.0.0.1".into(),
                    "--directory".into(),
                    dir.to_string_lossy().into_owned(),
                ],
                env: vec![],
                cwd: None,
            }
        })
    }

    async fn manager_with_faster_whisper_installed(
        idle_timeout: Option<Duration>,
    ) -> RuntimeManager {
        let manager = RuntimeManager::new(idle_timeout);
        let id = ProviderId::new("faster-whisper").unwrap();
        manager.register_install(&id, fake_launch()).await;
        manager
    }

    #[tokio::test]
    async fn start_stop_and_status_lifecycle() {
        let manager = manager_with_faster_whisper_installed(None).await;
        let id = ProviderId::new("faster-whisper").unwrap();

        assert_eq!(manager.status(&id).await, RuntimeStatus::Stopped);

        let descriptor = manager.start(&id).await.unwrap();
        assert_eq!(descriptor.provider, "faster-whisper");
        assert_eq!(descriptor.protocol, "voice-typer-v1");
        assert!(descriptor.base_url.starts_with("http://127.0.0.1:"));

        assert_eq!(manager.status(&id).await, RuntimeStatus::Running);

        let refetched = manager.descriptor(&id).await.unwrap();
        assert_eq!(refetched.base_url, descriptor.base_url);

        manager.stop(&id).await.unwrap();
        assert_eq!(manager.status(&id).await, RuntimeStatus::Stopped);
    }

    #[tokio::test]
    async fn start_is_idempotent_while_already_running() {
        let manager = manager_with_faster_whisper_installed(None).await;
        let id = ProviderId::new("faster-whisper").unwrap();

        let first = manager.start(&id).await.unwrap();
        let second = manager.start(&id).await.unwrap();

        assert_eq!(
            first.base_url, second.base_url,
            "second start should reuse the running instance, not spawn a new one"
        );

        manager.stop(&id).await.unwrap();
    }

    #[tokio::test]
    async fn stop_without_start_is_an_error() {
        let manager = manager_with_faster_whisper_installed(None).await;
        let id = ProviderId::new("faster-whisper").unwrap();
        assert!(matches!(
            manager.stop(&id).await,
            Err(RuntimeError::RuntimeNotRunning(_))
        ));
    }

    #[tokio::test]
    async fn start_rejects_unknown_provider() {
        let manager = manager_with_faster_whisper_installed(None).await;
        let id = ProviderId::new("does-not-exist").unwrap();
        assert!(matches!(
            manager.start(&id).await,
            Err(RuntimeError::ProviderNotFound(_))
        ));
    }

    #[tokio::test]
    async fn start_rejects_uninstalled_provider() {
        let manager = RuntimeManager::new(None);
        let id = ProviderId::new("faster-whisper").unwrap();
        assert!(matches!(
            manager.start(&id).await,
            Err(RuntimeError::ProviderNotInstalled(_))
        ));
    }

    #[tokio::test]
    async fn sweep_idle_stops_instances_past_the_timeout_and_leaves_fresh_ones() {
        let manager = manager_with_faster_whisper_installed(Some(Duration::from_millis(50))).await;
        let id = ProviderId::new("faster-whisper").unwrap();

        manager.start(&id).await.unwrap();
        assert_eq!(manager.status(&id).await, RuntimeStatus::Running);

        tokio::time::sleep(Duration::from_millis(150)).await;

        let stopped = manager.sweep_idle().await;
        assert_eq!(stopped, vec!["faster-whisper".to_string()]);
        assert_eq!(manager.status(&id).await, RuntimeStatus::Stopped);
    }

    #[tokio::test]
    async fn touch_resets_the_idle_clock() {
        let manager = manager_with_faster_whisper_installed(Some(Duration::from_millis(150))).await;
        let id = ProviderId::new("faster-whisper").unwrap();

        manager.start(&id).await.unwrap();

        // Touch partway through the window so the instance survives past
        // the original deadline.
        tokio::time::sleep(Duration::from_millis(100)).await;
        manager.touch(&id).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        let stopped = manager.sweep_idle().await;
        assert!(
            stopped.is_empty(),
            "touch() should have reset the idle clock, but the instance was swept: {stopped:?}"
        );
        assert_eq!(manager.status(&id).await, RuntimeStatus::Running);

        manager.stop(&id).await.unwrap();
    }

    #[tokio::test]
    async fn select_model_validates_against_the_provider_catalog() {
        let manager = manager_with_faster_whisper_installed(None).await;
        let id = ProviderId::new("faster-whisper").unwrap();

        assert_eq!(manager.selected_model(&id).await, None);

        assert!(matches!(
            manager.select_model(&id, "not-a-real-model").await,
            Err(RuntimeError::ModelNotFound(_))
        ));

        manager
            .select_model(&id, "Systran/faster-whisper-tiny")
            .await
            .unwrap();
        assert_eq!(
            manager.selected_model(&id).await,
            Some("Systran/faster-whisper-tiny".to_string())
        );
    }

    #[tokio::test]
    async fn sweep_idle_disabled_never_stops_anything() {
        let manager = manager_with_faster_whisper_installed(None).await; // idle timeout disabled
        let id = ProviderId::new("faster-whisper").unwrap();

        manager.start(&id).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert!(manager.sweep_idle().await.is_empty());
        assert_eq!(manager.status(&id).await, RuntimeStatus::Running);

        manager.stop(&id).await.unwrap();
    }
}
