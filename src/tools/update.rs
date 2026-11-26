//! `update` tool: atomic add/remove on an existing resource.

use super::create::{map_policy, property_to_quad, PropertyInput};
use super::{check_grounding, ToolError, ToolState};
use crate::policy::{execute_guarded, ExecutionContext, MutationOptions};
use crate::rdf::{GraphName, Term};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateArgs {
    pub uri: String,
    pub graph: String,
    #[serde(default)]
    pub add: Vec<PropertyInput>,
    #[serde(default)]
    pub remove: Vec<PropertyInput>,
}

pub fn handle(
    state: &ToolState,
    ctx: &ExecutionContext,
    args: UpdateArgs,
) -> Result<serde_json::Value, ToolError> {
    let graph = GraphName::named(args.graph.clone())
        .map_err(|e| ToolError::args("update", e.to_string()))?;
    check_grounding("update", state, ctx, Some(&args.uri), &graph)?;
    let subject =
        Term::iri(args.uri.clone()).map_err(|e| ToolError::args("update", e.to_string()))?;
    let mut to_add = Vec::new();
    let mut to_remove = Vec::new();
    for p in &args.add {
        to_add.push(property_to_quad("update", &subject, &graph, p)?);
    }
    for p in &args.remove {
        to_remove.push(property_to_quad("update", &subject, &graph, p)?);
    }
    if to_add.is_empty() && to_remove.is_empty() {
        return Err(ToolError::args("update", "add and remove are both empty"));
    }
    let outcome = execute_guarded(
        &*state.repo,
        &state.reasoner,
        &state.shapes,
        &state.audit,
        ctx,
        &args.uri,
        &graph,
        &to_add,
        &to_remove,
        MutationOptions::default(),
    )
    .map_err(|e| map_policy("update", e))?;
    let _ = state.search.sync_resource(&*state.repo, &args.uri, &graph);
    Ok(serde_json::json!({
        "uri": args.uri,
        "added": outcome.delta.added.len(),
        "removed": outcome.delta.removed.len(),
        "summary": outcome.summary,
    }))
}
