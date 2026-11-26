//! Live-model integration tests (Phase 6 entry).
//! Compiled only with `--features live-model`. Each test requires weights
//! (`NEEDLE_WEIGHTS_PATH` or the `Weights::fetch()` cache) and is
//! `#[ignore]` unless `RUN_MODEL_TESTS=1`, so default CI stays green.
//!
//! Run: `RUN_MODEL_TESTS=1 cargo test --features live-model -- --ignored`

#![cfg(feature = "live-model")]

use needle_rdf_agent::model::{LiveEmbedder, LiveEngine, LiveModelHandle, ModelAdapter};
use needle_rdf_agent::search::{Embedder, SearchService, TantivyIndex};
use needle_rdf_agent::tools::{ToolRegistry, ToolState};
use needle_rdf_agent::{agent::Agent, config::AppLimits};

fn live_required() {
    if std::env::var("RUN_MODEL_TESTS")
        .map(|v| v == "1")
        .unwrap_or(false)
    {
    } else {
        panic!("live test requires RUN_MODEL_TESTS=1");
    }
}

/// Process-global engine slot: only one `Needle` may exist per process, so
/// live tests must serialize engine construction (or run with
/// `-- --test-threads=1`). The guard holds the mutex for the whole test.
static LIVE_SLOT: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
struct LiveGuard {
    _slot: std::sync::MutexGuard<'static, ()>,
    engine: std::sync::Arc<LiveEngine>,
}
fn build_guarded() -> LiveGuard {
    let slot = LIVE_SLOT
        .get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap();
    let engine = LiveEngine::build_from_env()
        .expect("live weights must resolve (NEEDLE_WEIGHTS_PATH or fetch cache)");
    // Intentionally leak the slot guard into the return value: the mutex
    // stays held until the guard drops at the end of the test, so no two
    // tests build concurrent `Needle` instances.
    LiveGuard {
        _slot: slot,
        engine,
    }
}

#[test]
#[ignore]
fn live_weights_resolve_and_dim_is_3072() {
    live_required();
    let guard = build_guarded();
    let engine = guard.engine.clone();
    let dim = engine.embedding_dimension().expect("embedding_dimension");
    assert_eq!(dim, 3072, "Needle 3 embeddings are 3072-d");
}

#[test]
#[ignore]
fn live_complete_routes_simple_tool_call() {
    live_required();
    let _slot = LIVE_SLOT
        .get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap();
    let weights = LiveEngine::resolve_weights().expect("weights");
    let engine = LiveEngine::build(
        weights,
        "You control lights. Use set_light when asked.",
        vec![needle_rdf_agent::tools::ToolDef {
            name: "set_light",
            description: "Turn a room light on or off",
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "room": { "type": "string" },
                    "on": { "type": "boolean" }
                },
                "required": ["room", "on"],
                "additionalProperties": false
            }),
        }],
    )
    .expect("engine build");
    let mut model = LiveModelHandle::new(engine);
    let completion = model
        .complete("turn the kitchen light on")
        .expect("complete");
    match completion {
        needle_rdf_agent::model::ModelCompletion::ToolCalls(calls) => {
            assert_eq!(calls[0].name, "set_light");
            let room = calls[0]
                .args
                .get("room")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            assert!(room.contains("kitchen"), "args: {:?}", calls[0].args);
        }
        needle_rdf_agent::model::ModelCompletion::Done(text) => {
            panic!("expected set_light tool call, got Done({text:?})");
        }
    }
}

#[test]
#[ignore]
fn live_embed_is_normalized_and_semantic() {
    live_required();
    let guard = build_guarded();
    let engine = guard.engine.clone();
    let dim = engine.embedding_dimension().unwrap();
    let a = engine.embed_text("kitchen light").unwrap();
    let b = engine.embed_text("kitchen light").unwrap();
    assert_eq!(a.len(), dim);
    assert_eq!(a, b, "embed must be deterministic");
    let n: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!(
        (n - 1.0).abs() < 1e-4,
        "embeddings are L2-normalized, got {n}"
    );
    let q = engine.embed_text("turn on the kitchen lamp").unwrap();
    let unrelated = engine.embed_text("medieval maritime law").unwrap();
    let sim = |x: &[f32], y: &[f32]| x.iter().zip(y.iter()).map(|(u, v)| u * v).sum::<f32>();
    assert!(
        sim(&q, &a) > sim(&q, &unrelated),
        "paraphrase should beat unrelated"
    );
}

#[test]
#[ignore]
fn live_embedder_backs_hybrid_search() {
    live_required();
    let guard = build_guarded();
    let engine = guard.engine.clone();
    use needle_rdf_agent::policy::{ExecutionContext, Operation};
    use needle_rdf_agent::rdf::{GraphName, Quad, RdfRepository, Term};
    let embedder = LiveEmbedder::new(engine.clone()).unwrap();
    assert_eq!(embedder.dim(), 3072);
    let index =
        TantivyIndex::in_ram_with_embedder(Box::new(LiveEmbedder::new(engine).unwrap())).unwrap();
    let repo = needle_rdf_agent::rdf::OxigraphRepository::new_in_memory(500).unwrap();
    let g = GraphName::Named("urn:live:test".into());
    let ctx = ExecutionContext::new("r", "p", "t", vec![g.clone()], vec![Operation::Create]);
    let _ = &ctx;
    for (local, label) in [
        ("KitchenLight", "kitchen ceiling light"),
        ("HallThermostat", "hallway thermostat temperature"),
    ] {
        let uri = format!("urn:live:{local}");
        let q = Quad::new(
            Term::iri(uri.clone()).unwrap(),
            Term::iri("http://www.w3.org/2000/01/rdf-schema#label".to_string()).unwrap(),
            Term::literal(label),
            g.clone(),
        )
        .unwrap();
        repo.insert(std::slice::from_ref(&q)).unwrap();
        index.sync_resource(&repo, &uri, &g).unwrap();
    }
    let hits: Vec<_> = index
        .search("kitchen lamp", Some(&g), None, Some(5))
        .unwrap();
    assert!(!hits.is_empty(), "hybrid search should return hits");
}

#[test]
#[ignore]
fn live_agent_loop_search_then_done() {
    live_required();
    let guard = build_guarded();
    let engine = guard.engine.clone();
    let mut model = LiveModelHandle::new(engine.clone());
    let repo: std::sync::Arc<dyn needle_rdf_agent::rdf::RdfRepository> =
        std::sync::Arc::new(needle_rdf_agent::rdf::OxigraphRepository::new_in_memory(500).unwrap());
    let search: std::sync::Arc<dyn SearchService> = std::sync::Arc::new(
        TantivyIndex::in_ram_with_embedder(Box::new(LiveEmbedder::new(engine).unwrap())).unwrap(),
    );
    let reasoner = std::sync::Arc::new(needle_rdf_agent::ontology::OntologyReasoner::empty());
    let audit = std::sync::Arc::new(needle_rdf_agent::policy::AuditLog::in_memory());
    let state = ToolState::new(repo, search, reasoner, vec![], audit, AppLimits::default());
    let registry = ToolRegistry::new();
    let agent = Agent::new(AppLimits::default());
    let ctx = needle_rdf_agent::policy::ExecutionContext::new("r", "p", "t", vec![], vec![]);
    let resp = agent.run_request(&mut model, &registry, &state, &ctx, "say hello");
    assert!(resp.iterations >= 1 && resp.iterations <= 8);
}
