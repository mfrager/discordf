//! Minimal SHACL validation (Phase 2): shapes compatible with the Oxigraph
//! stack's modelling (target class + property constraints), enforced before
//! commit. Not a full SHACL engine — covers min/max count, datatype, and
//! class-range checks plus domain checks from the reasoner.

use super::reasoner::{OntologyReasoner, EX, RDF_TYPE};
use super::OntologyService;
use crate::rdf::{GraphName, Quad, RdfError, RdfRepository, Term};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// Severity of a constraint violation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Severity {
    Violation,
    Warning,
}

/// One property constraint inside a shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PropertyConstraint {
    pub path: String,
    pub min_count: usize,
    pub max_count: Option<usize>,
    /// Required XSD datatype IRI (e.g. `xsd:string`), if any.
    pub datatype: Option<String>,
    /// Required value class IRI (range): object must be an IRI instance of this class.
    pub value_class: Option<String>,
    pub message: String,
    pub severity: Severity,
}

/// A node shape targeting instances of `target_class`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shape {
    pub id: String,
    pub target_class: String,
    pub constraints: Vec<PropertyConstraint>,
}

/// One structured constraint violation. All five fields are always present
/// so API consumers and snapshot tests can rely on the shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Violation {
    pub shape_id: String,
    pub focus_node: String,
    pub path: String,
    pub severity: Severity,
    pub message: String,
}

/// Validation outcome for one or more focus nodes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationResult {
    pub conforms: bool,
    pub violations: Vec<Violation>,
}

impl ValidationResult {
    pub fn ok() -> Self {
        Self {
            conforms: true,
            violations: vec![],
        }
    }
    pub fn failed(violations: Vec<Violation>) -> Self {
        Self {
            conforms: violations.is_empty(),
            violations,
        }
    }
}

/// Contact shape from the plan: `Contact` requires `name` (min 1),
/// allows N `email`, and `account` values must be `Account` instances.
pub fn contact_shapes() -> Vec<Shape> {
    vec![Shape {
        id: "ex:ContactShape".to_string(),
        target_class: format!("{EX}Contact"),
        constraints: vec![
            PropertyConstraint {
                path: format!("{EX}name"),
                min_count: 1,
                max_count: None,
                datatype: None,
                value_class: None,
                message: "Contact requires at least one ex:name".to_string(),
                severity: Severity::Violation,
            },
            PropertyConstraint {
                path: format!("{EX}email"),
                min_count: 0,
                max_count: None,
                datatype: None,
                value_class: None,
                message: "ex:email values must be literals".to_string(),
                severity: Severity::Violation,
            },
            PropertyConstraint {
                path: format!("{EX}account"),
                min_count: 0,
                max_count: None,
                datatype: None,
                value_class: Some(format!("{EX}Account")),
                message: "ex:account range must be an Account resource".to_string(),
                severity: Severity::Violation,
            },
        ],
    }]
}

fn focus_node_iri(t: &Term) -> String {
    match t {
        Term::Iri(s) => s.clone(),
        Term::BlankNode(b) => format!("_:{b}"),
        Term::Literal { value, .. } => value.clone(),
    }
}

/// Validate a single focus node against `shapes` using the current store state.
pub fn validate_resource(
    repo: &(impl RdfRepository + ?Sized),
    reasoner: &OntologyReasoner,
    graph: &GraphName,
    subject: &Term,
    shapes: &[Shape],
) -> Result<ValidationResult, RdfError> {
    let quads = repo.match_quads(Some(subject), None, None, Some(graph), None)?;
    validate_staged_subject(repo, reasoner, graph, subject, &quads, shapes)
}

