//! `search_instances` + `describe_resource` (Plan 2 Phase 2).
//!
//! Instance discovery operates on **instance data only** (Tantivy index over
//! app graphs; ontology triples are excluded from the index and filtered
//! again here). Vocabulary definitions never appear in instance results.

use serde::{Deserialize, Serialize};

use super::vocabulary::ontology_subjects;
use super::{check_read_auth, ToolError, ToolState};
use crate::plan::GraphContext;
use crate::policy::ExecutionContext;
use crate::rdf::{GraphName, Term};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchInstancesArgs {
    pub text: String,
    #[serde(default)]
    pub types: Option<Vec<String>>,
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    /// Optional graph scope (tenant graph IRI). Unset = all allowed graphs.
    #[serde(default)]
    pub context: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DescribeResourceArgs {
    pub uris: Vec<String>,
    #[serde(default)]
    pub context: Option<String>,
}

fn is_vocab_type(type_uri: &str) -> bool {
    let l = type_uri.to_lowercase();
    l.ends_with("#class")
        || l.ends_with("/class")
        || l.contains("owl#class")
        || l.contains("owl#objectproperty")
        || l.contains("owl#datatypeproperty")
        || l.ends_with("#property")
        || l.contains("rdfs#class")
}

pub fn handle_search_instances(
    state: &ToolState,
    ctx: &ExecutionContext,
    graph_ctx: &GraphContext,
    args: SearchInstancesArgs,
) -> Result<serde_json::Value, ToolError> {
    if args.text.trim().is_empty() {
        return Err(ToolError::args("search_instances", "text must not be empty"));
    }
    let graph = match &args.context {
        Some(g) => Some(
            GraphName::named(g.clone())
                .map_err(|e| ToolError::args("search_instances", e.to_string()))?,
        ),
        None => None,
    };
    // Plan-level scope: concrete graph must be in the GraphContext when set.
    if let Some(g) = &graph {
        if !graph_ctx.allows_graph(g) {
            return Err(ToolError::auth(
                "search_instances",
                format!("graph {g} not in plan GraphContext"),
            ));
        }
    }
    check_read_auth("search_instances", ctx, graph.as_ref())?;

    // Namespace isolation against the plan context.
    if let Some(ns) = &args.namespace {
        if !graph_ctx.allows_namespace(ns) {
            return Ok(serde_json::json!({
                "results": [],
                "output": "ResourceCandidates",
                "scope_limited": true,
            }));
        }
    }

    let limit = args
        .limit
        .unwrap_or(state.limits.default_page_limit)
        .max(1)
        .min(state.limits.max_resources_per_read);
    let mut hits = state
        .search
        .search(
            &args.text,
            graph.as_ref(),
            args.namespace.as_deref(),
            Some(limit * 2),
        )
        .map_err(|e| ToolError::storage("search_instances", e.to_string()))?;

    // Exclude vocabulary: ontology subjects + schema-typed hits can never be
    // instance results (the index already skips the ontology graph; this is
    // the second, explicit gate).
    let vocab = ontology_subjects(state).unwrap_or_default();
    hits.retain(|h| !vocab.contains(&h.uri) && !is_vocab_type(&h.type_));
    // Optional client-side type filter.
    if let Some(types) = &args.types {
        if !types.is_empty() {
            hits.retain(|h| types.iter().any(|t| &h.type_ == t));
        }
    }
    hits.truncate(limit);

    let results: Vec<serde_json::Value> = hits
        .into_iter()
        .map(|h| {
            serde_json::json!({
                "uri": h.uri,
                "label": h.label,
                "type": h.type_,
                "description": h.description,
                "score": h.score,
            })
        })
        .collect();
    Ok(serde_json::json!({
        "results": results,
        "output": "ResourceCandidates",
        "scope_limited": false,
    }))
}

pub fn handle_describe_resource(
    state: &ToolState,
    ctx: &ExecutionContext,
    graph_ctx: &GraphContext,
    args: DescribeResourceArgs,
) -> Result<serde_json::Value, ToolError> {
    if args.uris.is_empty() {
        return Err(ToolError::args("describe_resource", "uris must not be empty"));
    }
    if args.uris.len() > state.limits.max_resources_per_read {
        return Err(ToolError::args(
            "describe_resource",
            format!("too many uris (max {})", state.limits.max_resources_per_read),
        ));
    }
    let graph = match &args.context {
        Some(g) => Some(
            GraphName::named(g.clone())
                .map_err(|e| ToolError::args("describe_resource", e.to_string()))?,
        ),
        None => None,
    };
    if let Some(g) = &graph {
        if !graph_ctx.allows_graph(g) {
            return Err(ToolError::auth(
                "describe_resource",
                format!("graph {g} not in plan GraphContext"),
            ));
        }
    }
    check_read_auth("describe_resource", ctx, graph.as_ref())?;

    let mut resources = vec![];
    for uri in &args.uris {
        let subject =
            Term::iri(uri.clone()).map_err(|e| ToolError::args("describe_resource", e.to_string()))?;
        let quads = state
            .repo
            .match_quads(
                Some(&subject),
                None,
                None,
                graph.as_ref(),
                Some(state.limits.max_triples_per_read),
            )
            .map_err(|e| ToolError::storage("describe_resource", e.to_string()))?;
        let mut props = vec![];
        let mut types = vec![];
        for q in quads {
            let pred = q.predicate.as_iri().unwrap_or("").to_string();
            if pred == "http://www.w3.org/1999/02/22-rdf-syntax-ns#type" {
                if let Some(t) = q.object.as_iri() {
                    types.push(t.to_string());
                }
                continue;
            }
            let (kind, value) = match &q.object {
                Term::Iri(s) => ("iri", s.clone()),
                Term::BlankNode(b) => ("bnode", b.clone()),
                Term::Literal { value, .. } => ("literal", value.clone()),
            };
            props.push(serde_json::json!({"predicate": pred, "kind": kind, "value": value}));
        }
        resources.push(serde_json::json!({
            "uri": uri,
            "types": types,
            "properties": props,
        }));
    }
    Ok(serde_json::json!({
        "resources": resources,
        "output": "ResourceDescriptions",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppLimits;
    use crate::ontology::{contact_shapes, load_default_ontology, OntologyReasoner};
    use crate::policy::{AuditLog, ExecutionContext, Operation};
    use crate::rdf::{GraphName, OxigraphRepository, RdfRepository, Term};
    use crate::search::TantivyIndex;
    use std::sync::Arc;

    pub(crate) fn vocab_fixture() -> (ToolState, ExecutionContext) {
        let repo = Arc::new(OxigraphRepository::new_in_memory(50_000).unwrap());
        load_default_ontology(&*repo).unwrap();
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
        {
            use crate::search::SearchService;
            search.rebuild_from_repo(&*repo).unwrap();
        }
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
    fn instance_search_returns_instances_never_vocab() {
        let (state, ctx) = vocab_fixture();
        let v = handle_search_instances(
            &state,
            &ctx,
            &GraphContext::default(),
            SearchInstancesArgs {
                text: "Alice".to_string(),
                types: None,
                namespace: None,
                limit: None,
                context: None,
            },
        )
        .unwrap();
        assert_eq!(v.get("output").unwrap(), "ResourceCandidates");
        let results = v.get("results").unwrap().as_array().unwrap();
        let uris: Vec<&str> = results
            .iter()
            .map(|r| r.get("uri").unwrap().as_str().unwrap())
            .collect();
        assert!(
            uris.contains(&"http://example.com/Alice"),
            "expected Alice in {uris:?}"
        );
        // Vocabulary definitions never leak into instance results.
        assert!(!uris.contains(&"http://example.com/Contact"));
        assert!(!uris.contains(&"http://example.com/Account"));
    }

    #[test]
    fn output_types_cannot_conflate() {
        // Vocabulary output tag differs from instance output tag by construction.
        assert_ne!("VocabularyCandidates", "ResourceCandidates");
        let (state, ctx) = vocab_fixture();
        let v = handle_search_instances(
            &state,
            &ctx,
            &GraphContext::default(),
            SearchInstancesArgs {
                text: "contact".to_string(),
                types: None,
                namespace: None,
                limit: Some(5),
                context: None,
            },
        )
        .unwrap();
        assert_eq!(v.get("output").unwrap(), "ResourceCandidates");
    }

    #[test]
    fn describe_resource_round_trips() {
        let (state, ctx) = vocab_fixture();
        let v = handle_describe_resource(
            &state,
            &ctx,
            &GraphContext::default(),
            DescribeResourceArgs {
                uris: vec!["http://example.com/Alice".to_string()],
                context: None,
            },
        )
        .unwrap();
        assert_eq!(v.get("output").unwrap(), "ResourceDescriptions");
        let resources = v.get("resources").unwrap().as_array().unwrap();
        assert_eq!(resources.len(), 1);
    }
}
