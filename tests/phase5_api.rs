//! Phase 5 acceptance: conversation durability, serialized worker, Axum API,
//! auth plumbing, progress events, kill/restart chaos, golden snapshots.

use needle_rdf_agent::agent::conversation::ConversationLog;
use needle_rdf_agent::agent::worker::{spawn_worker, WorkerHandle};
use needle_rdf_agent::agent::{Agent, FinalResponse, ToolResult};
use needle_rdf_agent::api::{router, ApiState};
use needle_rdf_agent::config::AppLimits;
use needle_rdf_agent::model::{MockModel, ModelAdapter, ModelCompletion, ModelError, ToolCall};
use needle_rdf_agent::ontology::{contact_shapes, load_default_ontology, OntologyReasoner};
use needle_rdf_agent::policy::{AuditLog, ExecutionContext, Operation};
use needle_rdf_agent::rdf::{GraphName, RdfRepository};
use needle_rdf_agent::search::SearchService;
use needle_rdf_agent::tools::{ToolRegistry, ToolState};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

fn ex(s: &str) -> String {
    format!("http://example.com/{s}")
}

fn all_ops_ctx(request_id: &str, tenant: &str) -> ExecutionContext {
    ExecutionContext::new(
        request_id,
        "alice",
        tenant,
        vec![GraphName::tenant(tenant).unwrap(), GraphName::ontology()],
        vec![
            Operation::Read,
            Operation::Create,
            Operation::Update,
            Operation::Delete,
        ],
    )
}

struct TestApp {
    state: ApiState,
    handle: WorkerHandle,
    join: tokio::task::JoinHandle<()>,
    metrics: Arc<needle_rdf_agent::agent::worker::WorkerMetrics>,
    audit: Arc<AuditLog>,
    log: Arc<ConversationLog>,
    tools: ToolState,
    registry: Arc<ToolRegistry>,
}

fn build_app(model: MockModel) -> TestApp {
    let repo = Arc::new(needle_rdf_agent::rdf::OxigraphRepository::new_in_memory(50_000).unwrap());
    load_default_ontology(&*repo).unwrap();
    let reasoner = Arc::new(OntologyReasoner::from_repository(&*repo).unwrap());
    let search = Arc::new(needle_rdf_agent::search::TantivyIndex::in_ram().unwrap());
    search.rebuild_from_repo(&*repo).unwrap();
    let repo_dyn: Arc<dyn RdfRepository> = repo;
    let search_dyn: Arc<dyn needle_rdf_agent::search::SearchService> = search;
    let audit = Arc::new(AuditLog::in_memory());
    let log = Arc::new(ConversationLog::in_memory());
    let tools = ToolState::new(
        repo_dyn,
        search_dyn,
        reasoner,
        contact_shapes(),
        audit.clone(),
        AppLimits::default(),
    );
    // Seed one Contact so search/read have something to find.
    let registry = Arc::new(ToolRegistry::new());
    let ctx = all_ops_ctx("seed-1", "acme");
    registry
        .dispatch(
            &tools,
            &ctx,
            "create",
            serde_json::json!({
                "uri": ex("Alice"),
                "graph": GraphName::tenant("acme").unwrap().to_string(),
                "types": [ex("Contact")],
                "properties": [{"predicate": ex("name"), "kind": "literal", "value": "Alice"}]
            }),
        )
        .expect("seed Alice");
    let (handle, join, metrics) = spawn_worker(
        Box::new(model),
        Agent::new(AppLimits::default()),
        registry.clone(),
        tools.clone(),
        log.clone(),
    );
    let state = ApiState::new(handle.clone(), registry.clone(), tools.clone(), log.clone());
    TestApp {
        state,
        handle,
        join,
        metrics,
        audit,
        log,
        tools,
        registry,
    }
}

// --- History: file-backed store → reload → identical transcript ---

#[test]
fn history_file_round_trip_with_paired_ids() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("conv.json");
    let t0 = {
        let log = ConversationLog::open(&p).unwrap();
        log.record_user_message("s1", "find alice").unwrap();
        log.record_tool_call("s1", "c1", "search", serde_json::json!({"query": "alice"}))
            .unwrap();
        log.record_tool_result(
            "s1",
            "c1",
            "search",
            true,
            serde_json::json!({"results": []}),
        )
        .unwrap();
        log.record_final("s1", "done", 1, 2).unwrap();
        log.transcript("s1").unwrap()
    };
    assert!(t0.all_results_paired());
    let reopened = ConversationLog::open(&p).unwrap();
    let t1 = reopened.transcript("s1").unwrap();
    assert_eq!(t0, t1, "reload must yield an identical transcript");
    assert!(t1.all_results_paired());
}

