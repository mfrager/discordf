//! Search subsystem (Phase 3): Tantivy lexical core + Rust vector hybrid.
//!
//! Invariants (DESIGN.md §§7-8, plan Phase 3):
//! - RDF store is authoritative; Tantivy index is disposable and rebuildable.
//! - Lexical search uses the `tantivy` crate (no hand-rolled inverted index).
//! - Vectors live in a sidecar keyed by `embedding_id` (Tantivy authoritative
//!   for lexical, sidecar for vectors) because Tantivy 0.22 has no native
//!   dense-vector field; this keeps the vector encoder pluggable
//!   (`Needle::embed()` 3072-d in prod, stub in tests) without reindexing
//!   lexical data.
//! - Results are compact [`Candidate`]s only — never full graphs.

pub mod embeddings;
pub mod rank;
pub mod tantivy_index;

pub use embeddings::{cosine_similarity, l2_normalize, normalize_text, Embedder, HashEmbedder};
pub use rank::{rank_hybrid, Candidate, Provenance, SearchMode};
pub use tantivy_index::TantivyIndex;

use crate::rdf::GraphName;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SearchError {
    #[error("tantivy error: {0}")]
    Tantivy(String),
    #[error("storage error: {0}")]
    Storage(String),
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
}

impl From<tantivy::TantivyError> for SearchError {
    fn from(e: tantivy::TantivyError) -> Self {
        SearchError::Tantivy(e.to_string())
    }
}

/// Search service: natural-language query → ranked resource URIs.
pub trait SearchService: Send + Sync {
    /// Hybrid search (default 0.5/0.5). Applies graph + namespace filters
    /// before ranking; returns compact candidates only.
    fn search(
        &self,
        query: &str,
        context_graph: Option<&GraphName>,
        namespace: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Vec<Candidate>, SearchError>;

    /// Index maintenance hook: refresh one resource from repo state
    /// (upsert when quads exist, delete when gone). Tools call this
    /// after every committed CRUD batch; `rebuild_from_repo` recovers.
    fn sync_resource(
        &self,
        repo: &dyn crate::rdf::RdfRepository,
        uri: &str,
        graph: &GraphName,
    ) -> Result<(), SearchError>;

    /// Full rebuild from the authoritative repo scan.
    fn rebuild_from_repo(&self, repo: &dyn crate::rdf::RdfRepository)
        -> Result<usize, SearchError>;
}
