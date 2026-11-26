//! Observability skeleton (Phase 1): `tracing` subscriber setup.
//!
//! Later phases add per-request / per-tool / per-repo-call spans here.

use tracing_subscriber::{fmt, EnvFilter};

/// Initialise the global `tracing` subscriber. Safe to call multiple times
/// (subsequent calls are no-ops) so tests can invoke it freely.
pub fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = fmt().with_env_filter(filter).with_target(true).try_init();
}