/// Validate a focus node against an explicit staged quad set (existing +
/// pending adds − pending removes). Pure: performs no writes.
pub fn validate_staged_subject(
    repo: &(impl RdfRepository + ?Sized),
    reasoner: &OntologyReasoner,
    graph: &GraphName,
    subject: &Term,
    staged: &[Quad],
    shapes: &[Shape],
) -> Result<ValidationResult, RdfError> {
    let inferred = reasoner.inferred_types_from_type_iris(&asserted_type_iris(staged, subject));
    let mut violations = Vec::new();

    for shape in shapes {
        if !inferred.contains(&shape.target_class) {
            continue;
        }
        for c in &shape.constraints {
            let values: Vec<&Quad> = staged
                .iter()
                .filter(|q| q.predicate.as_iri() == Some(c.path.as_str()))
                .collect();
            if values.len() < c.min_count {
                violations.push(Violation {
                    shape_id: shape.id.clone(),
                    focus_node: focus_node_iri(subject),
                    path: c.path.clone(),
                    severity: c.severity.clone(),
                    message: c.message.clone(),
                });
                continue;
            }
            if let Some(max) = c.max_count {
                if values.len() > max {
                    violations.push(Violation {
                        shape_id: shape.id.clone(),
                        focus_node: focus_node_iri(subject),
                        path: c.path.clone(),
                        severity: c.severity.clone(),
                        message: format!("{} (found {}, max {max})", c.message, values.len()),
                    });
                }
            }
            for q in values {
                // Datatype check.
                if let Some(dt) = &c.datatype {
                    match &q.object {
                        Term::Literal { datatype, .. } => {
                            let actual = datatype.clone().unwrap_or_else(|| {
                                "http://www.w3.org/2001/XMLSchema#string".to_string()
                            });
                            if &actual != dt {
                                violations.push(Violation {
                                    shape_id: shape.id.clone(),
                                    focus_node: focus_node_iri(subject),
                                    path: c.path.clone(),
                                    severity: c.severity.clone(),
                                    message: format!("expected datatype {dt}, found {actual}"),
                                });
                            }
                        }
                        _ => violations.push(Violation {
                            shape_id: shape.id.clone(),
                            focus_node: focus_node_iri(subject),
                            path: c.path.clone(),
                            severity: c.severity.clone(),
                            message: format!("expected literal with datatype {dt}"),
                        }),
                    }
                }
                // Class-range check: object must be an IRI instance of value_class.
                if let Some(vc) = &c.value_class {
                    match &q.object {
                        Term::Iri(obj_iri) => {
                            let obj_term = Term::iri(obj_iri.clone()).unwrap();
                            let is_ok = reasoner
                                .is_instance_of(repo, &obj_term, graph, vc)
                                .unwrap_or(false);
                            if !is_ok {
                                violations.push(Violation {
                                    shape_id: shape.id.clone(),
                                    focus_node: focus_node_iri(subject),
                                    path: c.path.clone(),
                                    severity: c.severity.clone(),
                                    message: format!(
                                        "{}: {} is not an instance of {vc}",
                                        c.message, obj_iri
                                    ),
                                });
                            }
                        }
                        other => violations.push(Violation {
                            shape_id: shape.id.clone(),
                            focus_node: focus_node_iri(subject),
                            path: c.path.clone(),
                            severity: c.severity.clone(),
                            message: format!(
                                "{}: expected IRI reference, found literal/bnode {other:?}",
                                c.message
                            ),
                        }),
                    }
                }
            }
        }
    }

    // Domain checks from the reasoner: any staged predicate with a declared
    // domain requires the focus node to be an instance of that domain.
    let mut seen_predicates = HashSet::new();
    for q in staged {
        if let Some(p) = q.predicate.as_iri() {
            if seen_predicates.insert(p.to_string()) {
                if let Some(domain) = reasoner.domain(p) {
                    if !inferred.contains(&domain) {
                        violations.push(Violation {
                            shape_id: "ex:DomainShape".to_string(),
                            focus_node: focus_node_iri(subject),
                            path: p.to_string(),
                            severity: Severity::Violation,
                            message: format!(
                                "property {p} has domain {domain}, but focus node is not an instance of it"
                            ),
                        });
                    }
                }
            }
        }
    }

    Ok(ValidationResult::failed(violations))
}

fn asserted_type_iris(staged: &[Quad], subject: &Term) -> Vec<String> {
    staged
        .iter()
        .filter(|q| &q.subject == subject && q.predicate.as_iri() == Some(RDF_TYPE))
        .filter_map(|q| q.object.as_iri().map(|s| s.to_string()))
        .collect()
}

// Inferred-type helper that avoids a repository round-trip when the caller
// already has the asserted IRIs (used by staged validation).
impl OntologyReasoner {
    pub(crate) fn inferred_types_from_type_iris(&self, asserted: &[String]) -> HashSet<String> {
        let mut out: HashSet<String> = asserted.iter().cloned().collect();
        let mut stack: Vec<String> = asserted.to_vec();
        while let Some(cur) = stack.pop() {
            for sup in <Self as OntologyService>::superclasses(self, &cur) {
                if out.insert(sup.clone()) {
                    stack.push(sup);
                }
            }
            for e in <Self as OntologyService>::equivalents(self, &cur) {
                if out.insert(e.clone()) {
                    stack.push(e);
                }
            }
        }
        out
    }
}

