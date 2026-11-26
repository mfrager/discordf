//! Phase 1 acceptance tests: CRUD, deltas, named-graph isolation,
//! atomicity (injected mid-batch failure), blank nodes, result caps.
//!
//! These run against the real Oxigraph backend through the
//! `RdfRepository` trait only — no Oxigraph imports here.

use needle_rdf_agent::rdf::{GraphName, OxigraphRepository, Quad, RdfRepository, Term};

fn repo() -> OxigraphRepository {
    OxigraphRepository::new_in_memory(500).expect("in-memory store")
}

fn ex(local: &str) -> Term {
    Term::iri(format!("http://example.com/{local}")).unwrap()
}

fn quad(s: &str, p: &str, o: Term, graph: &GraphName) -> Quad {
    Quad::new(ex(s), ex(p), o, graph.clone()).unwrap()
}

fn graph(name: &str) -> GraphName {
    GraphName::named(format!("urn:needle:test:{name}")).unwrap()
}

// --- CRUD + delta exactness -------------------------------------------------

#[test]
fn crud_returns_exact_deltas() {
    let r = repo();
    let g = graph("crud");
    let q1 = quad("Alice", "knows", ex("Bob"), &g);
    let q2 = quad(
        "Alice",
        "name",
        Term::lang_literal("Alice", "en").unwrap(),
        &g,
    );

    // Create.
    let d = r.insert(&[q1.clone(), q2.clone()]).unwrap();
    assert_eq!(d.added.len(), 2);
    assert!(d.removed.is_empty());

    // Read back.
    let got = r.quads_for_subject(&ex("Alice"), Some(&g), None).unwrap();
    assert_eq!(got.len(), 2);

    // Double insert is idempotent: nothing newly added.
    let d2 = r.insert(&[q1.clone(), q2.clone()]).unwrap();
    assert!(d2.added.is_empty(), "re-insert must report empty delta");

    // Update = atomic remove-old + add-new.
    let q2_new = quad("Alice", "name", Term::literal("Alicia"), &g);
    let d3 = r.apply_atomic(&[q2_new.clone()], &[q2.clone()]).unwrap();
    assert_eq!(d3.added, vec![q2_new.clone()]);
    assert_eq!(d3.removed, vec![q2.clone()]);
    assert!(r.contains(&q2_new).unwrap());
    assert!(!r.contains(&q2).unwrap());

    // Delete one quad.
    let d4 = r.remove(&[q1.clone()]).unwrap();
    assert_eq!(d4.removed, vec![q1.clone()]);
    assert!(!r.contains(&q1).unwrap());

    // Removing an absent quad reports an empty delta (no error).
    let d5 = r.remove(&[q1.clone()]).unwrap();
    assert!(d5.is_empty());
}

// --- Typed / language-tagged literals ---------------------------------------

#[test]
fn literal_kinds_round_trip_and_stay_distinct() {
    let r = repo();
    let g = graph("literals");
    let plain = quad("s", "p", Term::literal("42"), &g);
    let lang = quad("s", "p2", Term::lang_literal("chat", "fr").unwrap(), &g);
    let typed = quad(
        "s",
        "p3",
        Term::typed_literal("42", "http://www.w3.org/2001/XMLSchema#integer").unwrap(),
        &g,
    );
    r.insert(&[plain.clone(), lang.clone(), typed.clone()])
        .unwrap();
    for q in [&plain, &lang, &typed] {
        assert!(r.contains(q).unwrap(), "missing {q:?}");
    }
    // The IRI "http://example.com/42" must NOT match the literal "42".
    let iri_obj = quad("s", "p", ex("42"), &g);
    assert!(!r.contains(&iri_obj).unwrap());
}

// --- Named-graph isolation ---------------------------------------------------

#[test]
fn named_graphs_are_isolated() {
    let r = repo();
    let ga = graph("tenant-a");
    let gb = graph("tenant-b");
    let qa = quad("Alice", "knows", ex("Bob"), &ga);
    let qb = quad("Alice", "knows", ex("Carol"), &gb);
    r.insert(&[qa.clone(), qb.clone()]).unwrap();

    let in_a = r.quads_for_subject(&ex("Alice"), Some(&ga), None).unwrap();
    assert_eq!(in_a, vec![qa.clone()]);
    let in_b = r.quads_for_subject(&ex("Alice"), Some(&gb), None).unwrap();
    assert_eq!(in_b, vec![qb.clone()]);

    // Ontology graph stays separate from app data.
    let onto = GraphName::ontology();
    let qo = quad("Contact", "subClassOf", ex("Thing"), &onto);
    r.insert(&[qo.clone()]).unwrap();
    assert!(!r
        .quads_for_subject(&ex("Contact"), Some(&ga), None)
        .unwrap()
        .contains(&qo));

    let mut graphs = r.list_graphs().unwrap();
    graphs.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
    assert!(graphs.contains(&ga));
    assert!(graphs.contains(&gb));
    assert!(graphs.contains(&onto));
}

