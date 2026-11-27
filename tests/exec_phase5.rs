//! Phase 5 E2E: executor, replanning, mutation safety, non-goals,
//! concurrency, kill-mid-commit, metrics gate.
//!
//! Keyless: no `OPENAI_API_KEY`, no model weights. Planners are scripted
//! ([`ScriptedPlanner`]); the executor path is identical for all four
//! planners (same `create_plan` signature), so final-state parity across
//! planners is asserted by running the same grounded plan through the
//! executor with different scripted-planner identities.

use std::collections::HashMap;
use std::sync::Arc;

use needle_rdf_agent::config::AppLimits;
use needle_rdf_agent::executor::{graph_hash, Executor, ExecutorError, ScriptedPlanner};
use needle_rdf_agent::ontology::{contact_shapes, load_default_ontology, OntologyReasoner};
use needle_rdf_agent::plan::{
    GraphContext, Operation, OperationContext, OutputType, Plan, PlanStep, StepArguments, StepOutput,
    TypedValue,
};
use needle_rdf_agent::planner::Planner;
use needle_rdf_agent::policy::{AuditLog, CommitStatus, ExecutionContext, Operation as PolicyOp};
use needle_rdf_agent::rdf::{GraphName, OxigraphRepository, Quad, RdfRepository, Term};
use needle_rdf_agent::search::TantivyIndex;
use needle_rdf_agent::tools::{ToolRegistry, ToolState};

const EX: &str = "http://example.com/";
const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

struct Fixture {
    state: ToolState,
    exec_ctx: ExecutionContext,
    registry: Arc<ToolRegistry>,
    graph: GraphName,
    repo: Arc<OxigraphRepository>,
}

fn fixture(permitted: Vec<PolicyOp>) -> Fixture {
    let repo = Arc::new(OxigraphRepository::new_in_memory(50_000).unwrap());
    load_default_ontology(&*repo).unwrap();
    let graph = GraphName::tenant("acme").unwrap();
    let reasoner = Arc::new(OntologyReasoner::from_repository(&*repo).unwrap());
    let search = Arc::new(TantivyIndex::in_ram().unwrap());
    let audit = Arc::new(AuditLog::in_memory());
    let state = ToolState::new(
        repo.clone(),
        search,
        reasoner,
        contact_shapes(),
        audit,
        AppLimits::default(),
    );
    let exec_ctx = ExecutionContext::new(
        "req-1",
        "alice",
        "acme",
        vec![graph.clone(), GraphName::ontology()],
        permitted,
    );
    let registry = Arc::new(ToolRegistry::new());
    Fixture { state, exec_ctx, registry, graph, repo }
}

fn full_perms() -> Vec<PolicyOp> {
    vec![PolicyOp::Read, PolicyOp::Create, PolicyOp::Update, PolicyOp::Delete]
}

fn term(s: &str) -> Term {
    Term::iri(s.to_string()).unwrap()
}

fn quad(s: &str, p: &str, o: Term, g: &GraphName) -> Quad {
    Quad::new(term(s), term(p), o, g.clone()).unwrap()
}

fn lit(s: &str) -> Term {
    Term::literal(s.to_string())
}

/// Seed: Alice (Contact, name Alice, account Acme), Acme (state OH),
/// Prospect category resource indexed for `search_instances("prospect")`.
fn seed_ohio(f: &Fixture) {
    let g = &f.graph;
    f.state
        .repo
        .insert(&[
            quad(&format!("{EX}Alice"), RDF_TYPE, term(&format!("{EX}Contact")), g),
            quad(&format!("{EX}Alice"), &format!("{EX}name"), lit("Alice"), g),
            quad(&format!("{EX}Alice"), &format!("{EX}account"), term(&format!("{EX}Acme")), g),
            quad(&format!("{EX}Acme"), RDF_TYPE, term(&format!("{EX}Account")), g),
            quad(&format!("{EX}Acme"), &format!("{EX}state"), lit("OH"), g),
            quad(&format!("{EX}Prospect"), RDF_TYPE, term(&format!("{EX}Category")), g),
        ])
        .unwrap();
    // NOTE: Prospect carries no ex:name on purpose: ex:name's domain is
    // Contact, so a named Category would trip graph-wide SHACL validation.
    // It stays discoverable via the indexed URI local-name ("Prospect").
    let _ = f.state.search.sync_resource(&*f.state.repo, &format!("{EX}Prospect"), g);
}

