//! Oxigraph backend for [`RdfRepository`](super::RdfRepository) (Phase 1).
//!
//! - The Oxigraph [`Store`](oxigraph::store::Store) is authoritative.
//! - Every batch mutation runs inside `Store::transaction`, so multi-quad
//!   inserts/removes are all-or-nothing.
//! - Reads are bounded by `max_read` (from [`AppLimits`](crate::config::AppLimits)).
//! - A test-only failure-injection hook (`set_inject_failure_at`) lets
//!   atomicity tests force a mid-batch error and assert full rollback.

use super::{GraphName, Quad, RdfError, RdfRepository, Term, TripleDelta};
use oxigraph::model::{
    BlankNode, GraphName as OxGraphName, Literal, NamedNode, Quad as OxQuad, Subject as OxSubject,
    Term as OxTerm,
};
use oxigraph::store::Store;
use std::collections::HashSet;
use std::path::Path;
use std::sync::{atomic::AtomicUsize, Mutex};

/// Oxigraph-backed repository. Holds the result cap and a test-only
/// failure-injection counter.
pub struct OxigraphRepository {
    store: Store,
    max_read: usize,
    /// When `Some(n)`, the n-th write inside the next transaction fails
    /// with [`RdfError::InjectedFailure`]. Cleared after it fires once.
    /// `None` (production default) disables injection.
    inject_failure_at: Mutex<Option<usize>>,
    write_counter: AtomicUsize,
}

impl OxigraphRepository {
    pub fn new_in_memory(max_read: usize) -> Result<Self, RdfError> {
        let store = Store::new().map_err(|e| RdfError::Storage(e.to_string()))?;
        Ok(Self {
            store,
            max_read: max_read.max(1),
            inject_failure_at: Mutex::new(None),
            write_counter: AtomicUsize::new(0),
        })
    }

    pub fn open(path: impl AsRef<Path>, max_read: usize) -> Result<Self, RdfError> {
        let store = Store::open(path).map_err(|e| RdfError::Storage(e.to_string()))?;
        Ok(Self {
            store,
            max_read: max_read.max(1),
            inject_failure_at: Mutex::new(None),
            write_counter: AtomicUsize::new(0),
        })
    }

    /// Test-only hook: fail the `n`-th write (0-based) of the next batch.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn set_inject_failure_at(&self, n: Option<usize>) {
        *self.inject_failure_at.lock().unwrap() = n;
        self.write_counter
            .store(0, std::sync::atomic::Ordering::SeqCst);
    }

    /// Non-test builds still expose the hook for integration tests that
    /// depend on the library (integration tests are external crates).
    #[cfg(not(any(test, feature = "test-hooks")))]
    pub fn set_inject_failure_at(&self, n: Option<usize>) {
        *self.inject_failure_at.lock().unwrap() = n;
        self.write_counter
            .store(0, std::sync::atomic::Ordering::SeqCst);
    }

    fn check_injection(&self) -> Result<(), RdfError> {
        let guard = self.inject_failure_at.lock().unwrap();
        if let Some(at) = *guard {
            let n = self
                .write_counter
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n == at {
                return Err(RdfError::InjectedFailure(n));
            }
        }
        Ok(())
    }

    fn clear_injection(&self) {
        *self.inject_failure_at.lock().unwrap() = None;
    }

    fn effective_limit(&self, requested: Option<usize>) -> usize {
        requested.unwrap_or(self.max_read).max(1).min(self.max_read)
    }
}

// ---------------------------------------------------------------------------
// Conversions between our model and oxrdf.
// ---------------------------------------------------------------------------

fn ox_iri(s: &str) -> Result<NamedNode, RdfError> {
    NamedNode::new(s).map_err(|e| RdfError::InvalidTerm(format!("bad IRI {s:?}: {e}")))
}

fn term_to_ox_subject(t: &Term) -> Result<OxSubject, RdfError> {
    match t {
        Term::Iri(s) => Ok(OxSubject::NamedNode(ox_iri(s)?)),
        Term::BlankNode(id) => BlankNode::new(id.as_str())
            .map(OxSubject::BlankNode)
            .map_err(|e| RdfError::InvalidTerm(format!("bad blank node {id:?}: {e}"))),
        Term::Literal { .. } => Err(RdfError::InvalidTerm(
            "literals cannot appear in subject position".into(),
        )),
    }
}

fn term_to_ox_predicate(t: &Term) -> Result<NamedNode, RdfError> {
    match t {
        Term::Iri(s) => ox_iri(s),
        _ => Err(RdfError::InvalidPredicate(format!(
            "predicate must be an IRI, got {t:?}"
        ))),
    }
}

