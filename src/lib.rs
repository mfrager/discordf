//! Needle 3 RDF/OWL agent — library root.
//!
//! Phase 1: configuration, observability, RDF repository (`rdf`).
//! Phase 2: ontology reasoning view + SHACL (`ontology`), policy gates + audit (`policy`).
//! Later phases add search (Tantivy), tools, agent loop and API.

pub mod agent;
pub mod api;
pub mod bench;
pub mod config;
pub mod executor;
pub mod ladder_spec;
pub mod llm;
pub mod model;
pub mod observability;
pub mod ontology;
pub mod plan;
pub mod planner;
pub mod policy;
pub mod query;
pub mod rdf;
pub mod search;
pub mod tools;
