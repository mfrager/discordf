//! Needle 3 RDF/OWL agent — library root.
//!
//! Phase 1 exposes only the foundation: configuration, observability,
//! and the RDF repository abstraction (`rdf`) with its Oxigraph backend.
//! Later phases add ontology, search (Tantivy), tools, agent loop and API.

pub mod config;
pub mod observability;
pub mod rdf;
