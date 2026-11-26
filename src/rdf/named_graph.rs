//! Named-graph helpers: tenant / project / ontology isolation.
//!
//! Convention (DESIGN.md §5): keep ontology definitions in dedicated
//! graph(s) separate from ordinary application data, and isolate
//! tenants/projects by graph.

use super::RdfError;
use serde::{Deserialize, Serialize};

/// A dataset graph name: the default graph or a named IRI graph.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum GraphName {
    /// The RDF default graph.
    Default,
    /// A named graph identified by an IRI.
    Named(String),
}

impl GraphName {
    /// Well-known ontology graph for schema / OWL definitions.
    pub fn ontology() -> Self {
        GraphName::Named("urn:needle:ontology".to_string())
    }

    pub fn named(iri: impl Into<String>) -> Result<Self, RdfError> {
        let iri = iri.into();
        if iri.is_empty() {
            return Err(RdfError::InvalidGraph("graph IRI must not be empty".into()));
        }
        Ok(GraphName::Named(iri))
    }

    /// Tenant-scoped application graph, e.g. `urn:needle:tenant:acme`.
    pub fn tenant(tenant_id: &str) -> Result<Self, RdfError> {
        if tenant_id.is_empty() {
            return Err(RdfError::InvalidGraph("tenant id must not be empty".into()));
        }
        Ok(GraphName::Named(format!("urn:needle:tenant:{tenant_id}")))
    }

    pub fn is_ontology(&self) -> bool {
        matches!(self, GraphName::Named(n) if n == "urn:needle:ontology")
    }
}

impl std::fmt::Display for GraphName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GraphName::Default => write!(f, "default"),
            GraphName::Named(n) => write!(f, "{n}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tenant_graphs_are_distinct() {
        let a = GraphName::tenant("a").unwrap();
        let b = GraphName::tenant("b").unwrap();
        assert_ne!(a, b);
        assert_ne!(a, GraphName::Default);
    }

    #[test]
    fn empty_names_rejected() {
        assert!(GraphName::named("").is_err());
        assert!(GraphName::tenant("").is_err());
    }
}