// --- Worker: 20 concurrent requests serialize (max concurrency == 1) ---

#[tokio::test]
async fn worker_serializes_concurrent_requests() {
    let app = build_app(MockModel::done("ok"));
    let mut tasks = Vec::new();
    for i in 0..20u32 {
        let h = app.handle.clone();
        let ctx = all_ops_ctx(&format!("r-{i}"), "acme");
        tasks.push(tokio::spawn(async move {
            h.chat("shared-session", "hello", ctx).await
        }));
    }
    let mut ok_count = 0;
    for t in tasks {
        let r = t.await.unwrap().expect("chat must succeed");
        assert!(!r.final_text.is_empty());
        ok_count += 1;
    }
    assert_eq!(ok_count, 20);
    // Give the worker a beat to decrement its counter.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        app.metrics.max.load(Ordering::SeqCst),
        1,
        "single Needle instance must never run concurrently"
    );
    app.join.abort();
}

// --- Worker: sessions reset (no context leak) ---

struct ResetCounting {
    inner: MockModel,
    resets: Arc<AtomicUsize>,
}

impl ModelAdapter for ResetCounting {
    fn complete(&mut self, text: &str) -> Result<ModelCompletion, ModelError> {
        self.inner.complete(text)
    }
    fn submit_tool_results(&mut self, r: &[ToolResult]) -> Result<(), ModelError> {
        self.inner.submit_tool_results(r)
    }
    fn embed(&mut self, t: &str) -> Result<Vec<f32>, ModelError> {
        self.inner.embed(t)
    }
    fn reset(&mut self) {
        self.resets.fetch_add(1, Ordering::SeqCst);
        self.inner.reset();
    }
}

#[tokio::test]
async fn worker_resets_between_sessions() {
    let repo = Arc::new(needle_rdf_agent::rdf::OxigraphRepository::new_in_memory(1000).unwrap());
    let search = Arc::new(needle_rdf_agent::search::TantivyIndex::in_ram().unwrap());
    let repo_dyn: Arc<dyn RdfRepository> = repo;
    let search_dyn: Arc<dyn needle_rdf_agent::search::SearchService> = search;
    let audit = Arc::new(AuditLog::in_memory());
    let log = Arc::new(ConversationLog::in_memory());
    let tools = ToolState::new(
        repo_dyn,
        search_dyn,
        Arc::new(OntologyReasoner::empty()),
        vec![],
        audit,
        AppLimits::default(),
    );
    let registry = Arc::new(ToolRegistry::new());
    let resets = Arc::new(AtomicUsize::new(0));
    let model = ResetCounting {
        inner: MockModel::done("hi"),
        resets: resets.clone(),
    };
    let (handle, join, _) = spawn_worker(
        Box::new(model),
        Agent::new(AppLimits::default()),
        registry,
        tools,
        log.clone(),
    );
    let ctx = ExecutionContext::new("r", "p", "t", vec![], vec![]);
    let o1 = handle.chat("s1", "hello", ctx.clone()).await.unwrap();
    assert_eq!(o1.model_text, "hi");
    // Same session: no reset → script exhausted.
    let o2 = handle.chat("s1", "hello", ctx.clone()).await.unwrap();
    assert!(
        o2.model_text.contains("exhausted"),
        "unexpected: {}",
        o2.model_text
    );
    // New session: reset → script replays from the start.
    let o3 = handle.chat("s2", "hello", ctx).await.unwrap();
    assert_eq!(o3.model_text, "hi", "reset must restore fresh context");
    assert_eq!(resets.load(Ordering::SeqCst), 2);
    assert_eq!(log.session_ids(), vec!["s1", "s2"]);
    join.abort();
}

// --- API helpers ---

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

async fn call(app: axum::Router, req: Request<Body>) -> (StatusCode, serde_json::Value) {
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: serde_json::Value =
        serde_json::from_slice(&bytes).unwrap_or_else(|_| serde_json::json!({"raw": "?"}));
    (status, v)
}

