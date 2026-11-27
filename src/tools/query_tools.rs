//! `build_query` + `inspect_query` + `execute_query` (Plan 2 Phase 3).
//!
//! Planners propose typed graph patterns; Rust validates, compiles to the
//! graph query language, and runs the plan. No tool accepts a free-text
//! query string: the only query input shape is the typed pattern object.

use serde::{Deserialize, Serialize};

use super::{check_read_auth, ToolError, ToolState};
use crate::plan::GraphContext;
use crate::policy::ExecutionContext;
use crate::query::pattern_from_json;
use crate::rdf::GraphName;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildQueryArgs {
    pub pattern: serde_json::Value,
    #[serde(default)]
    pub context: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectQueryArgs {
    pub plan_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecuteQueryArgs {
    pub plan_id: String,
    #[serde(default)]
    pub limit: Option<usize>,
}

fn map_err(tool: &str, e: crate::query::QueryError) -> ToolError {
    ToolError::new(tool, e.category(), e.message())
}

fn resolve_graphs(
    tool: &str,
    pattern_graphs_empty: bool,
    context: Option<String>,
) -> Result<Option<GraphName>, ToolError> {
    // Legacy single-graph scope for auth pre-checks; the typed pattern
    // carries the authoritative graph list (validated in the query module).
    let _ = pattern_graphs_empty;
    match context {
        None => Ok(None),
        Some(g) => Ok(Some(
            GraphName::named(g).map_err(|e| ToolError::args(tool, e.to_string()))?,
        )),
    }
}

pub fn handle_build_query(
    state: &ToolState,
    ctx: &ExecutionContext,
    graph_ctx: &GraphContext,
    args: BuildQueryArgs,
) -> Result<serde_json::Value, ToolError> {
    let graph = resolve_graphs("build_query", false, args.context)?;
    check_read_auth("build_query", ctx, graph.as_ref())?;
    let mut pattern =
        pattern_from_json(args.pattern).map_err(|e| map_err("build_query", e))?;
    // Per-call graph override: when the caller supplies `context` and the
    // pattern has no graphs, scope to it (keeps single-graph callers working).
    if pattern.graphs.is_empty() {
        if let Some(g) = graph {
            pattern.graphs = vec![g];
        }
    }
    let stored = state
        .query_plans
        .build(pattern, &*state.repo, &state.reasoner, graph_ctx, ctx, &state.limits)
        .map_err(|e| map_err("build_query", e))?;
    Ok(serde_json::json!({
        "plan_id": stored.id,
        "compiled": stored.compiled,
        "sparql": stored.compiled,
        "explanation": stored.explanation,
        "representations": stored.representations,
        "effective_limit": stored.effective_limit,
        "output": "QueryPlanId",
    }))
}

/// Wire an `execute_query` result back into an [`OperationContext`] as a
/// typed `UriSet` output keyed by `QueryPlanId` (Plan 2 Phase 3 §4), so
/// later plan steps can consume it via `StepRef` / `UriSetRef`.
pub fn store_execute_result_in_context(
    op_ctx: &mut crate::plan::OperationContext,
    plan_id: &str,
    result: &crate::query::QueryResult,
) {
    op_ctx.record_query_result(plan_id, result.uris.clone());
}

pub fn handle_inspect_query(
    state: &ToolState,
    ctx: &ExecutionContext,
    graph_ctx: &GraphContext,
    args: InspectQueryArgs,
) -> Result<serde_json::Value, ToolError> {
    if args.plan_id.trim().is_empty() {
        return Err(ToolError::args("inspect_query", "plan_id must not be empty"));
    }
    let stored = state.query_plans.get(&args.plan_id).ok_or_else(|| {
        ToolError::args("inspect_query", format!("unknown query plan {:?}", args.plan_id))
    })?;
    // Auth re-check against both scopes (mirrors execution gates).
    for g in &stored.pattern.graphs {
        if !graph_ctx.allows_graph(g) {
            return Err(ToolError::auth(
                "inspect_query",
                format!("graph {g} not in plan GraphContext"),
            ));
        }
    }
    check_read_auth("inspect_query", ctx, stored.pattern.graphs.first())?;
    // Bounded estimate: run with a small cap and report the total count.
    let estimate = state
        .query_plans
        .execute(
            &stored.id,
            &*state.repo,
            &state.reasoner,
            graph_ctx,
            ctx,
            &state.limits,
            Some(5),
        )
        .map_err(|e| map_err("inspect_query", e))?;
    Ok(serde_json::json!({
        "plan_id": stored.id,
        "compiled": stored.compiled,
        "sparql": stored.compiled,
        "explanation": stored.explanation,
        "representations": stored.representations,
        "inferred_constraints": {
            "patterns": stored.pattern.patterns.len(),
            "filters": stored.pattern.filters.len(),
            "graphs": stored.pattern.graphs.iter().map(|g| g.to_string()).collect::<Vec<_>>(),
            "return_vars": stored.pattern.return_vars,
        },
        "match_estimate": estimate.count,
        "effective_limit": stored.effective_limit,
        "output": "QueryPlan",
    }))
}

pub fn handle_execute_query(
    state: &ToolState,
    ctx: &ExecutionContext,
    graph_ctx: &GraphContext,
    args: ExecuteQueryArgs,
) -> Result<serde_json::Value, ToolError> {
    if args.plan_id.trim().is_empty() {
        return Err(ToolError::args("execute_query", "plan_id must not be empty"));
    }
    if state.query_plans.get(&args.plan_id).is_none() {
        return Err(ToolError::args(
            "execute_query",
            format!("unknown query plan {:?}", args.plan_id),
        ));
    }
    let result = state
        .query_plans
        .execute(
            &args.plan_id,
            &*state.repo,
            &state.reasoner,
            graph_ctx,
            ctx,
            &state.limits,
            args.limit,
        )
        .map_err(|e| map_err("execute_query", e))?;
    Ok(serde_json::json!({
        "plan_id": args.plan_id,
        "uris": result.uris,
        "rows": result.rows,
        "count": result.count,
        "truncated": result.truncated,
        "output": "UriSet",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppLimits;
    use crate::ontology::{load_default_ontology, OntologyReasoner};
    use crate::plan::GraphContext;
    use crate::policy::{AuditLog, ExecutionContext, Operation};
    use crate::rdf::{GraphName, OxigraphRepository, RdfRepository, Term};
    use crate::search::TantivyIndex;
    use std::sync::Arc;

    const EX: &str = "http://example.com/";

    fn fixture() -> (ToolState, ExecutionContext, GraphContext, GraphName) {
        let repo = Arc::new(OxigraphRepository::new_in_memory(50_000).unwrap());
        load_default_ontology(&*repo).unwrap();
        let g = GraphName::tenant("acme").unwrap();
        let alice = Term::iri(format!("{EX}Alice")).unwrap();
        let acme = Term::iri(format!("{EX}Acme")).unwrap();
        repo.insert(&[
            crate::rdf::Quad::new(
                alice.clone(),
                Term::iri("http://www.w3.org/1999/02/22-rdf-syntax-ns#type".to_string()).unwrap(),
                Term::iri(format!("{EX}Contact")).unwrap(),
                g.clone(),
            )
            .unwrap(),
            crate::rdf::Quad::new(
                alice.clone(),
                Term::iri(format!("{EX}account")).unwrap(),
                acme.clone(),
                g.clone(),
            )
            .unwrap(),
            crate::rdf::Quad::new(
                acme.clone(),
                Term::iri(format!("{EX}state")).unwrap(),
                Term::literal("OH"),
                g.clone(),
            )
            .unwrap(),
        ])
        .unwrap();
        let reasoner = Arc::new(OntologyReasoner::from_repository(&*repo).unwrap());
        let search = Arc::new(TantivyIndex::in_ram().unwrap());
        let audit = Arc::new(AuditLog::in_memory());
        let state = ToolState::new(
            repo,
            search,
            reasoner,
            crate::ontology::contact_shapes(),
            audit,
            AppLimits::default(),
        );
        let ctx = ExecutionContext::new(
            "req-1",
            "alice",
            "acme",
            vec![g.clone(), GraphName::ontology()],
            vec![Operation::Read, Operation::Create],
        );
        let gctx = GraphContext {
            graphs: vec![g.clone()],
            namespaces: [("ex".to_string(), EX.to_string())].into_iter().collect(),
            ..Default::default()
        };
        (state, ctx, gctx, g)
    }

    fn ohio_value(graph: &str) -> serde_json::Value {
        serde_json::json!({
            "operation": "select",
            "graphs": [graph],
            "patterns": [
                {"subject": {"kind": "var", "name": "?contact"},
                 "predicate": {"kind": "iri", "iri": "http://www.w3.org/1999/02/22-rdf-syntax-ns#type"},
                 "object": {"kind": "iri", "iri": "http://example.com/Contact"}},
                {"subject": {"kind": "var", "name": "?contact"},
                 "predicate": {"kind": "iri", "iri": "http://example.com/account"},
                 "object": {"kind": "var", "name": "?account"}},
                {"subject": {"kind": "var", "name": "?account"},
                 "predicate": {"kind": "iri", "iri": "http://example.com/state"},
                 "object": {"kind": "literal", "value": "OH"}}
            ],
            "filters": [],
            "return_vars": ["?contact"],
            "limit": 50
        })
    }

    #[test]
    fn three_tool_flow_round_trip() {
        let (state, ctx, gctx, g) = fixture();
        let built = handle_build_query(
            &state,
            &ctx,
            &gctx,
            BuildQueryArgs { pattern: ohio_value(&g.to_string()), context: None },
        )
        .unwrap();
        let id = built["plan_id"].as_str().unwrap().to_string();
        assert!(built["explanation"].as_str().unwrap().contains("3 pattern"));

        let insp = handle_inspect_query(
            &state,
            &ctx,
            &gctx,
            InspectQueryArgs { plan_id: id.clone() },
        )
        .unwrap();
        assert!(insp["compiled"].as_str().unwrap().contains(&g.to_string()));

        let r1 = handle_execute_query(
            &state,
            &ctx,
            &gctx,
            ExecuteQueryArgs { plan_id: id.clone(), limit: None },
        )
        .unwrap();
        let r2 = handle_execute_query(
            &state,
            &ctx,
            &gctx,
            ExecuteQueryArgs { plan_id: id, limit: None },
        )
        .unwrap();
        assert_eq!(r1, r2);
        let uris: Vec<String> =
            serde_json::from_value(r1["uris"].clone()).unwrap();
        assert!(uris.contains(&format!("{EX}Alice")));
        assert_eq!(r1["truncated"], false);
    }
}
