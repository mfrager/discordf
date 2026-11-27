//! Phase 1 integration tests (keyless, scripted): per-operation tools
//! end to end through the real B loop, kind-default through the generic
//! path, exact-args plans through C, and planner-stats artifacts.
//!
//! The live ablation (`ladder_cli --rung 1`) decides whether the model
//! can use these paths; these tests pin the Rust side keylessly.

use needle_rdf_agent::config::AppLimits;
use needle_rdf_agent::ladder_spec::ladder;
use needle_rdf_agent::llm::{build_backend, config_from_vars, KeyMode};
use needle_rdf_agent::ontology::{contact_shapes, load_default_ontology, OntologyReasoner};
use needle_rdf_agent::plan::{validate_plan, GraphContext, Operation, OperationContext};
use needle_rdf_agent::planner::{
    per_op_add_tool, AgentBackend, AgentStructuredPlanner, AgentToolPlanner, Planner,
    PlanningServices, UserRequest,
};
use needle_rdf_agent::policy::{AuditLog, ExecutionContext, Operation as PolicyOp};
use needle_rdf_agent::rdf::{GraphName, OxigraphRepository};
use needle_rdf_agent::tools::{ToolRegistry, ToolState};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

fn text_payload(text: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "chatcmpl-perop",
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
        "id": "chatcmpl-perop",
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
        let stub = Arc::new(Self { bodies: Mutex::new(bodies.into()) });
        let app = {
            let stub = stub.clone();
            axum::Router::new().route(
                "/v1/chat/completions",
                axum::routing::post(move |_body: axum::Json<serde_json::Value>| {
                    let stub = stub.clone();
                    async move {
                        let payload = stub.bodies.lock().unwrap().pop_front()
                            .unwrap_or_else(|| text_payload("done"));
                        axum::Json(payload)
                    }
                }),
            )
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
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
    let state = ToolState::new(repo, search, reasoner, contact_shapes(), audit, AppLimits::default());
    let graph = GraphName::tenant("acme").unwrap();
    let exec_ctx = ExecutionContext::new(
        "req-perop", "alice", "acme",
        vec![graph.clone(), GraphName::ontology()],
        vec![PolicyOp::Read, PolicyOp::Create, PolicyOp::Update, PolicyOp::Delete],
    );
    let graph_ctx = GraphContext {
        graphs: vec![graph],
        namespaces: [("ex".to_string(), "http://example.com/".to_string())].into_iter().collect(),
        ..Default::default()
    };
    (PlanningServices::new(Arc::new(ToolRegistry::new()), state, exec_ctx), graph_ctx)
}

fn keyless_backend(base_url: &str, max_tokens: u32) -> AgentBackend {
    std::env::remove_var("OPENAI_API_KEY");
    let cfg = config_from_vars(Some(base_url), Some("perop-model"), None, Some("30")).expect("keyless");
    assert_eq!(cfg.key_mode, KeyMode::AbsentLocal);
    AgentBackend::new(build_backend(&cfg), cfg.model.clone()).with_max_tokens(max_tokens)
}

fn stats(ctx: &OperationContext) -> serde_json::Value {
    serde_json::from_str(ctx.artifacts.get("planner_stats").expect("planner_stats artifact")).expect("stats json")
}

/// Per-op tool script for a rung, with `kind` stripped from every args
/// object: proves Rust fills the discriminator from the tool name.
fn per_op_script(level: u8) -> Vec<serde_json::Value> {
    let rung = ladder().into_iter().find(|r| r.level == level).unwrap();
    let ids: Vec<String> = (0..rung.steps.len()).map(|i| format!("c{i}")).collect();
    let mut script: Vec<serde_json::Value> = rung.steps.iter().enumerate().map(|(i, s)| {
        let op: Operation = serde_json::from_value(serde_json::Value::String(s.operation.to_string())).unwrap();
        let mut args = s.args.clone();
        args.as_object_mut().unwrap().remove("kind");
        tool_payload(&[(
            per_op_add_tool(op),
            serde_json::json!({"id": s.id, "intent": s.intent, "args": args, "depends_on": s.depends_on}),
            ids[i].as_str(),
        )])
    }).collect();
    script.push(tool_payload(&[("finish_plan", serde_json::json!({"rationale": "per-op climb"}), "cf")]));
    script
}

#[tokio::test]
async fn per_op_tools_climb_l1_with_kindless_args() {
    let (planning, graph_ctx) = services();
    let (_stub, base) = QueueStub::serving(per_op_script(1)).await;
    let planner = AgentToolPlanner::new(keyless_backend(&base, 2048), planning);
    let request = UserRequest::new("List the vocabulary terms for contacts.");
    let mut ctx = OperationContext::new(request.text.clone(), graph_ctx);
    let plan = planner.create_plan(&request, &mut ctx).await.unwrap();
    validate_plan(&plan).unwrap();
    assert_eq!(plan.steps.len(), 1);
    assert_eq!(stats(&ctx)["fallback_uses"], 0);
}

#[tokio::test]
async fn per_op_tools_climb_l7_update_spine() {
    let (planning, graph_ctx) = services();
    let (_stub, base) = QueueStub::serving(per_op_script(7)).await;
    let planner = AgentToolPlanner::new(keyless_backend(&base, 2048), planning);
    let request = UserRequest::new("Classify Ohio contacts as prospects.");
    let mut ctx = OperationContext::new(request.text.clone(), graph_ctx);
    let plan = planner.create_plan(&request, &mut ctx).await.unwrap();
    validate_plan(&plan).unwrap();
    assert_eq!(plan.steps.len(), 6);
    assert_eq!(stats(&ctx)["fallback_uses"], 0);
}

#[tokio::test]
async fn generic_fallback_with_kindless_args_still_validates() {
    // The live L1 failure shape: generic add_step, kindless args.
    let (planning, graph_ctx) = services();
    let add = serde_json::json!({
        "id": "s1", "operation": "search_vocabulary", "intent": "discover",
        "args": {"terms": ["contact"]}, "depends_on": [],
        "output": "VocabularyCandidates"
    });
    let (_stub, base) = QueueStub::serving(vec![
        tool_payload(&[("add_step", add, "c1")]),
        tool_payload(&[("finish_plan", serde_json::json!({"rationale": "done"}), "cf")]),
    ]).await;
    let planner = AgentToolPlanner::new(keyless_backend(&base, 2048), planning);
    let request = UserRequest::new("List the vocabulary terms for contacts.");
    let mut ctx = OperationContext::new(request.text.clone(), graph_ctx);
    let plan = planner.create_plan(&request, &mut ctx).await.unwrap();
    validate_plan(&plan).unwrap();
    assert_eq!(plan.steps.len(), 1);
    assert_eq!(stats(&ctx)["fallback_uses"], 1, "generic path counts as fallback");
}

#[tokio::test]
async fn structured_path_accepts_exact_kind_args() {
    // C with schema-exact args (kind present): validates first try.
    let (_planning, graph_ctx) = services();
    let plan_json = serde_json::json!({
        "goal": "List the vocabulary terms for contacts.",
        "steps": [{
            "id": "s1", "operation": "search_vocabulary", "intent": "discover",
            "args": {"kind": "search_vocab", "terms": ["contact"]},
            "depends_on": [], "output": "VocabularyCandidates"
        }]
    }).to_string();
    let (_stub, base) = QueueStub::serving(vec![text_payload(&plan_json)]).await;
    let planner = AgentStructuredPlanner::new(keyless_backend(&base, 2048));
    let request = UserRequest::new("List the vocabulary terms for contacts.");
    let mut ctx = OperationContext::new(request.text.clone(), graph_ctx);
    let plan = planner.create_plan(&request, &mut ctx).await.unwrap();
    validate_plan(&plan).unwrap();
    assert_eq!(plan.steps.len(), 1);
}

#[tokio::test]
async fn structured_path_defaults_missing_kind() {
    // C with kindless args: kind-default normalization (item 3) applies.
    let (_planning, graph_ctx) = services();
    let plan_json = serde_json::json!({
        "goal": "List the vocabulary terms for contacts.",
        "steps": [{
            "id": "s1", "operation": "search_vocabulary", "intent": "discover",
            "args": {"terms": ["contact"]},
            "depends_on": [], "output": "VocabularyCandidates"
        }]
    }).to_string();
    let (_stub, base) = QueueStub::serving(vec![text_payload(&plan_json)]).await;
    let planner = AgentStructuredPlanner::new(keyless_backend(&base, 2048));
    let request = UserRequest::new("List the vocabulary terms for contacts.");
    let mut ctx = OperationContext::new(request.text.clone(), graph_ctx);
    let plan = planner.create_plan(&request, &mut ctx).await.unwrap();
    validate_plan(&plan).unwrap();
    assert_eq!(plan.steps.len(), 1);
}
