//! `search` tool: natural-language query → ranked URIs.

use super::{check_read_auth, ToolError, ToolState};
use crate::policy::ExecutionContext;
use crate::rdf::GraphName;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchArgs {
    pub query: String,
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

pub fn handle(
    state: &ToolState,
    _ctx: &ExecutionContext,
    args: SearchArgs,
) -> Result<serde_json::Value, ToolError> {
    if args.query.trim().is_empty() {
        return Err(ToolError::args("search", "query must not be empty"));
    }
    let graph = match args.context {
        Some(ref g) => Some(
            GraphName::named(g.clone()).map_err(|e| ToolError::args("search", e.to_string()))?,
        ),
        None => None,
    };
    check_read_auth("search", _ctx, graph.as_ref())?;
    let limit = args.limit.unwrap_or(state.limits.default_page_limit);
    let limit = limit.max(1).min(state.limits.max_resources_per_read);
    let hits = state
        .search
        .search(
            &args.query,
            graph.as_ref(),
            args.namespace.as_deref(),
            Some(limit),
        )
        .map_err(|e| ToolError::storage("search", e.to_string()))?;
    Ok(serde_json::json!({ "results": hits, "truncated": false }))
}