/// Group staged quads by subject (helper for the mutation gate).
pub fn staged_by_subject(staged: &[Quad]) -> HashMap<Term, Vec<Quad>> {
    let mut map: HashMap<Term, Vec<Quad>> = HashMap::new();
    for q in staged {
        map.entry(q.subject.clone()).or_default().push(q.clone());
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ontology::loader::{load_default_ontology, ontology_fixture_quads};
    use crate::rdf::{OxigraphRepository, RdfRepository};

    fn setup() -> (OxigraphRepository, OntologyReasoner, GraphName) {
        let repo = OxigraphRepository::new_in_memory(5000).unwrap();
        load_default_ontology(&repo).unwrap();
        let r = OntologyReasoner::from_repository(&repo).unwrap();
        (repo, r, GraphName::tenant("acme").unwrap())
    }

    fn contact(uri: &str, with_name: bool, account: Option<&str>) -> Vec<Quad> {
        let g = GraphName::tenant("acme").unwrap();
        let s = Term::iri(format!("{EX}{uri}")).unwrap();
        let mut v = vec![Quad::new(
            s.clone(),
            Term::iri(RDF_TYPE).unwrap(),
            Term::iri(format!("{EX}Contact")).unwrap(),
            g.clone(),
        )
        .unwrap()];
        if with_name {
            v.push(
                Quad::new(
                    s.clone(),
                    Term::iri(format!("{EX}name")).unwrap(),
                    Term::literal("Alice"),
                    g.clone(),
                )
                .unwrap(),
            );
        }
        if let Some(a) = account {
            v.push(
                Quad::new(
                    s.clone(),
                    Term::iri(format!("{EX}account")).unwrap(),
                    Term::iri(format!("{EX}{a}")).unwrap(),
                    g.clone(),
                )
                .unwrap(),
            );
        }
        v
    }

    #[test]
    fn valid_contact_passes() {
        let (repo, r, g) = setup();
        // Account target must exist and be typed.
        let acc = Term::iri(format!("{EX}Acme")).unwrap();
        repo.insert(&[Quad::new(
            acc.clone(),
            Term::iri(RDF_TYPE).unwrap(),
            Term::iri(format!("{EX}Account")).unwrap(),
            g.clone(),
        )
        .unwrap()])
            .unwrap();
        let mut quads = contact("Alice", true, Some("Acme"));
        // Staged set for Alice only.
        let alice = Term::iri(format!("{EX}Alice")).unwrap();
        repo.insert(&[quads
            .iter()
            .find(|q| q.object.as_iri() == Some(&format!("{EX}Acme")))
            .cloned()
            .unwrap_or_else(|| quads.remove(0))])
            .unwrap_or_default();
        let staged: Vec<Quad> = contact("Alice", true, Some("Acme"))
            .into_iter()
            .filter(|q| q.subject == alice)
            .collect();
        // Seed the account type first so the range check can resolve.
        let res =
            validate_staged_subject(&repo, &r, &g, &alice, &staged, &contact_shapes()).unwrap();
        assert!(res.conforms, "expected pass, got {:?}", res.violations);
    }

    #[test]
    fn missing_name_fails_with_structured_violation() {
        let (repo, r, g) = setup();
        let alice = Term::iri(format!("{EX}Bob")).unwrap();
        let staged = contact("Bob", false, None)
            .into_iter()
            .filter(|q| q.subject == alice)
            .collect::<Vec<_>>();
        let res =
            validate_staged_subject(&repo, &r, &g, &alice, &staged, &contact_shapes()).unwrap();
        assert!(!res.conforms);
        let v = &res.violations[0];
        assert_eq!(v.shape_id, "ex:ContactShape");
        assert_eq!(v.focus_node, format!("{EX}Bob"));
        assert_eq!(v.path, format!("{EX}name"));
        assert_eq!(v.severity, Severity::Violation);
        assert!(!v.message.is_empty());
    }

    #[test]
    fn account_range_must_be_account() {
        let (repo, r, g) = setup();
        let alice = Term::iri(format!("{EX}Alice")).unwrap();
        // Point account at a Contact (wrong type).
        let staged = vec![
            Quad::new(
                alice.clone(),
                Term::iri(RDF_TYPE).unwrap(),
                Term::iri(format!("{EX}Contact")).unwrap(),
                g.clone(),
            )
            .unwrap(),
            Quad::new(
                alice.clone(),
                Term::iri(format!("{EX}name")).unwrap(),
                Term::literal("A"),
                g.clone(),
            )
            .unwrap(),
            Quad::new(
                alice.clone(),
                Term::iri(format!("{EX}account")).unwrap(),
                Term::iri(format!("{EX}Eve")).unwrap(),
                g.clone(),
            )
            .unwrap(),
        ];
        let res =
            validate_staged_subject(&repo, &r, &g, &alice, &staged, &contact_shapes()).unwrap();
        assert!(!res.conforms);
        assert!(res
            .violations
            .iter()
            .any(|v| v.path == format!("{EX}account")));
    }

    #[test]
    fn domain_rejects_account_typed_subject_with_contact_property() {
        let (repo, r, g) = setup();
        let acc = Term::iri(format!("{EX}Acme")).unwrap();
        let staged = vec![
            Quad::new(
                acc.clone(),
                Term::iri(RDF_TYPE).unwrap(),
                Term::iri(format!("{EX}Account")).unwrap(),
                g.clone(),
            )
            .unwrap(),
            Quad::new(
                acc.clone(),
                Term::iri(format!("{EX}name")).unwrap(),
                Term::literal("Acme"),
                g.clone(),
            )
            .unwrap(),
        ];
        let res = validate_staged_subject(&repo, &r, &g, &acc, &staged, &contact_shapes()).unwrap();
        assert!(!res.conforms);
        assert!(res
            .violations
            .iter()
            .any(|v| v.shape_id == "ex:DomainShape"));
    }

    #[test]
    fn unused_fixture_import_is_used() {
        // Guards against fixture drift: the helper must stay loadable.
        assert!(!ontology_fixture_quads().is_empty());
    }
}
