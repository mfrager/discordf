//! `delete` tool: remove a resource or selected triples (confirmation-gated).

use super::create::map_policy;
use super::{check_grounding, ToolError, ToolState};
use crate::policy::{execute_guarded, ExecutionContext, MutationOptions};
use crate::rdf::{GraphName, Quad, Term};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TripleInput {
    pub subject: String,
    pub predicate: String,
    pub kind: String,
    pub value: String,
    #[serde(default)]
    pub datatype: Option<String>,
    #[serde(default)]
    pub lang: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteArgs {
    #[serde(default)]
    pub uri: Option<String>,
    pub graph: String,
    #[serde(default)]
    pub triples: Option<Vec<TripleInput>>,
    #[serde(default)]
    pub confirmation_token: Option<String>,
}

fn term_from_input(
    tool: &str,
    kind: &str,
    value: &str,
    dt: &Option<String>,
    lang: &Option<String>,
) -> Result<Term, ToolError> {
    match kind {
        "iri" => Term::iri(value.to_string()).map_err(|e| ToolError::args(tool, e.to_string())),
        "literal" => Ok(Term::literal(value.to_string())),
        "typed" => {
            let d = dt
                .clone()
                .ok_or_else(|| ToolError::args(tool, "typed value needs datatype"))?;
            Term::typed_literal(value.to_string(), d)
                .map_err(|e| ToolError::args(tool, e.to_string()))
        }
        "lang" => {
            let l = lang
                .clone()
                .ok_or_else(|| ToolError::args(tool, "lang value needs lang"))?;
            Term::lang_literal(value.to_string(), l)
                .map_err(|e| ToolError::args(tool, e.to_string()))
        }
        "bnode" => Term::bnode(value.to_string()).map_err(|e| ToolError::args(tool, e.to_string())),
        other => Err(ToolError::args(tool, format!("unsupported kind: {other}"))),
    }
}

pub fn handle(
    state: &ToolState,
    ctx: &ExecutionContext,
    args: DeleteArgs,
) -> Result<serde_json::Value, ToolError> {
    let graph = GraphName::named(args.graph.clone())
        .map_err(|e| ToolError::args("delete", e.to_string()))?;
    let triples = args.triples.unwrap_or_default();
    if args.uri.is_none() && triples.is_empty() {
        return Err(ToolError::args(
            "delete",
            "either uri or triples is required",
        ));
    }
    // Grounding: delete targets must exist.
    if let Some(ref u) = args.uri {
        check_grounding("delete", state, ctx, Some(u), &graph)?;
    }
    for t in &triples {
        check_grounding("delete", state, ctx, Some(&t.subject), &graph)?;
    }

    let to_remove: Vec<Quad> = if let Some(uri) = args.uri.clone() {
        let subject =
            Term::iri(uri.clone()).map_err(|e| ToolError::args("delete", e.to_string()))?;
        let mut existing = state
            .repo
            .match_quads(Some(&subject), None, None, Some(&graph), None)
            .map_err(|e| ToolError::storage("delete", e.to_string()))?;
        if !triples.is_empty() {
            // uri + triples: restrict to the listed triples.
            let wanted: Result<Vec<Quad>, ToolError> = triples
                .iter()
                .map(|t| {
                    let s = Term::iri(t.subject.clone())
                        .map_err(|e| ToolError::args("delete", e.to_string()))?;
                    let p = Term::iri(t.predicate.clone())
                        .map_err(|e| ToolError::args("delete", e.to_string()))?;
                    let o = term_from_input("delete", &t.kind, &t.value, &t.datatype, &t.lang)?;
                    Quad::new(s, p, o, graph.clone())
                        .map_err(|e| ToolError::args("delete", e.to_string()))
                })
                .collect();
            let wanted = wanted?;
            existing.retain(|q| wanted.contains(q));
            existing
        } else {
            existing
        }
    } else {
        triples
            .iter()
            .map(|t| {
                let s = Term::iri(t.subject.clone())
                    .map_err(|e| ToolError::args("delete", e.to_string()))?;
                let p = Term::iri(t.predicate.clone())
                    .map_err(|e| ToolError::args("delete", e.to_string()))?;
                let o = term_from_input("delete", &t.kind, &t.value, &t.datatype, &t.lang)?;
                Quad::new(s, p, o, graph.clone())
                    .map_err(|e| ToolError::args("delete", e.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?
    };

    if to_remove.is_empty() {
        return Err(ToolError::args("delete", "no matching triples to delete"));
    }

    let target = args
        .uri
        .clone()
        .unwrap_or_else(|| format!("{} triple(s)", to_remove.len()));
    let outcome = execute_guarded(
        &*state.repo,
        &state.reasoner,
        &state.shapes,
        &state.audit,
        ctx,
        &target,
        &graph,
        &[],
        &to_remove,
        MutationOptions {
            dry_run: false,
            confirmation_token: args.confirmation_token.clone(),
        },
    )
    .map_err(|e| map_policy("delete", e))?;
    if let Some(uri) = args.uri {
        let _ = state.search.sync_resource(&*state.repo, &uri, &graph);
    } else {
        // Triple-form: refresh each affected subject.
        use std::collections::HashSet;
        let mut subjects: HashSet<String> = HashSet::new();
        for q in &to_remove {
            if let Some(s) = q.subject.as_iri() {
                subjects.insert(s.to_string());
            }
        }
        for s in subjects {
            let _ = state.search.sync_resource(&*state.repo, &s, &graph);
        }
    }
    Ok(serde_json::json!({
        "removed": outcome.delta.removed.len(),
        "summary": outcome.summary,
    }))
}