fn term_to_ox_object(t: &Term) -> Result<OxTerm, RdfError> {
    match t {
        Term::Iri(s) => Ok(OxTerm::NamedNode(ox_iri(s)?)),
        Term::BlankNode(id) => BlankNode::new(id.as_str())
            .map(OxTerm::BlankNode)
            .map_err(|e| RdfError::InvalidTerm(format!("bad blank node {id:?}: {e}"))),
        Term::Literal {
            value,
            datatype,
            lang,
        } => {
            if let Some(lang) = lang {
                Literal::new_language_tagged_literal(value.as_str(), lang.as_str())
                    .map(OxTerm::Literal)
                    .map_err(|e| RdfError::InvalidTerm(format!("bad lang tag {lang:?}: {e}")))
            } else if let Some(dt) = datatype {
                Ok(OxTerm::Literal(Literal::new_typed_literal(
                    value.clone(),
                    ox_iri(dt)?,
                )))
            } else {
                Ok(OxTerm::Literal(Literal::new_simple_literal(value.clone())))
            }
        }
    }
}

fn graph_to_ox(g: &GraphName) -> Result<OxGraphName, RdfError> {
    match g {
        GraphName::Default => Ok(OxGraphName::DefaultGraph),
        GraphName::Named(n) => {
            if let Some(rest) = n.strip_prefix("_:") {
                BlankNode::new(rest)
                    .map(OxGraphName::BlankNode)
                    .map_err(|e| RdfError::InvalidGraph(format!("bad graph bnode {n:?}: {e}")))
            } else {
                ox_iri(n).map(OxGraphName::NamedNode).map_err(|e| match e {
                    RdfError::InvalidTerm(m) => RdfError::InvalidGraph(m),
                    other => other,
                })
            }
        }
    }
}

fn quad_to_ox(q: &Quad) -> Result<OxQuad, RdfError> {
    Ok(OxQuad::new(
        term_to_ox_subject(&q.subject)?,
        term_to_ox_predicate(&q.predicate)?,
        term_to_ox_object(&q.object)?,
        graph_to_ox(&q.graph)?,
    ))
}

fn ox_term_to_term(t: &OxTerm) -> Term {
    match t {
        OxTerm::NamedNode(n) => Term::Iri(n.as_str().to_string()),
        OxTerm::BlankNode(b) => Term::BlankNode(b.as_str().to_string()),
        OxTerm::Literal(l) => {
            if let Some(lang) = l.language() {
                Term::Literal {
                    value: l.value().to_string(),
                    datatype: None,
                    lang: Some(lang.to_string()),
                }
            } else {
                let dt = l.datatype();
                let dt_str = dt.as_str();
                // Plain `xsd:string` literals round-trip as untagged.
                if dt_str == "http://www.w3.org/2001/XMLSchema#string" {
                    Term::literal(l.value())
                } else {
                    Term::Literal {
                        value: l.value().to_string(),
                        datatype: Some(dt_str.to_string()),
                        lang: None,
                    }
                }
            }
        }
        #[allow(unreachable_patterns)]
        _ => Term::literal(t.to_string()),
    }
}

fn ox_quad_to_quad(q: OxQuad) -> Quad {
    let subject = match &q.subject {
        OxSubject::NamedNode(n) => Term::Iri(n.as_str().to_string()),
        OxSubject::BlankNode(b) => Term::BlankNode(b.as_str().to_string()),
        #[allow(unreachable_patterns)]
        _ => Term::BlankNode("unsupported-subject".to_string()),
    };
    let predicate = Term::Iri(q.predicate.as_str().to_string());
    let object = ox_term_to_term(&q.object);
    let graph = match &q.graph_name {
        OxGraphName::DefaultGraph => GraphName::Default,
        OxGraphName::NamedNode(n) => GraphName::Named(n.as_str().to_string()),
        OxGraphName::BlankNode(b) => GraphName::Named(format!("_:{}", b.as_str())),
        #[allow(unreachable_patterns)]
        _ => GraphName::Default,
    };
    // Predicate is always an IRI by construction above.
    Quad::new(subject, predicate, object, graph).expect("ox quad converts cleanly")
}

// ---------------------------------------------------------------------------
// Trait implementation.
// ---------------------------------------------------------------------------

impl RdfRepository for OxigraphRepository {
    fn insert(&self, quads: &[Quad]) -> Result<TripleDelta, RdfError> {
        self.apply_atomic(quads, &[])
    }

    fn remove(&self, quads: &[Quad]) -> Result<TripleDelta, RdfError> {
        self.apply_atomic(&[], quads)
    }

