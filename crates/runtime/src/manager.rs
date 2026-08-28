//! Top-level coordination: hardware, catalog, the running-instance table,
//! and idle lifecycle — exposed as one handle the control-plane routes and
//! CLI share.
//!
//! Runtimes are never started proactively: `start()` only runs a provider
//! when a caller asks for it, and an idle-timeout sweep (`sweep_idle`) stops
//! ones nobody has touched recently, so a provider that isn't in active use
//! doesn't sit around holding memory/GPU/CPU for no reason.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use tokio::sync::Mutex;
use tokio::time::Instant;
use tracing::{info, warn};
use uuid::Uuid;

use stt_common::{
    DescriptorAuth, RuntimeConnectionDescriptor, StreamingCapability,
    RUNTIME_DESCRIPTOR_SCHEMA_VERSION,
};

use crate::catalog::{self, CatalogEntry, ProviderId, ProviderInfo, RuntimeVariant};
use crate::error::RuntimeError;
use crate::hardware::{self, HardwareReport};
use crate::providers::faster_whisper;
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

/// Per-call start hints layered on top of the persistently-selected model:
/// which device/compute type to request, and (rarely) which host to bind.
/// `None` for `device`/`compute_type` means "let the runtime auto-detect,"
/// same as leaving the corresponding env var unset today.
///
/// `bind_host`: `None` (the default) binds loopback, same as always.
/// `Some("0.0.0.0")` is the only other accepted value — deliberately
/// wildcard-only, not an arbitrary interface IP, since picking "the" LAN
/// interface to bind is genuinely ambiguous on a multi-homed machine and
/// not this layer's job to guess. Requires a non-empty `auth_token` in the
/// same call (checked in `RuntimeManager::start`) — CONVENTIONS.md:
/// "Loopback is default; remote binding is explicit and authenticated."
///
/// `auth_token`: caller-supplied override for the per-instance auth token
/// (otherwise a fresh UUID is generated). Mandatory alongside a non-loopback
/// `bind_host`; optional and just a convenience otherwise.
#[derive(Debug, Clone, Default)]
pub struct StartOptions {
    pub device: Option<String>,
    pub compute_type: Option<String>,
    pub bind_host: Option<String>,
    pub auth_token: Option<String>,
}

/// Builds a [`Launch`] for a provider given the port it must bind, the auth
/// token it must accept, the currently selected model id (`None` means "use
/// the provider's own default"), and per-call start options. Registered
/// per-provider via `register_install` once that provider's artifact is
/// actually available locally (see `crates/runtime/src/providers`);
/// `RuntimeManager` itself stays ignorant of any one provider's launch
/// details.
pub type LaunchBuilder =
    Box<dyn Fn(u16, &str, Option<&str>, &StartOptions) -> Launch + Send + Sync>;

struct RunningEntry {
    instance: ManagedInstance,
    last_activity: Instant,
    streaming: Option<StreamingCapability>,
}

