//! OWL reasoning view (Phase 2): subclass/subproperty closure, domain/range,
//! inverses, equivalent classes. Computed on demand from asserted ontology
//! triples; never writes back to the store.

use super::OntologyService;
use crate::rdf::{GraphName, Quad, RdfRepository, Term};
use std::collections::{HashMap, HashSet};

/// Well-known IRIs.
pub const EX: &str = "http://example.com/";
pub const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
pub const RDFS_SUBCLASS_OF: &str = "http://www.w3.org/2000/01/rdf-schema#subClassOf";
pub const RDFS_SUBPROPERTY_OF: &str = "http://www.w3.org/2000/01/rdf-schema#subPropertyOf";
pub const RDFS_DOMAIN: &str = "http://www.w3.org/2000/01/rdf-schema#domain";
pub const RDFS_RANGE: &str = "http://www.w3.org/2000/01/rdf-schema#range";
pub const OWL_INVERSE_OF: &str = "http://www.w3.org/2002/07/owl#inverseOf";
pub const OWL_EQUIVALENT_CLASS: &str = "http://www.w3.org/2002/07/owl#equivalentClass";

/// In-memory reasoning view built from the ontology graph.
#[derive(Debug, Clone, Default)]
pub struct OntologyReasoner {
    subclass: HashMap<String, HashSet<String>>,
    subproperty: HashMap<String, HashSet<String>>,
    domain: HashMap<String, String>,
    range: HashMap<String, String>,
    inverse: HashMap<String, String>,
    equivalent: HashMap<String, HashSet<String>>,
}

impl OntologyReasoner {
    pub fn empty() -> Self {
        Self::default()
    }

    /// Build from an explicit quad slice (tests / offline use).
    pub fn from_quads(quads: &[Quad]) -> Self {
        let mut r = Self::default();
        for q in quads {
            r.ingest(q);
        }
        r
    }

    /// Build by scanning the ontology graph of a repository (read-only).
    pub fn from_repository(
        repo: &(impl RdfRepository + ?Sized),
    ) -> Result<Self, crate::rdf::RdfError> {
        let quads = repo.match_quads(None, None, None, Some(&GraphName::ontology()), None)?;
        Ok(Self::from_quads(&quads))
    }

    fn iri(t: &Term) -> Option<&str> {
        t.as_iri()
    }

    fn ingest(&mut self, q: &Quad) {
        let (Some(s), Some(p), Some(o)) = (
            Self::iri(&q.subject),
            Self::iri(&q.predicate),
            Self::iri(&q.object),
        ) else {
            return;
        };
        match p {
            RDFS_SUBCLASS_OF => {
                self.subclass
                    .entry(s.to_string())
                    .or_default()
                    .insert(o.to_string());
            }
            RDFS_SUBPROPERTY_OF => {
                self.subproperty
                    .entry(s.to_string())
                    .or_default()
                    .insert(o.to_string());
            }
            RDFS_DOMAIN => {
                self.domain.insert(s.to_string(), o.to_string());
            }
            RDFS_RANGE => {
                self.range.insert(s.to_string(), o.to_string());
            }
            OWL_INVERSE_OF => {
                self.inverse.insert(s.to_string(), o.to_string());
                self.inverse.insert(o.to_string(), s.to_string());
            }
            OWL_EQUIVALENT_CLASS => {
                self.equivalent
                    .entry(s.to_string())
                    .or_default()
                    .insert(o.to_string());
                self.equivalent
                    .entry(o.to_string())
                    .or_default()
                    .insert(s.to_string());
            }
            _ => {}
        }
    }

    /// Transitive closure over `edges` starting at `start` (excluding `start`).
    fn closure(edges: &HashMap<String, HashSet<String>>, start: &str) -> HashSet<String> {
        let mut seen = HashSet::new();
        let mut stack = vec![start.to_string()];
        while let Some(cur) = stack.pop() {
            if let Some(nexts) = edges.get(&cur) {
                for n in nexts {
                    if seen.insert(n.clone()) {
                        stack.push(n.clone());
                    }
                }
            }
        }
        // Bridge through equivalent classes one hop in both directions.
        seen
    }

