//! Five tools + registry (Phase 4): Needle proposes, Rust disposes.
//!
//! Tools depend on the `RdfRepository` trait (never Oxigraph directly) and
//! on the `SearchService` trait. Mutations go through the Phase 2 guarded
//! path (authz → destructive gate → SHACL → commit → audit) and refresh
//! the Phase 3 index after every commit.

pub mod create;
pub mod delete;
pub mod instances;
pub mod query_tools;
pub mod read;
pub mod search;
pub mod update;
pub mod validate_tool;
pub mod verify;
pub mod vocabulary;

pub use create::CreateArgs;
pub use delete::DeleteArgs;
pub use instances::{DescribeResourceArgs, SearchInstancesArgs};
pub use query_tools::{BuildQueryArgs, ExecuteQueryArgs, InspectQueryArgs};
pub use read::ReadArgs;
pub use search::SearchArgs;
pub use update::UpdateArgs;
pub use validate_tool::ValidateArgs;
pub use verify::VerifyArgs;
pub use vocabulary::{DescribeVocabularyArgs, SearchVocabularyArgs};

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

use crate::config::AppLimits;
use crate::ontology::{OntologyReasoner, Shape};
use crate::policy::{AuditLog, ExecutionContext};
use crate::rdf::RdfRepository;
use crate::search::SearchService;

/// Structured tool error: `{ tool, category, message }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolError {
    pub tool: String,
    pub category: String,
    pub message: String,
}

impl ToolError {
    pub fn new(tool: &str, category: &str, message: impl Into<String>) -> Self {
        Self {
            tool: tool.to_string(),
            category: category.to_string(),
            message: message.into(),
        }
    }
    pub fn args(tool: &str, message: impl Into<String>) -> Self {
        Self::new(tool, "args", message)
    }
    pub fn unknown(tool: &str) -> Self {
        Self::new(tool, "unknown_tool", format!("unknown tool: {tool}"))
    }
    pub fn grounding(tool: &str, message: impl Into<String>) -> Self {
        Self::new(tool, "grounding", message)
    }
    pub fn auth(tool: &str, message: impl Into<String>) -> Self {
        Self::new(tool, "auth", message)
    }
    pub fn validation(tool: &str, message: impl Into<String>) -> Self {
        Self::new(tool, "validation", message)
    }
    pub fn storage(tool: &str, message: impl Into<String>) -> Self {
        Self::new(tool, "storage", message)
    }
    pub fn confirmation(tool: &str, message: impl Into<String>) -> Self {
        Self::new(tool, "confirmation", message)
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}:{}] {}", self.tool, self.category, self.message)
    }
}

impl std::error::Error for ToolError {}

/// Shared state for all tool handlers.
#[derive(Clone)]
pub struct ToolState {
    pub repo: Arc<dyn RdfRepository>,
    pub search: Arc<dyn SearchService>,
    pub reasoner: Arc<OntologyReasoner>,
    pub shapes: Vec<Shape>,
    pub audit: Arc<AuditLog>,
    pub limits: AppLimits,
    pub query_plans: Arc<crate::query::QueryPlanStore>,
}

impl ToolState {
    pub fn new(
        repo: Arc<dyn RdfRepository>,
        search: Arc<dyn SearchService>,
        reasoner: Arc<OntologyReasoner>,
        shapes: Vec<Shape>,
        audit: Arc<AuditLog>,
        limits: AppLimits,
    ) -> Self {
        Self {
            repo,
            search,
            reasoner,
            shapes,
            audit,
            limits,
            query_plans: Arc::new(crate::query::QueryPlanStore::new()),
        }
    }
}

/// Tool definition: name + concise description + strict JSON schema.
/// Descriptions state when-to-use + args + returns (prompt-lean, no
/// ontology dumps). Each must stay under `DESCRIPTION_BUDGET` chars.
#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: serde_json::Value,
}

/// Max description length (prompt budget contract, tested).
pub const DESCRIPTION_BUDGET: usize = 600;

