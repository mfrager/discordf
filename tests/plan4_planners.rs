//! Plan 2 Phase 4 acceptance: B/C/D planners against a keyless local
//! OpenAI-compatible endpoint (no `OPENAI_API_KEY`, no weights).
//!
//! A queue-based stub serves canned `chat/completions` payloads verbatim;
//! the planners run through the real `agentstream` HTTP backend built from
//! `load_llm_config`-style vars with no key configured.

use needle_rdf_agent::config::AppLimits;
use needle_rdf_agent::llm::{build_backend, config_from_vars, KeyMode};
use needle_rdf_agent::ontology::{contact_shapes, load_default_ontology, OntologyReasoner};
use needle_rdf_agent::plan::{validate_plan, GraphContext, OperationContext};
use needle_rdf_agent::planner::{
    AgentBackend, AgentStructuredPlanner, AgentToolPlanner, AgentTwoPassPlanner, Planner,
    PlanningServices, UserRequest,
};
use needle_rdf_agent::policy::{AuditLog, ExecutionContext, Operation};
use needle_rdf_agent::rdf::{GraphName, OxigraphRepository};
use needle_rdf_agent::tools::{ToolRegistry, ToolState};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

fn valid_plan_json() -> String {
    serde_json::json!({
        "goal": "Classify Ohio contacts",
        "steps": [
            {"id": "s1", "operation": "search_vocabulary", "intent": "discover terms",
             "args": {"kind": "search_vocab", "terms": ["contact", "status"]},
             "depends_on": [], "output": "VocabularyCandidates"},
            {"id": "s2", "operation": "describe_vocabulary", "intent": "resolve domains",
             "args": {"kind": "describe_vocab", "iris": [{"kind": "step_ref", "step": "s1", "field": "candidates"}]},
             "depends_on": ["s1"], "output": "ResolvedVocabulary"},
            {"id": "s3", "operation": "query", "intent": "find matching contacts",
             "args": {"kind": "query", "pattern": {}},
             "depends_on": ["s2"], "output": "UriSet"},
            {"id": "s4", "operation": "update", "intent": "classify contacts",
             "args": {"kind": "update",
                     "targets": [{"kind": "uri_set_ref", "step": "s3"}],
                     "predicate": {"kind": "unresolved", "description": "status"},
                     "objects": [{"kind": "unresolved", "description": "prospect"}]},
             "depends_on": ["s2", "s3"], "output": "MutationResult"},
            {"id": "s5", "operation": "validate", "intent": "check graph",
             "args": {"kind": "validate"},
             "depends_on": ["s4"], "output": "ValidationResult"},
            {"id": "s6", "operation": "verify", "intent": "count classified",
             "args": {"kind": "verify", "question": "How many contacts were classified?"},
             "depends_on": ["s5"], "output": "VerificationResult"}
        ]
    })
    .to_string()
}

fn valid_prose() -> &'static str {
    "1. Search the vocabulary for contact and status terms.\n\
    2. Describe the discovered terms to resolve their domains; depends on step 1.\n\
    3. Query for the matching contacts; depends on step 2.\n\
    4. Update the matching contacts with the prospect classification; depends on step 2 and depends on step 3.\n\
    5. Validate the resulting graph; depends on step 4.\n\
    6. Verify the number of classified contacts; depends on step 5."
}

/// A chat-completions payload carrying plain assistant text.
fn text_payload(text: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "chatcmpl-test",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": text},
            "finish_reason": "stop"
        }],
        "usage": {}
    })
}

