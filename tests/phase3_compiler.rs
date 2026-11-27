//! Phase 3 integration tests (keyless unless marked live): the request
//! compiler end to end — zero-model compile, read-path execution,
//! mutation final states, loud boundaries, corpus rate, field replay,
//! and the live disambiguation gate.
//!
//! The happy path provably makes no model call: `compile_request`
//! takes no backend, and the planner-level test serves a stub that
//! panics on any HTTP request.

use needle_rdf_agent::bench::dataset::Scale;
use needle_rdf_agent::bench::runner::build_exec_fixture;
use needle_rdf_agent::config::AppLimits;
use needle_rdf_agent::executor::Executor;
use needle_rdf_agent::llm::{build_backend, config_from_vars, KeyMode};
use needle_rdf_agent::ontology::{contact_shapes, load_default_ontology, OntologyReasoner};
use needle_rdf_agent::plan::{validate_plan, GraphContext, OperationContext, StepOutput};
use needle_rdf_agent::planner::compiler::{
    compile_corpus, compile_request, escalation_decision, CompileOutcome, CompilerPlanner,
};
use needle_rdf_agent::planner::{AgentBackend, Planner, PlanningServices, UserRequest};
use needle_rdf_agent::policy::{AuditLog, ExecutionContext, Operation as PolicyOp};
use needle_rdf_agent::rdf::{GraphName, OxigraphRepository, Quad, RdfRepository, Term};
use needle_rdf_agent::search::SearchService;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

