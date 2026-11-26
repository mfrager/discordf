//! `read` tool: resource-oriented reads with bounded traversal.
//!
//! Depth 0/1 returns direct properties only. Depth ≥ 2 follows selected
//! IRI relations (allowlisted by `predicates` when given) subject to the
//! triple/resource/path caps from `AppLimits`. Literal-vs-IRI distinction
//! is preserved; IRI objects return real URIs for follow-up reads.

use super::{check_read_auth, ToolError, ToolState};
use crate::policy::ExecutionContext;
use crate::rdf::{GraphName, Quad, Term};
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadArgs {
    pub uris: Vec<String>,
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default)]
    pub predicates: Option<Vec<String>>,
    #[serde(default)]
    pub depth: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TermView {
    pub kind: String,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub datatype: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_uri: Option<String>,
}

impl TermView {
    pub fn from_term(t: &Term) -> Self {
        match t {
            Term::Iri(s) => Self {
                kind: "iri".to_string(),
                value: s.clone(),
                datatype: None,
                lang: None,
                target_uri: Some(s.clone()),
            },
            Term::BlankNode(id) => Self {
                kind: "bnode".to_string(),
                value: id.clone(),
                datatype: None,
                lang: None,
                target_uri: None,
            },
            Term::Literal {
                value,
                datatype,
                lang,
            } => Self {
                kind: "literal".to_string(),
                value: value.clone(),
                datatype: datatype.clone(),
                lang: lang.clone(),
                target_uri: None,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PropertyView {
    pub predicate: String,
    pub object: TermView,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourceView {
    pub uri: String,
    pub graph: String,
    pub properties: Vec<PropertyView>,
}

fn parse_graph(s: Option<&String>) -> Result<Option<GraphName>, ToolError> {
    match s {
        None => Ok(None),
        Some(g) => Ok(Some(
            GraphName::named(g.clone()).map_err(|e| ToolError::args("read", e.to_string()))?,
        )),
    }
}

pub fn handle(
    state: &ToolState,
    _ctx: &ExecutionContext,
    args: ReadArgs,
) -> Result<serde_json::Value, ToolError> {
    if args.uris.is_empty() {
        return Err(ToolError::args("read", "uris must not be empty"));
    }
    if args.uris.len() > state.limits.max_resources_per_read {
        return Err(ToolError::args(
            "read",
            format!(
                "too many uris (max {})",
                state.limits.max_resources_per_read
            ),
        ));
    }
    let depth = args.depth.unwrap_or(state.limits.default_read_depth).min(3);
    let graph = parse_graph(args.context.as_ref())?;
    check_read_auth("read", _ctx, graph.as_ref())?;
    let allow: Option<HashSet<String>> = args.predicates.map(|v| v.into_iter().collect());

    let mut resources: Vec<ResourceView> = Vec::new();
    let mut triple_budget = state.limits.max_triples_per_read;
    let mut path_count = 0usize;
    let mut truncated = false;
    let mut visited: HashSet<(String, String)> = HashSet::new();
    let mut queue: VecDeque<(String, usize)> = args.uris.iter().map(|u| (u.clone(), 0)).collect();

    while let Some((uri, d)) = queue.pop_front() {
        if resources.len() >= state.limits.max_resources_per_read {
            truncated = true;
            break;
        }
        // Resolve graph per request (single context for now).
        let g = graph.clone();
        let subject = Term::iri(uri.clone()).map_err(|e| ToolError::args("read", e.to_string()))?;
        let quads: Vec<Quad> = state
            .repo
            .match_quads(
                Some(&subject),
                None,
                None,
                g.as_ref(),
                Some(triple_budget.max(1)),
            )
            .map_err(|e| ToolError::storage("read", e.to_string()))?;
        if triple_budget == 0 {
            truncated = true;
            break;
        }
        let take = triple_budget.min(quads.len());
        if take < quads.len() {
            truncated = true;
        }
        let mut props = Vec::new();
        for q in quads.into_iter().take(take) {
            triple_budget = triple_budget.saturating_sub(1);
            let pred = q.predicate.as_iri().unwrap_or("").to_string();
            if let Some(ref allow) = allow {
                if !allow.contains(&pred) {
                    continue;
                }
            }
            // Follow-up traversal for depth ≥ 2 over IRI objects.
            if d < depth
                && depth >= 2
                && matches!(q.object, Term::Iri(_))
                && path_count < state.limits.max_traversal_paths
            {
                let target = q.object.as_iri().unwrap_or("").to_string();
                let key = (target.clone(), format!("{:?}", g));
                if !visited.contains(&key)
                    && resources.len() + queue.len() < state.limits.max_resources_per_read
                {
                    visited.insert(key);
                    path_count += 1;
                    queue.push_back((target, d + 1));
                } else if path_count >= state.limits.max_traversal_paths {
                    truncated = true;
                }
            }
            props.push(PropertyView {
                predicate: pred,
                object: TermView::from_term(&q.object),
            });
        }
        resources.push(ResourceView {
            uri: uri.clone(),
            graph: g
                .map(|x| x.to_string())
                .unwrap_or_else(|| "default".to_string()),
            properties: props,
        });
        if triple_budget == 0 && !queue.is_empty() {
            truncated = true;
            break;
        }
    }

    Ok(serde_json::json!({ "resources": resources, "truncated": truncated }))
}
