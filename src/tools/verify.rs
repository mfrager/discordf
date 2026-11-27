//! `verify` tool (Plan 2 Phase 2): independent re-query of the graph to
//! confirm an outcome. The model's declaration is never proof; this tool is.

use serde::{Deserialize, Serialize};

use super::{check_read_auth, ToolError, ToolState};
use crate::plan::GraphContext;
use crate::policy::ExecutionContext;
use crate::rdf::{GraphName, Term};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifyArgs {
    pub question: String,
    pub graph: String,
    /// Optional focus URI: count/report quads for this subject.
    #[serde(default)]
    pub uri: Option<String>,
}

pub fn handle_verify(
    state: &ToolState,
    ctx: &ExecutionContext,
    graph_ctx: &GraphContext,
    args: VerifyArgs,
) -> Result<serde_json::Value, ToolError> {
    if args.question.trim().is_empty() {
        return Err(ToolError::args("verify", "question must not be empty"));
    }
    let graph = GraphName::named(args.graph.clone())
        .map_err(|e| ToolError::args("verify", e.to_string()))?;
    if !graph_ctx.allows_graph(&graph) {
        return Err(ToolError::auth(
            "verify",
            format!("graph {graph} not in plan GraphContext"),
        ));
    }
    check_read_auth("verify", ctx, Some(&graph))?;

    let quad_count = match &args.uri {
        Some(uri) => {
            let subject = Term::iri(uri.clone())
                .map_err(|e| ToolError::args("verify", e.to_string()))?;
            state
                .repo
                .match_quads(
                    Some(&subject),
                    None,
                    None,
                    Some(&graph),
                    Some(state.limits.max_triples_per_read),
                )
                .map_err(|e| ToolError::storage("verify", e.to_string()))?
                .len()
        }
        None => state
            .repo
            .match_quads(None, None, None, Some(&graph), Some(state.limits.max_triples_per_read))
            .map_err(|e| ToolError::storage("verify", e.to_string()))?
            .len(),
    };
    Ok(serde_json::json!({
        "question": args.question,
        "graph": args.graph,
        "quad_count": quad_count,
        "answer": format!("{quad_count} quads in graph {}", args.graph),
        "output": "VerificationResult",
    }))
}
