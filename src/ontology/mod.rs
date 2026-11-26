//! Ontology service (Phase 2): OWL reasoning view + SHACL validation.
//!
//! Invariants:
//! - Ontology triples live in [`GraphName::ontology()`]; app data stays separate.
//! - Inferred statements are computed on demand and never persisted as asserted.
//! - SHACL validation runs before commit; violations are structured.

pub mod loader;
pub mod reasoner;
pub mod shacl;

pub use loader::{load_default_ontology, ontology_fixture_quads};
pub use reasoner::{
    OntologyReasoner, EX, OWL_EQUIVALENT_CLASS, OWL_INVERSE_OF, RDFS_DOMAIN, RDFS_RANGE,
    RDFS_SUBCLASS_OF, RDFS_SUBPROPERTY_OF, RDF_TYPE,
};
pub use shacl::{
    contact_shapes, validate_resource, validate_staged_subject, PropertyConstraint, Shape,
    ValidationResult, Violation,
};

/// Ontology service trait: reasoning view over the asserted store.
pub trait OntologyService: Send + Sync {
    /// All superclasses of `class` (transitive, excluding self).
    fn superclasses(&self, class: &str) -> Vec<String>;
    /// True if `sub` == `sup` or `sub` transitively implies `sup`
    /// (subclass chain or equivalent-class bridge).
    fn is_subclass_of(&self, sub: &str, sup: &str) -> bool;
    /// All properties that are sub-properties of `prop` (transitive, excluding self).
    fn subproperties(&self, prop: &str) -> Vec<String>;
    /// True if `sub` == `sup` or `sub` transitively implies `sup` via subPropertyOf.
    fn is_subproperty_of(&self, sub: &str, sup: &str) -> bool;
    /// Declared domain of a property, if any.
    fn domain(&self, prop: &str) -> Option<String>;
    /// Declared range of a property, if any.
    fn range(&self, prop: &str) -> Option<String>;
    /// Inverse property, if declared (symmetric lookup).
    fn inverse(&self, prop: &str) -> Option<String>;
    /// Equivalent classes (symmetric closure, excluding self).
    fn equivalents(&self, class: &str) -> Vec<String>;
}
