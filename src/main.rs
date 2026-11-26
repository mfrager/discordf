//! Binary entry (Phase 5 + live model): build the full stack — repo,
//! Tantivy index, tools, serialized worker (single model instance),
//! conversation log — and serve the Axum HTTP API.
//!
//! Model selection:
//! - `--features live-model` + weights available (`NEEDLE_WEIGHTS_PATH`
//!   or `Weights::fetch()` cache) → single shared [`LiveEngine`] drives
//!   both the agent tool-router and the 3072-d search embedder.
//! - Otherwise → `MockModel` + `HashEmbedder` so `/health` and tool routes
//!   work without weights. `NEEDLE_REQUIRE_LIVE=1` turns a missing live
//!   engine into a hard startup error instead of falling back.

use needle_rdf_agent::{
    agent::{conversation::ConversationLog, worker::spawn_worker, Agent},
    api::{router, ApiState},
    config::AppConfig,
    model::MockModel,
    observability,
    ontology::{contact_shapes, load_default_ontology, OntologyReasoner},
    policy::AuditLog,
    rdf::{OxigraphRepository, RdfRepository},
    search::SearchService,
    search::TantivyIndex,
    tools::{ToolRegistry, ToolState},
};
use std::sync::Arc;
use tracing::{info, warn};

fn require_live() -> bool {
    std::env::var("NEEDLE_REQUIRE_LIVE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    observability::init_tracing();
    let config = AppConfig::default();
    let repo = Arc::new(OxigraphRepository::new_in_memory(
        config.limits.max_triples_per_read,
    )?);
    load_default_ontology(&*repo)?;
    let reasoner = Arc::new(OntologyReasoner::from_repository(&*repo)?);

    // Live engine (single Needle shared by agent + embedder) when the
    // `live-model` feature is compiled in and weights resolve.
    #[cfg(feature = "live-model")]
    let live_engine: Option<Arc<needle_rdf_agent::model::LiveEngine>> =
        match needle_rdf_agent::model::LiveEngine::build_from_env() {
            Ok(e) => {
                let dim = e.embedding_dimension().unwrap_or(3072);
                info!("live Needle engine ready (embedding dim {dim})");
                Some(e)
            }
            Err(err) => {
                if require_live() {
                    anyhow::bail!("NEEDLE_REQUIRE_LIVE=1 but live engine failed: {err}");
                }
                warn!("live engine unavailable, falling back to mock: {err}");
                None
            }
        };
    #[cfg(not(feature = "live-model"))]
    let live_engine: Option<()> = None;

    #[cfg(feature = "live-model")]
    let search = {
        if let Some(ref engine) = live_engine {
            match needle_rdf_agent::model::LiveEmbedder::new(engine.clone()) {
                Ok(embedder) => {
                    info!("search using live 3072-d Needle embeddings");
                    Arc::new(TantivyIndex::in_ram_with_embedder(Box::new(embedder))?)
                }
                Err(e) => {
                    warn!("live embedder failed ({e}), using HashEmbedder");
                    Arc::new(TantivyIndex::in_ram()?)
                }
            }
        } else {
            Arc::new(TantivyIndex::in_ram()?)
        }
    };
    #[cfg(not(feature = "live-model"))]
    let search = Arc::new(TantivyIndex::in_ram()?);
    let _ = &live_engine;

    search.rebuild_from_repo(&*repo)?;
    let repo_dyn: Arc<dyn RdfRepository> = repo;
    let search_dyn: Arc<dyn needle_rdf_agent::search::SearchService> = search;
    let audit = Arc::new(AuditLog::in_memory());
    let log = Arc::new(ConversationLog::in_memory());
    let tools = ToolState::new(
        repo_dyn,
        search_dyn,
        reasoner,
        contact_shapes(),
        audit,
        config.limits.clone(),
    );
    let registry = Arc::new(ToolRegistry::new());

    #[cfg(feature = "live-model")]
    let model: Box<dyn needle_rdf_agent::model::ModelAdapter + Send> = match live_engine {
        Some(engine) => Box::new(needle_rdf_agent::model::LiveModelHandle::new(engine)),
        None => Box::new(MockModel::done(
            "needle-rdf-agent ready (mock; live unavailable)",
        )),
    };
    #[cfg(not(feature = "live-model"))]
    let model: Box<dyn needle_rdf_agent::model::ModelAdapter + Send> =
        Box::new(MockModel::done("needle-rdf-agent ready"));
    let (worker, _join, _metrics) = spawn_worker(
        model,
        Agent::new(config.limits.clone()),
        registry.clone(),
        tools.clone(),
        log.clone(),
    );
    let state = ApiState::new(worker, registry, tools, log);
    let app = router(state);
    let addr = std::env::var("NEEDLE_ADDR").unwrap_or_else(|_| "127.0.0.1:3000".to_string());
    info!("needle-rdf-agent listening on {addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
