pub mod catalog;
pub mod error;
pub mod hardware;
pub mod manager;
pub mod providers;
pub mod recommend;
pub mod supervisor;

pub use catalog::{
    preferred_variant, CatalogEntry, ModelEntry, ProviderId, ProviderInfo, RuntimeVariant,
    VariantInfo, CATALOG,
};
pub use error::RuntimeError;
pub use hardware::HardwareReport;
pub use manager::{
    InstallOperationState, InstallOperationStatus, InstallOutcome, Launch, LaunchBuilder,
    ModelPullOutcome, RuntimeManager, StartOptions, SwitchModelOutcome,
};
pub use recommend::{recommend, ModelRecommendation};
pub use supervisor::{ManagedInstance, RuntimeStatus, SpawnSpec};
