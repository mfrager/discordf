//! 10-step planning ladder: trivial → complex, one rung at a time.
//!
//! Rung specs live in `needle_rdf_agent::ladder_spec` (shared with the
//! `ladder_cli` binary). Each planner type (B/C/D) climbs the same ladder:
//!
//! - Scripted climbs (default `cargo test`, keyless): a queue stub serves
//!   the rung's canned valid plan through the real planner loop, so the
//!   climb proves each variant's loop mechanics handle every rung shape.
//! - Live climbs (`RUN_MODEL_TESTS=1`, ignored): the same rungs go to the
//!   real local model with per-turn budgets capped at 4092 tokens. The
//!   climber attempts every rung and reports the highest cleared rung
//!   plus the breaking outcome class — planning only, no execution.
//!
//! Live budgets: B 4092/turn, C/D 4092 per structured call. The 1B model's
//! reasoning fills whatever context it gets; 4092 keeps each attempt to a
//! few minutes instead of ~20 while leaving ample room for short plans.

use needle_rdf_agent::config::AppLimits;
use needle_rdf_agent::ladder_spec::{ladder, rung_plan_json, rung_prose, Rung};
use needle_rdf_agent::llm::{build_backend, config_from_vars, KeyMode};
use needle_rdf_agent::ontology::{contact_shapes, load_default_ontology, OntologyReasoner};
use needle_rdf_agent::plan::{validate_plan, GraphContext, OperationContext, Plan};
use needle_rdf_agent::planner::{
    spine::SpinePlanner, AgentBackend, AgentStructuredPlanner, AgentToolPlanner,
    AgentTwoPassPlanner, Planner, PlannerError, PlanningServices, UserRequest,
};
use needle_rdf_agent::policy::{AuditLog, ExecutionContext, Operation};
use needle_rdf_agent::rdf::{GraphName, OxigraphRepository};
use needle_rdf_agent::tools::{ToolRegistry, ToolState};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Stub + services (same keyless pattern as plan4_planners)
// ---------------------------------------------------------------------------

fn text_payload(text: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "chatcmpl-ladder",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": text},
            "finish_reason": "stop"
        }],
        "usage": {}
    })
}

fn tool_payload(calls: &[(&str, serde_json::Value, &str)]) -> serde_json::Value {
    serde_json::json!({
        "id": "chatcmpl-ladder",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "",
                "tool_calls": calls.iter().map(|(name, args, id)| serde_json::json!({
                    "id": id,
                    "type": "function",
                    "function": {"name": name, "arguments": args.to_string()}
                })).collect::<Vec<_>>()
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {}
    })
}

struct QueueStub {
    bodies: Mutex<VecDeque<serde_json::Value>>,
}