fn chat_req(
    session: &str,
    message: &str,
    principal: Option<&str>,
    tenant: Option<&str>,
) -> Request<Body> {
    let mut b = Request::builder()
        .uri("/chat")
        .method("POST")
        .header("content-type", "application/json");
    if let Some(p) = principal {
        b = b.header("x-principal", p);
    }
    if let Some(t) = tenant {
        b = b.header("x-tenant", t);
    }
    b.body(Body::from(
        serde_json::json!({"session_id": session, "message": message}).to_string(),
    ))
    .unwrap()
}

// --- API: happy path / health / 404 / oversized ---

#[tokio::test]
async fn api_chat_health_404_and_body_limit() {
    let script = vec![
        ModelCompletion::ToolCalls(vec![ToolCall::new(
            "search",
            serde_json::json!({"query": "alice"}),
            "c1",
        )]),
        ModelCompletion::Done("found Alice".to_string()),
    ];
    let app = build_app(MockModel::new(script));

    // Happy path: summarized ops, not a bare model echo.
    let (status, v) = call(
        router(app.state.clone()),
        chat_req("s1", "find alice", Some("alice"), Some("acme")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert!(v["final_text"]
        .as_str()
        .unwrap()
        .contains("completed 1 op(s)"));
    assert!(v["final_text"]
        .as_str()
        .unwrap()
        .contains("model said: found Alice"));
    assert_eq!(v["ops"], 1);
    // Progress events: start + tool + finish (≥2 by contract).
    assert!(v["progress"].as_array().unwrap().len() >= 2);
    assert!(!v["request_id"].as_str().unwrap().is_empty());

    // Health.
    let (status, v) = call(
        router(app.state.clone()),
        Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["status"], "ok");

    // Unknown route.
    let (status, _) = call(
        router(app.state.clone()),
        Request::builder().uri("/nope").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Oversized body rejected (limit layer = 64KiB).
    let big = "x".repeat(70 * 1024);
    let (status, _) = call(
        router(app.state.clone()),
        chat_req("s1", &big, Some("alice"), Some("acme")),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "oversized body must be rejected"
    );

    app.join.abort();
}

// --- API: auth plumbing ---

#[tokio::test]
async fn api_auth_missing_principal_and_wrong_tenant() {
    let app = build_app(MockModel::done("ok"));

    // Missing principal → 401.
    let (status, v) = call(
        router(app.state.clone()),
        chat_req("s1", "hi", None, Some("acme")),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{v}");

    // Wrong tenant reading another tenant's graph → denied (403).
    let req = Request::builder()
        .uri("/tools/read")
        .method("POST")
        .header("content-type", "application/json")
        .header("x-principal", "bob")
        .header("x-tenant", "bob")
        .body(Body::from(
            serde_json::json!({
                "uris": [ex("Alice")],
                "context": GraphName::tenant("acme").unwrap().to_string()
            })
            .to_string(),
        ))
        .unwrap();
    let (status, v) = call(router(app.state.clone()), req).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(v["category"], "auth");

    // Same-tenant read is allowed.
    let req = Request::builder()
        .uri("/tools/read")
        .method("POST")
        .header("content-type", "application/json")
        .header("x-principal", "alice")
        .header("x-tenant", "acme")
        .body(Body::from(
            serde_json::json!({
                "uris": [ex("Alice")],
                "context": GraphName::tenant("acme").unwrap().to_string()
            })
            .to_string(),
        ))
        .unwrap();
    let (status, v) = call(router(app.state.clone()), req).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["ok"], true);

    // Reindex works with valid auth.
    let req = Request::builder()
        .uri("/admin/reindex")
        .method("POST")
        .header("x-principal", "alice")
        .header("x-tenant", "acme")
        .body(Body::empty())
        .unwrap();
    let (status, v) = call(router(app.state.clone()), req).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert!(v["docs"].as_u64().unwrap() >= 1);

    app.join.abort();
}

// --- Progress: long multi-tool run streams ≥2 events ---

#[tokio::test]
async fn progress_events_stream_during_multi_tool_run() {
    let g = GraphName::tenant("acme").unwrap().to_string();
    let script = vec![
        ModelCompletion::ToolCalls(vec![ToolCall::new(
            "search",
            serde_json::json!({"query": "alice"}),
            "c1",
        )]),
        ModelCompletion::ToolCalls(vec![
            ToolCall::new(
                "read",
                serde_json::json!({"uris": [ex("Alice")], "context": g}),
                "c2",
            ),
            ToolCall::new("search", serde_json::json!({"query": "acme"}), "c3"),
        ]),
        ModelCompletion::Done("all three done".to_string()),
    ];
    let app = build_app(MockModel::new(script));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let ctx = all_ops_ctx("prog-1", "acme");
    let out = app
        .handle
        .chat_with_progress("prog-session", "do a few things", ctx, Some(tx))
        .await
        .unwrap();
    assert_eq!(out.ops, 3);
    assert!(out.final_text.contains("completed 3 op(s)"));
    // Stored progress + streamed progress agree and cover start/tool/finish.
    assert!(out.progress.len() >= 4, "{:?}", out.progress);
    let mut streamed = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        streamed.push(ev);
    }
    assert_eq!(streamed.len(), out.progress.len());
    assert_eq!(streamed.first().unwrap().tool, "start");
    assert_eq!(streamed.last().unwrap().tool, "finish");
    app.join.abort();
}

// --- Chaos: kill worker → clean failure → restart → success, no phantom ---

#[tokio::test]
async fn kill_restart_recovers_with_no_audit_phantom() {
    let app = build_app(MockModel::done("ok"));
    let ctx = all_ops_ctx("chaos-1", "acme");
    let o = app.handle.chat("s1", "hi", ctx.clone()).await.unwrap();
    assert!(o.final_text.contains("no tool ops"));

    // Kill the worker mid-service.
    app.join.abort();
    tokio::task::yield_now().await;
    let err = app.handle.chat("s1", "hi again", ctx.clone()).await;
    assert!(
        err.is_err(),
        "request against a dead worker must fail cleanly"
    );

    // Restart with the same shared state (repo/tools/audit/log survive).
    let (handle2, join2, _) = spawn_worker(
        Box::new(MockModel::done("back")),
        Agent::new(AppLimits::default()),
        app.registry.clone(),
        app.tools.clone(),
        app.log.clone(),
    );
    let o2 = handle2.chat("s1", "hi after restart", ctx).await.unwrap();
    assert!(o2.model_text.contains("back"));
    // No mutation ever ran through the worker, so the audit log holds
    // only the seeded create — no `committed` phantom for killed requests.
    assert_eq!(app.audit.len(), 1, "only the seeded create is audited");
    join2.abort();
}

// --- Snapshot: golden final responses for 3 canonical runs ---

fn ok_result(tool: &str, call: &str) -> (String, ToolResult) {
    (
        tool.to_string(),
        ToolResult {
            tool: tool.to_string(),
            call_id: call.to_string(),
            ok: true,
            result: serde_json::json!({"results": []}),
        },
    )
}

#[test]
fn golden_final_responses() {
    // 1. Search-only happy path.
    let h1 = vec![ok_result("search", "c1")];
    let f1 = FinalResponse::assemble("found Alice", &h1);
    assert_eq!(
        serde_json::to_value(&f1).unwrap(),
        serde_json::json!({
            "summary": "completed 1 op(s). ok search (c1) model said: found Alice",
            "ops": 1,
            "iterations": 0,
            "succeeded": ["search:c1"],
            "failed": [],
            "model_text": "found Alice",
        })
    );

    // 2. Search → create, both succeed.
    let h2 = vec![ok_result("search", "c1"), ok_result("create", "c2")];
    let f2 = FinalResponse::assemble("created Bob", &h2);
    assert_eq!(
        serde_json::to_value(&f2).unwrap(),
        serde_json::json!({
            "summary": "completed 2 op(s). ok search (c1) ok create (c2) model said: created Bob",
            "ops": 2,
            "iterations": 0,
            "succeeded": ["search:c1", "create:c2"],
            "failed": [],
            "model_text": "created Bob",
        })
    );

    // 3. Create rejected by validation, surfaced as residual error.
    let h3 = vec![(
        "create".to_string(),
        ToolResult {
            tool: "create".to_string(),
            call_id: "c1".to_string(),
            ok: false,
            result: serde_json::json!({
                "tool": "create",
                "category": "validation",
                "message": "SHACL failed: missing name"
            }),
        },
    )];
    let f3 = FinalResponse::assemble("cannot create", &h3);
    let v3 = serde_json::to_value(&f3).unwrap();
    assert_eq!(v3["ops"], 1);
    assert_eq!(v3["succeeded"], serde_json::json!([]));
    assert_eq!(v3["failed"][0]["category"], "validation");
    assert!(v3["summary"]
        .as_str()
        .unwrap()
        .contains("failed create (c1) [validation]"));
    assert!(v3["summary"]
        .as_str()
        .unwrap()
        .contains("model said: cannot create"));
}