pub fn tool_defs() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "search",
            description: "Use when you need to find a resource by words. Args: query (string), context (optional graph), namespace (optional URI prefix), limit (optional int). Returns: ranked URIs with label, type, description, score.",
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "context": { "type": "string" },
                    "namespace": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100 }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "read",
            description: "Use when you have URIs and need their properties. Args: uris (array of strings), context (optional graph), predicates (optional allowlist), depth (optional 0-3). Returns: resources with literal-vs-IRI properties and follow-up URIs.",
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "uris": { "type": "array", "items": { "type": "string" } },
                    "context": { "type": "string" },
                    "predicates": { "type": "array", "items": { "type": "string" } },
                    "depth": { "type": "integer", "minimum": 0, "maximum": 3 }
                },
                "required": ["uris"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "create",
            description: "Use when you must add a new resource. Args: uri (optional string), graph (string), types (array of class URIs), properties (array of {predicate, value}). Returns: committed delta with added triples.",
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "uri": { "type": "string" },
                    "graph": { "type": "string" },
                    "types": { "type": "array", "items": { "type": "string" } },
                    "properties": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "predicate": { "type": "string" },
                                "kind": { "type": "string", "enum": ["iri", "literal", "typed", "lang", "bnode"] },
                                "value": { "type": "string" },
                                "datatype": { "type": "string" },
                                "lang": { "type": "string" }
                            },
                            "required": ["predicate", "kind", "value"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["graph", "types", "properties"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "update",
            description: "Use when you must change an existing resource. Args: uri (string), graph (string), add (array of properties), remove (array of properties). Returns: committed delta with added and removed triples.",
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "uri": { "type": "string" },
                    "graph": { "type": "string" },
                    "add": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "predicate": { "type": "string" },
                                "kind": { "type": "string", "enum": ["iri", "literal", "typed", "lang", "bnode"] },
                                "value": { "type": "string" },
                                "datatype": { "type": "string" },
                                "lang": { "type": "string" }
                            },
                            "required": ["predicate", "kind", "value"],
                            "additionalProperties": false
                        }
                    },
                    "remove": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "predicate": { "type": "string" },
                                "kind": { "type": "string", "enum": ["iri", "literal", "typed", "lang", "bnode"] },
                                "value": { "type": "string" },
                                "datatype": { "type": "string" },
                                "lang": { "type": "string" }
                            },
                            "required": ["predicate", "kind", "value"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["uri", "graph"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "search_vocabulary",
            description: "Use when you need OWL classes or properties from the ontology. Args: query or terms (strings), namespaces (optional), limit. Returns: VocabularyCandidates with iri, kind, label, domain, range.",
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "terms": { "type": "array", "items": { "type": "string" } },
                    "namespaces": { "type": "array", "items": { "type": "string" } },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100 }
                },
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "describe_vocabulary",
            description: "Use when you have class or property IRIs and need definitions. Args: iris (array of strings). Returns: ResolvedVocabulary with kind, domain, range, superclasses.",
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "iris": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["iris"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "search_instances",
            description: "Use when you need real resources by words. Args: text (string), types (optional), namespace (optional), context (optional graph), limit. Returns: ResourceCandidates with uri, label, type, description.",
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string" },
                    "types": { "type": "array", "items": { "type": "string" } },
                    "namespace": { "type": "string" },
                    "context": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100 }
                },
                "required": ["text"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "describe_resource",
            description: "Use when you have instance URIs and need their types and properties. Args: uris (array), context (optional graph). Returns: ResourceDescriptions with types and properties.",
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "uris": { "type": "array", "items": { "type": "string" } },
                    "context": { "type": "string" }
                },
                "required": ["uris"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "validate",
            description: "Use when you must check SHACL constraints before or after a mutation. Args: graph (string), uri (optional). Returns: ValidationResult with conforms and violations.",
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "graph": { "type": "string" },
                    "uri": { "type": "string" }
                },
                "required": ["graph"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "build_query",
            description: "Use when you need to select resources by typed graph patterns. Args: pattern (typed object with graphs, patterns, filters, return_vars, limit), context (optional graph). Returns: plan_id with compiled form, explanation, representations.",
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "object" },
                    "context": { "type": "string" }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "inspect_query",
            description: "Use when you have a plan_id and need its compiled form and match estimate. Args: plan_id (string). Returns: compiled form, explanation, representations, match_estimate.",
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "plan_id": { "type": "string" }
                },
                "required": ["plan_id"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "execute_query",
            description: "Use when a built plan is validated and you need its matching URIs. Args: plan_id (string), limit (optional int). Returns: uris, rows, count, truncated.",
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "plan_id": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100 }
                },
                "required": ["plan_id"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "verify",
            description: "Use when you must independently confirm an outcome by re-querying. Args: question (string), graph (string), uri (optional). Returns: VerificationResult with quad_count and answer.",
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "question": { "type": "string" },
                    "graph": { "type": "string" },
                    "uri": { "type": "string" }
                },
                "required": ["question", "graph"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "delete",
            description: "Use when you must remove a resource or selected triples. Args: uri (optional string), triples (optional array), graph (string), confirmation_token (optional string, required for destructive ops). Returns: committed delta with removed triples or a dry-run summary.",
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "uri": { "type": "string" },
                    "triples": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "subject": { "type": "string" },
                                "predicate": { "type": "string" },
                                "kind": { "type": "string", "enum": ["iri", "literal", "typed", "lang", "bnode"] },
                                "value": { "type": "string" }
                            },
                            "required": ["subject", "predicate", "kind", "value"],
                            "additionalProperties": false
                        }
                    },
                    "graph": { "type": "string" },
                    "confirmation_token": { "type": "string" }
                },
                "required": ["graph"],
                "additionalProperties": false
            }),
        },
    ]
}

