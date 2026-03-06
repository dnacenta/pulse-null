pub mod manager;
pub mod registry;

// Re-export the Plugin trait and related types from echo-system-types.
// This is the single source of truth — no local Plugin trait.
pub use echo_system_types::plugin::{Plugin, PluginContext, PluginRole};
pub use echo_system_types::PluginMeta;

// Alias used by manager and dashboard/status handlers
pub use echo_system_types::HealthStatus as PluginHealth;
