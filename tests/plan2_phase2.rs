//! Plan 2 Phase 2 acceptance: canonical plan, split search, context gates.

use needle_rdf_agent::config::AppLimits;
use needle_rdf_agent::ontology::{contact_shapes, load_default_ontology, OntologyReasoner};
use needle_rdf_agent::plan::{canonical_ohio_plan, validate_plan, GraphContext, OperationContext};
use needle_rdf_agent::policy::{AuditLog, ExecutionContext, Operation};
use needle_rdf_agent::rdf::{GraphName, OxigraphRepository, RdfRepository, Term};
use needle_rdf_agent::tools::{ToolRegistry, ToolState};
use std::sync::Arc;

fn fixture() -> (ToolRegistry, ToolState, ExecutionContext, GraphName) {
    let repo = Arc::new(OxigraphRepository::new_in_memory(50_000).unwrap());
    load_default_ontology(&*repo).unwrap();
    let g = GraphName::tenant("acme").unwrap();
    // Instance data.
    repo.insert(&[needle_rdf_agent::rdf::Quad::new(
        Term::iri("http://example.com/Alice".to_string()).unwrap(),
        Term::iri("http://www.w3.org/1999/02/22-rdf-syntax-ns#type".to_string()).unwrap(),
        Term::iri("http://example.com/Contact".to_string()).unwrap(),
        g.clone(),
    )
    .unwrap()])
        .unwrap();
    repo.insert(&[needle_rdf_agent::rdf::Quad::new(
        Term::iri("http://example.com/Alice".to_string()).unwrap(),
        Term::iri("http://example.com/name".to_string()).unwrap(),
        Term::literal("Alice"),
        g.clone(),
    )
    .unwrap()])
        .unwrap();
    let reasoner = Arc::new(OntologyReasoner::from_repository(&*repo).unwrap());
    let search = Arc::new(needle_rdf_agent::search::TantivyIndex::in_ram().unwrap());
    {
        use needle_rdf_agent::search::SearchService;
        search.rebuild_from_repo(&*repo).unwrap();
    }
    let audit = Arc::new(AuditLog::in_memory());
    let state = ToolState::new(
        repo,
        search,
        reasoner,
        contact_shapes(),
        audit,
        AppLimits::default(),
    );
    let ctx = ExecutionContext::new(
        "req-1",
        "alice",
        "acme",
        vec![g.clone(), GraphName::ontology()],
        vec![
            Operation::Read,
            Operation::Create,
            Operation::Update,
            Operation::Delete,
        ],
    );
    (ToolRegistry::new(), state, ctx, g)
}

#[test]
fn canonical_ohio_plan_validates_and_summarizes() {
    let plan = canonical_ohio_plan();
    assert!(validate_plan(&plan).is_ok());
    let ctx = OperationContext::new(plan.goal.clone(), plan.context.clone());
    let s = ctx.status_summary(&plan);
    assert!(s.contains("Classify contacts"));
    assert!(s.len() < 4000);
    // Golden shape.
    let v = serde_json::to_value(&plan).unwrap();
    assert_eq!(v["steps"].as_array().unwrap().len(), 7);
}

#[test]
fn split_search_vocab_vs_instances() {
    let (reg, state, ctx, _) = fixture();
    let gctx = GraphContext::default();
    // Vocabulary: finds the class, never the instance.
    let v = reg
        .dispatch_with_context(
            &state,
            &ctx,
            &gctx,
            "search_vocabulary",
            serde_json::json!({"query": "contact"}),
        )
        .unwrap();
    assert_eq!(v["output"], "VocabularyCandidates");
    let iris: Vec<String> = v["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["iri"].as_str().unwrap().to_string())
        .collect();
    assert!(iris.contains(&"http://example.com/Contact".to_string()));
    assert!(!iris.contains(&"http://example.com/Alice".to_string()));

    // Instances: finds Alice, never the class.
    let v2 = reg
        .dispatch_with_context(
            &state,
            &ctx,
            &gctx,
            "search_instances",
            serde_json::json!({"text": "Alice"}),
        )
        .unwrap();
    assert_eq!(v2["output"], "ResourceCandidates");
    assert_ne!(v["output"], v2["output"]);
    let uris: Vec<String> = v2["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["uri"].as_str().unwrap().to_string())
        .collect();
    assert!(uris.contains(&"http://example.com/Alice".to_string()));
    assert!(!uris.contains(&"http://example.com/Contact".to_string()));
}

#[test]
fn context_gates_deny_and_scope_limit() {
    let (reg, state, ctx, g) = fixture();
    // Scoped GraphContext limited to acme; other graph denied.
    let scoped = GraphContext {
        graphs: vec![g.clone()],
        namespaces: [("ex".to_string(), "http://example.com/".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    // search_instances on a graph outside the plan context -> auth error.
    let e = reg
        .dispatch_with_context(
            &state,
            &ctx,
            &scoped,
            "search_instances",
            serde_json::json!({"text": "Alice", "context": "urn:needle:tenant:other"}),
        )
        .unwrap_err();
    assert_eq!(e.category, "auth");

    // create on an unauthorized graph -> denied via guarded path.
    let e2 = reg
        .dispatch(
            &state,
            &ctx,
            "create",
            serde_json::json!({
                "graph": "urn:needle:tenant:other",
                "types": ["http://example.com/Contact"],
                "properties": [{"predicate": "http://example.com/name", "kind": "literal", "value": "Zed"}]
            }),
        )
        .unwrap_err();
    assert_eq!(e2.category, "auth");

    // Vocabulary search scoped to an unlisted namespace -> empty + scope-limited.
    let v = reg
        .dispatch_with_context(
            &state,
            &ctx,
            &scoped,
            "search_vocabulary",
            serde_json::json!({"query": "contact", "namespaces": ["http://other.com/"]}),
        )
        .unwrap();
    assert_eq!(v["results"].as_array().unwrap().len(), 0);
    assert_eq!(v["scope_limited"], true);
}

#[test]
fn validate_and_verify_round_trip() {
    let (reg, state, ctx, g) = fixture();
    let gctx = GraphContext::default();
    let v = reg
        .dispatch_with_context(
            &state,
            &ctx,
            &gctx,
            "validate",
            serde_json::json!({"graph": g.to_string(), "uri": "http://example.com/Alice"}),
        )
        .unwrap();
    assert_eq!(v["output"], "ValidationResult");
    assert!(v.get("conforms").is_some());

    let v2 = reg
        .dispatch_with_context(
            &state,
            &ctx,
            &gctx,
            "verify",
            serde_json::json!({"question": "how many?", "graph": g.to_string()}),
        )
        .unwrap();
    assert_eq!(v2["output"], "VerificationResult");
    assert!(v2.get("quad_count").is_some());
}