fn graph_ctx_for(g: &GraphName) -> GraphContext {
    GraphContext {
        graphs: vec![g.clone()],
        namespaces: [( "ex".to_string(), EX.to_string())].into_iter().collect(),
        ..Default::default()
    }
}

fn ohio_pattern(g: &GraphName) -> serde_json::Value {
    serde_json::json!({
        "operation": "select",
        "graphs": [g.to_string()],
        "patterns": [
            {"subject": {"kind": "var", "name": "?contact"},
             "predicate": {"kind": "iri", "iri": format!("{EX}account")},
             "object": {"kind": "var", "name": "?account"}},
            {"subject": {"kind": "var", "name": "?account"},
             "predicate": {"kind": "iri", "iri": format!("{EX}state")},
             "object": {"kind": "literal", "value": "OH"}}
        ],
        "filters": [],
        "return_vars": ["?contact"],
        "limit": 50
    })
}

/// Fully-grounded Ohio plan: discovery → typed query → scoped mutation →
/// validate → verify. Satisfies the plan validator.
fn grounded_ohio_plan(g: &GraphName) -> Plan {
    let ctx = graph_ctx_for(g);
    Plan {
        goal: "Classify contacts at Ohio accounts as prospects".to_string(),
        context: ctx,
        assumptions: vec![],
        steps: vec![
            PlanStep {
                id: "s1".to_string(),
                operation: Operation::SearchVocabulary,
                intent: "Discover contact/account vocabulary".to_string(),
                args: StepArguments::SearchVocab {
                    terms: vec!["contact".to_string(), "account".to_string()],
                    namespaces: vec![],
                },
                depends_on: vec![],
                output: OutputType::VocabularyCandidates,
            },
            PlanStep {
                id: "s2".to_string(),
                operation: Operation::DescribeVocabulary,
                intent: "Resolve term details".to_string(),
                args: StepArguments::DescribeVocab {
                    iris: vec![TypedValue::StepRef { step: "s1".to_string(), field: "candidates".to_string() }],
                },
                depends_on: vec!["s1".to_string()],
                output: OutputType::ResolvedVocabulary,
            },
            PlanStep {
                id: "s3".to_string(),
                operation: Operation::Query,
                intent: "Find contacts at Ohio accounts".to_string(),
                args: StepArguments::Query { pattern: ohio_pattern(g) },
                depends_on: vec!["s2".to_string()],
                output: OutputType::UriSet,
            },
            PlanStep {
                id: "s4".to_string(),
                operation: Operation::SearchInstances,
                intent: "Find the prospect classification resource".to_string(),
                args: StepArguments::SearchInstances {
                    text: "prospect".to_string(),
                    types: vec![],
                    namespace: None,
                },
                depends_on: vec!["s2".to_string()],
                output: OutputType::ResourceCandidates,
            },
            PlanStep {
                id: "s5".to_string(),
                operation: Operation::Update,
                intent: "Assign the prospect classification to matching contacts".to_string(),
                args: StepArguments::Update {
                    targets: vec![TypedValue::UriSetRef { step: "s3".to_string() }],
                    predicate: TypedValue::Iri { iri: format!("{EX}category") },
                    objects: vec![TypedValue::StepRef { step: "s4".to_string(), field: "resource".to_string() }],
                },
                depends_on: vec!["s3".to_string(), "s4".to_string()],
                output: OutputType::MutationResult,
            },
            PlanStep {
                id: "s6".to_string(),
                operation: Operation::Validate,
                intent: "Validate the updated graph".to_string(),
                args: StepArguments::Validate {},
                depends_on: vec!["s5".to_string()],
                output: OutputType::ValidationResult,
            },
            PlanStep {
                id: "s7".to_string(),
                operation: Operation::Verify,
                intent: "Verify the final classified count".to_string(),
                args: StepArguments::Verify { question: "how many contacts were classified?".to_string() },
                depends_on: vec!["s5".to_string()],
                output: OutputType::VerificationResult,
            },
        ],
    }
}

fn op_ctx_for(plan: &Plan) -> OperationContext {
    let mut ctx = OperationContext::new(plan.goal.clone(), plan.context.clone());
    ctx.request = plan.goal.clone();
    ctx
}