/// Registry: name → definition. Dispatch validates args with `serde`
/// (missing/extra/wrong-typed rejected) before running the handler.
pub struct ToolRegistry {
    defs: HashMap<String, ToolDef>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        let mut defs = HashMap::new();
        for d in tool_defs() {
            defs.insert(d.name.to_string(), d);
        }
        Self { defs }
    }

    pub fn get(&self, name: &str) -> Option<&ToolDef> {
        self.defs.get(name)
    }

    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.defs.keys().cloned().collect();
        v.sort();
        v
    }

    /// Dispatch one tool call with an execution context.
    /// Legacy path: new Plan-2 tools run under `GraphContext::default()`
    /// (unconstrained), preserving Plan-1 E2E behavior.
    pub fn dispatch(
        &self,
        state: &ToolState,
        ctx: &ExecutionContext,
        name: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, ToolError> {
        self.dispatch_with_context(state, ctx, &crate::plan::GraphContext::default(), name, args)
    }

    /// Dispatch with an explicit plan `GraphContext`. All handlers take the
    /// context; namespace isolation + graph allowlist are enforced here for
    /// the Plan-2 tools (legacy tools enforce their own `context` graph arg
    /// plus `check_read_auth`).
    pub fn dispatch_with_context(
        &self,
        state: &ToolState,
        ctx: &ExecutionContext,
        graph_ctx: &crate::plan::GraphContext,
        name: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, ToolError> {
        match name {
            "search" => {
                let a: SearchArgs = parse_args(name, args)?;
                search::handle(state, ctx, a)
            }
            "read" => {
                let a: ReadArgs = parse_args(name, args)?;
                read::handle(state, ctx, a)
            }
            "create" => {
                let a: CreateArgs = parse_args(name, args)?;
                create::handle(state, ctx, a)
            }
            "update" => {
                let a: UpdateArgs = parse_args(name, args)?;
                update::handle(state, ctx, a)
            }
            "delete" => {
                let a: DeleteArgs = parse_args(name, args)?;
                delete::handle(state, ctx, a)
            }
            "search_vocabulary" => {
                let a: SearchVocabularyArgs = parse_args(name, args)?;
                vocabulary::handle_search_vocabulary(state, ctx, graph_ctx, a)
            }
            "describe_vocabulary" => {
                let a: DescribeVocabularyArgs = parse_args(name, args)?;
                vocabulary::handle_describe_vocabulary(state, ctx, graph_ctx, a)
            }
            "search_instances" => {
                let a: SearchInstancesArgs = parse_args(name, args)?;
                instances::handle_search_instances(state, ctx, graph_ctx, a)
            }
            "describe_resource" => {
                let a: DescribeResourceArgs = parse_args(name, args)?;
                instances::handle_describe_resource(state, ctx, graph_ctx, a)
            }
            "validate" => {
                let a: ValidateArgs = parse_args(name, args)?;
                validate_tool::handle_validate(state, ctx, graph_ctx, a)
            }
            "build_query" => {
                let a: BuildQueryArgs = parse_args(name, args)?;
                query_tools::handle_build_query(state, ctx, graph_ctx, a)
            }
            "inspect_query" => {
                let a: InspectQueryArgs = parse_args(name, args)?;
                query_tools::handle_inspect_query(state, ctx, graph_ctx, a)
            }
            "execute_query" => {
                let a: ExecuteQueryArgs = parse_args(name, args)?;
                query_tools::handle_execute_query(state, ctx, graph_ctx, a)
            }
            "verify" => {
                let a: VerifyArgs = parse_args(name, args)?;
                verify::handle_verify(state, ctx, graph_ctx, a)
            }
            other => Err(ToolError::unknown(other)),
        }
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Strict arg parsing: unknown fields denied, types checked by `serde`.
pub fn parse_args<T: for<'de> Deserialize<'de>>(
    tool: &str,
    args: serde_json::Value,
) -> Result<T, ToolError> {
    serde_json::from_value(args).map_err(|e| ToolError::args(tool, format!("invalid args: {e}")))
}

/// Validate an IRI string (no spaces, has a scheme or is urn:/http(s):).
pub fn valid_iri(s: &str) -> bool {
    if s.trim().is_empty() || s.contains([' ', '\n', '\t']) {
        return false;
    }
    s.contains(':')
}

/// Read-side authorization (Phase 5): a concrete graph context must be in
/// the ctx allowlist and `Read` must be permitted. A `None` graph (scan)
/// is allowed so legacy/no-context callers keep working; HTTP callers
/// should pass the tenant graph explicitly. Fully-empty contexts (no
/// graphs, no ops) are treated as unconstrained legacy test contexts.
/// Mutations are always gated by `execute_guarded` regardless.
pub fn check_read_auth(
    tool: &str,
    ctx: &ExecutionContext,
    graph: Option<&crate::rdf::GraphName>,
) -> Result<(), ToolError> {
    use crate::policy::Operation;
    if ctx.allowed_graphs.is_empty() && ctx.permitted_ops.is_empty() {
        return Ok(());
    }
    if !ctx.permitted_ops.contains(&Operation::Read) {
        return Err(ToolError::auth(
            tool,
            format!("principal {} may not perform read", ctx.principal),
        ));
    }
    if let Some(g) = graph {
        if !ctx.allowed_graphs.contains(g) {
            return Err(ToolError::auth(
                tool,
                format!("graph {g} not in allowlist for principal {}", ctx.principal),
            ));
        }
    }
    Ok(())
}

/// Grounding gate: reject invented/unsupported values before sensitive ops.
/// Read-only tools (search/read/vocabulary/instance/validate/verify) are
/// always allowed. Create/update/delete require well-formed IRIs;
/// update/delete targets must already exist (otherwise the model invented
/// a URI).
pub fn check_grounding(
    tool: &str,
    state: &ToolState,
    ctx: &ExecutionContext,
    target_uri: Option<&str>,
    graph: &crate::rdf::GraphName,
) -> Result<(), ToolError> {
    let _ = ctx;
    match tool {
        "search" | "read" | "search_vocabulary" | "describe_vocabulary" | "search_instances"
        | "describe_resource" | "validate" | "verify" | "build_query" | "inspect_query"
        | "execute_query" => Ok(()),
        _ => {
            if let Some(uri) = target_uri {
                if !valid_iri(uri) {
                    return Err(ToolError::grounding(
                        tool,
                        format!("unsupported URI value: {uri:?}"),
                    ));
                }
                if tool == "update" || tool == "delete" {
                    let subj = crate::rdf::Term::iri(uri.to_string())
                        .map_err(|e| ToolError::grounding(tool, e.to_string()))?;
                    let existing = state
                        .repo
                        .match_quads(Some(&subj), None, None, Some(graph), Some(1))
                        .map_err(|e| ToolError::storage(tool, e.to_string()))?;
                    if existing.is_empty() {
                        return Err(ToolError::grounding(
                            tool,
                            format!("invented URI (not in store): {uri}"),
                        ));
                    }
                }
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_lists_legacy_five_plus_plan2_six() {
        let r = ToolRegistry::new();
        assert_eq!(
            r.names(),
            vec![
                "build_query",
                "create",
                "delete",
                "describe_resource",
                "describe_vocabulary",
                "execute_query",
                "inspect_query",
                "read",
                "search",
                "search_instances",
                "search_vocabulary",
                "update",
                "validate",
                "verify"
            ]
        );
        // Legacy five still present (Plan-1 E2E compat).
        for legacy in ["create", "delete", "read", "search", "update"] {
            assert!(r.get(legacy).is_some(), "{legacy} missing");
        }
        assert!(r.get("nope").is_none());
    }

    #[test]
    fn descriptions_have_sections_and_budget() {
        for d in tool_defs() {
            let lower = d.description.to_lowercase();
            assert!(lower.contains("when"), "{} missing when-to-use", d.name);
            assert!(lower.contains("args"), "{} missing args", d.name);
            assert!(lower.contains("return"), "{} missing returns", d.name);
            assert!(
                d.description.len() <= DESCRIPTION_BUDGET,
                "{} exceeds budget ({} chars)",
                d.name,
                d.description.len()
            );
        }
    }
}