impl QueueStub {
    async fn serving(bodies: Vec<serde_json::Value>) -> (Arc<Self>, String) {
        let stub = Arc::new(Self {
            bodies: Mutex::new(bodies.into()),
        });
        let app = {
            let stub = stub.clone();
            axum::Router::new().route(
                "/v1/chat/completions",
                axum::routing::post(move |_body: axum::Json<serde_json::Value>| {
                    let stub = stub.clone();
                    async move {
                        let payload = stub
                            .bodies
                            .lock()
                            .unwrap()
                            .pop_front()
                            .unwrap_or_else(|| text_payload("done"));
                        axum::Json(payload)
                    }
                }),
            )
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        (stub, format!("http://{addr}"))
    }
}

fn services() -> (PlanningServices, GraphContext) {
    let repo = Arc::new(OxigraphRepository::new_in_memory(50_000).unwrap());
    load_default_ontology(&*repo).unwrap();
    let reasoner = Arc::new(OntologyReasoner::from_repository(&*repo).unwrap());
    let search = Arc::new(needle_rdf_agent::search::TantivyIndex::in_ram().unwrap());
    let audit = Arc::new(AuditLog::in_memory());
    let state = ToolState::new(
        repo,
        search,
        reasoner,
        contact_shapes(),
        audit,
        AppLimits::default(),
    );
    let graph = GraphName::tenant("acme").unwrap();
    let exec_ctx = ExecutionContext::new(
        "req-ladder",
        "alice",
        "acme",
        vec![graph.clone(), GraphName::ontology()],
        vec![
            Operation::Read,
            Operation::Create,
            Operation::Update,
            Operation::Delete,
        ],
    );
    let graph_ctx = GraphContext {
        graphs: vec![graph],
        namespaces: [("ex".to_string(), "http://example.com/".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    (
        PlanningServices::new(Arc::new(ToolRegistry::new()), state, exec_ctx),
        graph_ctx,
    )
}

fn keyless_backend(base_url: &str, model: &str, max_tokens: u32) -> AgentBackend {
    std::env::remove_var("OPENAI_API_KEY");
    let cfg = config_from_vars(Some(base_url), Some(model), None, Some("30"))
        .expect("keyless ladder config");
    assert_eq!(cfg.key_mode, KeyMode::AbsentLocal);
    AgentBackend::new(build_backend(&cfg), cfg.model.clone()).with_max_tokens(max_tokens)
}

// ---------------------------------------------------------------------------
// B's tool-call script: one `add_step` turn per step, then `finish_plan`
// ---------------------------------------------------------------------------

fn rung_tool_script(rung: &Rung) -> Vec<serde_json::Value> {
    let ids: Vec<String> = (1..=rung.steps.len()).map(|i| format!("c{i}")).collect();
    let mut script: Vec<serde_json::Value> = rung
        .steps
        .iter()
        .enumerate()
        .map(|(i, s)| {
            tool_payload(&[(
                "add_step",
                serde_json::json!({
                    "id": s.id, "operation": s.operation, "intent": s.intent,
                    "args": s.args, "depends_on": s.depends_on, "output": s.output
                }),
                ids[i].as_str(),
            )])
        })
        .collect();
    script.push(tool_payload(&[(
        "finish_plan",
        serde_json::json!({"rationale": format!("ladder rung {} complete", rung.level)}),
        "cf",
    )]));
    script
}

fn assert_rung_shape(rung: &Rung, plan: &Plan) {
    validate_plan(plan).expect("ladder plan validates");
    assert_eq!(
        plan.steps.len(),
        rung.steps.len(),
        "L{}: step count",
        rung.level
    );
    for (got, want) in plan.steps.iter().zip(rung.steps.iter()) {
        assert_eq!(got.id, want.id, "L{}: step id", rung.level);
        assert_eq!(got.operation.as_str(), want.operation, "L{}: op", rung.level);
        let mut got_deps = got.depends_on.clone();
        got_deps.sort();
        let mut want_deps: Vec<String> =
            want.depends_on.iter().map(|s| s.to_string()).collect();
        want_deps.sort();
        assert_eq!(got_deps, want_deps, "L{}: deps of {}", rung.level, got.id);
    }
}

// ---------------------------------------------------------------------------
// Scripted climbs (default cargo test): every type through every rung
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ladder_b_tool_loop_climbs_all_rungs() {
    for rung in ladder() {
        let (planning, graph_ctx) = services();
        let (_stub, base) = QueueStub::serving(rung_tool_script(&rung)).await;
        let backend = keyless_backend(&base, "ladder-model-b", 2048);
        let planner = AgentToolPlanner::new(backend, planning);
        let request = UserRequest::new(rung.request);
        let mut ctx = OperationContext::new(request.text.clone(), graph_ctx);
        let plan = planner.create_plan(&request, &mut ctx).await.expect("L? plans");
        assert_rung_shape(&rung, &plan);
        println!("LADDER B L{} ({} steps): cleared", rung.level, rung.steps.len());
    }
}

#[tokio::test]
async fn ladder_c_structured_climbs_all_rungs() {
    for rung in ladder() {
        let (_planning, graph_ctx) = services();
        let (_stub, base) =
            QueueStub::serving(vec![text_payload(&rung_plan_json(&rung))]).await;
        let backend = keyless_backend(&base, "ladder-model-c", 2048);
        let planner = AgentStructuredPlanner::new(backend);
        let request = UserRequest::new(rung.request);
        let mut ctx = OperationContext::new(request.text.clone(), graph_ctx);
        let plan = planner.create_plan(&request, &mut ctx).await.expect("L? plans");
        assert_rung_shape(&rung, &plan);
        println!("LADDER C L{} ({} steps): cleared", rung.level, rung.steps.len());
    }
}

#[tokio::test]
async fn ladder_d_two_pass_climbs_all_rungs() {
    for rung in ladder() {
        let (_planning, graph_ctx) = services();
        let (_stub, base) = QueueStub::serving(vec![
            text_payload(&rung_prose(&rung)),
            text_payload(&rung_plan_json(&rung)),
        ])
        .await;
        let backend = keyless_backend(&base, "ladder-model-d", 2048);
        let planner = AgentTwoPassPlanner::new(backend);
        let request = UserRequest::new(rung.request);
        let mut ctx = OperationContext::new(request.text.clone(), graph_ctx);
        let plan = planner.create_plan(&request, &mut ctx).await.expect("L? plans");
        assert_rung_shape(&rung, &plan);
        println!("LADDER D L{} ({} steps): cleared", rung.level, rung.steps.len());
    }
}

/// Elicitation script for a rung, derived from the rung spec itself:
/// terms from the first vocabulary search, second terms from the second
/// search (L10), text from the first instance search — served in the
/// order the spine elicits them.
fn elicitation_script(rung: &Rung) -> Vec<serde_json::Value> {
    let terms_of = |n: usize| {
        rung.steps
            .iter()
            .filter(|s| s.operation == "search_vocabulary")
            .nth(n)
            .and_then(|s| s.args.get("terms"))
            .cloned()
    };
    let text = rung
        .steps
        .iter()
        .find(|s| s.operation == "search_instances")
        .and_then(|s| s.args.get("text"))
        .cloned();
    let mut script = vec![];
    if let Some(terms) = terms_of(0) {
        script.push(text_payload(&serde_json::json!({"terms": terms}).to_string()));
    }
    if let Some(terms) = terms_of(1) {
        script.push(text_payload(&serde_json::json!({"terms": terms}).to_string()));
    }
    if let Some(text) = text {
        script.push(text_payload(&serde_json::json!({"text": text}).to_string()));
    }
    script
}

#[tokio::test]
async fn ladder_s_spine_climbs_all_rungs() {
    for rung in ladder() {
        let (planning, graph_ctx) = services();
        let (_stub, base) = QueueStub::serving(elicitation_script(&rung)).await;
        let backend = keyless_backend(&base, "ladder-model-s", 2048);
        let planner = SpinePlanner::new(backend, planning);
        let request = UserRequest::new(rung.request);
        let mut ctx = OperationContext::new(request.text.clone(), graph_ctx);
        let plan = planner.create_plan(&request, &mut ctx).await.expect("L? plans");
        assert_rung_shape(&rung, &plan);
        assert!(
            ctx.artifacts.get("routing").map(|r| r.starts_with("spine:")).unwrap_or(false),
            "L{}: spine path, not fallback",
            rung.level
        );
        println!("LADDER S L{} ({} steps): cleared", rung.level, rung.steps.len());
    }
}

// ---------------------------------------------------------------------------
// Live climbs (ignored unless RUN_MODEL_TESTS=1): how high does the model get?
// ---------------------------------------------------------------------------

fn live_required() {
    assert!(
        std::env::var("RUN_MODEL_TESTS").map(|v| v == "1").unwrap_or(false),
        "live ladder requires RUN_MODEL_TESTS=1"
    );
}

fn server_base() -> String {
    std::env::var("OPENAI_BASE_URL").unwrap_or_else(|_| "http://localhost:8080".to_string())
}

fn server_model() -> String {
    std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "MiniCPM5-1B-F16".to_string())
}

fn live_backend(base_url: &str, model: &str, max_tokens: u32) -> AgentBackend {
    std::env::remove_var("OPENAI_API_KEY");
    let cfg = config_from_vars(Some(base_url), Some(model), None, Some("1800"))
        .expect("keyless live ladder config");
    assert_eq!(cfg.key_mode, KeyMode::AbsentLocal, "live runs are keyless");
    AgentBackend::new(build_backend(&cfg), cfg.model.clone()).with_max_tokens(max_tokens)
}

/// `Ok(())` when the rung produced a validating plan with the rung's
/// shape; `Err(class)` with the typed outcome otherwise.
/// Transport-level failures panic: the live policy requires a clean channel.
fn check_live_result(
    name: &str,
    rung: &Rung,
    result: Result<Plan, PlannerError>,
) -> Result<(), String> {
    match result {
        Ok(plan) => match validate_plan(&plan) {
            Ok(()) if plan.steps.len() == rung.steps.len() => Ok(()),
            // Over-plan counter (Phase 1, item 5): validating but longer
            // than the rung is measurement, not success — the baseline
            // phase 2 must beat.
            Ok(()) if plan.steps.len() > rung.steps.len() => Err(format!(
                "OverPlan({} steps, want {})",
                plan.steps.len(),
                rung.steps.len()
            )),
            Ok(()) => Err(format!(
                "UnderPlan({} steps, want {})",
                plan.steps.len(),
                rung.steps.len()
            )),
            Err(errs) => Err(format!("InvalidPlan({})", errs[0])),
        },
        Err(e) => {
            let msg = format!("{e:?}");
            let transport = [
                "timeout", "timed out", "connection", "hyper", "reqwest", "transport", "dns",
                "TcpStream", "pool",
            ];
            if transport.iter().any(|m| msg.contains(m)) {
                panic!("LIVE {name} L{}: transport failure (must be clean): {e:?}", rung.level);
            }
            Err(outcome_class(&e))
        }
    }
}

fn outcome_class(e: &PlannerError) -> String {
    let s = format!("{e:?}");
    for class in ["Budget", "Schema", "Model", "Consistency", "Clarification", "Plan"] {
        if s.contains(class) {
            return class.to_string();
        }
    }
    "Other".to_string()
}

/// Max tokens per live call anywhere in the ladder: 4092. The 1B model's
/// reasoning fills whatever context it gets; this keeps each attempt to a
/// few minutes while leaving ample room for short plans.
const LIVE_MAX_TOKENS: u32 = 4092;

async fn climb_live_b() -> (u8, Vec<String>) {
    let mut cleared = 0u8;
    let mut log = vec![];
    for rung in ladder() {
        let (planning, graph_ctx) = services();
        let backend = live_backend(&server_base(), &server_model(), LIVE_MAX_TOKENS);
        let planner = AgentToolPlanner::new(backend, planning);
        let request = UserRequest::new(rung.request);
        let mut ctx = OperationContext::new(request.text.clone(), graph_ctx);
        let result = planner.create_plan(&request, &mut ctx).await;
        match check_live_result("B", &rung, result) {
            Ok(()) => {
                cleared = rung.level;
                log.push(format!("LIVE B L{}: CLEARED", rung.level));
            }
            Err(class) => {
                log.push(format!("LIVE B L{}: broke ({class})", rung.level));
                break;
            }
        }
    }
    (cleared, log)
}

async fn climb_live_c() -> (u8, Vec<String>) {
    let mut cleared = 0u8;
    let mut log = vec![];
    for rung in ladder() {
        let (_planning, graph_ctx) = services();
        let backend = live_backend(&server_base(), &server_model(), LIVE_MAX_TOKENS);
        let planner = AgentStructuredPlanner::new(backend);
        let request = UserRequest::new(rung.request);
        let mut ctx = OperationContext::new(request.text.clone(), graph_ctx);
        let result = planner.create_plan(&request, &mut ctx).await;
        match check_live_result("C", &rung, result) {
            Ok(()) => {
                cleared = rung.level;
                log.push(format!("LIVE C L{}: CLEARED", rung.level));
            }
            Err(class) => {
                log.push(format!("LIVE C L{}: broke ({class})", rung.level));
                break;
            }
        }
    }
    (cleared, log)
}

async fn climb_live_d() -> (u8, Vec<String>) {
    let mut cleared = 0u8;
    let mut log = vec![];
    for rung in ladder() {
        let (_planning, graph_ctx) = services();
        let backend = live_backend(&server_base(), &server_model(), LIVE_MAX_TOKENS);
        let planner = AgentTwoPassPlanner::new(backend);
        let request = UserRequest::new(rung.request);
        let mut ctx = OperationContext::new(request.text.clone(), graph_ctx);
        let result = planner.create_plan(&request, &mut ctx).await;
        match check_live_result("D", &rung, result) {
            Ok(()) => {
                cleared = rung.level;
                log.push(format!("LIVE D L{}: CLEARED", rung.level));
            }
            Err(class) => {
                log.push(format!("LIVE D L{}: broke ({class})", rung.level));
                break;
            }
        }
    }
    (cleared, log)
}

#[tokio::test]
#[ignore]
async fn live_ladder_b_climb() {
    live_required();
    let (cleared, log) = climb_live_b().await;
    for line in &log {
        println!("{line}");
    }
    println!("LIVE B ladder: highest cleared rung = L{cleared}/10");
}

#[tokio::test]
#[ignore]
async fn live_ladder_c_climb() {
    live_required();
    let (cleared, log) = climb_live_c().await;
    for line in &log {
        println!("{line}");
    }
    println!("LIVE C ladder: highest cleared rung = L{cleared}/10");
}

async fn climb_live_s() -> (u8, Vec<String>) {
    let mut cleared = 0u8;
    let mut log = vec![];
    for rung in ladder() {
        let (planning, graph_ctx) = services();
        let backend = live_backend(&server_base(), &server_model(), LIVE_MAX_TOKENS);
        let planner = SpinePlanner::new(backend, planning);
        let request = UserRequest::new(rung.request);
        let mut ctx = OperationContext::new(request.text.clone(), graph_ctx);
        let result = planner.create_plan(&request, &mut ctx).await;
        let routing = ctx.artifacts.get("routing").cloned().unwrap_or_default();
        match check_live_result("S", &rung, result) {
            Ok(()) => {
                cleared = rung.level;
                log.push(format!("LIVE S L{}: CLEARED ({routing})", rung.level));
            }
            Err(class) => {
                log.push(format!("LIVE S L{}: broke ({class}, {routing})", rung.level));
                break;
            }
        }
    }
    (cleared, log)
}

#[tokio::test]
#[ignore]
async fn live_ladder_s_climb() {
    live_required();
    let (cleared, log) = climb_live_s().await;
    for line in &log {
        println!("{line}");
    }
    println!("LIVE S ladder: highest cleared rung = L{cleared}/10");
}

#[tokio::test]
#[ignore]
async fn live_ladder_d_climb() {
    live_required();
    let (cleared, log) = climb_live_d().await;
    for line in &log {
        println!("{line}");
    }
    println!("LIVE D ladder: highest cleared rung = L{cleared}/10");
}