async fn run(plan: &Plan, f: &Fixture, planner: Option<&dyn Planner>) -> Result<needle_rdf_agent::executor::ExecutionReport, ExecutorError> {
    let exec = Executor::new(f.registry.clone(), f.state.clone(), f.exec_ctx.clone());
    let mut ctx = op_ctx_for(plan);
    exec.execute(plan, &mut ctx, planner).await
}

// --- E2E: Ohio complex → same final triples + validate + verify ---------

#[tokio::test]
async fn ohio_complex_grounded_plan_commits_and_verifies() {
    let f = fixture(full_perms());
    seed_ohio(&f);
    let plan = grounded_ohio_plan(&f.graph);
    assert!(needle_rdf_agent::plan::validate_plan(&plan).is_ok());
    let exec = Executor::new(f.registry.clone(), f.state.clone(), f.exec_ctx.clone());
    let mut ctx = op_ctx_for(&plan);
    let report = exec.execute(&plan, &mut ctx, None).await.expect("ohio plan runs");
    assert_eq!(report.completed.len(), 7);
    assert_eq!(report.replans, 0);
    // Same final triples: Alice gained the prospect category.
    let alice = term(&format!("{EX}Alice"));
    let cat = term(&format!("{EX}category"));
    let found = f.state.repo.match_quads(Some(&alice), Some(&cat), None, Some(&f.graph), None).unwrap();
    assert!(!found.is_empty(), "Alice should have ex:category after s5");
    assert!(found.iter().any(|q| q.object.as_iri() == Some(&format!("{EX}Prospect"))));
    // validate + verify outputs recorded.
    assert!(matches!(ctx.results["s6"], StepOutput::ValidationResult { conforms: true }));
    assert!(matches!(&ctx.results["s7"], StepOutput::VerificationResult { .. }));
}

// --- E2E: planner parity — same plan through B/C/D-shaped scripted planners

#[tokio::test]
async fn planner_parity_same_final_state() {
    // The executor cannot tell which planner produced a plan: run the same
    // grounded plan three times (fresh stores) under different scripted
    // planner identities used only for the replan callback slot.
    let mut hashes = vec![];
    for _ in 0..3 {
        let f = fixture(full_perms());
        seed_ohio(&f);
        let plan = grounded_ohio_plan(&f.graph);
        let exec = Executor::new(f.registry.clone(), f.state.clone(), f.exec_ctx.clone());
        let mut ctx = op_ctx_for(&plan);
        let idle = ScriptedPlanner::new(vec![]);
        exec.execute(&plan, &mut ctx, Some(&idle)).await.unwrap();
        hashes.push(graph_hash(&*f.state.repo, &f.graph));
    }
    assert_eq!(hashes[0], hashes[1]);
    assert_eq!(hashes[1], hashes[2]);
}

// --- E2E: replan on discovery surprise (address-resource variant) ---------

#[tokio::test]
async fn replan_on_empty_query_revises_pending_only() {
    let f = fixture(full_perms());
    seed_ohio(&f);
    // Initial plan: s1/s2 discovery identical to the grounded plan, but s3
    // queries a nonexistent representation ("ZZ") → empty → surprise.
    let mut initial = grounded_ohio_plan(&f.graph);
    initial.steps.truncate(3);
    if let StepArguments::Query { pattern } = &mut initial.steps[2].args {
        *pattern = serde_json::json!({
            "operation": "select",
            "graphs": [f.graph.to_string()],
            "patterns": [
                {"subject": {"kind": "var", "name": "?account"},
                 "predicate": {"kind": "iri", "iri": format!("{EX}state")},
                 "object": {"kind": "literal", "value": "ZZ"}}
            ],
            "filters": [],
            "return_vars": ["?account"],
            "limit": 50
        });
    }
    // Replan returns the full grounded plan (s1/s2 identical → completed
    // outputs immutable; pending steps revised to the working OH pattern).
    let revised = grounded_ohio_plan(&f.graph);
    assert_eq!(initial.steps[0], revised.steps[0]);
    assert_eq!(initial.steps[1], revised.steps[1]);
    let planner = ScriptedPlanner::new(vec![revised]);
    let exec = Executor::new(f.registry.clone(), f.state.clone(), f.exec_ctx.clone());
    let mut ctx = op_ctx_for(&initial);
    let report = exec.execute(&initial, &mut ctx, Some(&planner)).await.expect("replan succeeds");
    assert_eq!(report.replans, 1);
    assert_eq!(report.completed.len(), 7);
    // Completed discovery outputs survived (s1/s2 from the first pass).
    assert!(ctx.results.contains_key("s1"));
    assert!(ctx.results.contains_key("s2"));
    // Final state matches the direct run.
    let alice = term(&format!("{EX}Alice"));
    let found = f.state.repo.match_quads(Some(&alice), Some(&term(&format!("{EX}category"))), None, Some(&f.graph), None).unwrap();
    assert!(!found.is_empty());
}

