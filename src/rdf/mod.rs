//! RDF repository abstraction (Phase 1).
//!
//! Design invariants (DESIGN.md §§1,5):
//! - Tools and agent code depend on [`RdfRepository`], never on Oxigraph.
//! - IRIs, blank nodes, language-tagged and typed literals are distinct.
//!   A relationship between two resources is an IRI object, never a string.
//! - The store is a dataset of named graphs (tenants / projects /
//!   ontologies). Ontology graphs stay separate from application data.
//! - Every mutation is atomic and reports exactly which quads changed.

pub mod named_graph;
pub mod oxigraph_impl;

pub use named_graph::GraphName;
pub use oxigraph_impl::OxigraphRepository;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// An RDF term with strict typing. IRI-objects and string literals are
/// different variants and are never conflated.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Term {
    /// IRI reference, e.g. `http://example.com/Alice`.
    Iri(String),
    /// Blank node identifier (without the `_:` prefix).
    BlankNode(String),
    /// Literal value with optional datatype IRI and/or language tag.
    /// `lang` and `datatype` are mutually exclusive per RDF 1.1.
    Literal {
        value: String,
        datatype: Option<String>,
        lang: Option<String>,
    },
}

impl Term {
    pub fn iri(s: impl Into<String>) -> Result<Self, RdfError> {
        let s = s.into();
        if s.is_empty() {
            return Err(RdfError::InvalidTerm("IRI must not be empty".into()));
        }
        Ok(Term::Iri(s))
    }

    pub fn bnode(id: impl Into<String>) -> Result<Self, RdfError> {
        let id = id.into();
        if id.is_empty() {
            return Err(RdfError::InvalidTerm(
                "blank node id must not be empty".into(),
            ));
        }
        Ok(Term::BlankNode(id))
    }

    pub fn literal(value: impl Into<String>) -> Self {
        Term::Literal {
            value: value.into(),
            datatype: None,
            lang: None,
        }
    }

    pub fn lang_literal(
        value: impl Into<String>,
        lang: impl Into<String>,
    ) -> Result<Self, RdfError> {
        let lang = lang.into();
        if lang.is_empty() {
            return Err(RdfError::InvalidTerm("lang tag must not be empty".into()));
        }
        Ok(Term::Literal {
            value: value.into(),
            datatype: None,
            lang: Some(lang),
        })
    }

    pub fn typed_literal(
        value: impl Into<String>,
        datatype: impl Into<String>,
    ) -> Result<Self, RdfError> {
        let datatype = datatype.into();
        if datatype.is_empty() {
            return Err(RdfError::InvalidTerm(
                "datatype IRI must not be empty".into(),
            ));
        }
        Ok(Term::Literal {
            value: value.into(),
            datatype: Some(datatype),
            lang: None,
        })
    }

    /// `true` for [`Term::Iri`].
    pub fn is_iri(&self) -> bool {
        matches!(self, Term::Iri(_))
    }

    /// Borrow the IRI string, if this term is an IRI.
    pub fn as_iri(&self) -> Option<&str> {
        match self {
            Term::Iri(s) => Some(s),
            _ => None,
        }
    }
}

/// A subject-predicate-object triple (graph-agnostic).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Triple {
    pub subject: Term,
    pub predicate: Term,
    pub object: Term,
}

impl Triple {
    /// Build a triple; the predicate must be an IRI (RDF 1.1 rule).
    pub fn new(subject: Term, predicate: Term, object: Term) -> Result<Self, RdfError> {
        if !predicate.is_iri() {
            return Err(RdfError::InvalidPredicate(format!(
                "predicate must be an IRI, got {predicate:?}"
            )));
        }
        Ok(Self {
            subject,
            predicate,
            object,
        })
    }
}

/// A triple in a concrete graph — the storage unit of the dataset.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Quad {
    pub subject: Term,
    pub predicate: Term,
    pub object: Term,
    pub graph: GraphName,
}

impl Quad {
    pub fn new(
        subject: Term,
        predicate: Term,
        object: Term,
        graph: GraphName,
    ) -> Result<Self, RdfError> {
        if !predicate.is_iri() {
            return Err(RdfError::InvalidPredicate(format!(
                "predicate must be an IRI, got {predicate:?}"
            )));
        }
        Ok(Self {
            subject,
            predicate,
            object,
            graph,
        })
    }

    pub fn triple(&self) -> Triple {
        Triple {
            subject: self.subject.clone(),
            predicate: self.predicate.clone(),
            object: self.object.clone(),
        }
    }
}

/// Exact change produced by one atomic mutation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TripleDelta {
    pub added: Vec<Quad>,
    pub removed: Vec<Quad>,
}

