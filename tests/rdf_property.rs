//! Phase 1 — property test: random quad batches round-trip exactly.
//!
//! Uses a tiny deterministic LCG (no extra dependencies) so the test is
//! reproducible in CI: seed N generates batch N.

use needle_rdf_agent::rdf::{GraphName, OxigraphRepository, Quad, RdfRepository, Term};

/// Deterministic xorshift64* RNG (fixed seeds → reproducible batches).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % (n as u64)) as usize
    }
}

fn random_term(rng: &mut Rng, kind: usize) -> Term {
    match kind % 4 {
        0 => Term::iri(format!("http://example.com/n{}", rng.below(50))).unwrap(),
        1 => Term::bnode(format!("b{}", rng.below(20))).unwrap(),
        2 => Term::literal(format!("v{}", rng.below(100))),
        _ => Term::lang_literal(format!("w{}", rng.below(100)), "en").unwrap(),
    }
}

#[test]
fn random_batches_create_then_read_identically() {
    for seed in 1u64..=20 {
        let mut rng = Rng(seed.wrapping_mul(0x9E3779B97F4A7C15));
        let repo = OxigraphRepository::new_in_memory(10_000).unwrap();
        let g = GraphName::named(format!("urn:needle:prop:{seed}")).unwrap();

        // Subjects are always IRIs/bnodes (literals illegal there).
        let mut batch: Vec<Quad> = Vec::new();
        for _ in 0..30 {
            let s = if rng.below(2) == 0 {
                Term::iri(format!("http://example.com/s{}", rng.below(10))).unwrap()
            } else {
                Term::bnode(format!("sb{}", rng.below(10))).unwrap()
            };
            let p = Term::iri(format!("http://example.com/p{}", rng.below(5))).unwrap();
            let kind = rng.below(4);
            let o = random_term(&mut rng, kind);
            // Our Term model allows literal subjects at type level but the
            // backend must reject them; skip those here (covered elsewhere).
            batch.push(Quad::new(s, p, o, g.clone()).unwrap());
        }

        let delta = repo.insert(&batch).unwrap();
        // Delta + store must agree: every reported add is contained…
        for q in &delta.added {
            assert!(repo.contains(q).unwrap(), "seed {seed}: {q:?} not found");
        }
        // …and every distinct batch quad is either contained or was a
        // duplicate of a contained one.
        let mut distinct = batch.clone();
        distinct.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
        distinct.dedup_by(|a, b| format!("{a:?}") == format!("{b:?}"));
        for q in &distinct {
            assert!(repo.contains(q).unwrap(), "seed {seed}: {q:?} missing");
        }
        // Full-graph read returns exactly the distinct set (dedup semantics).
        let all = repo.match_quads(None, None, None, Some(&g), None).unwrap();
        assert_eq!(
            all.len(),
            distinct.len(),
            "seed {seed}: store holds duplicates or lost quads"
        );
    }
}

#[test]
fn invalid_subject_position_rejected() {
    let repo = OxigraphRepository::new_in_memory(100).unwrap();
    let g = GraphName::named("urn:needle:prop:bad").unwrap();
    // Manually construct a Quad with a literal subject, bypassing helpers:
    // Quad::new does not check subject position, the backend must.
    let q = Quad {
        subject: Term::literal("oops"),
        predicate: Term::iri("http://example.com/p").unwrap(),
        object: Term::literal("v"),
        graph: g,
    };
    assert!(repo.insert(&[q]).is_err());
}