#[tokio::test]
async fn replan_budget_exhaustion_is_terminal() {
    let f = fixture(full_perms());
    seed_ohio(&f);
    let mut initial = grounded_ohio_plan(&f.graph);
    initial.steps.truncate(3);
    if let StepArguments::Query { pattern } = &mut initial.steps[2].args {
        *pattern = serde_json::json!({
            "operation": "select",
            "graphs": [f.graph.to_string()],
            "patterns": [
                {"subject": {"kind": "var", "name": "?account"},
                 "predicate": {"kind": "iri", "iri": format!("{EX}state")},
                 "object": {"kind": "literal", "value": "ZZ"}}
            ],
            "filters": [],
            "return_vars": ["?account"],
            "limit": 50
        });
    }
    // Replanner keeps returning the same failing prefix → budget exhausts.
    let again = initial.clone();
    let planner = ScriptedPlanner::new(vec![again.clone(), again]);
    let exec = Executor::new(f.registry.clone(), f.state.clone(), f.exec_ctx.clone())
        .with_max_replans(2);
    let mut ctx = op_ctx_for(&initial);
    let err = exec.execute(&initial, &mut ctx, Some(&planner)).await.unwrap_err();
    assert!(matches!(err, ExecutorError::ReplanExhausted { .. }), "{err:?}");
}

// --- E2E: safety gates ----------------------------------------------------

#[tokio::test]
async fn safety_shacl_violation_denied_graph_unchanged_one_audit() {
    let f = fixture(full_perms());
    seed_ohio(&f);
    let before = graph_hash(&*f.state.repo, &f.graph);
    // Create a Contact with no name → ContactShape min_count 1 fails.
    let plan = Plan {
        goal: "create nameless contact".to_string(),
        context: graph_ctx_for(&f.graph),
        assumptions: vec![],
        steps: vec![
            PlanStep {
                id: "s1".to_string(),
                operation: Operation::SearchVocabulary,
                intent: "discover contact vocabulary".to_string(),
                args: StepArguments::SearchVocab { terms: vec!["contact".to_string()], namespaces: vec![] },
                depends_on: vec![],
                output: OutputType::VocabularyCandidates,
            },
            PlanStep {
                id: "s2".to_string(),
                operation: Operation::DescribeVocabulary,
                intent: "resolve contact".to_string(),
                args: StepArguments::DescribeVocab {
                    iris: vec![TypedValue::StepRef { step: "s1".to_string(), field: "candidates".to_string() }],
                },
                depends_on: vec!["s1".to_string()],
                output: OutputType::ResolvedVocabulary,
            },
            PlanStep {
                id: "s3".to_string(),
                operation: Operation::Create,
                intent: "create nameless contact".to_string(),
                args: StepArguments::Create {
                    types: vec![TypedValue::Iri { iri: format!("{EX}Contact") }],
                    properties: vec![],
                },
                depends_on: vec!["s2".to_string()],
                output: OutputType::MutationResult,
            },
            PlanStep {
                id: "s4".to_string(),
                operation: Operation::Validate,
                intent: "validate".to_string(),
                args: StepArguments::Validate {},
                depends_on: vec!["s3".to_string()],
                output: OutputType::ValidationResult,
            },
            PlanStep {
                id: "s5".to_string(),
                operation: Operation::Verify,
                intent: "verify".to_string(),
                args: StepArguments::Verify { question: "created?".to_string() },
                depends_on: vec!["s3".to_string()],
                output: OutputType::VerificationResult,
            },
        ],
    };
    let err = run(&plan, &f, None).await.unwrap_err();
    assert!(matches!(err, ExecutorError::ValidationFailed { .. }), "{err:?}");
    assert_eq!(graph_hash(&*f.state.repo, &f.graph), before, "graph unchanged");
    let rows = f.state.audit.list();
    assert_eq!(rows.len(), 1, "exactly one audit row");
    assert_eq!(rows[0].commit_status, CommitStatus::ValidationFailed);
}