/// A chat-completions payload carrying OpenAI tool calls.
fn tool_payload(calls: &[(&str, serde_json::Value, &str)]) -> serde_json::Value {
    serde_json::json!({
        "id": "chatcmpl-test",
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

/// Minimal OpenAI-compatible stub: serves queued payloads verbatim at
/// `POST /v1/chat/completions` and records the `model` each request asked
/// for (proves the `OPENAI_MODEL` id propagates end to end).
struct QueueStub {
    bodies: Mutex<VecDeque<serde_json::Value>>,
    seen_models: Mutex<Vec<String>>,
}

impl QueueStub {
    async fn serving(bodies: Vec<serde_json::Value>) -> (Arc<Self>, String) {
        let stub = Arc::new(Self {
            bodies: Mutex::new(bodies.into()),
            seen_models: Mutex::new(vec![]),
        });
        let app = {
            let stub = stub.clone();
            axum::Router::new().route(
                "/v1/chat/completions",
                axum::routing::post(move |body: axum::Json<serde_json::Value>| {
                    let stub = stub.clone();
                    async move {
                        if let Some(m) = body.get("model").and_then(|v| v.as_str()) {
                            stub.seen_models.lock().unwrap().push(m.to_string());
                        }
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
        // Bound socket accepts immediately; give the accept loop a tick.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        (stub, format!("http://{addr}"))
    }

    fn models(&self) -> Vec<String> {
        self.seen_models.lock().unwrap().clone()
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
        "req-1",
        "alice",
        "acme",
        vec![graph.clone(), GraphName::ontology()],
        vec![Operation::Read],
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

fn keyless_backend(base_url: &str, model: &str) -> AgentBackend {
    // No key on purpose: the local stub must work keyless (Phase 1 rule).
    let cfg =
        config_from_vars(Some(base_url), Some(model), None, Some("10")).expect("keyless config");
    assert_eq!(cfg.key_mode, KeyMode::AbsentLocal);
    let backend = build_backend(&cfg);
    AgentBackend::new(backend, cfg.model.clone())
}

#[tokio::test]
async fn structured_planner_works_keyless_against_stub() {
    let (_services, graph_ctx) = services();
    let (stub, base) = QueueStub::serving(vec![text_payload(&valid_plan_json())]).await;
    // Explicitly unset: CI must not depend on ambient keys.
    std::env::remove_var("OPENAI_API_KEY");
    let backend = keyless_backend(&base, "ci-model-c");
    let planner = AgentStructuredPlanner::new(backend);
    let request = UserRequest::new("Classify Ohio contacts");
    let mut ctx = OperationContext::new(request.text.clone(), graph_ctx);
    let plan = planner.create_plan(&request, &mut ctx).await.unwrap();
    assert_eq!(plan.steps.len(), 6);
    validate_plan(&plan).unwrap();
    assert_eq!(stub.models().as_slice(), ["ci-model-c"]);
}

#[tokio::test]
async fn two_pass_planner_works_keyless_and_retains_artifacts() {
    let (_services, graph_ctx) = services();
    let (stub, base) = QueueStub::serving(vec![
        text_payload(valid_prose()),
        text_payload(&valid_plan_json()),
    ]).await;
    std::env::remove_var("OPENAI_API_KEY");
    let backend = keyless_backend(&base, "ci-model-d");
    let planner = AgentTwoPassPlanner::new(backend);
    let request = UserRequest::new("Classify Ohio contacts");
    let mut ctx = OperationContext::new(request.text.clone(), graph_ctx);
    let plan = planner.create_plan(&request, &mut ctx).await.unwrap();
    assert_eq!(plan.steps.len(), 6);
    assert_eq!(ctx.artifact("nl_plan"), Some(valid_prose()));
    assert!(ctx
        .artifact("structured_plan")
        .expect("structured artifact")
        .contains("s6"));
    assert_eq!(stub.models().as_slice(), ["ci-model-d", "ci-model-d"]);
}

#[tokio::test]
async fn tool_loop_planner_works_keyless_with_tool_calls() {
    let (services, graph_ctx) = services();
    let add_s1 = serde_json::json!({
        "id": "s1", "operation": "search_vocabulary",
        "intent": "discover contact and status vocabulary",
        "args": {"kind": "search_vocab", "terms": ["contact", "status"]},
        "depends_on": [], "output": "VocabularyCandidates"
    });
    let add_s2 = serde_json::json!({
        "id": "s2", "operation": "describe_vocabulary",
        "intent": "resolve domains",
        "args": {"kind": "describe_vocab",
                "iris": [{"kind": "step_ref", "step": "s1", "field": "candidates"}]},
        "depends_on": ["s1"], "output": "ResolvedVocabulary"
    });
    let finish = serde_json::json!({"rationale": "grounded discovery plan"});
    let (stub, base) = QueueStub::serving(vec![
        tool_payload(&[("add_step", add_s1, "c1")]),
        tool_payload(&[("add_step", add_s2, "c2")]),
        tool_payload(&[("finish_plan", finish, "c3")]),
    ]).await;
    std::env::remove_var("OPENAI_API_KEY");
    let backend = keyless_backend(&base, "ci-model-b");
    let planner = AgentToolPlanner::new(backend, services);
    let request = UserRequest::new("Classify Ohio contacts");
    let mut ctx = OperationContext::new(request.text.clone(), graph_ctx);
    let plan = planner.create_plan(&request, &mut ctx).await.unwrap();
    assert_eq!(plan.steps.len(), 2);
    assert_eq!(plan.steps[1].depends_on, vec!["s1".to_string()]);
    validate_plan(&plan).unwrap();
    assert!(stub.models().iter().all(|m| m == "ci-model-b"));
    assert_eq!(stub.models().len(), 3);
}
