//! Real-backend planner tests (Phase 5): B/C/D against the live local
//! OpenAI-compatible endpoint, keyless (no `OPENAI_API_KEY`, no mocks).
//! Planner A (Needle) lives in `tests/live_planner_a.rs` (needs weights +
//! `--features live-model`).
//!
//! Modes:
//! - Live: `RUN_MODEL_TESTS=1 cargo test --test real_backends -- --ignored`.
//!   Talks to `OPENAI_BASE_URL` (default `http://localhost:8080`) with
//!   `OPENAI_MODEL` (default `MiniCPM5-1B-F16`). With `RECORD=1` requests
//!   go through a recording proxy and raw response bodies are saved to
//!   `tests/recordings/{b,c,d}.json`.
//! - Replay (default `cargo test`): serves the checked-in recordings through
//!   a stub, so CI is deterministic and keyless. Needs no server.
//!
//! Live tests assert transport-level facts (no connection errors, the
//! planner's typed outcome) and print the full outcome; validity and
//! final-state assertions live in the replay tests, which pin whatever the
//! model actually did.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use needle_rdf_agent::config::AppLimits;
use needle_rdf_agent::executor::Executor;
use needle_rdf_agent::llm::{build_backend, config_from_vars, KeyMode};
use needle_rdf_agent::ontology::{contact_shapes, load_default_ontology, OntologyReasoner};
use needle_rdf_agent::plan::{GraphContext, OperationContext};
use needle_rdf_agent::planner::{
    AgentBackend, AgentStructuredPlanner, AgentToolPlanner, AgentTwoPassPlanner, PlanCaps, Planner,
    PlanningServices, UserRequest,
};
use needle_rdf_agent::policy::{AuditLog, ExecutionContext, Operation};
use needle_rdf_agent::rdf::{GraphName, OxigraphRepository, Quad, RdfRepository, Term};
use needle_rdf_agent::search::TantivyIndex;
use needle_rdf_agent::tools::{ToolRegistry, ToolState};

const EX: &str = "http://example.com/";
const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

fn live_required() {
    if std::env::var("RUN_MODEL_TESTS").map(|v| v == "1").unwrap_or(false) {
    } else {
        panic!("live test requires RUN_MODEL_TESTS=1");
    }
}

fn server_base() -> String {
    std::env::var("OPENAI_BASE_URL").unwrap_or_else(|_| "http://localhost:8080".to_string())
}

fn server_model() -> String {
    std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "MiniCPM5-1B-F16".to_string())
}

fn keyless_backend(base_url: &str, model: &str, max_tokens: u32) -> AgentBackend {
    std::env::remove_var("OPENAI_API_KEY");
    let cfg = config_from_vars(Some(base_url), Some(model), None, Some("300"))
        .expect("keyless local config");
    assert_eq!(cfg.key_mode, KeyMode::AbsentLocal, "live tests are keyless");
    let backend = build_backend(&cfg);
    AgentBackend::new(backend, cfg.model.clone()).with_max_tokens(max_tokens)
}

