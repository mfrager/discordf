//! Binary entry: starts the Tokio runtime, loads config, opens an
//! in-memory knowledge store and waits (Phase 1 skeleton).
//!
//! Later phases bind the Axum HTTP API and the serialized Needle worker here.

use needle_rdf_agent::{
    config::AppConfig,
    observability,
    rdf::{OxigraphRepository, RdfRepository},
};
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    observability::init_tracing();
    let config = AppConfig::default();
    let repo = OxigraphRepository::new_in_memory(config.limits.max_triples_per_read)?;
    info!(
        iterations = config.limits.max_iterations,
        quads = repo.len()?,
        "needle-rdf-agent phase 1 ready (repository only, no model/tools yet)"
    );
    Ok(())
}