    /// Asserted `rdf:type` IRIs for `subject` in `graph`.
    pub fn asserted_types(
        &self,
        repo: &(impl RdfRepository + ?Sized),
        subject: &Term,
        graph: &GraphName,
    ) -> Result<Vec<String>, crate::rdf::RdfError> {
        let type_pred = Term::iri(RDF_TYPE).unwrap();
        let quads = repo.match_quads(Some(subject), Some(&type_pred), None, Some(graph), None)?;
        Ok(quads
            .iter()
            .filter_map(|q| q.object.as_iri().map(|s| s.to_string()))
            .collect())
    }

    /// Inferred types: asserted + superclass closure + equivalent closure.
    pub fn inferred_types(
        &self,
        repo: &(impl RdfRepository + ?Sized),
        subject: &Term,
        graph: &GraphName,
    ) -> Result<Vec<String>, crate::rdf::RdfError> {
        let asserted = self.asserted_types(repo, subject, graph)?;
        let mut out: HashSet<String> = asserted.iter().cloned().collect();
        // Expand transitively: for each type, add superclasses and equivalents,
        // iterating until fixpoint (handles equivalent-bridged hierarchies).
        let mut stack: Vec<String> = asserted;
        while let Some(cur) = stack.pop() {
            for sup in Self::closure(&self.subclass, &cur) {
                if out.insert(sup.clone()) {
                    stack.push(sup);
                }
            }
            if let Some(eqs) = self.equivalent.get(&cur) {
                for e in eqs {
                    if out.insert(e.clone()) {
                        stack.push(e.clone());
                    }
                }
            }
        }
        let mut v: Vec<String> = out.into_iter().collect();
        v.sort();
        Ok(v)
    }

    /// True if the resource is (transitively) an instance of `class`.
    pub fn is_instance_of(
        &self,
        repo: &(impl RdfRepository + ?Sized),
        subject: &Term,
        graph: &GraphName,
        class: &str,
    ) -> Result<bool, crate::rdf::RdfError> {
        Ok(self
            .inferred_types(repo, subject, graph)?
            .iter()
            .any(|t| t == class))
    }

    /// Inverse entailments visible in the reasoned view for one quad:
    /// `(s p o)` with `p ⇄ q` entails `(o q s)` when both ends are IRIs.
    pub fn inverse_entailments(&self, quad: &Quad) -> Vec<Quad> {
        let Some(p) = quad.predicate.as_iri() else {
            return vec![];
        };
        let Some(inv) = self.inverse.get(p) else {
            return vec![];
        };
        // Only IRI↔IRI relationships entail; literals never flip to subject.
        if quad.subject.as_iri().is_none() || quad.object.as_iri().is_none() {
            return vec![];
        }
        match Quad::new(
            quad.object.clone(),
            Term::iri(inv.clone()).unwrap(),
            quad.subject.clone(),
            quad.graph.clone(),
        ) {
            Ok(q) => vec![q],
            Err(_) => vec![],
        }
    }
}

impl OntologyService for OntologyReasoner {
    fn superclasses(&self, class: &str) -> Vec<String> {
        let mut v: Vec<String> = Self::closure(&self.subclass, class).into_iter().collect();
        v.sort();
        v
    }

    fn is_subclass_of(&self, sub: &str, sup: &str) -> bool {
        if sub == sup {
            return true;
        }
        if Self::closure(&self.subclass, sub).contains(sup) {
            return true;
        }
        // Equivalent-class bridge: sub ≡ mid and mid ⊆ sup (or reverse).
        if let Some(eqs) = self.equivalent.get(sub) {
            if eqs.contains(&sup.to_string()) {
                return true;
            }
            for e in eqs {
                if Self::closure(&self.subclass, e).contains(sup) {
                    return true;
                }
            }
        }
        // sup's equivalents: sub ⊆ eq where eq ≡ sup.
        for (cls, eqs) in &self.equivalent {
            if eqs.contains(&sup.to_string())
                && (cls == sub || Self::closure(&self.subclass, sub).contains(cls))
            {
                return true;
            }
        }
        false
    }