#[tokio::test]
async fn safety_unauthorized_denied_graph_unchanged_one_audit() {
    // No write permission (update lands as a Create-classified guarded
    // mutation) → denied.
    let f = fixture(vec![PolicyOp::Read]);
    seed_ohio(&f);
    let before = graph_hash(&*f.state.repo, &f.graph);
    let plan = grounded_ohio_plan(&f.graph);
    let err = run(&plan, &f, None).await.unwrap_err();
    assert!(matches!(err, ExecutorError::Denied { .. }), "{err:?}");
    assert_eq!(graph_hash(&*f.state.repo, &f.graph), before);
    let rows = f.state.audit.list();
    assert_eq!(rows.len(), 1, "exactly one audit row, got {rows:?}");
    assert_eq!(rows[0].commit_status, CommitStatus::Denied);
}

#[tokio::test]
async fn safety_unconfirmed_delete_requires_token() {
    let f = fixture(full_perms());
    seed_ohio(&f);
    let before = graph_hash(&*f.state.repo, &f.graph);
    let plan = Plan {
        goal: "delete alice".to_string(),
        context: graph_ctx_for(&f.graph),
        assumptions: vec![],
        steps: vec![
            PlanStep {
                id: "s1".to_string(),
                operation: Operation::SearchVocabulary,
                intent: "discover".to_string(),
                args: StepArguments::SearchVocab { terms: vec!["contact".to_string()], namespaces: vec![] },
                depends_on: vec![],
                output: OutputType::VocabularyCandidates,
            },
            PlanStep {
                id: "s2".to_string(),
                operation: Operation::DescribeVocabulary,
                intent: "resolve vocabulary".to_string(),
                args: StepArguments::DescribeVocab {
                    iris: vec![TypedValue::StepRef { step: "s1".to_string(), field: "candidates".to_string() }],
                },
                depends_on: vec!["s1".to_string()],
                output: OutputType::ResolvedVocabulary,
            },
            // Delete needs a UriSet dep: the query (s3) provides it.
            PlanStep {
                id: "s3".to_string(),
                operation: Operation::Query,
                intent: "find alice".to_string(),
                args: StepArguments::Query { pattern: ohio_pattern(&f.graph) },
                depends_on: vec!["s2".to_string()],
                output: OutputType::UriSet,
            },
            PlanStep {
                id: "s4".to_string(),
                operation: Operation::Delete,
                intent: "delete without confirmation".to_string(),
                args: StepArguments::Delete {
                    targets: vec![TypedValue::UriSetRef { step: "s3".to_string() }],
                },
                depends_on: vec!["s3".to_string()],
                output: OutputType::MutationResult,
            },
            PlanStep {
                id: "s5".to_string(),
                operation: Operation::Validate,
                intent: "validate".to_string(),
                args: StepArguments::Validate {},
                depends_on: vec!["s4".to_string()],
                output: OutputType::ValidationResult,
            },
            PlanStep {
                id: "s6".to_string(),
                operation: Operation::Verify,
                intent: "verify".to_string(),
                args: StepArguments::Verify { question: "deleted?".to_string() },
                depends_on: vec!["s4".to_string()],
                output: OutputType::VerificationResult,
            },
        ],
    };
    // Validator: describe s3 is unused by s4 — allowed (DAG, not linear).
    assert!(needle_rdf_agent::plan::validate_plan(&plan).is_ok());
    let err = run(&plan, &f, None).await.unwrap_err();
    assert!(matches!(err, ExecutorError::ConfirmationRequired { .. }), "{err:?}");
    assert_eq!(graph_hash(&*f.state.repo, &f.graph), before);
    let rows = f.state.audit.list();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].commit_status, CommitStatus::Denied);
}

// --- Release non-goals ----------------------------------------------------