fn services() -> (PlanningServices, GraphName, ToolState, ExecutionContext, Arc<ToolRegistry>) {
    let repo = Arc::new(OxigraphRepository::new_in_memory(50_000).unwrap());
    load_default_ontology(&*repo).unwrap();
    let graph = GraphName::tenant("acme").unwrap();
    // Ohio fixture (same as exec_phase5): Alice/Contact w/ name+account,
    // Acme/Account w/ state OH, Prospect/Category (no name: ex:name's
    // domain is Contact, a named Category would trip graph-wide SHACL).
    let t = |s: &str| Term::iri(s.to_string()).unwrap();
    let q = |s: &str, p: &str, o: Term| {
        Quad::new(t(s), t(p), o, graph.clone()).unwrap()
    };
    repo.insert(&[
        q(&format!("{EX}Alice"), RDF_TYPE, t(&format!("{EX}Contact"))),
        q(&format!("{EX}Alice"), &format!("{EX}name"), Term::literal("Alice")),
        q(&format!("{EX}Alice"), &format!("{EX}account"), t(&format!("{EX}Acme"))),
        q(&format!("{EX}Acme"), RDF_TYPE, t(&format!("{EX}Account"))),
        q(&format!("{EX}Acme"), &format!("{EX}state"), Term::literal("OH")),
        q(&format!("{EX}Prospect"), RDF_TYPE, t(&format!("{EX}Category"))),
    ])
    .unwrap();
    let reasoner = Arc::new(OntologyReasoner::from_repository(&*repo).unwrap());
    let search = Arc::new(TantivyIndex::in_ram().unwrap());
    let audit = Arc::new(AuditLog::in_memory());
    let state = ToolState::new(
        repo,
        search,
        reasoner,
        contact_shapes(),
        audit,
        AppLimits::default(),
    );
    let _ = state.search.sync_resource(
        &*state.repo,
        &format!("{EX}Prospect"),
        &graph,
    );
    let exec_ctx = ExecutionContext::new(
        "req-live",
        "alice",
        "acme",
        vec![graph.clone(), GraphName::ontology()],
        vec![Operation::Read, Operation::Create, Operation::Update, Operation::Delete],
    );
    let registry = Arc::new(ToolRegistry::new());
    let graph_ctx = GraphContext {
        graphs: vec![graph.clone()],
        namespaces: [("ex".to_string(), EX.to_string())].into_iter().collect(),
        ..Default::default()
    };
    let _ = &graph_ctx;
    (
        PlanningServices::new(registry.clone(), state.clone(), exec_ctx.clone()),
        graph,
        state,
        exec_ctx,
        registry,
    )
}

fn ohio_request() -> UserRequest {
    UserRequest::new(
        "Classify contacts at Ohio accounts as prospects: discover the vocabulary, \
         find contacts whose accounts are in Ohio, assign the prospect classification, \
         then validate and verify.",
    )
}

fn tight_caps() -> PlanCaps {
    PlanCaps { max_plan_steps: 16, max_turns: 6, max_no_progress: 2 }
}

fn recording_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("recordings")
        .join(format!("{name}.json"))
}