fn text_payload(text: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "chatcmpl-compiler",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": text},
            "finish_reason": "stop"
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

static MODEL_TOUCHED: AtomicBool = AtomicBool::new(false);

async fn counting_stub() -> String {
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move |_body: axum::Json<serde_json::Value>| async move {
            MODEL_TOUCHED.store(true, Ordering::SeqCst);
            axum::Json(text_payload("touched"))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    format!("http://{addr}")
}

fn keyless_backend(base_url: &str) -> AgentBackend {
    std::env::remove_var("OPENAI_API_KEY");
    let cfg = config_from_vars(Some(base_url), Some("compiler-e2e"), None, Some("30"))
        .expect("keyless");
    assert_eq!(cfg.key_mode, KeyMode::AbsentLocal);
    AgentBackend::new(build_backend(&cfg), cfg.model.clone()).with_max_tokens(2048)
}

fn live_backend() -> AgentBackend {
    let base = std::env::var("OPENAI_BASE_URL").expect("live base url");
    let model = std::env::var("OPENAI_MODEL").expect("live model");
    let cfg = config_from_vars(Some(&base), Some(&model), None, Some("1800")).expect("live cfg");
    AgentBackend::new(build_backend(&cfg), cfg.model.clone()).with_max_tokens(4092)
}

fn live_enabled() -> bool {
    matches!(std::env::var("RUN_MODEL_TESTS"), Ok(v) if v == "1")
}

/// Bench fixture (tiny): full ontology + instances, all ops allowed.
/// The search index is rebuilt so instance search executes for real.
fn bench_fixture() -> (
    PlanningServices,
    GraphContext,
    Arc<OxigraphRepository>,
    GraphName,
    GraphName,
) {
    let fx = build_exec_fixture(Scale::Tiny);
    fx.services
        .state
        .search
        .rebuild_from_repo(&*fx.repo)
        .unwrap();
    let graph_ctx = GraphContext {
        graphs: vec![fx.graphs.tenant_a.clone()],
        ..Default::default()
    };
    let tenant_a = fx.graphs.tenant_a.clone();
    let tenant_b = fx.graphs.tenant_b.clone();
    (fx.services, graph_ctx, fx.repo, tenant_a, tenant_b)
}

/// Default-ontology fixture with seeded instances for read-path E2E.
fn default_fixture() -> (PlanningServices, GraphContext, Arc<OxigraphRepository>) {
    use needle_rdf_agent::rdf::Quad;
    let repo = Arc::new(OxigraphRepository::new_in_memory(50_000).unwrap());
    load_default_ontology(&*repo).unwrap();
    let g = GraphName::tenant("acme").unwrap();
    let alice = Term::iri("http://example.com/Alice".to_string()).unwrap();
    let acme = Term::iri("http://example.com/AcmeCorp".to_string()).unwrap();
    let contact = Term::iri("http://example.com/Contact".to_string()).unwrap();
    let account = Term::iri("http://example.com/Account".to_string()).unwrap();
    let name = Term::iri("http://example.com/name".to_string()).unwrap();
    let rdf_type =
        Term::iri("http://www.w3.org/1999/02/22-rdf-syntax-ns#type".to_string()).unwrap();
    for (s, t, o) in [
        (alice.clone(), rdf_type.clone(), contact.clone()),
        (alice.clone(), name.clone(), Term::literal("Alice")),
        (acme.clone(), rdf_type.clone(), account.clone()),
        (acme.clone(), name.clone(), Term::literal("Acme")),
    ] {
        repo.insert(&[Quad::new(s, t, o, g.clone()).unwrap()]).unwrap();
    }
    let reasoner = Arc::new(OntologyReasoner::from_repository(&*repo).unwrap());
    let search = Arc::new(needle_rdf_agent::search::TantivyIndex::in_ram().unwrap());
    search.rebuild_from_repo(&*repo).unwrap();
    let audit = Arc::new(AuditLog::in_memory());
    let state = needle_rdf_agent::tools::ToolState::new(
        repo.clone(),
        search,
        reasoner,
        contact_shapes(),
        audit,
        AppLimits::default(),
    );
    let exec_ctx = ExecutionContext::new(
        "req-compiler",
        "alice",
        "acme",
        vec![g.clone(), GraphName::ontology()],
        vec![PolicyOp::Read, PolicyOp::Create, PolicyOp::Update, PolicyOp::Delete],
    );
    let graph_ctx = GraphContext {
        graphs: vec![g],
        ..Default::default()
    };
    (
        PlanningServices::new(Arc::new(needle_rdf_agent::tools::ToolRegistry::new()), state, exec_ctx),
        graph_ctx,
        repo,
    )
}

fn executor_for(services: &PlanningServices) -> Executor {
    Executor::new(
        services.registry.clone(),
        services.state.clone(),
        services.exec_ctx.clone(),
    )
    .with_confirmation_token("confirm-e2e")
    .with_max_replans(0)
}

fn type_term() -> Term {
    Term::iri("http://www.w3.org/1999/02/22-rdf-syntax-ns#type".to_string()).unwrap()
}

fn count_instances(
    repo: &OxigraphRepository,
    class_iri: &str,
    graph: &GraphName,
) -> usize {
    repo.match_quads(
        None,
        Some(&type_term()),
        Some(&Term::iri(class_iri.to_string()).unwrap()),
        Some(graph),
        None,
    )
    .unwrap()
    .len()
}

/// Every ladder rung either compiles with zero model calls or names
/// its missing slot — never an unmatched guess. L8/L9 need the
/// instance label the request does not give.
#[tokio::test]
async fn compiler_happy_path_uses_zero_model_calls() {
    let (services, graph_ctx, _, _, _) = bench_fixture();
    let rungs = needle_rdf_agent::ladder_spec::ladder();
    assert_eq!(rungs.len(), 10);
    for rung in &rungs {
        let outcome = compile_request(&rung.request, &services, &graph_ctx);
        match rung.level {
            5 | 6 => match &outcome {
                CompileOutcome::NeedsSlots { spine, missing, .. } => {
                    assert_eq!(missing.len(), 1, "L{}: exactly one missing slot", rung.level);
                    assert_eq!(missing[0].name(), "terms", "L{}", rung.level);
                    eprintln!("L{}: needs-slots:{spine}:terms (steps {})", rung.level, rung.steps.len());
                }
                other => panic!("L{} must name its terms gap, got {other:?}", rung.level),
            },
            8 | 9 => match &outcome {
                CompileOutcome::NeedsSlots { spine, missing, .. } => {
                    assert_eq!(missing.len(), 1, "L{}: exactly one missing slot", rung.level);
                    assert_eq!(missing[0].name(), "text", "L{}", rung.level);
                    eprintln!("L{}: needs-slots:{spine}:text (steps {})", rung.level, rung.steps.len());
                }
                other => panic!("L{} must need its label slot, got {other:?}", rung.level),
            },
            _ => match &outcome {
                CompileOutcome::Compiled { plan, spine, level } => {
                    assert_eq!(*level, rung.level);
                    validate_plan(plan).unwrap();
                    assert_eq!(
                        plan.steps.len(),
                        rung.steps.len(),
                        "L{level}: compiler preserves the spine shape ({spine})"
                    );
                    eprintln!("L{level}: compiled:{spine} steps={} model_turns=0", plan.steps.len());
                }
                other => panic!("L{} must compile, got {other:?}", rung.level),
            },
        }
    }
}

/// Read-path rungs execute to real discovery outputs on the
/// default-ontology fixture: compiled (0 model calls) → validated →
/// executed, candidates resolved, audit untouched.
#[tokio::test]
async fn compiler_e2e_read_paths_execute() {
    let (services, graph_ctx, _) = default_fixture();
    let audit_before = services.state.audit.len();
    for (request, step) in [
        ("List the vocabulary terms for contacts.", "s1"),
        ("Find contact vocabulary and search for Acme instances.", "s2"),
        ("Resolve the contact and status vocabulary terms.", "s2"),
    ] {
        let outcome = compile_request(request, &services, &graph_ctx);
        let plan = match outcome {
            CompileOutcome::Compiled { plan, spine, .. } => {
                eprintln!("read E2E compiled:{spine}");
                plan
            }
            other => panic!("{request} must compile, got {other:?}"),
        };
        validate_plan(&plan).unwrap();
        let mut ctx = OperationContext::new(request.to_string(), graph_ctx.clone());
        let exec = executor_for(&services);
        let report = exec.execute(&plan, &mut ctx, None).await.unwrap();
        assert!(report.planner_calls == 0, "no replan turns: {request}");
        match ctx.results.get(step).unwrap() {
            StepOutput::VocabularyCandidates { items } => assert!(!items.is_empty(), "{request}"),
            StepOutput::ResolvedVocabulary { items } => assert!(!items.is_empty(), "{request}"),
            StepOutput::ResourceCandidates { items } => assert!(!items.is_empty(), "{request}"),
            StepOutput::ResourceDescriptions { count } => assert!(*count > 0, "{request}"),
            StepOutput::UriSet { uris } => assert!(!uris.is_empty(), "{request}"),
            other => panic!("{request}: unexpected terminal output {other:?}"),
        }
    }
    // L5/L6 name no ontology term, so the compiler asks for the one
    // missing field; scripted answers replay the model keylessly, and
    // the assembled plans execute against the seeded instances.
    let (_stub, base) = QueueStub::serving(vec![
        text_payload("{\"terms\": [\"contact\", \"account\"]}"),
        text_payload("{\"terms\": [\"contact\", \"account\"]}"),
    ])
    .await;
    let exec = executor_for(&services);
    let audit = services.state.audit.clone();
    let planner = CompilerPlanner::new(keyless_backend(&base), services);
    for (request, step) in [
        ("Find Acme-related instances using the resolved vocabulary.", "s3"),
        ("Describe the candidate Acme instances.", "s4"),
    ] {
        let sreq = UserRequest::new(request);
        let mut ctx = OperationContext::new(request.to_string(), graph_ctx.clone());
        let plan = planner.create_plan(&sreq, &mut ctx).await.unwrap();
        assert_eq!(
            ctx.artifacts.get("routing").map(|s| s.starts_with("compiler-slots:")),
            Some(true),
            "{request}"
        );
        validate_plan(&plan).unwrap();
        let report = exec.execute(&plan, &mut ctx, None).await.unwrap();
        assert!(report.planner_calls == 0, "no replan turns: {request}");
        match ctx.results.get(step).unwrap() {
            StepOutput::ResourceCandidates { items } => assert!(!items.is_empty(), "{request}"),
            StepOutput::ResourceDescriptions { count } => assert!(*count > 0, "{request}"),
            other => panic!("{request}: unexpected terminal output {other:?}"),
        }
    }
    assert_eq!(audit.len(), audit_before, "read-only: no audit rows");
}

/// Covered create class: the SHACL-required name comes from the
/// quoted label, the type from linking — new Contact instance in the
/// repo, validate conforms, verify agrees.
#[tokio::test]
async fn compiler_e2e_create_executes_final_state() {
    let (services, graph_ctx, repo, tenant, _) = bench_fixture();
    let crm = "http://bench.example/crm/";
    let before = count_instances(&repo, &format!("{crm}Contact"), &tenant);
    assert_eq!(before, 10);
    let request = "Create contact 'Contact 99'.";
    let plan = match compile_request(request, &services, &graph_ctx) {
        CompileOutcome::Compiled { plan, spine, .. } => {
            assert_eq!(spine, "create");
            plan
        }
        other => panic!("create must compile, got {other:?}"),
    };
    validate_plan(&plan).unwrap();
    let mut ctx = OperationContext::new(request.to_string(), graph_ctx.clone());
    let report = executor_for(&services).execute(&plan, &mut ctx, None).await.unwrap();
    assert!(report.planner_calls == 0);
    assert_eq!(count_instances(&repo, &format!("{crm}Contact"), &tenant), 11);
    let name_pred = Term::iri(format!("{crm}name")).unwrap();
    let named: Vec<_> = repo
        .match_quads(None, Some(&name_pred), Some(&Term::literal("Contact 99")), Some(&tenant), None)
        .unwrap();
    assert_eq!(named.len(), 1);
    assert!(matches!(
        ctx.results.get("s5"),
        Some(StepOutput::VerificationResult { .. })
    ));
}

/// Covered classify class: linked Account + ProspectStatus ground the
/// predicate and object deterministically — every account ends with
/// the prospect status, SHACL range holds.
#[tokio::test]
async fn compiler_e2e_classify_accounts_mutates_status() {
    // Clean-room accounts in tenant_b: the bench tenant_a accounts carry
    // pre-existing SHACL-dirt (names under a Contact-domain property),
    // and status individuals are typed only in the ontology graph while
    // the guard's instance check is instance-graph scoped — both
    // documented engine/fixture limits, worked around transparently
    // here so the test proves the commit path, not the dirt.
    let (services, _, repo, _, tenant_b) = bench_fixture();
    let crm = "http://bench.example/crm/";
    let sales = "http://bench.example/sales/";
    let account = Term::iri(format!("{crm}Account")).unwrap();
    let prospect = Term::iri(format!("{sales}ProspectStatus")).unwrap();
    let active = Term::iri(format!("{sales}ActiveStatus")).unwrap();
    let acc_status = Term::iri(format!("{sales}AccountStatus")).unwrap();
    let mut seeds = vec![];
    for name in ["classify-acct-A", "classify-acct-B"] {
        let a = Term::iri(format!("{crm}{name}")).unwrap();
        seeds.push(Quad::new(a, type_term(), account.clone(), tenant_b.clone()).unwrap());
    }
    for status in [&prospect, &active] {
        seeds.push(
            Quad::new(status.clone(), type_term(), acc_status.clone(), tenant_b.clone()).unwrap(),
        );
    }
    repo.insert(&seeds).unwrap();
    services.state.search.rebuild_from_repo(&*repo).unwrap();
    // The fixture context allowlists tenant_a; the clean room lives in
    // tenant_b, so re-issue the context with the same principal.
    let exec_ctx = ExecutionContext::new(
        "req-compiler",
        "bench",
        "bench",
        vec![tenant_b.clone(), GraphName::ontology()],
        vec![PolicyOp::Read, PolicyOp::Create, PolicyOp::Update, PolicyOp::Delete],
    );
    let services = PlanningServices::new(services.registry.clone(), services.state.clone(), exec_ctx);
    let graph_ctx = GraphContext {
        graphs: vec![tenant_b.clone()],
        ..Default::default()
    };
    let request = "Classify Ohio accounts as prospects.";
    let plan = match compile_request(request, &services, &graph_ctx) {
        CompileOutcome::Compiled { plan, spine, .. } => {
            assert_eq!(spine, "classify");
            plan
        }
        other => panic!("classify must compile, got {other:?}"),
    };
    let update = &plan.steps[3];
    assert_eq!(update.id, "s4");
    validate_plan(&plan).unwrap();
    let mut ctx = OperationContext::new(request.to_string(), graph_ctx.clone());
    let report = executor_for(&services).execute(&plan, &mut ctx, None).await.unwrap();
    assert!(report.planner_calls == 0);
    let status_pred = Term::iri(format!("{crm}hasStatus")).unwrap();
    for name in ["classify-acct-A", "classify-acct-B"] {
        let acc = Term::iri(format!("{crm}{name}")).unwrap();
        let has: Vec<_> = repo
            .match_quads(Some(&acc), Some(&status_pred), Some(&prospect), Some(&tenant_b), None)
            .unwrap();
        assert_eq!(has.len(), 1, "{name} classified as prospect");
    }
    assert!(matches!(
        ctx.results.get("s6"),
        Some(StepOutput::VerificationResult { .. })
    ));
}

/// Covered delete class: the linked Contact class grounds the target
/// query — all contacts removed, validate conforms (no shape touches
/// the dangling employs refs), verify closes the spine.
#[tokio::test]
async fn compiler_e2e_delete_executes_final_state() {
    let (services, graph_ctx, repo, tenant, _) = bench_fixture();
    let crm = "http://bench.example/crm/";
    assert_eq!(count_instances(&repo, &format!("{crm}Contact"), &tenant), 10);
    let request = "Delete dormant 'Contact 1' records.";
    let plan = match compile_request(request, &services, &graph_ctx) {
        CompileOutcome::Compiled { plan, spine, .. } => {
            assert_eq!(spine, "delete");
            plan
        }
        other => panic!("delete must compile, got {other:?}"),
    };
    validate_plan(&plan).unwrap();
    let mut ctx = OperationContext::new(request.to_string(), graph_ctx.clone());
    let report = executor_for(&services).execute(&plan, &mut ctx, None).await.unwrap();
    assert!(report.planner_calls == 0);
    match ctx.results.get("s3") {
        Some(StepOutput::UriSet { uris }) => assert_eq!(uris.len(), 10),
        other => panic!("s3 must hold the 10 contact targets, got {other:?}"),
    }
    assert_eq!(count_instances(&repo, &format!("{crm}Contact"), &tenant), 0);
    assert!(matches!(
        ctx.results.get("s7"),
        Some(StepOutput::VerificationResult { .. })
    ));
}

/// The honest boundary: contacts have no status property in the
/// ontology, so the update keeps `unresolved` and execution escalates
/// loudly instead of inventing a predicate.
#[tokio::test]
async fn compiler_e2e_contacts_classify_stays_loud() {
    let (services, graph_ctx, _, _, _) = bench_fixture();
    let request = "Classify Ohio contacts as prospects.";
    let plan = match compile_request(request, &services, &graph_ctx) {
        CompileOutcome::Compiled { plan, .. } => plan,
        other => panic!("contacts classify must compile, got {other:?}"),
    };
    validate_plan(&plan).unwrap();
    let mut ctx = OperationContext::new(request.to_string(), graph_ctx.clone());
    let err = executor_for(&services).execute(&plan, &mut ctx, None).await.unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("unresolved"), "loud unresolved surprise, got: {msg}");
    // The compile verdict still owns the request (default path); the
    // execution surprise is the counted escalation input.
    assert_eq!(
        escalation_decision(&compile_request(request, &services, &graph_ctx)),
        "compiler-default"
    );
}

/// Planner level: the happy path makes zero model calls (the stub
/// panics if touched) and writes the compiler routing + stats.
#[tokio::test]
async fn compiler_planner_happy_path_needs_no_model() {
    let (services, graph_ctx, _, _, _) = bench_fixture();
    MODEL_TOUCHED.store(false, Ordering::SeqCst);
    let base = counting_stub().await;
    let planner = CompilerPlanner::new(keyless_backend(&base), services);
    let request = UserRequest::new("Classify Ohio accounts as prospects.");
    let mut ctx = OperationContext::new(request.text.clone(), graph_ctx);
    let plan = planner.create_plan(&request, &mut ctx).await.unwrap();
    assert!(!MODEL_TOUCHED.load(Ordering::SeqCst), "happy path made a model call");
    validate_plan(&plan).unwrap();
    assert_eq!(ctx.artifacts.get("routing").map(|s| s.as_str()), Some("compiler:classify"));
    let stats = ctx.artifacts.get("compiler_stats").expect("compiler_stats artifact");
    assert!(stats.contains("\"model_slot_calls\":0"), "{stats}");
}

/// Field-level replay: scripted scalar answers assemble a validating
/// plan keylessly (pins the disambiguation path without a model).
#[tokio::test]
async fn compiler_field_replay_pins_disambiguation() {
    let (services, graph_ctx, _, _, _) = bench_fixture();
    let (_stub, base) =
        QueueStub::serving(vec![text_payload("{\"text\": \"stale prospect\"}")]).await;
    let planner = CompilerPlanner::new(keyless_backend(&base), services);
    let request = UserRequest::new("Delete stale prospect records.");
    let mut ctx = OperationContext::new(request.text.clone(), graph_ctx);
    let plan = planner.create_plan(&request, &mut ctx).await.unwrap();
    validate_plan(&plan).unwrap();
    assert_eq!(
        ctx.artifacts.get("routing").map(|s| s.as_str()),
        Some("compiler-slots:delete")
    );
    let s4 = plan.steps.iter().find(|s| s.id == "s4").unwrap();
    match &s4.args {
        needle_rdf_agent::plan::StepArguments::SearchInstances { text, .. } => {
            assert_eq!(text, "stale prospect")
        }
        other => panic!("s4 must carry the replayed text, got {other:?}"),
    }
}

/// Corpus gate: all 100 bench requests get an outcome + a fate; the
/// unmatched rate is published and the compiler owns at least the
/// phase-2 spine floor (15/100).
#[tokio::test]
async fn compiler_corpus_rate_and_steps() {
    let (services, graph_ctx, _, _, _) = bench_fixture();
    let entries = std::fs::read_dir("tests/bench_cases").expect("bench cases dir");
    let mut paths: Vec<_> = entries.map(|e| e.unwrap().path()).collect();
    paths.sort();
    let mut cases: Vec<(String, String)> = vec![];
    for path in paths {
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let raw = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        cases.push((
            v.get("id").and_then(|i| i.as_str()).unwrap_or("?").to_string(),
            v.get("request").and_then(|r| r.as_str()).unwrap_or("").to_string(),
        ));
    }
    assert_eq!(cases.len(), 100, "bench Set 1 size");
    let (rows, summary) = compile_corpus(&cases, &services, &graph_ctx);
    assert_eq!(rows.len(), 100);
    for row in &rows {
        assert!(!row.outcome.is_empty(), "{}", row.id);
        assert!(!row.fate.is_empty(), "{}", row.id);
        assert!(
            row.fate == "compiler-default"
                || row.fate == "accepted-llm"
                || row.fate.starts_with("rule-candidate:"),
            "{}: fate carries the next-rule decision",
            row.id
        );
    }
    let owned = summary.compiled + summary.needs_slots;
    eprintln!(
        "COMPILER CORPUS: {}/{} compiled, {}/{} needs-slots, {}/{} unmatched (rate {:.2})",
        summary.compiled,
        summary.total,
        summary.needs_slots,
        summary.total,
        summary.unmatched,
        summary.total,
        summary.unmatched_rate
    );
    assert!(
        owned >= 15,
        "compiler must own at least the phase-2 spine floor (15/100), got {owned}"
    );
    let mut unmatched_rows: Vec<&str> = rows
        .iter()
        .filter(|r| r.fate == "accepted-llm")
        .map(|r| r.id.as_str())
        .collect();
    unmatched_rows.sort();
    eprintln!("unmatched-accepted-llm ({}): {}", unmatched_rows.len(), unmatched_rows.join(", "));
}

/// Live nightly gate (ignored locally): the deterministic create E2E
/// guards fixture drift on the live stack, and the disambiguation
/// path fills one missing slot through the live 1B model.
#[tokio::test]
#[ignore]
async fn live_compiler_ohio_e2e() {
    if !live_enabled() {
        eprintln!("skipping live compiler gate (set RUN_MODEL_TESTS=1)");
        return;
    }
    // Deterministic leg: create compiles with zero model calls even
    // on the live stack (guards fixture drift, not the model).
    let (services, graph_ctx, repo, tenant, _) = bench_fixture();
    let crm = "http://bench.example/crm/";
    let request = "Create contact 'Contact 99'.";
    let plan = match compile_request(request, &services, &graph_ctx) {
        CompileOutcome::Compiled { plan, .. } => plan,
        other => panic!("create must compile live, got {other:?}"),
    };
    validate_plan(&plan).unwrap();
    let mut ctx = OperationContext::new(request.to_string(), graph_ctx.clone());
    executor_for(&services).execute(&plan, &mut ctx, None).await.unwrap();
    assert_eq!(count_instances(&repo, &format!("{crm}Contact"), &tenant), 11);
    // Disambiguation leg: the bare delete request names its missing
    // slot; the live model fills exactly that field.
    let planner = CompilerPlanner::new(live_backend(), services);
    let request = UserRequest::new("Delete stale prospect records.");
    let mut ctx = OperationContext::new(request.text.clone(), graph_ctx);
    let plan = planner.create_plan(&request, &mut ctx).await.unwrap();
    validate_plan(&plan).unwrap();
    assert_eq!(
        ctx.artifacts.get("routing").map(|s| s.as_str()),
        Some("compiler-slots:delete")
    );
}