/// Status of an in-flight or finished variant download, tracked by
/// [`RuntimeManager::begin_install`]/polled via
/// [`RuntimeManager::install_operation`].
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallOperationStatus {
    Downloading,
    Complete,
    Failed,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallOperationState {
    pub operation_id: String,
    pub provider_id: String,
    /// `Some` for a provider-variant install, `None` for a model pull —
    /// exactly one of `variant`/`model_id` is ever set, distinguishing which
    /// kind of download this operation is without a separate `kind` enum.
    pub variant: Option<String>,
    pub model_id: Option<String>,
    pub status: InstallOperationStatus,
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    pub error: Option<String>,
}

/// Result of [`RuntimeManager::begin_model_pull`]: either the model's
/// weights were already present on disk (instant, no network — the common
/// case for a model pulled in an earlier session), or a download was kicked
/// off in the background, pollable via the same `/v1/install-operations/:id`
/// mechanism [`RuntimeManager::begin_install`] uses.
#[derive(Debug, Clone)]
pub enum ModelPullOutcome {
    Cached,
    Downloading { operation_id: String },
}

/// Result of [`RuntimeManager::begin_install`]: either the requested
/// variant was already available locally (instant, no network — the
/// common case), or a download was kicked off in the background.
#[derive(Debug, Clone)]
pub enum InstallOutcome {
    Installed {
        provider_id: String,
        variant: String,
    },
    Downloading {
        operation_id: String,
        variant: String,
    },
}

/// Oldest finished (`Complete`/`Failed`) operations are evicted once the
/// table exceeds this many entries, so a long-running daemon's install
/// history doesn't grow unbounded. In-flight `Downloading` entries are
/// never evicted.
const MAX_FINISHED_INSTALL_OPERATIONS: usize = 8;

/// A provider's registered launch spec plus the variant it was resolved
/// for. Tracked so `start()` can validate a requested `device` is actually
/// achievable by what's currently registered — impossible before, since
/// only the `LaunchBuilder` itself was kept.
struct InstalledProvider {
    variant: RuntimeVariant,
    launch: LaunchBuilder,
}

pub struct RuntimeManager {
    hardware: HardwareReport,
    instances: Mutex<HashMap<String, RunningEntry>>,
    installed: Mutex<HashMap<String, InstalledProvider>>,
    selected_models: Mutex<HashMap<String, String>>,
    /// Synchronous (not `tokio::sync::Mutex`) because it's also updated
    /// from a plain, non-async progress callback passed into
    /// `faster_whisper::download_variant` — see `begin_install`. Critical
    /// sections here are always tiny (a struct field write), never held
    /// across an `.await`.
    installs: StdMutex<HashMap<String, InstallOperationState>>,
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
            installs: StdMutex::new(HashMap::new()),
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
    pub async fn register_install(
        &self,
        id: &ProviderId,
        variant: RuntimeVariant,
        launch: LaunchBuilder,
    ) {
        self.installed.lock().await.insert(
            id.as_str().to_string(),
            InstalledProvider { variant, launch },
        );
    }

    pub async fn is_installed(&self, id: &ProviderId) -> bool {
        self.installed.lock().await.contains_key(id.as_str())
    }

    /// Install `variant` of a provider: instant/local if already present
    /// (a vendored dev copy, or a previously-downloaded copy of exactly
    /// this variant), otherwise kicks off a background download and
    /// returns immediately with an operation id to poll. Only one provider
    /// exists in the catalog today (`faster-whisper`); this dispatches on
    /// id via a `match` rather than a plugin trait, same rationale as
    /// `crates/server/src/routes/providers.rs`'s pre-existing per-provider
    /// dispatch — not worth an abstraction for a single entry.
    pub async fn begin_install(
        self: &Arc<Self>,
        id: &ProviderId,
        variant: RuntimeVariant,
    ) -> Result<InstallOutcome, RuntimeError> {
        catalog::find_provider(id)?;
        if id.as_str() != "faster-whisper" {
            return Err(RuntimeError::ProviderNotFound(id.to_string()));
        }

        if let Some(launch) = faster_whisper::install_local(variant) {
            self.register_install(id, variant, launch).await;
            return Ok(InstallOutcome::Installed {
                provider_id: id.to_string(),
                variant: variant.as_str().to_string(),
            });
        }

        // Reuse an already-in-flight download for the same (provider,
        // variant) instead of starting a duplicate one — mirrors `start()`'s
        // "already running → return the existing result" idempotency.
        {
            let installs = self.installs.lock().expect("installs mutex poisoned");
            if let Some(existing) = installs.values().find(|op| {
                op.provider_id == id.as_str()
                    && op.variant.as_deref() == Some(variant.as_str())
                    && matches!(op.status, InstallOperationStatus::Downloading)
            }) {
                return Ok(InstallOutcome::Downloading {
                    operation_id: existing.operation_id.clone(),
                    variant: variant.as_str().to_string(),
                });
            }
        }

        let operation_id = Uuid::new_v4().to_string();
        {
            let mut installs = self.installs.lock().expect("installs mutex poisoned");
            evict_finished_operations_if_over_capacity(&mut installs);
            installs.insert(
                operation_id.clone(),
                InstallOperationState {
                    operation_id: operation_id.clone(),
                    provider_id: id.to_string(),
                    variant: Some(variant.as_str().to_string()),
                    model_id: None,
                    status: InstallOperationStatus::Downloading,
                    downloaded_bytes: 0,
                    total_bytes: None,
                    error: None,
                },
            );
        }

        let manager = Arc::clone(self);
        let id_owned = id.clone();
        let op_id = operation_id.clone();
        tokio::spawn(async move {
            let manager_for_progress = Arc::clone(&manager);
            let progress_op_id = op_id.clone();
            let result = faster_whisper::download_variant(variant, move |progress| {
                let mut installs = manager_for_progress
                    .installs
                    .lock()
                    .expect("installs mutex poisoned");
                if let Some(state) = installs.get_mut(&progress_op_id) {
                    state.downloaded_bytes = progress.downloaded_bytes;
                    state.total_bytes = progress.total_bytes;
                }
            })
            .await;

            match result {
                Ok(launch) => {
                    manager.register_install(&id_owned, variant, launch).await;
                    let mut installs = manager.installs.lock().expect("installs mutex poisoned");
                    if let Some(state) = installs.get_mut(&op_id) {
                        state.status = InstallOperationStatus::Complete;
                    }
                }
                Err(e) => {
                    warn!(provider = %id_owned, variant = %variant, error = %e, "variant download failed");
                    let mut installs = manager.installs.lock().expect("installs mutex poisoned");
                    if let Some(state) = installs.get_mut(&op_id) {
                        state.status = InstallOperationStatus::Failed;
                        state.error = Some(e.to_string());
                    }
                }
            }
        });

        Ok(InstallOutcome::Downloading {
            operation_id,
            variant: variant.as_str().to_string(),
        })
    }

    pub async fn install_operation(&self, operation_id: &str) -> Option<InstallOperationState> {
        self.installs
            .lock()
            .expect("installs mutex poisoned")
            .get(operation_id)
            .cloned()
    }

    /// Remove a downloaded variant's cached copy, stopping the provider
    /// first if it's currently running that variant. Never touches a
    /// vendored dev copy (`install_local` prefers that over the cache and
    /// this only clears the cache directory).
    pub async fn uninstall_variant(
        &self,
        id: &ProviderId,
        variant: RuntimeVariant,
    ) -> Result<(), RuntimeError> {
        catalog::find_provider(id)?;
        let _ = self.stop(id).await; // fine if it wasn't running
        faster_whisper::remove_cached_variant(variant)
    }

    /// Remove a provider's install registration, stopping it first if
    /// running, *and* cascade-delete every cached variant binary on disk —
    /// not just whichever variant happened to be registered. A full
    /// provider uninstall means "get rid of this provider entirely"; a
    /// stray cached GPU binary from earlier testing that was never
    /// re-registered this session would otherwise silently survive it,
    /// which is exactly the class of leftover-state bug this exists to
    /// close (compare `uninstall_variant`, which correctly real-cleans a
    /// single variant already — this brings whole-provider uninstall up to
    /// the same standard rather than leaving it memory-only).
    pub async fn uninstall(&self, id: &ProviderId) -> Result<(), RuntimeError> {
        let _ = self.stop(id).await; // fine if it wasn't running
        let mut installed = self.installed.lock().await;
        if installed.remove(id.as_str()).is_none() {
            return Err(RuntimeError::ProviderNotInstalled(id.to_string()));
        }
        drop(installed);
        for variant in [RuntimeVariant::Cpu, RuntimeVariant::Gpu] {
            faster_whisper::remove_cached_variant(variant)?;
        }
        Ok(())
    }

    /// Download `model_id`'s weights for `id`, reusing the same
    /// `InstallOperationState` progress-polling table `begin_install` uses
    /// (`GET /v1/install-operations/:id` works for either kind unchanged).
    /// Only `faster-whisper` is a real provider today, same dispatch
    /// rationale as `begin_install`.
    pub async fn begin_model_pull(
        self: &Arc<Self>,
        id: &ProviderId,
        model_id: &str,
    ) -> Result<ModelPullOutcome, RuntimeError> {
        let entry = catalog::find_provider(id)?;
        catalog::find_model(entry, model_id)
            .ok_or_else(|| RuntimeError::ModelNotFound(model_id.to_string()))?;
        if id.as_str() != "faster-whisper" {
            return Err(RuntimeError::ProviderNotFound(id.to_string()));
        }

        if faster_whisper::verify_cached_model(model_id)?.is_some() {
            return Ok(ModelPullOutcome::Cached);
        }

        {
            let installs = self.installs.lock().expect("installs mutex poisoned");
            if let Some(existing) = installs.values().find(|op| {
                op.provider_id == id.as_str()
                    && op.model_id.as_deref() == Some(model_id)
                    && matches!(op.status, InstallOperationStatus::Downloading)
            }) {
                return Ok(ModelPullOutcome::Downloading {
                    operation_id: existing.operation_id.clone(),
                });
            }
        }

        let operation_id = Uuid::new_v4().to_string();
        {
            let mut installs = self.installs.lock().expect("installs mutex poisoned");
            evict_finished_operations_if_over_capacity(&mut installs);
            installs.insert(
                operation_id.clone(),
                InstallOperationState {
                    operation_id: operation_id.clone(),
                    provider_id: id.to_string(),
                    variant: None,
                    model_id: Some(model_id.to_string()),
                    status: InstallOperationStatus::Downloading,
                    downloaded_bytes: 0,
                    total_bytes: None,
                    error: None,
                },
            );
        }

        let manager = Arc::clone(self);
        let model_id_owned = model_id.to_string();
        let op_id = operation_id.clone();
        tokio::spawn(async move {
            let output_dir = faster_whisper::cached_model_dir(&model_id_owned);
            let result = faster_whisper::download_model(&model_id_owned, &output_dir).await;
            match result {
                Ok(()) => {
                    let mut installs = manager.installs.lock().expect("installs mutex poisoned");
                    if let Some(state) = installs.get_mut(&op_id) {
                        state.status = InstallOperationStatus::Complete;
                    }
                }
                Err(e) => {
                    warn!(model = %model_id_owned, error = %e, "model download failed");
                    let mut installs = manager.installs.lock().expect("installs mutex poisoned");
                    if let Some(state) = installs.get_mut(&op_id) {
                        state.status = InstallOperationStatus::Failed;
                        state.error = Some(e.to_string());
                    }
                }
            }
        });

        Ok(ModelPullOutcome::Downloading { operation_id })
    }

    /// Whether `model_id`'s weights are present on disk for `id`, and their
    /// size if so. A pure filesystem check — no subprocess, no network.
    pub fn verify_model(
        &self,
        id: &ProviderId,
        model_id: &str,
    ) -> Result<Option<u64>, RuntimeError> {
        let entry = catalog::find_provider(id)?;
        catalog::find_model(entry, model_id)
            .ok_or_else(|| RuntimeError::ModelNotFound(model_id.to_string()))?;
        faster_whisper::verify_cached_model(model_id)
    }

    /// Delete a previously-downloaded model's cached weights. Idempotent —
    /// `Ok` if it was never downloaded.
    pub fn remove_model(&self, id: &ProviderId, model_id: &str) -> Result<(), RuntimeError> {
        let entry = catalog::find_provider(id)?;
        catalog::find_model(entry, model_id)
            .ok_or_else(|| RuntimeError::ModelNotFound(model_id.to_string()))?;
        faster_whisper::remove_cached_model(model_id)
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
        options: &StartOptions,
    ) -> Result<RuntimeConnectionDescriptor, RuntimeError> {
        let entry = catalog::find_provider(id)?;

        {
            let mut instances = self.instances.lock().await;
            if let Some(existing) = instances.get_mut(id.as_str()) {
                if existing.instance.status() == RuntimeStatus::Running {
                    existing.last_activity = Instant::now();
                    return Ok(descriptor_for(
                        entry,
                        &existing.instance,
                        existing.streaming.clone(),
                    ));
                }
                // Not running (stopped/crashed): fall through and replace it.
                instances.remove(id.as_str());
            }
        }

        let bind_host = options.bind_host.as_deref().unwrap_or("127.0.0.1");
        if !stt_common::is_loopback_host(bind_host) {
            if bind_host != "0.0.0.0" {
                return Err(RuntimeError::InvalidStartOptions(format!(
                    "unsupported bind_host {bind_host:?}: expected a loopback host, or \"0.0.0.0\" for LAN mode"
                )));
            }
            if options
                .auth_token
                .as_deref()
                .unwrap_or("")
                .trim()
                .is_empty()
            {
                return Err(RuntimeError::InvalidStartOptions(
                    "starting on a non-loopback bind_host (\"0.0.0.0\") requires a non-empty auth_token".into(),
                ));
            }
        }

        if matches!(options.device.as_deref(), Some("cuda")) {
            let installed = self.installed.lock().await;
            if let Some(entry) = installed.get(id.as_str()) {
                if entry.variant != RuntimeVariant::Gpu {
                    return Err(RuntimeError::InvalidStartOptions(format!(
                        "device \"cuda\" was requested but the currently-registered {id} build is the {} variant, which cannot run CUDA inference; install the gpu variant first",
                        entry.variant
                    )));
                }
            }
            // Not installed at all: the installed.get(...) lookup below still
            // raises ProviderNotInstalled as it already does.
        }

        let port = supervisor::allocate_port(bind_host)?;
        let auth_token = options
            .auth_token
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let selected_model = self.selected_model(id).await;
        let launch = {
            let installed = self.installed.lock().await;
            let entry = installed
                .get(id.as_str())
                .ok_or_else(|| RuntimeError::ProviderNotInstalled(id.to_string()))?;
            (entry.launch)(port, &auth_token, selected_model.as_deref(), options)
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

        let streaming = fetch_streaming_capability(instance.port).await;
        let descriptor = descriptor_for(entry, &instance, streaming.clone());

        let mut instances = self.instances.lock().await;
        instances.insert(
            id.as_str().to_string(),
            RunningEntry {
                instance,
                last_activity: Instant::now(),
                streaming,
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
        Ok(descriptor_for(
            entry,
            &running.instance,
            running.streaming.clone(),
        ))
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

/// Drop the oldest finished (non-`Downloading`) entries once the table
/// exceeds [`MAX_FINISHED_INSTALL_OPERATIONS`]. In-flight entries are never
/// evicted, so this only ever trims install history, never loses progress
/// a caller might still be polling.
fn evict_finished_operations_if_over_capacity(
    installs: &mut HashMap<String, InstallOperationState>,
) {
    let finished_count = installs
        .values()
        .filter(|op| !matches!(op.status, InstallOperationStatus::Downloading))
        .count();
    if finished_count < MAX_FINISHED_INSTALL_OPERATIONS {
        return;
    }
    // HashMap has no insertion order; operation ids are UUIDv4 (time is
    // opaque), so "oldest" isn't recoverable without an extra timestamp
    // field this struct doesn't otherwise need. Evicting an arbitrary
    // finished entry still bounds growth, which is the actual goal — exact
    // LRU ordering isn't load-bearing for a debug/progress-polling table.
    if let Some(victim_id) = installs
        .iter()
        .find(|(_, op)| !matches!(op.status, InstallOperationStatus::Downloading))
        .map(|(id, _)| id.clone())
    {
        installs.remove(&victim_id);
    }
}

fn descriptor_for(
    entry: &CatalogEntry,
    instance: &ManagedInstance,
    streaming: Option<StreamingCapability>,
) -> RuntimeConnectionDescriptor {
    RuntimeConnectionDescriptor {
        schema_version: RUNTIME_DESCRIPTOR_SCHEMA_VERSION,
        provider: entry.id.to_string(),
        protocol: entry.protocol.to_string(),
        transport: entry.transport.to_string(),
        base_url: format!("http://127.0.0.1:{}", instance.port),
        streaming,
        auth: Some(DescriptorAuth {
            auth_type: "token".to_string(),
            value: instance.auth_token.clone(),
        }),
    }
}

/// Fetch the managed runtime's own `GET /v1/config` and extract just the
/// `streaming` block, which is already exactly the shape
/// `stt_common::StreamingCapability` expects. Non-fatal on any failure —
/// unreachable/malformed `/v1/config`, or a runtime that doesn't advertise
/// streaming at all — since a batch-only descriptor (`streaming: None`) is
/// still a valid, usable result; `start()` must not fail just because this
/// best-effort enrichment didn't pan out.
async fn fetch_streaming_capability(port: u16) -> Option<StreamingCapability> {
    #[derive(serde::Deserialize)]
    struct ConfigResponse {
        streaming: Option<StreamingCapability>,
    }

    let url = format!("http://127.0.0.1:{port}/v1/config");
    match reqwest::get(&url).await {
        Ok(resp) => match resp.json::<ConfigResponse>().await {
            Ok(config) => config.streaming,
            Err(e) => {
                tracing::warn!(url = %url, error = %e, "runtime's /v1/config response didn't match expected shape; issuing a batch-only descriptor");
                None
            }
        },
        Err(e) => {
            tracing::warn!(url = %url, error = %e, "could not fetch runtime's /v1/config; issuing a batch-only descriptor");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `python -m venv` only creates `Scripts\python.exe` on Windows — no
    /// `python3.exe` — matching `providers::faster_whisper::python_candidates`'s
    /// same rationale. These fake-runtime test helpers need a name
    /// guaranteed to resolve to a real interpreter, not Windows's
    /// `python3.exe` "app execution alias" stub.
    fn python_bin() -> &'static str {
        if cfg!(windows) {
            "python"
        } else {
            "python3"
        }
    }

    /// The `faster-whisper` catalog entry's health path is `/health`, so the
    /// fake runtime needs an actual file there for `http.server` to serve
    /// (unlike supervisor.rs's tests, which use `/` directly). No `/v1/config`
    /// file is served, so `fetch_streaming_capability` degrades to `None` —
    /// exactly the "runtime doesn't advertise streaming" path, exercised
    /// implicitly by every test using this launcher.
    fn fake_launch() -> LaunchBuilder {
        Box::new(|port, _auth_token, _selected_model, _options| {
            let dir = std::env::temp_dir().join(format!("stt-runtime-test-{port}"));
            std::fs::create_dir_all(&dir).expect("create fake runtime temp dir");
            std::fs::write(dir.join("health"), b"ok").expect("write fake health file");
            Launch {
                program: python_bin().into(),
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

    /// Like `fake_launch`, but also serves a `/v1/config` file whose body is
    /// a JSON `ConfigResponse` shape including a `streaming` block — enough
    /// for `fetch_streaming_capability` to actually populate something.
    fn fake_launch_with_streaming_config() -> LaunchBuilder {
        Box::new(|port, _auth_token, _selected_model, _options| {
            let dir = std::env::temp_dir().join(format!("stt-runtime-test-streaming-{port}"));
            std::fs::create_dir_all(dir.join("v1")).expect("create fake runtime temp dir");
            std::fs::write(dir.join("health"), b"ok").expect("write fake health file");
            std::fs::write(
                dir.join("v1").join("config"),
                serde_json::json!({
                    "streaming": {
                        "enabled": true,
                        "endpoint": "/v1/audio/stream",
                        "protocolVersion": 1,
                        "encodings": ["pcm_s16le"],
                        "sampleRates": [16000],
                        "resample": true,
                        "channels": [1]
                    }
                })
                .to_string(),
            )
            .expect("write fake config file");
            Launch {
                program: python_bin().into(),
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

    type RecordedCalls = std::sync::Arc<std::sync::Mutex<Vec<(Option<String>, StartOptions)>>>;

    /// Captures the `StartOptions`/model each call was invoked with, so a
    /// test can assert device/compute_type actually reach the launch spec.
    fn recording_launch(calls: RecordedCalls) -> LaunchBuilder {
        Box::new(move |port, _auth_token, selected_model, options| {
            calls
                .lock()
                .unwrap()
                .push((selected_model.map(str::to_string), options.clone()));
            let dir = std::env::temp_dir().join(format!("stt-runtime-test-recording-{port}"));
            std::fs::create_dir_all(&dir).expect("create fake runtime temp dir");
            std::fs::write(dir.join("health"), b"ok").expect("write fake health file");
            Launch {
                program: python_bin().into(),
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
        manager
            .register_install(&id, RuntimeVariant::Cpu, fake_launch())
            .await;
        manager
    }

    #[tokio::test]
    async fn start_stop_and_status_lifecycle() {
        let manager = manager_with_faster_whisper_installed(None).await;
        let id = ProviderId::new("faster-whisper").unwrap();

        assert_eq!(manager.status(&id).await, RuntimeStatus::Stopped);

        let descriptor = manager.start(&id, &StartOptions::default()).await.unwrap();
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

        let first = manager.start(&id, &StartOptions::default()).await.unwrap();
        let second = manager.start(&id, &StartOptions::default()).await.unwrap();

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
            manager.start(&id, &StartOptions::default()).await,
            Err(RuntimeError::ProviderNotFound(_))
        ));
    }

    #[tokio::test]
    async fn start_rejects_uninstalled_provider() {
        let manager = RuntimeManager::new(None);
        let id = ProviderId::new("faster-whisper").unwrap();
        assert!(matches!(
            manager.start(&id, &StartOptions::default()).await,
            Err(RuntimeError::ProviderNotInstalled(_))
        ));
    }

    #[tokio::test]
    async fn sweep_idle_stops_instances_past_the_timeout_and_leaves_fresh_ones() {
        let manager = manager_with_faster_whisper_installed(Some(Duration::from_millis(50))).await;
        let id = ProviderId::new("faster-whisper").unwrap();

        manager.start(&id, &StartOptions::default()).await.unwrap();
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

        manager.start(&id, &StartOptions::default()).await.unwrap();

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
    async fn verify_and_remove_model_validate_against_the_catalog() {
        let manager = manager_with_faster_whisper_installed(None).await;
        let id = ProviderId::new("faster-whisper").unwrap();

        assert!(matches!(
            manager.verify_model(&id, "not-a-real-model"),
            Err(RuntimeError::ModelNotFound(_))
        ));
        assert!(matches!(
            manager.remove_model(&id, "not-a-real-model"),
            Err(RuntimeError::ModelNotFound(_))
        ));

        let bogus_provider = ProviderId::new("not-a-real-provider").unwrap();
        assert!(matches!(
            manager.verify_model(&bogus_provider, "Systran/faster-whisper-tiny"),
            Err(RuntimeError::ProviderNotFound(_))
        ));
    }

    #[tokio::test]
    async fn verify_and_remove_model_round_trip_against_real_cached_weights() {
        let manager = manager_with_faster_whisper_installed(None).await;
        let id = ProviderId::new("faster-whisper").unwrap();
        let model_id = "Systran/faster-whisper-tiny";

        // Acquired after the only `.await` in this test (registering the
        // fake install above touches none of the env vars this guards) so
        // the lock never spans an await point.
        let _guard = faster_whisper::lock_env_test();
        let root = std::env::temp_dir().join(format!(
            "stt-manager-model-verify-test-{}-{}",
            std::process::id(),
            line!()
        ));
        // SAFETY: test-only env var mutation, scoped to this single test.
        unsafe {
            std::env::set_var(faster_whisper::MODEL_CACHE_DIR_ENV_VAR, &root);
        }

        assert_eq!(manager.verify_model(&id, model_id).unwrap(), None);
        manager.remove_model(&id, model_id).unwrap(); // no-op, not an error

        let dir = faster_whisper::cached_model_dir(model_id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("model.bin"), b"fake weights").unwrap();
        assert!(manager.verify_model(&id, model_id).unwrap().is_some());

        manager.remove_model(&id, model_id).unwrap();
        assert_eq!(manager.verify_model(&id, model_id).unwrap(), None);

        unsafe {
            std::env::remove_var(faster_whisper::MODEL_CACHE_DIR_ENV_VAR);
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    // `begin_model_pull` reads `MODEL_CACHE_DIR_ENV_VAR` synchronously
    // inside the awaited call itself, so unlike the other env-var-guarded
    // tests here, `_guard` genuinely must span that one `.await` — this is
    // a `#[tokio::test]`'s default single-threaded runtime, so holding a
    // std `Mutex` across it can't deadlock (no other task on the same
    // thread contends for it); it only ever blocks a *different OS thread*
    // running a different test that needs the same process-wide env var.
    #[allow(clippy::await_holding_lock)]
    async fn begin_model_pull_returns_cached_instantly_when_weights_already_present() {
        let _guard = faster_whisper::lock_env_test();
        let manager = std::sync::Arc::new(manager_with_faster_whisper_installed(None).await);
        let id = ProviderId::new("faster-whisper").unwrap();
        let model_id = "Systran/faster-whisper-tiny";

        let root = std::env::temp_dir().join(format!(
            "stt-manager-model-pull-cached-test-{}-{}",
            std::process::id(),
            line!()
        ));
        // SAFETY: test-only env var mutation, scoped to this single test.
        unsafe {
            std::env::set_var(faster_whisper::MODEL_CACHE_DIR_ENV_VAR, &root);
        }
        let dir = faster_whisper::cached_model_dir(model_id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("model.bin"), b"fake weights").unwrap();

        let outcome = manager.begin_model_pull(&id, model_id).await.unwrap();

        unsafe {
            std::env::remove_var(faster_whisper::MODEL_CACHE_DIR_ENV_VAR);
        }
        std::fs::remove_dir_all(&root).ok();

        assert!(
            matches!(outcome, ModelPullOutcome::Cached),
            "expected already-cached weights to short-circuit the download, got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn begin_model_pull_rejects_an_unknown_model() {
        let manager = std::sync::Arc::new(manager_with_faster_whisper_installed(None).await);
        let id = ProviderId::new("faster-whisper").unwrap();

        assert!(matches!(
            manager.begin_model_pull(&id, "not-a-real-model").await,
            Err(RuntimeError::ModelNotFound(_))
        ));
    }

    #[tokio::test]
    async fn sweep_idle_disabled_never_stops_anything() {
        let manager = manager_with_faster_whisper_installed(None).await; // idle timeout disabled
        let id = ProviderId::new("faster-whisper").unwrap();

        manager.start(&id, &StartOptions::default()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert!(manager.sweep_idle().await.is_empty());
        assert_eq!(manager.status(&id).await, RuntimeStatus::Running);

        manager.stop(&id).await.unwrap();
    }

    #[tokio::test]
    async fn start_threads_device_and_compute_type_into_the_launch_builder() {
        let manager = RuntimeManager::new(None);
        let id = ProviderId::new("faster-whisper").unwrap();
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        manager
            .register_install(&id, RuntimeVariant::Cpu, recording_launch(calls.clone()))
            .await;

        let options = StartOptions {
            device: Some("cpu".to_string()),
            compute_type: Some("int8".to_string()),
            ..StartOptions::default()
        };
        manager.start(&id, &options).await.unwrap();

        {
            let recorded = calls.lock().unwrap();
            assert_eq!(recorded.len(), 1);
            assert_eq!(recorded[0].1.device.as_deref(), Some("cpu"));
            assert_eq!(recorded[0].1.compute_type.as_deref(), Some("int8"));
        }
        manager.stop(&id).await.unwrap();
    }

    #[tokio::test]
    async fn start_rejects_a_non_loopback_bind_host_that_isnt_the_wildcard() {
        let manager = manager_with_faster_whisper_installed(None).await;
        let id = ProviderId::new("faster-whisper").unwrap();
        let options = StartOptions {
            bind_host: Some("192.168.1.5".to_string()),
            auth_token: Some("secret".to_string()),
            ..StartOptions::default()
        };
        assert!(matches!(
            manager.start(&id, &options).await,
            Err(RuntimeError::InvalidStartOptions(_))
        ));
    }

    #[tokio::test]
    async fn start_rejects_wildcard_bind_host_without_an_auth_token() {
        let manager = manager_with_faster_whisper_installed(None).await;
        let id = ProviderId::new("faster-whisper").unwrap();
        let options = StartOptions {
            bind_host: Some("0.0.0.0".to_string()),
            auth_token: None,
            ..StartOptions::default()
        };
        assert!(matches!(
            manager.start(&id, &options).await,
            Err(RuntimeError::InvalidStartOptions(_))
        ));
    }

    #[tokio::test]
    async fn start_rejects_cuda_device_when_the_registered_variant_is_cpu() {
        // `manager_with_faster_whisper_installed` registers with `RuntimeVariant::Cpu`.
        let manager = manager_with_faster_whisper_installed(None).await;
        let id = ProviderId::new("faster-whisper").unwrap();
        let options = StartOptions {
            device: Some("cuda".to_string()),
            ..StartOptions::default()
        };
        assert!(matches!(
            manager.start(&id, &options).await,
            Err(RuntimeError::InvalidStartOptions(_))
        ));
    }

    #[tokio::test]
    async fn start_accepts_cuda_device_when_the_registered_variant_is_gpu() {
        let manager = RuntimeManager::new(None);
        let id = ProviderId::new("faster-whisper").unwrap();
        manager
            .register_install(&id, RuntimeVariant::Gpu, fake_launch())
            .await;
        let options = StartOptions {
            device: Some("cuda".to_string()),
            ..StartOptions::default()
        };
        assert!(manager.start(&id, &options).await.is_ok());
        manager.stop(&id).await.unwrap();
    }

    #[tokio::test]
    async fn start_accepts_wildcard_bind_host_with_an_auth_token_and_keeps_descriptor_loopback() {
        let manager = RuntimeManager::new(None);
        let id = ProviderId::new("faster-whisper").unwrap();
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        manager
            .register_install(&id, RuntimeVariant::Cpu, recording_launch(calls.clone()))
            .await;

        let options = StartOptions {
            bind_host: Some("0.0.0.0".to_string()),
            auth_token: Some("shared-secret".to_string()),
            ..StartOptions::default()
        };
        let descriptor = manager.start(&id, &options).await.unwrap();

        // The wire descriptor is always loopback — LAN reachability is a
        // different fact for a different audience (see `descriptor_for`).
        assert!(descriptor.base_url.starts_with("http://127.0.0.1:"));
        assert_eq!(descriptor.auth.unwrap().value, "shared-secret");
        {
            let recorded = calls.lock().unwrap();
            assert_eq!(recorded.len(), 1);
            assert_eq!(recorded[0].1.bind_host.as_deref(), Some("0.0.0.0"));
        }

        manager.stop(&id).await.unwrap();
    }

    #[tokio::test]
    async fn start_populates_streaming_capability_when_the_runtime_advertises_it() {
        let manager = RuntimeManager::new(None);
        let id = ProviderId::new("faster-whisper").unwrap();
        manager
            .register_install(
                &id,
                RuntimeVariant::Cpu,
                fake_launch_with_streaming_config(),
            )
            .await;

        let descriptor = manager.start(&id, &StartOptions::default()).await.unwrap();
        let streaming = descriptor
            .streaming
            .expect("expected a populated streaming block");
        assert_eq!(streaming.endpoint, "/v1/audio/stream");
        assert!(streaming.enabled);

        manager.stop(&id).await.unwrap();
    }

    #[tokio::test]
    async fn start_falls_back_to_batch_only_when_config_has_no_streaming_block() {
        // fake_launch() (used throughout this module) serves no /v1/config at
        // all, exercising the "unreachable" branch of fetch_streaming_capability.
        let manager = manager_with_faster_whisper_installed(None).await;
        let id = ProviderId::new("faster-whisper").unwrap();

        let descriptor = manager.start(&id, &StartOptions::default()).await.unwrap();
        assert!(descriptor.streaming.is_none());

        manager.stop(&id).await.unwrap();
    }

    #[tokio::test]
    async fn begin_install_rejects_unknown_provider() {
        let manager = Arc::new(RuntimeManager::new(None));
        let id = ProviderId::new("does-not-exist").unwrap();
        assert!(matches!(
            manager.begin_install(&id, RuntimeVariant::Cpu).await,
            Err(RuntimeError::ProviderNotFound(_))
        ));
    }

    #[tokio::test]
    async fn uninstall_variant_rejects_unknown_provider() {
        let manager = RuntimeManager::new(None);
        let id = ProviderId::new("does-not-exist").unwrap();
        assert!(matches!(
            manager.uninstall_variant(&id, RuntimeVariant::Cpu).await,
            Err(RuntimeError::ProviderNotFound(_))
        ));
    }

    #[tokio::test]
    async fn install_operation_returns_none_for_an_unknown_operation_id() {
        let manager = RuntimeManager::new(None);
        assert!(manager
            .install_operation("not-a-real-op-id")
            .await
            .is_none());
    }

    /// Spins up a local `python3 -m http.server` serving a fake release
    /// asset, same test-double pattern `faster_whisper.rs`'s own download
    /// test uses, and points a fresh temp cache dir + `RELEASE_BASE_URL_ENV_VAR`
    /// at it. Returns the running server (kill it when done) and the
    /// (serve_dir, cache_root) temp dirs to clean up afterward.
    async fn spawn_fake_release_server(
        variant: RuntimeVariant,
    ) -> (
        tokio::process::Child,
        std::path::PathBuf,
        std::path::PathBuf,
        String,
    ) {
        let unique = format!("{}-{}", std::process::id(), uuid::Uuid::new_v4());
        let serve_dir = std::env::temp_dir().join(format!("stt-manager-test-serve-{unique}"));
        std::fs::create_dir_all(&serve_dir).unwrap();
        let os = if cfg!(windows) { "windows" } else { "linux" };
        let ext = if cfg!(windows) { ".exe" } else { "" };
        let asset_name = format!("faster-whisper-runtime-{os}-{}{ext}", variant.as_str());
        std::fs::write(
            serve_dir.join(&asset_name),
            b"pretend this is a packaged exe",
        )
        .unwrap();

        let port = supervisor::allocate_loopback_port().unwrap();
        let server = tokio::process::Command::new(python_bin())
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

        let base_url = format!("http://127.0.0.1:{port}");
        let client = reqwest::Client::new();
        for _ in 0..50 {
            if client.get(&base_url).send().await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let cache_root = std::env::temp_dir().join(format!("stt-manager-test-cache-{unique}"));
        (server, serve_dir, cache_root, base_url)
    }

    #[tokio::test]
    // Held across `.await` deliberately: this guard serializes tests
    // that mutate process-wide env vars, and each `#[tokio::test]` here runs
    // on its own single-threaded (current_thread) runtime, so holding it
    // through the awaits below just serializes those tests, it can't starve
    // unrelated async work on another task.
    #[allow(clippy::await_holding_lock)]
    async fn begin_install_returns_installed_immediately_when_the_variant_is_already_cached() {
        let _guard = faster_whisper::lock_env_test();
        let (mut server, serve_dir, cache_root, base_url) =
            spawn_fake_release_server(RuntimeVariant::Cpu).await;

        // SAFETY: test-only env var mutation, scoped tightly around the
        // network calls below and cleared before this test returns — same
        // convention `faster_whisper.rs`'s download test uses.
        unsafe {
            std::env::set_var(faster_whisper::RELEASE_BASE_URL_ENV_VAR, &base_url);
            std::env::set_var(faster_whisper::RUNTIME_CACHE_DIR_ENV_VAR, &cache_root);
            std::env::set_var(faster_whisper::RUNTIME_DIR_ENV_VAR, "/nonexistent/for/sure");
        }

        // Pre-populate the cache directly, so `begin_install` below should
        // find it locally and never need the (still-running) fake server.
        let _ = faster_whisper::download_variant(RuntimeVariant::Cpu, |_| {})
            .await
            .expect("pre-populating the cache should succeed against the local fake server");

        let manager = Arc::new(RuntimeManager::new(None));
        let id = ProviderId::new("faster-whisper").unwrap();
        let outcome = manager.begin_install(&id, RuntimeVariant::Cpu).await;

        unsafe {
            std::env::remove_var(faster_whisper::RELEASE_BASE_URL_ENV_VAR);
            std::env::remove_var(faster_whisper::RUNTIME_CACHE_DIR_ENV_VAR);
            std::env::remove_var(faster_whisper::RUNTIME_DIR_ENV_VAR);
        }
        let _ = server.kill().await;
        std::fs::remove_dir_all(&serve_dir).ok();
        std::fs::remove_dir_all(&cache_root).ok();

        assert!(matches!(outcome, Ok(InstallOutcome::Installed { .. })));
        assert!(manager.is_installed(&id).await);
    }

    #[tokio::test]
    // Held across `.await` deliberately: this guard serializes tests
    // that mutate process-wide env vars, and each `#[tokio::test]` here runs
    // on its own single-threaded (current_thread) runtime, so holding it
    // through the awaits below just serializes those tests, it can't starve
    // unrelated async work on another task.
    #[allow(clippy::await_holding_lock)]
    async fn begin_install_downloads_and_completes_when_nothing_is_cached_locally() {
        let _guard = faster_whisper::lock_env_test();
        let (mut server, serve_dir, cache_root, base_url) =
            spawn_fake_release_server(RuntimeVariant::Cpu).await;

        // SAFETY: test-only env var mutation, scoped tightly around the
        // download this test triggers and cleared as soon as `begin_install`
        // has kicked off (its background task captures the values it reads
        // at call time; env doesn't need to stay set for the task to finish).
        unsafe {
            std::env::set_var(faster_whisper::RELEASE_BASE_URL_ENV_VAR, &base_url);
            std::env::set_var(faster_whisper::RUNTIME_CACHE_DIR_ENV_VAR, &cache_root);
            std::env::set_var(faster_whisper::RUNTIME_DIR_ENV_VAR, "/nonexistent/for/sure");
        }

        let manager = Arc::new(RuntimeManager::new(None));
        let id = ProviderId::new("faster-whisper").unwrap();
        let outcome = manager
            .begin_install(&id, RuntimeVariant::Cpu)
            .await
            .unwrap();
        let operation_id = match outcome {
            InstallOutcome::Downloading { operation_id, .. } => operation_id,
            InstallOutcome::Installed { .. } => {
                panic!("expected a fresh cache to require a download")
            }
        };

        let mut final_state = None;
        for _ in 0..100 {
            let state = manager.install_operation(&operation_id).await.unwrap();
            if !matches!(state.status, InstallOperationStatus::Downloading) {
                final_state = Some(state);
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        unsafe {
            std::env::remove_var(faster_whisper::RELEASE_BASE_URL_ENV_VAR);
            std::env::remove_var(faster_whisper::RUNTIME_CACHE_DIR_ENV_VAR);
            std::env::remove_var(faster_whisper::RUNTIME_DIR_ENV_VAR);
        }
        let _ = server.kill().await;
        std::fs::remove_dir_all(&serve_dir).ok();
        std::fs::remove_dir_all(&cache_root).ok();

        let final_state = final_state.expect("download should have finished within the timeout");
        assert!(
            matches!(final_state.status, InstallOperationStatus::Complete),
            "expected the download to complete, got {final_state:?}"
        );
        assert!(manager.is_installed(&id).await);
    }

    #[tokio::test]
    // Held across `.await` deliberately: this guard serializes tests
    // that mutate process-wide env vars, and each `#[tokio::test]` here runs
    // on its own single-threaded (current_thread) runtime, so holding it
    // through the awaits below just serializes those tests, it can't starve
    // unrelated async work on another task.
    #[allow(clippy::await_holding_lock)]
    async fn uninstall_variant_removes_a_previously_cached_copy() {
        let _guard = faster_whisper::lock_env_test();
        let (mut server, serve_dir, cache_root, base_url) =
            spawn_fake_release_server(RuntimeVariant::Cpu).await;

        // SAFETY: test-only env var mutation, scoped tightly around the
        // network call below and cleared before this test returns.
        unsafe {
            std::env::set_var(faster_whisper::RELEASE_BASE_URL_ENV_VAR, &base_url);
            std::env::set_var(faster_whisper::RUNTIME_CACHE_DIR_ENV_VAR, &cache_root);
            std::env::set_var(faster_whisper::RUNTIME_DIR_ENV_VAR, "/nonexistent/for/sure");
        }

        let _ = faster_whisper::download_variant(RuntimeVariant::Cpu, |_| {})
            .await
            .expect("pre-populating the cache should succeed against the local fake server");
        assert!(
            faster_whisper::install_local(RuntimeVariant::Cpu).is_some(),
            "expected the pre-populated cache to be found before uninstalling"
        );

        let manager = RuntimeManager::new(None);
        let id = ProviderId::new("faster-whisper").unwrap();
        manager
            .uninstall_variant(&id, RuntimeVariant::Cpu)
            .await
            .unwrap();

        let found_after_uninstall = faster_whisper::install_local(RuntimeVariant::Cpu);

        unsafe {
            std::env::remove_var(faster_whisper::RELEASE_BASE_URL_ENV_VAR);
            std::env::remove_var(faster_whisper::RUNTIME_CACHE_DIR_ENV_VAR);
            std::env::remove_var(faster_whisper::RUNTIME_DIR_ENV_VAR);
        }
        let _ = server.kill().await;
        std::fs::remove_dir_all(&serve_dir).ok();
        std::fs::remove_dir_all(&cache_root).ok();

        assert!(
            found_after_uninstall.is_none(),
            "expected the cached copy to be gone after uninstall_variant"
        );
    }

    #[tokio::test]
    // Regression test for the cascade-delete fix: a full provider
    // uninstall used to only clear the in-memory registration
    // (`installed.remove(...)`), leaving cached variant binaries on disk
    // untouched — including a variant that was never re-registered this
    // session (e.g. a stray GPU binary from earlier testing). This proves
    // *both* variants' cache directories are gone afterward, not just
    // whichever one happened to be the registered launch. `_guard` must
    // span `uninstall(...).await` (it reads `RUNTIME_CACHE_DIR_ENV_VAR`
    // internally) — safe under `#[tokio::test]`'s single-threaded runtime,
    // see the identical justification on `begin_model_pull_returns_cached_instantly_when_weights_already_present`.
    #[allow(clippy::await_holding_lock)]
    async fn uninstall_cascades_and_removes_every_cached_variant_not_just_the_registered_one() {
        let _guard = faster_whisper::lock_env_test();
        let cache_root = std::env::temp_dir().join(format!(
            "stt-uninstall-cascade-test-{}-{}",
            std::process::id(),
            line!()
        ));
        for variant in [RuntimeVariant::Cpu, RuntimeVariant::Gpu] {
            let dir = cache_root.join("faster-whisper").join(variant.as_str());
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("fake-exe"), b"fake").unwrap();
        }

        // SAFETY: test-only env var mutation, scoped to this single test.
        unsafe {
            std::env::set_var(faster_whisper::RUNTIME_CACHE_DIR_ENV_VAR, &cache_root);
        }

        let manager = manager_with_faster_whisper_installed(None).await; // registers only the Cpu variant
        let id = ProviderId::new("faster-whisper").unwrap();
        manager.uninstall(&id).await.unwrap();

        let cpu_survived = cache_root.join("faster-whisper").join("cpu").exists();
        let gpu_survived = cache_root.join("faster-whisper").join("gpu").exists();

        unsafe {
            std::env::remove_var(faster_whisper::RUNTIME_CACHE_DIR_ENV_VAR);
        }
        std::fs::remove_dir_all(&cache_root).ok();

        assert!(
            !cpu_survived,
            "expected the registered cpu variant's cache to be gone"
        );
        assert!(
            !gpu_survived,
            "expected the never-registered gpu variant's stray cache to be gone too"
        );
    }
}