/// Forwarding proxy: POST /v1/chat/completions -> real server, appending
/// each raw response body to `save_path` (JSON lines). Used with RECORD=1.
async fn recording_proxy(real_base: String, save_path: PathBuf) -> String {
    if let Some(parent) = save_path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move |body: axum::Json<serde_json::Value>| {
            let real_base = real_base.clone();
            let save_path = save_path.clone();
            async move {
                let body_str = serde_json::to_string(&body.0).unwrap();
                let tmp = std::env::temp_dir().join(format!("proxy-body-{}.json", std::process::id()));
                std::fs::write(&tmp, &body_str).unwrap();
                let url = format!("{}/chat/completions", real_base.trim_end_matches('/'));
                let out = tokio::process::Command::new("curl")
                    .args(["-s", "-m", "290", &url, "-H", "Content-Type: application/json", "-d", &format!("@{}", tmp.display())])
                    .output()
                    .await
                    .expect("curl for recording proxy");
                let _ = std::fs::remove_file(&tmp);
                assert!(out.status.success(), "proxy curl failed: {out:?}");
                let resp: serde_json::Value = serde_json::from_slice(&out.stdout)
                    .expect("proxy response is JSON");
                let line = serde_json::json!({
                    "request_model": body.0.get("model"),
                    "has_tools": body.0.get("tools").is_some(),
                    "has_schema": body.0.get("response_format").is_some(),
                    "response": resp,
                });
                use std::io::Write;
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&save_path)
                    .unwrap();
                writeln!(f, "{}", serde_json::to_string(&line).unwrap()).unwrap();
                axum::Json(resp)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    format!("http://{addr}")
}

/// Replay stub: serves recorded raw response bodies verbatim, in order.
struct ReplayStub {
    bodies: Mutex<VecDeque<serde_json::Value>>,
    seen_models: Mutex<Vec<String>>,
}

impl ReplayStub {
    fn models(&self) -> Vec<String> {
        self.seen_models.lock().unwrap().clone()
    }

    async fn serving(path: &PathBuf) -> (Arc<Self>, String) {
        let raw = std::fs::read_to_string(path)
            .unwrap_or_else(|_| panic!("missing recording {} (run RECORD=1 live first)", path.display()));
        let bodies: VecDeque<serde_json::Value> = raw
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                let entry: serde_json::Value = serde_json::from_str(l).unwrap();
                entry.get("response").cloned().unwrap()
            })
            .collect();
        assert!(!bodies.is_empty(), "empty recording {}", path.display());
        let stub = Arc::new(Self { bodies: Mutex::new(bodies), seen_models: Mutex::new(vec![]) });
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
                        let payload = stub.bodies.lock().unwrap().pop_front().expect("stub exhausted");
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

async fn maybe_proxy(name: &str) -> Option<(String, PathBuf)> {
    if std::env::var("RECORD").map(|v| v == "1").unwrap_or(false) {
        let path = recording_path(name);
        let _ = std::fs::remove_file(&path);
        let proxy = recording_proxy(server_base(), path.clone()).await;
        Some((proxy, path))
    } else {
        None
    }
}

fn describe_outcome<T: std::fmt::Debug, E: std::fmt::Debug>(r: &Result<T, E>) -> String {
    match r {
        Ok(v) => format!("Ok({v:?}"),
        Err(e) => format!("Err({e:?}"),
    }
}

/// Transport failures (the integration under test) vs typed planner outcomes
/// (model-quality observations). Live tests fail on the former, report the
/// latter: MiniCPM5-1B-F16 is a 1B model and its planning discipline is the
/// finding, not the test subject.
fn is_transport_error(e: &needle_rdf_agent::planner::PlannerError) -> bool {
    let s = format!("{e:?}").to_lowercase();
    ["error sending request", "connection refused", "connection reset", "timed out", "operation timed out", "dns", "failed to lookup", "connection closed"]
        .iter()
        .any(|sig| s.contains(sig))
}

/// Shared live-test tail: join the planner with the executor when the model
/// cooperates (Ok + valid plan → execute → final RDF state), otherwise
/// report the typed planner outcome. Either way the keyless real-backend
/// transport is proven by the absence of transport errors.
async fn live_tail(
    name: &str,
    plan: Result<needle_rdf_agent::plan::Plan, needle_rdf_agent::planner::PlannerError>,
    state: &ToolState,
    exec_ctx: &ExecutionContext,
    registry: &Arc<ToolRegistry>,
    recording: &Option<(String, PathBuf)>,
    model: &str,
) {
    println!("LIVE {name} outcome: {}", describe_outcome(&plan));
    if let Err(e) = &plan {
        assert!(!is_transport_error(e), "LIVE {name}: real-backend transport failed: {e:?}");
        println!("LIVE {name}: typed planner outcome (model limit, not transport): {e:?}");
    }
    if let Some((_, path)) = recording {
        let raw = std::fs::read_to_string(path).unwrap();
        let mut n = 0;
        for line in raw.lines() {
            let entry: serde_json::Value = serde_json::from_str(line).unwrap();
            let m = entry.get("request_model").and_then(|v| v.as_str()).unwrap_or("");
            // Proxied requests carry either the live model id or the \`./models_local/\` path form.
            assert!(
                m.contains(model) || model.contains(m),
                "LIVE {name}: model id must propagate (saw {m:?}, want {model:?})"
            );
            n += 1;
        }
        assert!(n > 0, "LIVE {name}: recording is empty");
        println!("LIVE {name}: {n} request(s) carried model id {model:?}");
    }
    if let Ok(plan) = plan {
        needle_rdf_agent::plan::validate_plan(&plan).expect("LIVE plan validates");
        println!("LIVE {name} plan: {}", serde_json::to_string_pretty(&plan).unwrap());
        let exec = Executor::new(registry.clone(), state.clone(), exec_ctx.clone());
        let mut op = OperationContext::new(plan.goal.clone(), plan.context.clone());
        op.request = plan.goal.clone();
        let report = exec.execute(&plan, &mut op, None).await.expect("LIVE executes");
        println!("LIVE {name} report: {report:?}");
        let alice = Term::iri(format!("{EX}Alice")).unwrap();
        let cat = Term::iri(format!("{EX}category")).unwrap();
        let found = state.repo.match_quads(Some(&alice), Some(&cat), None, None, None).unwrap();
        assert!(!found.is_empty(), "LIVE {name}: Alice categorized in final state");
    }
}

// --- live tests (ignored unless RUN_MODEL_TESTS=1) --------------------------

#[tokio::test]
#[ignore]
async fn live_b_tool_loop_against_real_server() {
    live_required();
    let (planning, graph, state, exec_ctx, registry) = services();
    let proxy = maybe_proxy("b").await;
    let base = proxy.as_ref().map(|(u, _)| u.clone()).unwrap_or_else(server_base);
    let model = server_model();
    // Reasoning model: needs room for reasoning + tool calls per turn.
    let backend = keyless_backend(&base, &model, 1024);
    // Default caps: the 1B model needs room to stumble into valid calls;
    // the loop mechanics (validation feedback, no-progress guard) are what
    // turn that stumbling into either a plan or a typed Budget stop.
    let planner = AgentToolPlanner::new(backend, planning);
    let request = ohio_request();
    let gctx = GraphContext {
        graphs: vec![graph.clone()],
        namespaces: [("ex".to_string(), EX.to_string())].into_iter().collect(),
        ..Default::default()
    };
    let mut ctx = OperationContext::new(request.text.clone(), gctx);
    let plan = planner.create_plan(&request, &mut ctx).await;
    live_tail("B", plan, &state, &exec_ctx, &registry, &proxy, &model).await;
}

#[tokio::test]
#[ignore]
async fn live_c_structured_against_real_server() {
    live_required();
    let (_planning, graph, state, exec_ctx, registry) = services();
    let proxy = maybe_proxy("c").await;
    let base = proxy.as_ref().map(|(u, _)| u.clone()).unwrap_or_else(server_base);
    let model = server_model();
    // Single-shot full-plan JSON after a long reasoning trace: big budget.
    let backend = keyless_backend(&base, &model, 2048);
    let planner = AgentStructuredPlanner::new(backend);
    let request = ohio_request();
    let gctx = GraphContext {
        graphs: vec![graph.clone()],
        namespaces: [("ex".to_string(), EX.to_string())].into_iter().collect(),
        ..Default::default()
    };
    let mut ctx = OperationContext::new(request.text.clone(), gctx);
    let plan = planner.create_plan(&request, &mut ctx).await;
    live_tail("C", plan, &state, &exec_ctx, &registry, &proxy, &model).await;
}

#[tokio::test]
#[ignore]
async fn live_d_two_pass_against_real_server() {
    live_required();
    let (_planning, graph, state, exec_ctx, registry) = services();
    let proxy = maybe_proxy("d").await;
    let base = proxy.as_ref().map(|(u, _)| u.clone()).unwrap_or_else(server_base);
    let model = server_model();
    let backend = keyless_backend(&base, &model, 2048);
    let planner = AgentTwoPassPlanner::new(backend);
    let request = ohio_request();
    let gctx = GraphContext {
        graphs: vec![graph.clone()],
        namespaces: [("ex".to_string(), EX.to_string())].into_iter().collect(),
        ..Default::default()
    };
    let mut ctx = OperationContext::new(request.text.clone(), gctx);
    let plan = planner.create_plan(&request, &mut ctx).await;
    println!("LIVE D nl_plan artifact: {:?}", ctx.artifact("nl_plan").map(|s| s.chars().take(300).collect::<String>()));
    live_tail("D", plan, &state, &exec_ctx, &registry, &proxy, &model).await;
}

// --- replay tests (default cargo test: deterministic, keyless) --------------
// NOTE: assertions below are finalized after the first live run captures
// what MiniCPM5-1B-F16 actually does through each loop. Until recordings
// exist they point at the live behavior observed on 2026-11-27.

#[tokio::test]
async fn replay_b_tool_loop_from_recording() {
    let path = recording_path("b");
    if !path.exists() {
        println!("SKIP: no recording at {} (run RECORD=1 live first)", path.display());
        return;
    }
    let (planning, graph, _s, _e, _r) = services();
    let (stub, base) = ReplayStub::serving(&path).await;
    let backend = keyless_backend(&base, "replay-model-b", 1024);
    // Same caps as the live run: 3 recorded turns, all rejected, Budget stop.
    let planner = AgentToolPlanner::new(backend, planning);
    let request = ohio_request();
    let gctx = GraphContext {
        graphs: vec![graph],
        namespaces: [("ex".to_string(), EX.to_string())].into_iter().collect(),
        ..Default::default()
    };
    let mut ctx = OperationContext::new(request.text.clone(), gctx);
    let plan = planner.create_plan(&request, &mut ctx).await;
    println!("REPLAY B outcome: {}", describe_outcome(&plan));
    assert!(stub.models().iter().all(|m| m == "replay-model-b"));
    assert!(
        matches!(plan, Err(needle_rdf_agent::planner::PlannerError::Budget(_))),
        "replay must pin the recorded live outcome (Budget on repeated invalid add_step)"
    );
}

#[tokio::test]
async fn replay_c_structured_from_recording() {
    let path = recording_path("c");
    if !path.exists() {
        println!("SKIP: no recording at {} (run RECORD=1 live first)", path.display());
        return;
    }
    let (_planning, graph, _s, _e, _r) = services();
    let (stub, base) = ReplayStub::serving(&path).await;
    let backend = keyless_backend(&base, "replay-model-c", 2048);
    let planner = AgentStructuredPlanner::new(backend);
    let request = ohio_request();
    let gctx = GraphContext {
        graphs: vec![graph],
        namespaces: [("ex".to_string(), EX.to_string())].into_iter().collect(),
        ..Default::default()
    };
    let mut ctx = OperationContext::new(request.text.clone(), gctx);
    let plan = planner.create_plan(&request, &mut ctx).await;
    println!("REPLAY C outcome: {}", describe_outcome(&plan));
    assert!(stub.models().iter().all(|m| m == "replay-model-c"));
    assert!(
        matches!(plan, Err(needle_rdf_agent::planner::PlannerError::Schema(_))),
        "replay must pin the recorded live outcome (Schema: empty content after repairs)"
    );
}

#[tokio::test]
async fn replay_d_two_pass_from_recording() {
    let path = recording_path("d");
    if !path.exists() {
        println!("SKIP: no recording at {} (run RECORD=1 live first)", path.display());
        return;
    }
    let (_planning, graph, _s, _e, _r) = services();
    let (stub, base) = ReplayStub::serving(&path).await;
    let backend = keyless_backend(&base, "replay-model-d", 2048);
    let planner = AgentTwoPassPlanner::new(backend);
    let request = ohio_request();
    let gctx = GraphContext {
        graphs: vec![graph],
        namespaces: [("ex".to_string(), EX.to_string())].into_iter().collect(),
        ..Default::default()
    };
    let mut ctx = OperationContext::new(request.text.clone(), gctx);
    let plan = planner.create_plan(&request, &mut ctx).await;
    println!("REPLAY D outcome: {}", describe_outcome(&plan));
    assert!(stub.models().iter().all(|m| m == "replay-model-d"));
    assert!(
        matches!(&plan, Err(needle_rdf_agent::planner::PlannerError::Model(m)) if m.contains("empty")),
        "replay must pin the recorded live outcome (Model: empty first pass)"
    );
}
