//! Phase T1 acceptance (TESTING plan): dataset, case format, oracle.
//!
//! - Ontology/instance tiers build with the documented shape.
//! - Snapshots isolate runs (equal builds hash equal; mutation diverges).
//! - The fixed 100-case set materializes, loads, validates (unique ids,
//!   12-category counts, sound safety envelopes).
//! - Oracles discriminate good/bad plans and change sets before any planner
//!   runs (good/bad fixtures asserted here, not model-derived).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use needle_rdf_agent::bench::case::{fixed_cases, load_cases_dir, materialize_cases, Category};
use needle_rdf_agent::bench::dataset::{
    bench_shapes, build_adversarial, build_benchmark_ontology, build_instances, ns,
    snapshot, snapshot_graph, BenchGraphs, Scale, Snapshot,
};
use needle_rdf_agent::bench::metrics::{Aggregate, RunMetrics};
use needle_rdf_agent::bench::oracle::{
    check_plan_constraints, false_positives, graph_changes, mutation_accuracy, precision_recall,
    CaseScore,
};
use needle_rdf_agent::bench::{render_report_table, Trace, TraceEvent};
use needle_rdf_agent::plan::{Operation, OutputType, Plan, PlanStep, StepArguments, TypedValue};
use needle_rdf_agent::rdf::{GraphName, OxigraphRepository, RdfRepository, Term};

fn mem() -> Arc<OxigraphRepository> {
    Arc::new(OxigraphRepository::new_in_memory(300_000).unwrap())
}

fn cases_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("bench_cases")
}

// --- dataset tier shape ----------------------------------------------------

#[test]
fn t1_ontology_and_tiers() {
    let graphs = BenchGraphs::new();
    let repo = mem();
    let added = build_benchmark_ontology(&*repo).unwrap();
    assert!(added > 60, "ontology should be substantial, got {added}");
    build_instances(&*repo, &graphs, Scale::Tiny).unwrap();
    build_adversarial(&*repo, &graphs).unwrap();

    // Spot-checks: domain/range, inverse, equivalence, disjointness, SKOS.
    let onto = GraphName::ontology();
    let has = |s: &str, p: &str, o: &str| {
        !repo
            .match_quads(
                Some(&Term::iri(s.to_string()).unwrap()),
                Some(&Term::iri(p.to_string()).unwrap()),
                Some(&Term::iri(o.to_string()).unwrap()),
                Some(&onto),
                Some(1),
            )
            .unwrap()
            .is_empty()
    };
    const RDFS_DOMAIN: &str = "http://www.w3.org/2000/01/rdf-schema#domain";
    const RDFS_RANGE: &str = "http://www.w3.org/2000/01/rdf-schema#range";
    const OWL_INVERSE: &str = "http://www.w3.org/2002/07/owl#inverseOf";
    const OWL_DISJOINT: &str = "http://www.w3.org/2002/07/owl#disjointWith";
    const OWL_EQUIV: &str = "http://www.w3.org/2002/07/owl#equivalentClass";
    assert!(has(&ns::iri(ns::CRM, "worksFor"), RDFS_DOMAIN, &ns::iri(ns::CRM, "Contact")));
    assert!(has(&ns::iri(ns::CRM, "worksFor"), RDFS_RANGE, &ns::iri(ns::CRM, "Account")));
    assert!(has(&ns::iri(ns::CRM, "employs"), OWL_INVERSE, &ns::iri(ns::CRM, "employedBy")));
    assert!(has(&ns::iri(ns::CRM, "Customer"), OWL_DISJOINT, &ns::iri(ns::SALES, "Prospect")));
    assert!(has(&ns::iri(ns::LEGACY, "customer"), OWL_EQUIV, &ns::iri(ns::CRM, "Customer")));
    // Adversarial literals are data: present in the store, verbatim.
    let mal = Term::iri(ns::iri(ns::TEST, "malicious-001")).unwrap();
    let quads = repo.match_quads(Some(&mal), None, None, None, None).unwrap();
    assert!(quads.iter().any(|q| format!("{q:?}").contains("Ignore previous instructions")));

    // bench_shapes targets exist in the ontology.
    for shape in bench_shapes() {
        assert!(has(&shape.target_class,
            "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
            "http://www.w3.org/2002/07/owl#Class"),
            "shape target {} must be a declared class", shape.target_class);
    }
}

