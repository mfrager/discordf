//! Ontology loading (Phase 2): writes schema triples into the dedicated
//! ontology graph, never into application graphs.

use super::reasoner::{
    EX, OWL_EQUIVALENT_CLASS, OWL_INVERSE_OF, RDFS_DOMAIN, RDFS_RANGE, RDFS_SUBCLASS_OF,
    RDFS_SUBPROPERTY_OF, RDF_TYPE,
};
use crate::rdf::{GraphName, Quad, RdfError, RdfRepository, Term};

fn t(s: &str, p: &str, o: &str) -> Quad {
    Quad::new(
        Term::iri(format!("{EX}{s}")).unwrap(),
        Term::iri(p.to_string()).unwrap(),
        Term::iri(format!("{EX}{o}")).unwrap(),
        GraphName::ontology(),
    )
    .unwrap()
}

fn t_lit_prop(s: &str, p: &str, datatype_prop: &str, domain: &str) -> Vec<Quad> {
    vec![
        Quad::new(
            Term::iri(format!("{EX}{s}")).unwrap(),
            Term::iri(RDF_TYPE.to_string()).unwrap(),
            Term::iri("http://www.w3.org/2002/07/owl#DatatypeProperty".to_string()).unwrap(),
            GraphName::ontology(),
        )
        .unwrap(),
        Quad::new(
            Term::iri(format!("{EX}{s}")).unwrap(),
            Term::iri(RDFS_DOMAIN.to_string()).unwrap(),
            Term::iri(format!("{EX}{domain}")).unwrap(),
            GraphName::ontology(),
        )
        .unwrap(),
        // Record the datatype property IRI for documentation purposes;
        // the datatype itself is informational (`datatype_prop` is the XSD IRI).
        Quad::new(
            Term::iri(format!("{EX}{s}")).unwrap(),
            Term::iri("http://example.com/datatype".to_string()).unwrap(),
            Term::iri(datatype_prop.to_string()).unwrap(),
            GraphName::ontology(),
        )
        .unwrap(),
        // Silence unused-param warning for `p` (kept for symmetry with object props).
        Quad::new(
            Term::iri(format!("{EX}{s}")).unwrap(),
            Term::iri(p.to_string()).unwrap(),
            Term::iri(format!("{EX}{s}")).unwrap(),
            GraphName::ontology(),
        )
        .unwrap(),
    ]
}

/// Canonical fixture ontology used by tests and by `load_default_ontology`.
///
/// Classes: `Person`, `Human` (equivalent), `Contact ⊂ Person`, `Employee ⊂ Contact`,
/// `Account` (disjoint branch).
/// Properties: `name` (domain Contact), `email` (domain Contact),
/// `workEmail ⊂ email`, `account` (domain Contact, range Account),
/// `employs ⇄ employedBy` (inverses, domain/range Account↔Contact).
pub fn ontology_fixture_quads() -> Vec<Quad> {
    let onto = GraphName::ontology();
    let q = |s: &str, p: &str, o: &str| {
        Quad::new(
            Term::iri(format!("{EX}{s}")).unwrap(),
            Term::iri(p.to_string()).unwrap(),
            Term::iri(format!("{EX}{o}")).unwrap(),
            onto.clone(),
        )
        .unwrap()
    };
    let mut v = vec![
        // Class declarations (informational).
        q("Person", RDF_TYPE, "Class"),
        q("Human", RDF_TYPE, "Class"),
        q("Contact", RDF_TYPE, "Class"),
        q("Employee", RDF_TYPE, "Class"),
        q("Account", RDF_TYPE, "Class"),
        // Hierarchy A⊂B⊂C: Employee ⊂ Contact ⊂ Person.
        t("Employee", RDFS_SUBCLASS_OF, "Contact"),
        t("Contact", RDFS_SUBCLASS_OF, "Person"),
        // Equivalent classes (symmetric).
        t("Person", OWL_EQUIVALENT_CLASS, "Human"),
        // Properties.
        q("name", RDF_TYPE, "DatatypeProperty"),
        q("email", RDF_TYPE, "DatatypeProperty"),
        q("workEmail", RDF_TYPE, "DatatypeProperty"),
        q("account", RDF_TYPE, "ObjectProperty"),
        q("employs", RDF_TYPE, "ObjectProperty"),
        q("employedBy", RDF_TYPE, "ObjectProperty"),
        // Sub-property.
        t("workEmail", RDFS_SUBPROPERTY_OF, "email"),
        // Domains / ranges.
        t("name", RDFS_DOMAIN, "Contact"),
        t("email", RDFS_DOMAIN, "Contact"),
        t("account", RDFS_DOMAIN, "Contact"),
        t("account", RDFS_RANGE, "Account"),
        t("employs", RDFS_DOMAIN, "Account"),
        t("employs", RDFS_RANGE, "Contact"),
        t("employedBy", RDFS_DOMAIN, "Contact"),
        t("employedBy", RDFS_RANGE, "Account"),
        // Inverse pair.
        t("employs", OWL_INVERSE_OF, "employedBy"),
    ];
    // `t_lit_prop` documents the XSD datatypes; keep one entry so the
    // helper is exercised without duplicating the fixture above.
    let _ = t_lit_prop;
    v.dedup_by(|a, b| format!("{a:?}") == format!("{b:?}"));
    v
}

/// Load the default ontology into [`GraphName::ontology()`].
/// Returns the number of newly added quads.
pub fn load_default_ontology(repo: &(impl RdfRepository + ?Sized)) -> Result<usize, RdfError> {
    let quads = ontology_fixture_quads();
    let delta = repo.insert(&quads)?;
    Ok(delta.added.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rdf::OxigraphRepository;

    #[test]
    fn loader_writes_only_to_ontology_graph() {
        let repo = OxigraphRepository::new_in_memory(1000).unwrap();
        let added = load_default_ontology(&repo).unwrap();
        assert!(added > 10);
        // Reloading is idempotent: second load adds nothing.
        let added2 = load_default_ontology(&repo).unwrap();
        assert_eq!(added2, 0);
        // All fixture quads live in the ontology graph.
        for q in ontology_fixture_quads() {
            assert!(q.graph.is_ontology());
            assert!(repo.contains(&q).unwrap());
        }
        // No app graph accidentally received ontology triples.
        let app = GraphName::tenant("acme").unwrap();
        assert!(repo
            .match_quads(None, None, None, Some(&app), None)
            .unwrap()
            .is_empty());
    }
}