    fn apply_atomic(&self, to_add: &[Quad], to_remove: &[Quad]) -> Result<TripleDelta, RdfError> {
        // Validate everything before opening the transaction so malformed
        // input never reaches storage.
        let add_ox: Vec<OxQuad> = to_add.iter().map(quad_to_ox).collect::<Result<_, _>>()?;
        let remove_ox: Vec<OxQuad> = to_remove.iter().map(quad_to_ox).collect::<Result<_, _>>()?;
        let add_src: Vec<Quad> = to_add.to_vec();

        // Reset per-batch write counter (injection counts writes in this batch).
        self.write_counter
            .store(0, std::sync::atomic::Ordering::SeqCst);

        let result: Result<TripleDelta, RdfError> = self.store.transaction(|mut tx| {
            let mut delta = TripleDelta::default();
            // Removals first so an update's remove+add is atomic.
            for q in &remove_ox {
                self.check_injection()?;
                if tx.contains(q.as_ref())? {
                    tx.remove(q.as_ref())?;
                    delta.removed.push(ox_quad_to_quad(q.clone()));
                }
            }
            for (q, src) in add_ox.iter().zip(add_src.iter()) {
                self.check_injection()?;
                if !tx.contains(q.as_ref())? {
                    tx.insert(q.as_ref())?;
                    delta.added.push(src.clone());
                }
            }
            Ok(delta)
        });
        // Injection is one-shot: clear it so later batches are unaffected,
        // whether this batch succeeded or rolled back.
        self.clear_injection();
        result
    }

    fn match_quads(
        &self,
        subject: Option<&Term>,
        predicate: Option<&Term>,
        object: Option<&Term>,
        graph: Option<&GraphName>,
        limit: Option<usize>,
    ) -> Result<Vec<Quad>, RdfError> {
        let cap = self.effective_limit(limit);
        let s_ox = subject.map(term_to_ox_subject).transpose()?;
        let p_ox = predicate.map(term_to_ox_predicate).transpose()?;
        let o_ox = object.map(term_to_ox_object).transpose()?;
        let g_ox = graph.map(graph_to_ox).transpose()?;

        let out: Vec<Quad> = self
            .store
            .quads_for_pattern(
                s_ox.as_ref().map(|s| s.as_ref()),
                p_ox.as_ref().map(|p| p.as_ref()),
                o_ox.as_ref().map(|o| o.as_ref()),
                g_ox.as_ref().map(|g| g.as_ref()),
            )
            .take(cap)
            .collect::<Result<Vec<OxQuad>, _>>()
            .map_err(|e| RdfError::Storage(e.to_string()))?
            .into_iter()
            .map(ox_quad_to_quad)
            .collect();
        Ok(out)
    }

    fn delete_subject(&self, subject: &Term, graph: &GraphName) -> Result<TripleDelta, RdfError> {
        let existing = self.match_quads(Some(subject), None, None, Some(graph), None)?;
        self.apply_atomic(&[], &existing)
    }

    fn list_graphs(&self) -> Result<Vec<GraphName>, RdfError> {
        let mut set: HashSet<GraphName> = HashSet::new();
        for q in self
            .store
            .iter()
            .collect::<Result<Vec<OxQuad>, _>>()
            .map_err(|e| RdfError::Storage(e.to_string()))?
        {
            set.insert(ox_quad_to_quad(q).graph);
        }
        let mut out: Vec<GraphName> = set.into_iter().collect();
        out.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
        Ok(out)
    }

    fn len(&self) -> Result<usize, RdfError> {
        self.store
            .len()
            .map_err(|e| RdfError::Storage(e.to_string()))
    }

    fn contains(&self, quad: &Quad) -> Result<bool, RdfError> {
        let q = quad_to_ox(quad)?;
        self.store
            .contains(q.as_ref())
            .map_err(|e| RdfError::Storage(e.to_string()))
    }
}

#[cfg(test)]
mod conv_tests {
    use super::*;

    #[test]
    fn literal_round_trips() {
        for t in [
            Term::literal("plain"),
            Term::lang_literal("chat", "en").unwrap(),
            Term::typed_literal("42", "http://www.w3.org/2001/XMLSchema#integer").unwrap(),
        ] {
            let ox = term_to_ox_object(&t).unwrap();
            // Subject conversion must reject literals.
            assert!(term_to_ox_subject(&t).is_err());
            let back = ox_term_to_term(&ox);
            assert_eq!(back, t);
        }
    }

    #[test]
    fn bad_iri_rejected() {
        // Spaces are not valid in IRIs; oxrdf validation must fire.
        assert!(ox_iri("not a valid iri with spaces").is_err());
    }
}
