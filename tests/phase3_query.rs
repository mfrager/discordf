//! Plan 2 Phase 3 acceptance: typed query plans only, no raw query strings.

use needle_rdf_agent::config::AppLimits;
use needle_rdf_agent::ontology::{contact_shapes, load_default_ontology, OntologyReasoner};
use needle_rdf_agent::plan::GraphContext;
use needle_rdf_agent::policy::{AuditLog, ExecutionContext, Operation};
use needle_rdf_agent::rdf::{GraphName, OxigraphRepository, RdfRepository, Term};
use needle_rdf_agent::tools::{ToolRegistry, ToolState};
use std::sync::Arc;

const EX: &str = "http://example.com/";
const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

fn fixture() -> (ToolRegistry, ToolState, ExecutionContext, GraphContext, GraphName) {
    let repo = Arc::new(OxigraphRepository::new_in_memory(50_000).unwrap());
    load_default_ontology(&*repo).unwrap();
    let g = GraphName::tenant("acme").unwrap();
    let alice = Term::iri(format!("{EX}Alice")).unwrap();
    let acme = Term::iri(format!("{EX}Acme")).unwrap();
    let bob = Term::iri(format!("{EX}Bob")).unwrap();
    repo.insert(&[
        needle_rdf_agent::rdf::Quad::new(
            alice.clone(),
            Term::iri(RDF_TYPE.to_string()).unwrap(),
            Term::iri(format!("{EX}Contact")).unwrap(),
            g.clone(),
        )
        .unwrap(),
        needle_rdf_agent::rdf::Quad::new(
            alice.clone(),
            Term::iri(format!("{EX}name")).unwrap(),
            Term::literal("Alice"),
            g.clone(),
        )
        .unwrap(),
        needle_rdf_agent::rdf::Quad::new(
            alice.clone(),
            Term::iri(format!("{EX}account")).unwrap(),
            acme.clone(),
            g.clone(),
        )
        .unwrap(),
        needle_rdf_agent::rdf::Quad::new(
            acme.clone(),
            Term::iri(RDF_TYPE.to_string()).unwrap(),
            Term::iri(format!("{EX}Account")).unwrap(),
            g.clone(),
        )
        .unwrap(),
        needle_rdf_agent::rdf::Quad::new(
            acme.clone(),
            Term::iri(format!("{EX}state")).unwrap(),
            Term::literal("OH"),
            g.clone(),
        )
        .unwrap(),
        // Second account/contact for limit/truncation checks.
        needle_rdf_agent::rdf::Quad::new(
            bob.clone(),
            Term::iri(RDF_TYPE.to_string()).unwrap(),
            Term::iri(format!("{EX}Contact")).unwrap(),
            g.clone(),
        )
        .unwrap(),
        needle_rdf_agent::rdf::Quad::new(
            bob.clone(),
            Term::iri(format!("{EX}name")).unwrap(),
            Term::literal("Bob"),
            g.clone(),
        )
        .unwrap(),
        needle_rdf_agent::rdf::Quad::new(
            bob.clone(),
            Term::iri(format!("{EX}account")).unwrap(),
            acme.clone(),
            g.clone(),
        )
        .unwrap(),
    ])
    .unwrap();
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
    let ctx = ExecutionContext::new(
        "req-1",
        "alice",
        "acme",
        vec![g.clone(), GraphName::ontology()],
        vec![Operation::Read, Operation::Create],
    );
    let gctx = GraphContext {
        graphs: vec![g.clone()],
        namespaces: [("ex".to_string(), EX.to_string())].into_iter().collect(),
        ..Default::default()
    };
    (ToolRegistry::new(), state, ctx, gctx, g)
}