impl TripleDelta {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
    }
}

/// Repository-level errors. Tool handlers (Phase 4) map these to the
/// structured `{ tool, category, message }` error shape.
#[derive(Debug, Error)]
pub enum RdfError {
    #[error("invalid term: {0}")]
    InvalidTerm(String),
    #[error("invalid predicate: {0}")]
    InvalidPredicate(String),
    #[error("invalid graph name: {0}")]
    InvalidGraph(String),
    #[error("result limit exceeded (limit {limit}, truncated)")]
    LimitExceeded { limit: usize },
    #[error("storage error: {0}")]
    Storage(String),
    #[error("injected failure for atomicity test at write {0}")]
    InjectedFailure(usize),
}

impl From<oxigraph::store::StorageError> for RdfError {
    fn from(e: oxigraph::store::StorageError) -> Self {
        RdfError::Storage(e.to_string())
    }
}

/// Authoritative knowledge-store interface. All agent tools (Phase 4) and
/// the search indexer (Phase 3) program against this trait.
pub trait RdfRepository: Send + Sync {
    /// Insert quads atomically. Returns exactly the quads actually added
    /// (already-present quads are not reported as added).
    fn insert(&self, quads: &[Quad]) -> Result<TripleDelta, RdfError>;

    /// Remove quads atomically. Returns exactly the quads actually removed.
    fn remove(&self, quads: &[Quad]) -> Result<TripleDelta, RdfError>;

    /// Atomically remove `to_remove` then insert `to_add` in one
    /// transaction boundary. Either the whole delta applies or nothing does.
    fn apply_atomic(&self, to_add: &[Quad], to_remove: &[Quad]) -> Result<TripleDelta, RdfError>;

    /// Pattern match with optional S/P/O/G filters, truncated to `limit`
    /// ( repository cap applies when `None`).
    fn match_quads(
        &self,
        subject: Option<&Term>,
        predicate: Option<&Term>,
        object: Option<&Term>,
        graph: Option<&GraphName>,
        limit: Option<usize>,
    ) -> Result<Vec<Quad>, RdfError>;

    /// All quads with the given subject (optionally restricted to a graph).
    fn quads_for_subject(
        &self,
        subject: &Term,
        graph: Option<&GraphName>,
        limit: Option<usize>,
    ) -> Result<Vec<Quad>, RdfError> {
        self.match_quads(Some(subject), None, None, graph, limit)
    }

    /// Delete every quad with `subject` in `graph` atomically.
    fn delete_subject(&self, subject: &Term, graph: &GraphName) -> Result<TripleDelta, RdfError>;

    /// Distinct named graphs present in the dataset (+ default when used).
    fn list_graphs(&self) -> Result<Vec<GraphName>, RdfError>;

    /// Total number of quads in the dataset.
    fn len(&self) -> Result<usize, RdfError>;

    fn is_empty(&self) -> Result<bool, RdfError> {
        Ok(self.len()? == 0)
    }

    /// Membership probe (useful for idempotency tests).
    fn contains(&self, quad: &Quad) -> Result<bool, RdfError>;

    /// Placeholder reserving the search-hook signature for Phase 3.
    /// The real search subsystem lives outside the CRUD handlers.
    fn search_stub(&self, _query: &str) -> Result<Vec<Quad>, RdfError> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod term_tests {
    use super::*;

    #[test]
    fn iri_and_literal_are_distinct_types() {
        let iri = Term::iri("http://example.com/Bob").unwrap();
        let lit = Term::literal("http://example.com/Bob");
        assert_ne!(iri, lit, "IRI relationships must not equal string literals");
        assert!(iri.is_iri());
        assert!(!lit.is_iri());
    }

    #[test]
    fn predicate_must_be_iri() {
        let s = Term::iri("http://example.com/a").unwrap();
        let p = Term::literal("knows");
        let o = Term::iri("http://example.com/b").unwrap();
        assert!(Triple::new(s, p, o).is_err());
    }

    #[test]
    fn lang_and_datatype_are_mutually_exclusive_by_construction() {
        // Constructors only allow one of lang / datatype, never both.
        let l1 = Term::lang_literal("chat", "en").unwrap();
        let l2 = Term::typed_literal("42", "http://www.w3.org/2001/XMLSchema#integer").unwrap();
        match (&l1, &l2) {
            (
                Term::Literal {
                    lang: Some(_),
                    datatype: None,
                    ..
                },
                Term::Literal {
                    lang: None,
                    datatype: Some(_),
                    ..
                },
            ) => {}
            _ => panic!("unexpected literal shapes"),
        }
    }
}
