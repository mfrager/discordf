//! Needle 3 RDF/OWL agent — library root.
//!
//! Phase 1: configuration, observability, RDF repository (`rdf`).
//! Phase 2: ontology reasoning view + SHACL (`ontology`), policy gates + audit (`policy`).
//! Later phases add search (Tantivy), tools, agent loop and API.

pub mod agent;
pub mod api;
pub mod config;
pub mod model;
pub mod observability;
pub mod ontology;
pub mod policy;
pub mod rdf;
pub mod search;
pub mod tools;