fn ohio_pattern(graph: &str) -> serde_json::Value {
    serde_json::json!({
        "operation": "select",
        "graphs": [graph],
        "patterns": [
            {"subject": {"kind": "var", "name": "?contact"},
             "predicate": {"kind": "iri", "iri": RDF_TYPE},
             "object": {"kind": "iri", "iri": format!("{EX}Contact")}},
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

#[test]
fn compile_emits_graph_limit_and_escapes() {
    let (reg, state, ctx, gctx, g) = fixture();
    let built = reg
        .dispatch_with_context(
            &state,
            &ctx,
            &gctx,
            "build_query",
            serde_json::json!({"pattern": ohio_pattern(&g.to_string())}),
        )
        .unwrap();
    let compiled = built["compiled"].as_str().unwrap();
    assert!(compiled.contains(&format!("GRAPH <{}>", g)), "{compiled}");
    assert!(compiled.contains("LIMIT 50"), "{compiled}");
    assert!(!compiled.to_uppercase().contains("UPDATE DATA"), "{compiled}");
}

#[test]
fn rejection_matrix() {
    let (reg, state, ctx, gctx, g) = fixture();

    // Unknown predicate -> grounding.
    let mut bad = ohio_pattern(&g.to_string());
    bad["patterns"][1]["predicate"]["iri"] = serde_json::json!(format!("{EX}nope"));
    let e = reg
        .dispatch_with_context(
            &state,
            &ctx,
            &gctx,
            "build_query",
            serde_json::json!({"pattern": bad}),
        )
        .unwrap_err();
    assert_eq!(e.category, "grounding", "{e:?}");

    // Domain mismatch: account predicate on an Account subject.
    let mut bad2 = ohio_pattern(&g.to_string());
    bad2["patterns"][1]["subject"] = serde_json::json!({"kind": "iri", "iri": format!("{EX}Acme")});
    let e2 = reg
        .dispatch_with_context(
            &state,
            &ctx,
            &gctx,
            "build_query",
            serde_json::json!({"pattern": bad2}),
        )
        .unwrap_err();
    assert_eq!(e2.category, "validation", "{e2:?}");

    // Cross-graph escape.
    let bad3 = ohio_pattern("urn:needle:tenant:other");
    let e3 = reg
        .dispatch_with_context(
            &state,
            &ctx,
            &gctx,
            "build_query",
            serde_json::json!({"pattern": bad3}),
        )
        .unwrap_err();
    assert_eq!(e3.category, "auth", "{e3:?}");

    // Smuggled update language in a filter -> query-safety.
    let mut bad4 = ohio_pattern(&g.to_string());
    bad4["filters"] = serde_json::json!([{"kind": "contains", "var": "?contact", "substring": "x } DELETE { ?s ?p ?o"}]);
    let e4 = reg
        .dispatch_with_context(
            &state,
            &ctx,
            &gctx,
            "build_query",
            serde_json::json!({"pattern": bad4}),
        )
        .unwrap_err();
    assert_eq!(e4.category, "query-safety", "{e4:?}");
}

#[test]
fn three_tool_flow_deterministic_with_truncation_flag() {
    let (reg, state, ctx, gctx, g) = fixture();
    let built = reg
        .dispatch_with_context(
            &state,
            &ctx,
            &gctx,
            "build_query",
            serde_json::json!({"pattern": ohio_pattern(&g.to_string())}),
        )
        .unwrap();
    let id = built["plan_id"].as_str().unwrap().to_string();

    let insp = reg
        .dispatch_with_context(
            &state,
            &ctx,
            &gctx,
            "inspect_query",
            serde_json::json!({"plan_id": id}),
        )
        .unwrap();
    assert!(insp["explanation"]
        .as_str()
        .unwrap()
        .contains("3 pattern"));
    assert!(insp.get("match_estimate").is_some());

    let r1 = reg
        .dispatch_with_context(
            &state,
            &ctx,
            &gctx,
            "execute_query",
            serde_json::json!({"plan_id": id}),
        )
        .unwrap();
    let r2 = reg
        .dispatch_with_context(
            &state,
            &ctx,
            &gctx,
            "execute_query",
            serde_json::json!({"plan_id": id}),
        )
        .unwrap();
    assert_eq!(r1, r2, "execution must be deterministic");
    let uris: Vec<String> = serde_json::from_value(r1["uris"].clone()).unwrap();
    assert!(uris.contains(&format!("{EX}Alice")));
    assert!(uris.contains(&format!("{EX}Bob")));
    assert!(r1.get("truncated").is_some());
}

#[test]
fn representation_trap_ohio_vs_oh() {
    let (reg, state, ctx, gctx, g) = fixture();
    // Planner assumes "Ohio"; store has "OH": expect 0 rows + a hint, not success.
    let mut trap = ohio_pattern(&g.to_string());
    trap["patterns"][2]["object"] = serde_json::json!({"kind": "literal", "value": "Ohio"});
    let built = reg
        .dispatch_with_context(
            &state,
            &ctx,
            &gctx,
            "build_query",
            serde_json::json!({"pattern": trap}),
        )
        .unwrap();
    let reps = built["representations"].as_array().unwrap();
    assert!(!reps.is_empty());
    let hint = reps[0]["hint"].as_str().unwrap();
    assert!(hint.contains("NOT found"), "{hint}");
    assert!(reps[0]["stored_samples"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s.as_str().unwrap().contains("OH")));

    let id = built["plan_id"].as_str().unwrap();
    let run = reg
        .dispatch_with_context(
            &state,
            &ctx,
            &gctx,
            "execute_query",
            serde_json::json!({"plan_id": id}),
        )
        .unwrap();
    assert_eq!(run["count"], 0);
    assert_eq!(run["uris"].as_array().unwrap().len(), 0);
}

#[test]
fn inspect_exposes_sparql_alias_and_result_wires_into_context() {
    use needle_rdf_agent::plan::OperationContext;
    let (reg, state, ctx, gctx, g) = fixture();
    let built = reg
        .dispatch_with_context(
            &state,
            &ctx,
            &gctx,
            "build_query",
            serde_json::json!({"pattern": ohio_pattern(&g.to_string())}),
        )
        .unwrap();
    // Spec field name: inspect/build expose the compiled text as `sparql`.
    assert_eq!(built["sparql"], built["compiled"]);
    let id = built["plan_id"].as_str().unwrap().to_string();
    let insp = reg
        .dispatch_with_context(
            &state,
            &ctx,
            &gctx,
            "inspect_query",
            serde_json::json!({"plan_id": id}),
        )
        .unwrap();
    assert_eq!(insp["sparql"], insp["compiled"]);

    let run = reg
        .dispatch_with_context(
            &state,
            &ctx,
            &gctx,
            "execute_query",
            serde_json::json!({"plan_id": id}),
        )
        .unwrap();
    // Phase 3 §4: results wire back into OperationContext keyed by plan id.
    let mut op_ctx = OperationContext::new("ohio prospects", gctx.clone());
    let uris: Vec<String> = serde_json::from_value(run["uris"].clone()).unwrap();
    op_ctx.record_query_result(&id, uris.clone());
    assert_eq!(op_ctx.query_uris(&id).unwrap(), uris.as_slice());
}