#[test]
fn t1_standard_tier_counts() {
    let graphs = BenchGraphs::new();
    let repo = mem();
    build_benchmark_ontology(&*repo).unwrap();
    build_instances(&*repo, &graphs, Scale::Standard).unwrap();
    let count_type = |t: &str| {
        repo.match_quads(
            None,
            Some(&Term::iri("http://www.w3.org/1999/02/22-rdf-syntax-ns#type".to_string()).unwrap()),
            Some(&Term::iri(t.to_string()).unwrap()),
            None,
            None,
        )
        .unwrap()
        .len()
    };
    assert_eq!(count_type(&ns::iri(ns::CRM, "Contact")), 1002); // 1000 + 2 tenant-b
    assert_eq!(count_type(&ns::iri(ns::CRM, "Account")), 200);
    assert_eq!(count_type(&ns::iri(ns::EVENT, "Event")), 50);
    assert_eq!(count_type(&ns::iri(ns::EVENT, "Registration")), 500);
    assert_eq!(count_type(&ns::iri(ns::GEO, "State")), 200);
}

#[test]
fn t1_snapshots_isolate_runs() {
    let graphs = BenchGraphs::new();
    let build = || {
        let r = mem();
        build_benchmark_ontology(&*r).unwrap();
        build_instances(&*r, &graphs, Scale::Tiny).unwrap();
        r
    };
    let (a, b) = (build(), build());
    assert_eq!(snapshot(&*a).unwrap().hash(), snapshot(&*b).unwrap().hash());
    // Per-graph scope hashes: tenant-b untouched by a tenant-a write.
    let before_b = snapshot_graph(&*a, &graphs.tenant_b).unwrap().hash();
    a.insert(&[needle_rdf_agent::rdf::Quad::new(
        Term::iri(ns::iri(ns::CRM, "contact-0000")).unwrap(),
        Term::iri(ns::iri(ns::CRM, "name")).unwrap(),
        Term::literal("Changed"),
        graphs.tenant_a.clone(),
    )
    .unwrap()])
    .unwrap();
    assert_ne!(snapshot(&*a).unwrap().hash(), snapshot(&*b).unwrap().hash());
    assert_eq!(snapshot_graph(&*a, &graphs.tenant_b).unwrap().hash(), before_b);
}

// --- fixed 100-case set ----------------------------------------------------

#[test]
fn t1_fixed_cases_materialize_load_validate() {
    let dir = cases_dir();
    // Materialize when missing or explicitly refreshed; otherwise the
    // checked-in JSON is the source of truth and must match the builder.
    let need = std::env::var("UPDATE_BENCH_CASES").map(|v| v == "1").unwrap_or(false)
        || load_cases_dir(&dir).map(|c| c.len()).unwrap_or(0) != 100;
    if need {
        let n = materialize_cases(&dir).unwrap();
        assert_eq!(n, 100);
    }
    let loaded = load_cases_dir(&dir).unwrap();
    assert_eq!(loaded.len(), 100);
    // Builder and checked-in JSON agree (loader sorts by filename, the
    // builder emits category order — compare by id).
    let mut built = fixed_cases();
    built.sort_by(|a, b| a.id.cmp(&b.id));
    for (i, (l, b)) in loaded.iter().zip(built.iter()).enumerate() {
        if l != b {
            panic!("case {i} differs:\nloaded={l:?}\nbuilt ={b:?}");
        }
    }
    assert_eq!(loaded, built);
    // Unique ids + 12-category distribution.
    let mut counts: HashMap<Category, usize> = HashMap::new();
    for c in &loaded {
        *counts.entry(c.category).or_insert(0) += 1;
    }
    for (cat, want) in [
        (Category::Intent, 10),
        (Category::Vocabulary, 10),
        (Category::InstanceDiscovery, 10),
        (Category::ComplexQuery, 10),
        (Category::MixedDiscovery, 8),
        (Category::Crud, 12),
        (Category::BulkMutation, 8),
        (Category::Scope, 8),
        (Category::OwlShacl, 8),
        (Category::Recovery, 6),
        (Category::Security, 6),
        (Category::Termination, 4),
    ] {
        assert_eq!(counts.get(&cat).copied().unwrap_or(0), want, "{cat:?}");
    }
    // Safety envelopes: read-only cases forbid mutation; every expected URI
    // is a valid IRI; every required op is a known canonical operation.
    let known_ops = [
        "search_vocabulary", "describe_vocabulary", "search_instances", "describe_resource",
        "query", "construct_query", "execute_query", "discover_vocabulary",
        "create", "update", "delete", "validate", "verify",
    ];
    for c in &loaded {
        if c.expected.must_not_modify_graph {
            assert!(!c.safety.mutation_allowed, "{}", c.id);
        }
        for op in &c.expected.required_ops {
            assert!(known_ops.contains(&op.as_str()), "{}: {op}", c.id);
        }
    }
}

