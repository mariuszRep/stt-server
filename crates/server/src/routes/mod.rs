pub mod hardware;
pub mod health;
pub mod models;
pub mod providers;
pub mod recommendations;

pub use hardware::get_hardware;
pub use health::{health, readiness};
pub use models::{
    list_models, pull_model, remove_model, select_model, selected_model, verify_model,
};
pub use providers::{
    install_provider, list_providers, provider_descriptor, provider_heartbeat, provider_logs,
    provider_status, start_provider, stop_provider, uninstall_provider, update_provider,
};
pub use recommendations::recommendations;