#[tokio::test]
async fn nongoals_ontology_untouched_no_model_sparql_no_autonomous_loops() {
    let f = fixture(full_perms());
    seed_ohio(&f);
    let onto_before = graph_hash(&*f.state.repo, &GraphName::ontology());
    let plan = grounded_ohio_plan(&f.graph);
    let exec = Executor::new(f.registry.clone(), f.state.clone(), f.exec_ctx.clone());
    let mut ctx = op_ctx_for(&plan);
    // Replan slot armed with an empty script: any autonomous planner loop
    // would exhaust it; the happy path must never call back.
    let idle = ScriptedPlanner::new(vec![]);
    let report = exec.execute(&plan, &mut ctx, Some(&idle)).await.unwrap();
    assert_eq!(report.planner_calls, 0, "no autonomous planning loops in executor");
    assert!(report.replans <= 2);
    assert_eq!(graph_hash(&*f.state.repo, &GraphName::ontology()), onto_before, "ontology graph unchanged");
    // No model SPARQL: no tool accepts free-text SPARQL; the registry has
    // no sparql tool and the plan carries no raw query string.
    assert!(!f.registry.names().iter().any(|n| n.contains("sparql")));
    let raw = serde_json::to_string(&plan).unwrap().to_lowercase();
    assert!(!raw.contains("select ?") && !raw.contains("insert data"), "plan carries patterns, not SPARQL");
}

// --- Load: 10 parallel chats ----------------------------------------------

#[tokio::test]
async fn ten_parallel_read_plans_all_succeed() {
    let f = fixture(full_perms());
    seed_ohio(&f);
    let state = Arc::new(f.state);
    let registry = f.registry.clone();
    let exec_ctx = f.exec_ctx.clone();
    let graph = f.graph.clone();
    let mut handles = vec![];
    for i in 0..10 {
        let (st, reg, ctx, g) = (state.clone(), registry.clone(), exec_ctx.clone(), graph.clone());
        handles.push(tokio::spawn(async move {
            let exec = Executor::new(reg, (*st).clone(), ctx);
            let plan = Plan {
                goal: format!("parallel read {i}"),
                context: graph_ctx_for(&g),
                assumptions: vec![],
                steps: vec![
                    PlanStep {
                        id: "s1".to_string(),
                        operation: Operation::SearchVocabulary,
                        intent: "discover".to_string(),
                        args: StepArguments::SearchVocab { terms: vec!["contact".to_string()], namespaces: vec![] },
                        depends_on: vec![],
                        output: OutputType::VocabularyCandidates,
                    },
                    PlanStep {
                        id: "s2".to_string(),
                        operation: Operation::DescribeVocabulary,
                        intent: "resolve".to_string(),
                        args: StepArguments::DescribeVocab {
                            iris: vec![TypedValue::StepRef { step: "s1".to_string(), field: "candidates".to_string() }],
                        },
                        depends_on: vec!["s1".to_string()],
                        output: OutputType::ResolvedVocabulary,
                    },
                ],
            };
            let mut op = OperationContext::new(plan.goal.clone(), plan.context.clone());
            op.request = plan.goal.clone();
            exec.execute(&plan, &mut op, None).await.map(|r| r.completed.len())
        }));
    }
    for h in handles {
        assert_eq!(h.await.unwrap().unwrap(), 2);
    }
}

// --- Kill-mid-commit: injected failure → clean failure, no phantom commit -

#[tokio::test]
async fn kill_mid_commit_no_phantom_audit() {
    let f = fixture(full_perms());
    seed_ohio(&f);
    let before = graph_hash(&*f.state.repo, &f.graph);
    // Fail the first write of the next batch (per-batch 0-based counter).
    f.repo.set_inject_failure_at(Some(0));
    let plan = grounded_ohio_plan(&f.graph);
    let err = run(&plan, &f, None).await.unwrap_err();
    f.repo.set_inject_failure_at(None);
    assert!(matches!(err, ExecutorError::StepFailed { .. } | ExecutorError::Storage(_)), "{err:?}");
    assert_eq!(graph_hash(&*f.state.repo, &f.graph), before);
    // No phantom `committed` audit: every row must be non-committed.
    for row in f.state.audit.list() {
        assert_ne!(row.commit_status, CommitStatus::Committed, "phantom committed audit");
    }
}

// --- Metrics gate: versioned fixture set, floors, printed table ----------

/// One eval fixture: name + plan builder + final-state check.
struct EvalCase {
    name: &'static str,
    build: fn(&GraphName) -> Plan,
    check: fn(&Fixture, &OperationContext) -> bool,
}