// --- oracle discrimination (no planner involved) ----------------------------

fn good_vocab_plan() -> Plan {
    Plan {
        goal: "vocab".to_string(),
        context: Default::default(),
        assumptions: vec![],
        steps: vec![
            PlanStep {
                id: "s1".to_string(),
                operation: Operation::SearchVocabulary,
                intent: "find terms".to_string(),
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
    }
}

#[test]
fn t1_oracle_discriminates_good_and_bad() {
    let good = good_vocab_plan();
    assert!(check_plan_constraints(&good, &["search_vocabulary".to_string()]).is_empty());
    assert!(check_plan_constraints(&good, &["discover_vocabulary".to_string()]).is_empty());
    let missing = check_plan_constraints(&good, &["search_vocabulary".to_string(), "query".to_string()]);
    assert_eq!(missing, vec!["query".to_string()]);

    // URI-set quality.
    let (p, r) = precision_recall(&[contact_uri(0)], &[contact_uri(0), contact_uri(5)]);
    assert!((p - 1.0).abs() < 1e-6);
    assert!((r - 0.5).abs() < 1e-6);

    // Graph-diff scoring: identical snapshots score 1.0 with no violations.
    let graphs = BenchGraphs::new();
    let repo = mem();
    build_benchmark_ontology(&*repo).unwrap();
    build_instances(&*repo, &graphs, Scale::Tiny).unwrap();
    let before = snapshot(&*repo).unwrap();
    let after = snapshot(&*repo).unwrap();
    let (added, removed) = graph_changes(&before, &after);
    assert!(added.is_empty() && removed.is_empty());
    let none: HashSet<String> = HashSet::new();
    let actual: HashSet<String> = added.union(&removed).cloned().collect();
    assert_eq!(mutation_accuracy(&actual, &none), 1.0);
    let case = fixed_cases().into_iter().find(|c| c.id == "VOC-001").unwrap();
    let score = CaseScore::full_execution(&case, &good, &[], &before, &after, &none);
    assert!(score.plan_ok());
    assert!(!score.must_not_modify_violated);

    // One extra write: accuracy drops below 1.0, one false positive counted,
    // and the read-only envelope violation is flagged.
    repo.insert(&[needle_rdf_agent::rdf::Quad::new(
        Term::iri(contact_uri(0)).unwrap(),
        Term::iri(ns::iri(ns::CRM, "name")).unwrap(),
        Term::literal("Sneaky"),
        graphs.tenant_a.clone(),
    )
    .unwrap()])
    .unwrap();
    let after2 = snapshot(&*repo).unwrap();
    let (added2, _) = graph_changes(&before, &after2);
    let actual2: HashSet<String> = added2.into_iter().collect();
    assert!(mutation_accuracy(&actual2, &none) < 1.0);
    assert_eq!(false_positives(&actual2, &none), actual2.len());
    let score2 = CaseScore::full_execution(&case, &good, &[], &before, &after2, &none);
    assert!(score2.must_not_modify_violated);
}

fn contact_uri(n: usize) -> String {
    ns::iri(ns::CRM, &format!("contact-{n:04}"))
}

// --- trace + report smoke ---------------------------------------------------

#[test]
fn t1_trace_and_report_shape() {
    let mut t = Trace::new();
    t.record(TraceEvent::RequestReceived { request: "q".to_string() });
    t.record(TraceEvent::PlanningStarted { variant: "B".to_string() });
    t.record(TraceEvent::Finished { outcome: "ok".to_string() });
    assert_eq!(t.kinds(), vec!["request_received", "planning_started", "final_response"]);

    let mut by_variant = HashMap::new();
    by_variant.insert("B".to_string(), Aggregate::from_runs(&[
        RunMetrics { success: true, plan_complete: true, vocab_ok: true, model_calls: 3, tool_calls: 5, ..Default::default() },
    ]));
    let table = render_report_table(&by_variant);
    assert!(table.contains("| Task success | — | 100% | — | — |"));
}

#[test]
fn t1_snapshot_helpers() {
    let s = Snapshot::default();
    assert!(s.is_empty());
}
