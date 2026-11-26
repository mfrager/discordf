//! Authorization (Phase 2): deny-by-default graph + operation allowlists.

use crate::ontology::shacl::Violation;
use crate::rdf::GraphName;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Operation a request wants to perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Operation {
    Read,
    Create,
    Update,
    Delete,
}

impl Operation {
    pub fn as_str(&self) -> &'static str {
        match self {
            Operation::Read => "read",
            Operation::Create => "create",
            Operation::Update => "update",
            Operation::Delete => "delete",
        }
    }
}

/// Request-scoped execution context. Every mutation handler takes one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionContext {
    pub request_id: String,
    pub principal: String,
    pub tenant: String,
    pub allowed_graphs: Vec<GraphName>,
    pub permitted_ops: Vec<Operation>,
}

impl ExecutionContext {
    pub fn new(
        request_id: impl Into<String>,
        principal: impl Into<String>,
        tenant: impl Into<String>,
        allowed_graphs: Vec<GraphName>,
        permitted_ops: Vec<Operation>,
    ) -> Self {
        Self {
            request_id: request_id.into(),
            principal: principal.into(),
            tenant: tenant.into(),
            allowed_graphs,
            permitted_ops,
        }
    }

    /// Convenience: single-tenant context allowed to use only its own graph.
    pub fn for_tenant(
        request_id: impl Into<String>,
        principal: impl Into<String>,
        tenant: &str,
        permitted_ops: Vec<Operation>,
    ) -> Self {
        let graph = GraphName::tenant(tenant).expect("tenant id valid");
        Self {
            request_id: request_id.into(),
            principal: principal.into(),
            tenant: tenant.into(),
            allowed_graphs: vec![graph, GraphName::ontology()],
            permitted_ops,
        }
    }
}

/// Policy errors. Handlers map these to structured API errors in Phase 5.
#[derive(Debug, Error)]
pub enum PolicyError {
    #[error("denied: {0}")]
    Denied(String),
    #[error("confirmation required: {0}")]
    ConfirmationRequired(String),
    #[error("validation failed with {0:?} violation(s)")]
    ValidationFailed(Vec<Violation>),
    #[error("storage error: {0}")]
    Storage(String),
}

/// Deny-by-default: the graph must be in `allowed_graphs` AND the op in
/// `permitted_ops`. Cross-tenant graphs are denied unless explicitly listed.
pub fn authorize(
    ctx: &ExecutionContext,
    graph: &GraphName,
    op: Operation,
) -> Result<(), PolicyError> {
    if !ctx.permitted_ops.contains(&op) {
        return Err(PolicyError::Denied(format!(
            "principal {} may not perform {} (request {})",
            ctx.principal,
            op.as_str(),
            ctx.request_id
        )));
    }
    if !ctx.allowed_graphs.contains(graph) {
        return Err(PolicyError::Denied(format!(
            "graph {graph} not in allowlist for principal {} (request {})",
            ctx.principal, ctx.request_id
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ExecutionContext {
        ExecutionContext::new(
            "req-1",
            "alice",
            "acme",
            vec![GraphName::tenant("acme").unwrap()],
            vec![Operation::Read, Operation::Create],
        )
    }

    #[test]
    fn allowlisted_graph_and_op_passes() {
        let c = ctx();
        assert!(authorize(&c, &GraphName::tenant("acme").unwrap(), Operation::Create).is_ok());
    }

    #[test]
    fn cross_tenant_denied() {
        let c = ctx();
        assert!(authorize(&c, &GraphName::tenant("other").unwrap(), Operation::Create).is_err());
    }

    #[test]
    fn disallowed_op_denied() {
        let c = ctx();
        assert!(authorize(&c, &GraphName::tenant("acme").unwrap(), Operation::Delete).is_err());
    }
}
