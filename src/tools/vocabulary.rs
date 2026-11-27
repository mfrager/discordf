//! `search_vocabulary` + `describe_vocabulary` (Plan 2 Phase 2).
//!
//! Vocabulary discovery operates on the **ontology graph only** — never on
//! instance data. `search_instances` (see `instances.rs`) is the only tool
//! that returns instance URIs, so the two output types can never conflate.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use super::{check_read_auth, ToolError, ToolState};
use crate::ontology::OntologyService;
use crate::plan::GraphContext;
use crate::policy::ExecutionContext;
use crate::rdf::{GraphName, Term};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchVocabularyArgs {
    /// Free-text query; split into terms on whitespace when `terms` is empty.
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub terms: Vec<String>,
    #[serde(default)]
    pub namespaces: Vec<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DescribeVocabularyArgs {
    pub iris: Vec<String>,
}

/// Short name after the last `/` or `#`.
fn short_name(iri: &str) -> String {
    iri.rsplit(['/', '#']).next().unwrap_or(iri).to_string()
}

fn local_lower(iri: &str) -> String {
    short_name(iri).to_lowercase()
}

/// All subject IRIs asserted in the ontology graph.
pub fn ontology_subjects(state: &ToolState) -> Result<HashSet<String>, ToolError> {
    let quads = state
        .repo
        .match_quads(None, None, None, Some(&GraphName::ontology()), None)
        .map_err(|e| ToolError::storage("search_vocabulary", e.to_string()))?;
    Ok(quads
        .iter()
        .filter_map(|q| q.subject.as_iri().map(|s| s.to_string()))
        .collect())
}

fn kind_of(
    iri: &str,
    types: &HashMap<String, Vec<String>>,
    reasoner: &crate::ontology::OntologyReasoner,
) -> String {
    if let Some(ts) = types.get(iri) {
        for t in ts {
            let l = short_name(t).to_lowercase();
            if l == "class" {
                return "class".to_string();
            }
            if l == "objectproperty" {
                return "object_property".to_string();
            }
            if l == "datatypeproperty" {
                return "datatype_property".to_string();
            }
            if l == "annotationproperty" {
                return "annotation_property".to_string();
            }
        }
    }
    // Heuristic fallback from reasoner knowledge.
    if reasoner.domain(iri).is_some() || reasoner.range(iri).is_some() {
        return "property".to_string();
    }
    if !reasoner.superclasses(iri).is_empty() || !reasoner.equivalents(iri).is_empty() {
        return "class".to_string();
    }
    "term".to_string()
}

pub fn handle_search_vocabulary(
    state: &ToolState,
    ctx: &ExecutionContext,
    graph_ctx: &GraphContext,
    args: SearchVocabularyArgs,
) -> Result<serde_json::Value, ToolError> {
    // Vocab lives in the ontology graph: require read access to it unless the
    // caller is a fully-unconstrained legacy context.
    check_read_auth("search_vocabulary", ctx, Some(&GraphName::ontology()))?;

    // Collect terms.
    let mut terms: Vec<String> = args.terms.clone();
    if terms.is_empty() {
        if let Some(q) = args.query.clone() {
            terms = q.split_whitespace().map(|s| s.to_string()).collect();
        }
    }
    terms.retain(|t| !t.trim().is_empty());
    if terms.is_empty() {
        return Err(ToolError::args(
            "search_vocabulary",
            "provide query or non-empty terms",
        ));
    }

    // Namespace scoping: an explicitly requested namespace outside the
    // GraphContext allowlist yields empty + scope-limited (never leaks).
    if graph_ctx.scope_limited(&args.namespaces) {
        return Ok(serde_json::json!({
            "results": [],
            "output": "VocabularyCandidates",
            "scope_limited": true,
        }));
    }

    let quads = state
        .repo
        .match_quads(None, None, None, Some(&GraphName::ontology()), None)
        .map_err(|e| ToolError::storage("search_vocabulary", e.to_string()))?;

    // Index ontology subjects: types + label-ish literals.
    let mut subjects: HashSet<String> = HashSet::new();
    let mut types: HashMap<String, Vec<String>> = HashMap::new();
    for q in &quads {
        if let Some(s) = q.subject.as_iri() {
            subjects.insert(s.to_string());
            if q.predicate.as_iri()
                == Some("http://www.w3.org/1999/02/22-rdf-syntax-ns#type")
            {
                if let Some(o) = q.object.as_iri() {
                    types.entry(s.to_string()).or_default().push(o.to_string());
                }
            }
        }
    }

    let lowered: Vec<String> = terms.iter().map(|t| t.to_lowercase()).collect();
    let mut hits: Vec<serde_json::Value> = vec![];
    let mut ordered: Vec<String> = subjects.into_iter().collect();
    ordered.sort();
    for iri in ordered {
        // Namespace isolation: GraphContext first, then explicit request filter.
        if !graph_ctx.allows_namespace(&iri) {
            continue;
        }
        if !args.namespaces.is_empty() && !args.namespaces.iter().any(|ns| iri.starts_with(ns)) {
            continue;
        }
        let local = local_lower(&iri);
        let matched: Vec<String> = lowered
            .iter()
            .zip(terms.iter())
            .filter(|(l, _)| local.contains(l.as_str()) || iri.to_lowercase().contains(l.as_str()))
            .map(|(_, t)| t.clone())
            .collect();
        if matched.is_empty() {
            continue;
        }
        let kind = kind_of(&iri, &types, &state.reasoner);
        hits.push(serde_json::json!({
            "iri": iri,
            "kind": kind,
            "label": short_name(&iri),
            "comment": "",
            "matched_terms": matched,
            "domain": state.reasoner.domain(&iri),
            "range": state.reasoner.range(&iri),
        }));
    }

    let limit = args
        .limit
        .unwrap_or(state.limits.default_page_limit)
        .max(1)
        .min(state.limits.max_resources_per_read);
    let truncated = hits.len() > limit;
    hits.truncate(limit);
    Ok(serde_json::json!({
        "results": hits,
        "output": "VocabularyCandidates",
        "scope_limited": false,
        "truncated": truncated,
    }))
}

pub fn handle_describe_vocabulary(
    state: &ToolState,
    ctx: &ExecutionContext,
    _graph_ctx: &GraphContext,
    args: DescribeVocabularyArgs,
) -> Result<serde_json::Value, ToolError> {
    check_read_auth("describe_vocabulary", ctx, Some(&GraphName::ontology()))?;
    if args.iris.is_empty() {
        return Err(ToolError::args("describe_vocabulary", "iris must not be empty"));
    }
    if args.iris.len() > state.limits.max_resources_per_read {
        return Err(ToolError::args(
            "describe_vocabulary",
            format!("too many iris (max {})", state.limits.max_resources_per_read),
        ));
    }
    let subjects = ontology_subjects(state)?;
    let mut out = vec![];
    for iri in &args.iris {
        let known = subjects.contains(iri);
        // Domain/range/superclasses from the reasoner (None when unknown).
        let subj = Term::iri(iri.clone())
            .map_err(|e| ToolError::args("describe_vocabulary", e.to_string()))?;
        let supers = state.reasoner.superclasses(iri);
        let equivs = state.reasoner.equivalents(iri);
        // Kind via rdf:type quads in the ontology graph.
        let type_pred =
            Term::iri("http://www.w3.org/1999/02/22-rdf-syntax-ns#type".to_string()).unwrap();
        let type_quads = state
            .repo
            .match_quads(
                Some(&subj),
                Some(&type_pred),
                None,
                Some(&GraphName::ontology()),
                None,
            )
            .map_err(|e| ToolError::storage("describe_vocabulary", e.to_string()))?;
        let type_iris: Vec<String> = type_quads
            .iter()
            .filter_map(|q| q.object.as_iri().map(|s| s.to_string()))
            .collect();
        let mut type_map = HashMap::new();
        type_map.insert(iri.clone(), type_iris);
        let kind = kind_of(iri, &type_map, &state.reasoner);
        out.push(serde_json::json!({
            "iri": iri,
            "known": known,
            "kind": kind,
            "label": short_name(iri),
            "comment": "",
            "domain": state.reasoner.domain(iri),
            "range": state.reasoner.range(iri),
            "inverse": state.reasoner.inverse(iri),
            "superclasses": supers,
            "equivalents": equivs,
        }));
    }
    Ok(serde_json::json!({
        "terms": out,
        "output": "ResolvedVocabulary",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppLimits;
    use crate::ontology::{contact_shapes, load_default_ontology, OntologyReasoner};
    use crate::policy::{AuditLog, ExecutionContext, Operation};
    use crate::rdf::{GraphName, OxigraphRepository, RdfRepository, Term};
    use crate::search::{SearchService, TantivyIndex};
    use std::sync::Arc;

    pub fn fixture() -> (ToolState, ExecutionContext) {
        let repo = Arc::new(OxigraphRepository::new_in_memory(50_000).unwrap());
        load_default_ontology(&*repo).unwrap();
        // One instance so the vocab/instance split is testable.
        let g = GraphName::tenant("acme").unwrap();
        let alice = Term::iri("http://example.com/Alice".to_string()).unwrap();
        repo.insert(&[crate::rdf::Quad::new(
            alice,
            Term::iri("http://example.com/name".to_string()).unwrap(),
            Term::literal("Alice"),
            g.clone(),
        )
        .unwrap()])
        .unwrap();
        let reasoner = Arc::new(OntologyReasoner::from_repository(&*repo).unwrap());
        let search = Arc::new(TantivyIndex::in_ram().unwrap());
        search.rebuild_from_repo(&*repo).unwrap();
        let audit = Arc::new(AuditLog::in_memory());
        let state = ToolState::new(
            repo,
            search,
            reasoner,
            contact_shapes(),
            audit,
            AppLimits::default(),
        );
        let ctx = ExecutionContext::new(
            "req-1",
            "alice",
            "acme",
            vec![g, GraphName::ontology()],
            vec![Operation::Read, Operation::Create],
        );
        (state, ctx)
    }

    #[test]
    fn vocab_search_returns_classes_never_instances() {
        let (state, ctx) = fixture();
        let v = handle_search_vocabulary(
            &state,
            &ctx,
            &GraphContext::default(),
            SearchVocabularyArgs {
                query: Some("contact".to_string()),
                terms: vec![],
                namespaces: vec![],
                limit: None,
            },
        )
        .unwrap();
        let results = v.get("results").unwrap().as_array().unwrap();
        assert!(!results.is_empty());
        assert_eq!(v.get("output").unwrap(), "VocabularyCandidates");
        let iris: Vec<&str> = results
            .iter()
            .map(|r| r.get("iri").unwrap().as_str().unwrap())
            .collect();
        assert!(iris.contains(&"http://example.com/Contact"));
        // Instance URIs never appear in vocabulary results.
        assert!(!iris.contains(&"http://example.com/Alice"));
    }

    #[test]
    fn vocab_search_unlisted_namespace_is_scope_limited_empty() {
        let (state, ctx) = fixture();
        let gctx = GraphContext {
            namespaces: [("ex".to_string(), "http://example.com/".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let v = handle_search_vocabulary(
            &state,
            &ctx,
            &gctx,
            SearchVocabularyArgs {
                query: Some("contact".to_string()),
                terms: vec![],
                namespaces: vec!["http://other.com/".to_string()],
                limit: None,
            },
        )
        .unwrap();
        assert_eq!(v.get("results").unwrap().as_array().unwrap().len(), 0);
        assert_eq!(v.get("scope_limited").unwrap(), true);
    }

    #[test]
    fn describe_vocabulary_resolves_domain_range() {
        let (state, ctx) = fixture();
        let v = handle_describe_vocabulary(
            &state,
            &ctx,
            &GraphContext::default(),
            DescribeVocabularyArgs {
                iris: vec!["http://example.com/account".to_string()],
            },
        )
        .unwrap();
        let terms = v.get("terms").unwrap().as_array().unwrap();
        assert_eq!(terms.len(), 1);
        assert_eq!(terms[0].get("known").unwrap(), true);
        assert_eq!(
            terms[0].get("domain").unwrap().as_str().unwrap(),
            "http://example.com/Contact"
        );
        assert_eq!(
            terms[0].get("range").unwrap().as_str().unwrap(),
            "http://example.com/Account"
        );
    }
}
