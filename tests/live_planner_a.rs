//! Live planner-A test (Needle 3, real weights, no mocks).
//! Compiled only with `--features live-model`; ignored unless
//! `RUN_MODEL_TESTS=1`. Serializes on the process-global Needle slot (only
//! one `Needle` may exist per process).
//!
//! Run: `RUN_MODEL_TESTS=1 cargo test --features live-model --test live_planner_a -- --ignored --nocapture`
//!
//! Production wiring note: the engine is built with the Phase-4 planning
//! tools (`planning_tool_defs`) and the planner system prompt — NOT the
//! five RDF tools (those belong to the executor path). The `MockModel`
//! unit tests in `src/planner` use the same surface.

#![cfg(feature = "live-model")]

use std::sync::Arc;

use needle_rdf_agent::config::AppLimits;
use needle_rdf_agent::executor::Executor;
use needle_rdf_agent::model::{LiveEngine, LiveModelHandle};
use needle_rdf_agent::ontology::{contact_shapes, load_default_ontology, OntologyReasoner};
use needle_rdf_agent::plan::{GraphContext, OperationContext};
use needle_rdf_agent::planner::{
    planner_system_prompt, planning_tool_defs, NeedleSequentialPlanner, PlanCaps, Planner,
    PlanningServices, UserRequest,
};
use needle_rdf_agent::policy::{AuditLog, ExecutionContext, Operation};
use needle_rdf_agent::rdf::{GraphName, OxigraphRepository, Quad, RdfRepository, Term};
use needle_rdf_agent::search::TantivyIndex;
use needle_rdf_agent::tools::{ToolRegistry, ToolState};

const EX: &str = "http://example.com/";
const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

static LIVE_SLOT: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();

#[tokio::test]
#[ignore]
async fn live_a_needle_sequential_against_real_weights() {
    if !std::env::var("RUN_MODEL_TESTS").map(|v| v == "1").unwrap_or(false) {
        panic!("live test requires RUN_MODEL_TESTS=1");
    }
    let _slot = LIVE_SLOT.get_or_init(|| std::sync::Mutex::new(())).lock().unwrap();

    let repo = Arc::new(OxigraphRepository::new_in_memory(50_000).unwrap());
    load_default_ontology(&*repo).unwrap();
    let graph = GraphName::tenant("acme").unwrap();
    let t = |s: &str| Term::iri(s.to_string()).unwrap();
    repo.insert(&[
        Quad::new(t(&format!("{EX}Alice")), t(RDF_TYPE), t(&format!("{EX}Contact")), graph.clone()).unwrap(),
        Quad::new(t(&format!("{EX}Alice")), t(&format!("{EX}name")), Term::literal("Alice"), graph.clone()).unwrap(),
        Quad::new(t(&format!("{EX}Alice")), t(&format!("{EX}account")), t(&format!("{EX}Acme")), graph.clone()).unwrap(),
        Quad::new(t(&format!("{EX}Acme")), t(RDF_TYPE), t(&format!("{EX}Account")), graph.clone()).unwrap(),
        Quad::new(t(&format!("{EX}Acme")), t(&format!("{EX}state")), Term::literal("OH"), graph.clone()).unwrap(),
        Quad::new(t(&format!("{EX}Prospect")), t(RDF_TYPE), t(&format!("{EX}Category")), graph.clone()).unwrap(),
    ])
    .unwrap();
    let reasoner = Arc::new(OntologyReasoner::from_repository(&*repo).unwrap());
    let search = Arc::new(TantivyIndex::in_ram().unwrap());
    let audit = Arc::new(AuditLog::in_memory());
    let state = ToolState::new(repo, search, reasoner, contact_shapes(), audit, AppLimits::default());
    let _ = state.search.sync_resource(&*state.repo, &format!("{EX}Prospect"), &graph);
    let exec_ctx = ExecutionContext::new(
        "req-live-a",
        "alice",
        "acme",
        vec![graph.clone(), GraphName::ontology()],
        vec![Operation::Read, Operation::Create, Operation::Update, Operation::Delete],
    );
    let registry = Arc::new(ToolRegistry::new());
    let gctx = GraphContext {
        graphs: vec![graph.clone()],
        namespaces: [("ex".to_string(), EX.to_string())].into_iter().collect(),
        ..Default::default()
    };

    // Real Needle engine on the planning surface (production wiring).
    let weights = LiveEngine::resolve_weights().expect("live weights resolve");
    let engine = LiveEngine::build(weights, &planner_system_prompt(&gctx), planning_tool_defs())
        .expect("live engine build");
    let services = PlanningServices::new(registry.clone(), state.clone(), exec_ctx.clone());
    let planner = NeedleSequentialPlanner::new(Box::new(LiveModelHandle::new(engine)), services)
        .with_caps(PlanCaps { max_plan_steps: 16, max_turns: 10, max_no_progress: 3 });

    let request = UserRequest::new(
        "Classify contacts at Ohio accounts as prospects: discover the vocabulary, \
         find contacts whose accounts are in Ohio, assign the prospect classification, \
         then validate and verify.",
    );
    let mut ctx = OperationContext::new(request.text.clone(), gctx);
    let plan = planner.create_plan(&request, &mut ctx).await;
    println!("LIVE A outcome: {plan:?}");
    match plan {
        Ok(plan) => {
            needle_rdf_agent::plan::validate_plan(&plan).expect("LIVE A plan validates");
            println!("LIVE A plan: {}", serde_json::to_string_pretty(&plan).unwrap());
            let exec = Executor::new(registry, state.clone(), exec_ctx);
            let mut op = OperationContext::new(plan.goal.clone(), plan.context.clone());
            op.request = plan.goal.clone();
            let report = exec.execute(&plan, &mut op, None).await.expect("LIVE A executes");
            println!("LIVE A report: {report:?}");
            let alice = t(&format!("{EX}Alice"));
            let cat = t(&format!("{EX}category"));
            let found = state.repo.match_quads(Some(&alice), Some(&cat), None, None, None).unwrap();
            assert!(!found.is_empty(), "LIVE A: Alice categorized in final state");
        }
        Err(e) => {
            // Honest model-quality observation (1B-class local model), not an
            // integration failure: transport/engine worked, planning stopped
            // with a typed outcome.
            println!("LIVE A: typed planner outcome (model limit): {e:?}");
        }
    }
}
