//! Phase 1 — persistence: reopening the store preserves data and graphs.

use needle_rdf_agent::rdf::{GraphName, OxigraphRepository, Quad, RdfRepository, Term};

#[test]
fn reopen_preserves_data_and_graphs() {
    let dir = tempfile::tempdir().unwrap();
    let g = GraphName::named("urn:needle:test:persist").unwrap();

    let q1 = Quad::new(
        Term::iri("http://example.com/Alice").unwrap(),
        Term::iri("http://example.com/knows").unwrap(),
        Term::iri("http://example.com/Bob").unwrap(),
        g.clone(),
    )
    .unwrap();
    let q2 = Quad::new(
        Term::iri("http://example.com/Alice").unwrap(),
        Term::iri("http://example.com/name").unwrap(),
        Term::lang_literal("Alice", "en").unwrap(),
        g.clone(),
    )
    .unwrap();

    {
        let r = OxigraphRepository::open(dir.path(), 500).unwrap();
        let d = r.insert(&[q1.clone(), q2.clone()]).unwrap();
        assert_eq!(d.added.len(), 2);
        // Concurrent readers: several back-to-back reads see the same data.
        for _ in 0..5 {
            let got = r
                .quads_for_subject(
                    &Term::iri("http://example.com/Alice").unwrap(),
                    Some(&g),
                    None,
                )
                .unwrap();
            assert_eq!(got.len(), 2);
        }
    } // store dropped, data flushed to disk

    {
        let r = OxigraphRepository::open(dir.path(), 500).unwrap();
        assert!(r.contains(&q1).unwrap());
        assert!(r.contains(&q2).unwrap());
        assert_eq!(r.len().unwrap(), 2);
        assert!(r.list_graphs().unwrap().contains(&g));
    }
}