    fn subproperties(&self, prop: &str) -> Vec<String> {
        let mut v: Vec<String> = Self::closure(&self.subproperty, prop).into_iter().collect();
        v.sort();
        v
    }

    fn is_subproperty_of(&self, sub: &str, sup: &str) -> bool {
        sub == sup || Self::closure(&self.subproperty, sub).contains(sup)
    }

    fn domain(&self, prop: &str) -> Option<String> {
        self.domain.get(prop).cloned()
    }

    fn range(&self, prop: &str) -> Option<String> {
        self.range.get(prop).cloned()
    }

    fn inverse(&self, prop: &str) -> Option<String> {
        self.inverse.get(prop).cloned()
    }

    fn equivalents(&self, class: &str) -> Vec<String> {
        let mut v: Vec<String> = self
            .equivalent
            .get(class)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect();
        v.sort();
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ontology::loader::ontology_fixture_quads;
    use crate::rdf::{OxigraphRepository, RdfRepository};

    fn reasoner() -> OntologyReasoner {
        OntologyReasoner::from_quads(&ontology_fixture_quads())
    }

    #[test]
    fn subclass_transitive_closure() {
        let r = reasoner();
        assert!(r.is_subclass_of(&format!("{EX}Employee"), &format!("{EX}Contact")));
        assert!(r.is_subclass_of(&format!("{EX}Contact"), &format!("{EX}Person")));
        assert!(r.is_subclass_of(&format!("{EX}Employee"), &format!("{EX}Person")));
        assert!(!r.is_subclass_of(&format!("{EX}Person"), &format!("{EX}Employee")));
        assert_eq!(r.superclasses(&format!("{EX}Employee")).len(), 2);
    }

    #[test]
    fn equivalent_classes_are_symmetric() {
        let r = reasoner();
        assert_eq!(
            r.equivalents(&format!("{EX}Person")),
            vec![format!("{EX}Human")]
        );
        assert_eq!(
            r.equivalents(&format!("{EX}Human")),
            vec![format!("{EX}Person")]
        );
        assert!(r.equivalents(&format!("{EX}Account")).is_empty());
    }

    #[test]
    fn subproperty_closure() {
        let r = reasoner();
        assert!(r.is_subproperty_of(&format!("{EX}workEmail"), &format!("{EX}email")));
        assert!(!r.is_subproperty_of(&format!("{EX}email"), &format!("{EX}workEmail")));
    }

    #[test]
    fn domain_range_inverse_lookup() {
        let r = reasoner();
        assert_eq!(
            r.domain(&format!("{EX}account")),
            Some(format!("{EX}Contact"))
        );
        assert_eq!(
            r.range(&format!("{EX}account")),
            Some(format!("{EX}Account"))
        );
        assert_eq!(
            r.inverse(&format!("{EX}employs")),
            Some(format!("{EX}employedBy"))
        );
        assert_eq!(
            r.inverse(&format!("{EX}employedBy")),
            Some(format!("{EX}employs"))
        );
    }

    #[test]
    fn inferred_types_include_superclasses_but_store_does_not() {
        let repo = OxigraphRepository::new_in_memory(1000).unwrap();
        crate::ontology::loader::load_default_ontology(&repo).unwrap();
        let r = OntologyReasoner::from_repository(&repo).unwrap();
        let g = GraphName::tenant("acme").unwrap();
        let alice = Term::iri(format!("{EX}Alice")).unwrap();
        let contact = Term::iri(format!("{EX}Contact")).unwrap();
        let type_pred = Term::iri(RDF_TYPE).unwrap();
        let before = repo.len().unwrap();
        repo.insert(&[Quad::new(alice.clone(), type_pred, contact, g.clone()).unwrap()])
            .unwrap();
        let inferred = r.inferred_types(&repo, &alice, &g).unwrap();
        assert!(inferred.contains(&format!("{EX}Contact")));
        assert!(inferred.contains(&format!("{EX}Person")));
        // Asserted view has only the direct type.
        assert_eq!(
            r.asserted_types(&repo, &alice, &g).unwrap(),
            vec![format!("{EX}Contact")]
        );
        // Reasoner performed no writes: only the one explicit quad was added.
        assert_eq!(repo.len().unwrap(), before + 1);
    }
}
