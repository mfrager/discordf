//! `validate` tool (Plan 2 Phase 2): SHACL check stub backed by the
//! existing `ontology/shacl.rs`. Full executor wiring lands in Phase 5.

use serde::{Deserialize, Serialize};

use super::{check_read_auth, ToolError, ToolState};
use crate::plan::GraphContext;
use crate::policy::ExecutionContext;
use crate::rdf::{GraphName, Term};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidateArgs {
    pub graph: String,
    #[serde(default)]
    pub uri: Option<String>,
}

pub fn handle_validate(
    state: &ToolState,
    ctx: &ExecutionContext,
    graph_ctx: &GraphContext,
    args: ValidateArgs,
) -> Result<serde_json::Value, ToolError> {
    let graph = GraphName::named(args.graph.clone())
        .map_err(|e| ToolError::args("validate", e.to_string()))?;
    if !graph_ctx.allows_graph(&graph) {
        return Err(ToolError::auth(
            "validate",
            format!("graph {graph} not in plan GraphContext"),
        ));
    }
    check_read_auth("validate", ctx, Some(&graph))?;

    if let Some(uri) = args.uri {
        let subject =
            Term::iri(uri.clone()).map_err(|e| ToolError::args("validate", e.to_string()))?;
        let res = crate::ontology::validate_resource(
            &*state.repo,
            &state.reasoner,
            &graph,
            &subject,
            &state.shapes,
        )
        .map_err(|e| ToolError::storage("validate", e.to_string()))?;
        return Ok(serde_json::json!({
            "conforms": res.conforms,
            "violations": res.violations,
            "output": "ValidationResult",
        }));
    }

    // Graph-wide: validate every typed subject (bounded).
    let quads = state
        .repo
        .match_quads(None, None, None, Some(&graph), Some(state.limits.max_triples_per_read))
        .map_err(|e| ToolError::storage("validate", e.to_string()))?;
    let mut subjects: Vec<Term> = vec![];
    for q in &quads {
        if !subjects.contains(&q.subject) {
            subjects.push(q.subject.clone());
        }
        if subjects.len() >= state.limits.max_resources_per_read {
            break;
        }
    }
    let mut violations = vec![];
    for s in &subjects {
        let r = crate::ontology::validate_resource(
            &*state.repo,
            &state.reasoner,
            &graph,
            s,
            &state.shapes,
        )
        .map_err(|e| ToolError::storage("validate", e.to_string()))?;
        violations.extend(r.violations);
    }
    Ok(serde_json::json!({
        "conforms": violations.is_empty(),
        "violations": violations,
        "subjects_checked": subjects.len(),
        "output": "ValidationResult",
    }))
}