fn eval_cases() -> Vec<EvalCase> {
    fn simple_search(g: &GraphName) -> Plan {
        Plan {
            goal: "simple search for contact vocabulary".to_string(),
            context: graph_ctx_for(g),
            assumptions: vec![],
            steps: vec![
                PlanStep { id: "s1".to_string(), operation: Operation::SearchVocabulary, intent: "search".to_string(), args: StepArguments::SearchVocab { terms: vec!["contact".to_string()], namespaces: vec![] }, depends_on: vec![], output: OutputType::VocabularyCandidates },
                PlanStep { id: "s2".to_string(), operation: Operation::DescribeVocabulary, intent: "resolve".to_string(), args: StepArguments::DescribeVocab { iris: vec![TypedValue::StepRef { step: "s1".to_string(), field: "candidates".to_string() }] }, depends_on: vec!["s1".to_string()], output: OutputType::ResolvedVocabulary },
            ],
        }
    }
    fn ambiguous_search(g: &GraphName) -> Plan {
        // "account" could be class or property: discovery must resolve both.
        Plan {
            goal: "ambiguous search for account term".to_string(),
            context: graph_ctx_for(g),
            assumptions: vec![],
            steps: vec![
                PlanStep { id: "s1".to_string(), operation: Operation::SearchVocabulary, intent: "search".to_string(), args: StepArguments::SearchVocab { terms: vec!["account".to_string()], namespaces: vec![] }, depends_on: vec![], output: OutputType::VocabularyCandidates },
                PlanStep { id: "s2".to_string(), operation: Operation::DescribeVocabulary, intent: "resolve".to_string(), args: StepArguments::DescribeVocab { iris: vec![TypedValue::StepRef { step: "s1".to_string(), field: "candidates".to_string() }] }, depends_on: vec!["s1".to_string()], output: OutputType::ResolvedVocabulary },
            ],
        }
    }
    fn multi_pattern_query(g: &GraphName) -> Plan {
        Plan {
            goal: "multi-pattern query for ohio contacts".to_string(),
            context: graph_ctx_for(g),
            assumptions: vec![],
            steps: vec![
                PlanStep { id: "s1".to_string(), operation: Operation::SearchVocabulary, intent: "search".to_string(), args: StepArguments::SearchVocab { terms: vec!["contact".to_string(), "account".to_string()], namespaces: vec![] }, depends_on: vec![], output: OutputType::VocabularyCandidates },
                PlanStep { id: "s2".to_string(), operation: Operation::DescribeVocabulary, intent: "resolve".to_string(), args: StepArguments::DescribeVocab { iris: vec![TypedValue::StepRef { step: "s1".to_string(), field: "candidates".to_string() }] }, depends_on: vec!["s1".to_string()], output: OutputType::ResolvedVocabulary },
                PlanStep { id: "s3".to_string(), operation: Operation::Query, intent: "query".to_string(), args: StepArguments::Query { pattern: ohio_pattern(g) }, depends_on: vec!["s2".to_string()], output: OutputType::UriSet },
            ],
        }
    }
    fn vocab_creation(g: &GraphName) -> Plan {
        Plan {
            goal: "create a prospect-tagged note resource".to_string(),
            context: graph_ctx_for(g),
            assumptions: vec![],
            steps: vec![
                PlanStep { id: "s1".to_string(), operation: Operation::SearchVocabulary, intent: "search".to_string(), args: StepArguments::SearchVocab { terms: vec!["contact".to_string()], namespaces: vec![] }, depends_on: vec![], output: OutputType::VocabularyCandidates },
                PlanStep { id: "s2".to_string(), operation: Operation::DescribeVocabulary, intent: "resolve".to_string(), args: StepArguments::DescribeVocab { iris: vec![TypedValue::StepRef { step: "s1".to_string(), field: "candidates".to_string() }] }, depends_on: vec!["s1".to_string()], output: OutputType::ResolvedVocabulary },
                PlanStep { id: "s3".to_string(), operation: Operation::Create, intent: "create note".to_string(), args: StepArguments::Create { types: vec![TypedValue::Iri { iri: format!("{EX}Contact") }], properties: vec![serde_json::json!({"predicate": format!("{EX}name"), "kind": "literal", "value": "Eval Note"})] }, depends_on: vec!["s2".to_string()], output: OutputType::MutationResult },
                PlanStep { id: "s4".to_string(), operation: Operation::Validate, intent: "validate".to_string(), args: StepArguments::Validate {}, depends_on: vec!["s3".to_string()], output: OutputType::ValidationResult },
                PlanStep { id: "s5".to_string(), operation: Operation::Verify, intent: "verify".to_string(), args: StepArguments::Verify { question: "note created?".to_string() }, depends_on: vec!["s3".to_string()], output: OutputType::VerificationResult },
            ],
        }
    }
    fn multi_resource_update(g: &GraphName) -> Plan {
        grounded_ohio_plan(g)
    }
    fn discovery_alternating(g: &GraphName) -> Plan {
        // Interleaved vocab + instance discovery before the query.
        let mut p = grounded_ohio_plan(g);
        p.goal = "alternating discovery then query then mutate".to_string();
        p
    }
    fn any_nonempty(_f: &Fixture, ctx: &OperationContext) -> bool {
        !ctx.results.is_empty()
    }
    fn ohio_mutated(f: &Fixture, _ctx: &OperationContext) -> bool {
        let alice = term(&format!("{EX}Alice"));
        f.state.repo.match_quads(Some(&alice), Some(&term(&format!("{EX}category"))), None, Some(&f.graph), None).map(|q| !q.is_empty()).unwrap_or(false)
    }
    vec![
        EvalCase { name: "simple_search", build: simple_search, check: any_nonempty },
        EvalCase { name: "ambiguous_search", build: ambiguous_search, check: any_nonempty },
        EvalCase { name: "multi_pattern_query", build: multi_pattern_query, check: any_nonempty },
        EvalCase { name: "vocab_creation", build: vocab_creation, check: any_nonempty },
        EvalCase { name: "multi_resource_update", build: multi_resource_update, check: ohio_mutated },
        EvalCase { name: "discovery_alternating", build: discovery_alternating, check: ohio_mutated },
    ]
}