// --- Transaction atomicity ---------------------------------------------------

#[test]
fn mid_batch_failure_rolls_back_everything() {
    let r = repo();
    let g = graph("atomic");
    let seed = quad("Seed", "name", Term::literal("seed"), &g);
    r.insert(&[seed.clone()]).unwrap();

    let batch: Vec<Quad> = (0..5)
        .map(|i| quad("Alice", "knows", ex(&format!("P{i}")), &g))
        .collect();

    // Fail on the 3rd write of this batch.
    r.set_inject_failure_at(Some(2));
    let err = r.insert(&batch).expect_err("injection must fail the batch");
    assert!(
        format!("{err:?}").contains("InjectedFailure"),
        "unexpected error: {err:?}"
    );

    // Nothing from the batch survived; the seed is untouched.
    for q in &batch {
        assert!(!r.contains(q).unwrap(), "partial write leaked: {q:?}");
    }
    assert!(r.contains(&seed).unwrap());
    assert_eq!(
        r.quads_for_subject(&ex("Alice"), Some(&g), None)
            .unwrap()
            .len(),
        0
    );
}

#[test]
fn update_is_atomic_when_second_half_fails() {
    let r = repo();
    let g = graph("atomic-update");
    let old = quad("Alice", "name", Term::literal("old"), &g);
    r.insert(&[old.clone()]).unwrap();
    let new = quad("Alice", "name", Term::literal("new"), &g);

    // apply_atomic removes first, then adds; fail on the add (2nd write).
    r.set_inject_failure_at(Some(1));
    r.apply_atomic(&[new.clone()], &[old.clone()])
        .expect_err("must fail");
    // Rollback: old still present, new absent.
    assert!(r.contains(&old).unwrap());
    assert!(!r.contains(&new).unwrap());
}

// --- Blank nodes ---------------------------------------------------------------

#[test]
fn blank_nodes_preserved_not_skolemized() {
    let r = repo();
    let g = graph("bnode");
    let addr = Term::bnode("addr1").unwrap();
    let q = Quad::new(ex("Alice"), ex("address"), addr.clone(), g.clone()).unwrap();
    let q2 = Quad::new(addr.clone(), ex("city"), Term::literal("Paris"), g.clone()).unwrap();
    r.insert(&[q.clone(), q2.clone()]).unwrap();

    let got = r.quads_for_subject(&ex("Alice"), Some(&g), None).unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].object, addr, "bnode object must round-trip");

    let via_bnode = r.quads_for_subject(&addr, Some(&g), None).unwrap();
    assert_eq!(via_bnode.len(), 1);
    assert_eq!(via_bnode[0].object, Term::literal("Paris"));
}

// --- Result caps ------------------------------------------------------------

#[test]
fn reads_are_bounded_by_limit() {
    let r = OxigraphRepository::new_in_memory(3).unwrap();
    let g = graph("caps");
    let batch: Vec<Quad> = (0..10)
        .map(|i| quad("S", "p", ex(&format!("O{i}")), &g))
        .collect();
    r.insert(&batch).unwrap();

    let got = r
        .match_quads(Some(&ex("S")), None, None, Some(&g), None)
        .unwrap();
    assert_eq!(got.len(), 3, "default cap must truncate");
    let got2 = r
        .match_quads(Some(&ex("S")), None, None, Some(&g), Some(2))
        .unwrap();
    assert_eq!(got2.len(), 2);
}

// --- delete_subject -----------------------------------------------------------

#[test]
fn delete_subject_removes_only_that_graph() {
    let r = repo();
    let ga = graph("del-a");
    let gb = graph("del-b");
    let qa = quad("Alice", "knows", ex("Bob"), &ga);
    let qb = quad("Alice", "knows", ex("Bob"), &gb);
    r.insert(&[qa.clone(), qb.clone()]).unwrap();

    let d = r.delete_subject(&ex("Alice"), &ga).unwrap();
    assert_eq!(d.removed, vec![qa.clone()]);
    assert!(!r.contains(&qa).unwrap());
    assert!(r.contains(&qb).unwrap(), "other graph must survive");
}