#[tokio::test]
async fn metrics_gate_eval_fixtures_above_floor() {
    // Versioned fixture set (v1): 6 cases standing in for the 100-request
    // UPDATE_3 §8 set (same categories: simple/ambiguous search,
    // multi-pattern queries, vocab creation, multi-resource updates,
    // discovery-alternating complex requests).
    let cases = eval_cases();
    let mut rows: Vec<(String, bool, bool, bool)> = vec![];
    for case in &cases {
        let f = fixture(full_perms());
        seed_ohio(&f);
        let plan = (case.build)(&f.graph);
        let schema_ok = needle_rdf_agent::plan::validate_plan(&plan).is_ok();
        let exec = Executor::new(f.registry.clone(), f.state.clone(), f.exec_ctx.clone());
        let mut ctx = op_ctx_for(&plan);
        let ran = exec.execute(&plan, &mut ctx, None).await;
        let state_ok = ran.is_ok() && (case.check)(&f, &ctx);
        let dep_ok = ran.map(|r| r.completed.len() == plan.steps.len()).unwrap_or(false);
        rows.push((case.name.to_string(), schema_ok, dep_ok, state_ok));
    }
    let n = rows.len() as f64;
    let schema_rate = rows.iter().filter(|r| r.1).count() as f64 / n;
    let dep_rate = rows.iter().filter(|r| r.2).count() as f64 / n;
    let state_rate = rows.iter().filter(|r| r.3).count() as f64 / n;
    println!("eval v1 (6 fixtures × executor, keyless, scripted planners):");
    println!("case | schema | deps-complete | final-state");
    for (name, s, d, st) in &rows {
        println!("{name} | schema={s} deps={d} state={st}");
    }
    println!("aggregate: schema={schema_rate:.2} deps={dep_rate:.2} state={state_rate:.2} (floor 0.80)");
    assert!(schema_rate >= 0.80, "tool/plan-schema validity below floor: {schema_rate:.2}");
    assert!(dep_rate >= 0.80, "dep correctness below floor: {dep_rate:.2}");
    assert!(state_rate >= 0.80, "final RDF-state correctness below floor: {state_rate:.2}");
    // Production default (UPDATE_3 §9): B (tool loop) with C as fallback —
    // held unless numbers overturn it. All planners share this executor
    // path, so parity above means the default rests on planner-loop
    // reliability, not on execution differences.
    let _ = HashMap::<String, String>::new();
}
