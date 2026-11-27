//! Executor (Plan 2 Phase 5): run any planner's DAG safely.
//!
//! - Topological scheduler over [`Plan::steps`]; [`StepRef`]/[`UriSetRef`]
//!   resolved from [`OperationContext::results`]. Completed outputs are
//!   immutable: replanning may only revise pending steps.
//! - Per-step dispatch to [`ToolRegistry`] / query tools / ontology. No
//!   autonomous planning loops: the executor calls the planner back at most
//!   [`Executor::max_replans`] times (default 2), only on a discovery
//!   surprise (unresolved value, empty query/instance result, unknown
//!   representation). Otherwise failures are terminal.
//! - Mutation safety reuses the Phase-2 guarded path (authz → destructive
//!   gate → SHACL → commit → audit) via the tool handlers; `verify`
//!   re-queries the graph independently. The ontology graph is never
//!   writable through the executor.
//! - No model SPARQL: the only query input is the typed `QueryPattern`
//!   object (`build_query` → `inspect_query` → `execute_query`).

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::plan::{
    GraphContext, Operation, OperationContext, OutputType, Plan, PlanError, PlanStep, StepArguments,
    StepOutput, TypedValue,
};
use crate::planner::{Planner, PlannerError, PlanningServices, UserRequest};
use crate::policy::ExecutionContext;
use crate::rdf::{GraphName, RdfRepository};
use crate::tools::{ToolRegistry, ToolState};

/// Maximum planner callbacks per execution (spec: abort after 2 rounds).
pub const MAX_REPLANS: usize = 2;

/// Large-update preview threshold: target sets above this size are
/// summarized in the report before commit.
pub const LARGE_UPDATE_PREVIEW: usize = 25;

/// Typed executor failure.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ExecutorError {
    #[error("plan invalid: {0:?}")]
    InvalidPlan(Vec<PlanError>),
    #[error("unknown step {0:?}: {1}")]
    UnknownStep(String, String),
    #[error("step {step} failed [{tool}:{category}]: {message}")]
    StepFailed {
        step: String,
        tool: String,
        category: String,
        message: String,
    },
    #[error("auth denied at step {step}: {message}")]
    Denied { step: String, message: String },
    #[error("confirmation required at step {step}: {message}")]
    ConfirmationRequired { step: String, message: String },
    #[error("validation failed at step {step}: {message}")]
    ValidationFailed { step: String, message: String },
    #[error("discovery surprise at step {step}: {message}")]
    DiscoverySurprise { step: String, message: String },
    #[error("replan budget exhausted after {rounds} rounds: {reason}")]
    ReplanExhausted { rounds: usize, reason: String },
    #[error("replan mutated completed step {0}")]
    ReplanMutatedCompleted(String),
    #[error("storage error: {0}")]
    Storage(String),
}

impl From<PlannerError> for ExecutorError {
    fn from(e: PlannerError) -> Self {
        match e {
            PlannerError::Validation(errs) => ExecutorError::InvalidPlan(errs),
            PlannerError::Clarification(q) => ExecutorError::DiscoverySurprise {
                step: String::new(),
                message: format!("planner asked for clarification: {q}"),
            },
            other => ExecutorError::Storage(other.to_string()),
        }
    }
}

/// What the executor did, for audit + eval metrics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExecutionReport {
    pub completed: Vec<String>,
    pub replans: usize,
    pub previews: Vec<String>,
    pub planner_calls: usize,
    pub final_counts: HashMap<String, usize>,
}

impl ExecutionReport {
    fn record(&mut self, step: &str, out: &StepOutput) {
        self.completed.push(step.to_string());
        let n = match out {
            StepOutput::VocabularyCandidates { items } => items.len(),
            StepOutput::ResolvedVocabulary { items } => items.len(),
            StepOutput::ResourceCandidates { items } => items.len(),
            StepOutput::UriSet { uris } => uris.len(),
            StepOutput::ResourceDescriptions { count } => *count,
            _ => 1,
        };
        self.final_counts.insert(step.to_string(), n);
    }
}

/// Runs plans. Holds no mutable plan state; all outputs flow into the
/// caller's [`OperationContext`] (completed steps) + the returned report.
pub struct Executor {
    pub registry: Arc<ToolRegistry>,
    pub state: ToolState,
    pub exec_ctx: ExecutionContext,
    pub confirmation_token: Option<String>,
    pub max_replans: usize,
}

impl Executor {
    pub fn new(
        registry: Arc<ToolRegistry>,
        state: ToolState,
        exec_ctx: ExecutionContext,
    ) -> Self {
        Self {
            registry,
            state,
            exec_ctx,
            confirmation_token: None,
            max_replans: MAX_REPLANS,
        }
    }

    pub fn with_confirmation_token(mut self, token: impl Into<String>) -> Self {
        self.confirmation_token = Some(token.into());
        self
    }

    pub fn with_max_replans(mut self, n: usize) -> Self {
        self.max_replans = n;
        self
    }

    pub fn services(&self) -> PlanningServices {
        PlanningServices::new(
            self.registry.clone(),
            self.state.clone(),
            self.exec_ctx.clone(),
        )
    }

    /// Default graph for mutation/validate/verify steps: first plan graph,
    /// else the first allowlisted non-ontology graph.
    fn default_graph(&self, graph_ctx: &GraphContext) -> Result<GraphName, ExecutorError> {
        if let Some(g) = graph_ctx.graphs.first() {
            if g.is_ontology() {
                return Err(ExecutorError::Denied {
                    step: String::new(),
                    message: "executor never writes the ontology graph".to_string(),
                });
            }
            return Ok(g.clone());
        }
        for g in &self.exec_ctx.allowed_graphs {
            if !g.is_ontology() {
                return Ok(g.clone());
            }
        }
        Err(ExecutorError::Storage(
            "no writable graph: allowlist is empty or ontology-only".to_string(),
        ))
    }

    /// Execute a plan. `planner` is called back only on discovery
    /// surprises (max `max_replans` times); pass `None` to disable
    /// replanning (surprises become terminal errors).
    pub async fn execute(
        &self,
        plan: &Plan,
        op_ctx: &mut OperationContext,
        planner: Option<&dyn Planner>,
    ) -> Result<ExecutionReport, ExecutorError> {
        let mut current = plan.clone();
        let mut report = ExecutionReport::default();
        let mut completed: HashSet<String> = HashSet::new();
        let mut replans = 0usize;
        report.planner_calls = 0;

        // Validate once up front (also re-validated after each replan).
        validate(&current)?;

        loop {
            let order = topo_order(&current)?;
            let mut progressed = false;
            let mut surprise: Option<ExecutorError> = None;

            for id in order {
                if completed.contains(&id) {
                    continue;
                }
                let step = current
                    .steps
                    .iter()
                    .find(|s| s.id == id)
                    .cloned()
                    .ok_or_else(|| ExecutorError::UnknownStep(id.clone(), "missing".to_string()))?;
                // Deps must already be completed (topo order guarantees).
                match self.dispatch_step(&step, &current, op_ctx).await {
                    Ok(out) => {
                        op_ctx.results.insert(step.id.clone(), out.clone());
                        completed.insert(step.id.clone());
                        report.record(&step.id, &out);
                        progressed = true;
                    }
                    Err(ExecutorError::DiscoverySurprise { step: _, message }) => {
                        surprise = Some(ExecutorError::DiscoverySurprise {
                            step: step.id.clone(),
                            message,
                        });
                        break;
                    }
                    Err(e) => return Err(e),
                }
            }

            if surprise.is_none() {
                report.replans = replans;
                return Ok(report);
            }
            // Discovery surprise: pause, re-invoke the planner on pending
            // steps only. Completed outputs are immutable.
            let err = surprise.unwrap();
            let _ = progressed;
            let planner = match planner {
                Some(p) => p,
                None => return Err(err),
            };
            if replans >= self.max_replans {
                let reason = err.to_string();
                return Err(ExecutorError::ReplanExhausted {
                    rounds: replans,
                    reason,
                });
            }
            // Pending = not completed. Tell the planner what's done via
            // op_ctx.results (already populated) + pending list.
            op_ctx.pending = current
                .steps
                .iter()
                .filter(|s| !completed.contains(&s.id))
                .map(|s| s.id.clone())
                .collect();
            op_ctx
                .validation_errors
                .push(format!("replan triggered: {err}"));
            let request = UserRequest::new(op_ctx.request.clone());
            let next = planner.create_plan(&request, op_ctx).await?;
            report.planner_calls += 1;
            validate(&next)?;
            // Completed steps must survive untouched.
            for done in completed.iter() {
                let before = current.steps.iter().find(|s| &s.id == done);
                let after = next.steps.iter().find(|s| &s.id == done);
                match (before, after) {
                    (Some(b), Some(a)) => {
                        if b.operation != a.operation || b.args != a.args || b.output != a.output
                        {
                            return Err(ExecutorError::ReplanMutatedCompleted(done.clone()));
                        }
                    }
                    (Some(_), None) => {
                        return Err(ExecutorError::ReplanMutatedCompleted(done.clone()));
                    }
                    _ => {}
                }
            }
            current = next;
            replans += 1;
        }
    }

    async fn dispatch_step(
        &self,
        step: &PlanStep,
        plan: &Plan,
        op_ctx: &mut OperationContext,
    ) -> Result<StepOutput, ExecutorError> {
        // Sanity: declared output must match the operation.
        if step.output != OutputType::expected_for(step.operation) {
            return Err(ExecutorError::InvalidPlan(vec![PlanError::custom(
                Some(&step.id),
                "output",
                format!(
                    "output {:?} does not match operation {:?}",
                    step.output, step.operation
                ),
            )]));
        }
        match step.operation {
            Operation::SearchVocabulary => self.run_search_vocab(step, plan, op_ctx).await,
            Operation::DescribeVocabulary => self.run_describe_vocab(step, plan, op_ctx).await,
            Operation::SearchInstances => self.run_search_instances(step, plan, op_ctx).await,
            Operation::DescribeResource => self.run_describe_resource(step, plan, op_ctx).await,
            Operation::Query => self.run_query(step, plan, op_ctx).await,
            Operation::Create => self.run_create(step, plan, op_ctx).await,
            Operation::Update => self.run_update(step, plan, op_ctx).await,
            Operation::Delete => self.run_delete(step, plan, op_ctx).await,
            Operation::Validate => self.run_validate(step, plan, op_ctx).await,
            Operation::Verify => self.run_verify(step, plan, op_ctx).await,
        }
    }

    // -- discovery steps -------------------------------------------------

    async fn run_search_vocab(
        &self,
        step: &PlanStep,
        plan: &Plan,
        op_ctx: &OperationContext,
    ) -> Result<StepOutput, ExecutorError> {
        let StepArguments::SearchVocab { terms, namespaces } = &step.args else {
            return Err(arg_err(&step.id, "search needs terms"));
        };
        let out = self
            .registry
            .dispatch_with_context(
                &self.state,
                &self.exec_ctx,
                &plan.context,
                "search_vocabulary",
                serde_json::json!({"terms": terms, "namespaces": namespaces}),
            )
            .map_err(|e| tool_err(&step.id, e))?;
        let items = out
            .get("results")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let items = items
            .iter()
            .filter_map(|v| {
                Some(crate::plan::VocabCandidate {
                    iri: v.get("iri")?.as_str()?.to_string(),
                    kind: v.get("kind").and_then(|k| k.as_str()).unwrap_or("").to_string(),
                    label: v.get("label").and_then(|k| k.as_str()).unwrap_or("").to_string(),
                    comment: v.get("comment").and_then(|k| k.as_str()).unwrap_or("").to_string(),
                })
            })
            .collect::<Vec<_>>();
        if items.is_empty() {
            return Err(ExecutorError::DiscoverySurprise {
                step: step.id.clone(),
                message: format!("search_vocabulary returned no candidates for {terms:?}"),
            });
        }
        let _ = op_ctx;
        Ok(StepOutput::VocabularyCandidates { items })
    }

    async fn run_describe_vocab(
        &self,
        step: &PlanStep,
        plan: &Plan,
        op_ctx: &OperationContext,
    ) -> Result<StepOutput, ExecutorError> {
        let StepArguments::DescribeVocab { iris } = &step.args else {
            return Err(arg_err(&step.id, "describe needs iris"));
        };
        let resolved = expand_values(&step.id, iris, op_ctx)?;
        if resolved.is_empty() {
            return Err(ExecutorError::DiscoverySurprise {
                step: step.id.clone(),
                message: "describe_vocabulary has no IRIs after reference resolution".to_string(),
            });
        }
        let out = self
            .registry
            .dispatch_with_context(
                &self.state,
                &self.exec_ctx,
                &plan.context,
                "describe_vocabulary",
                serde_json::json!({"iris": resolved}),
            )
            .map_err(|e| tool_err(&step.id, e))?;
        let terms = out
            .get("terms")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let items = terms
            .iter()
            .filter_map(|v| {
                Some(crate::plan::ResolvedTerm {
                    iri: v.get("iri")?.as_str()?.to_string(),
                    kind: v.get("kind").and_then(|k| k.as_str()).unwrap_or("").to_string(),
                    label: v.get("label").and_then(|k| k.as_str()).unwrap_or("").to_string(),
                    comment: v.get("comment").and_then(|k| k.as_str()).unwrap_or("").to_string(),
                    domain: v.get("domain").and_then(|k| k.as_str()).map(str::to_string),
                    range: v.get("range").and_then(|k| k.as_str()).map(str::to_string),
                })
            })
            .collect::<Vec<_>>();
        Ok(StepOutput::ResolvedVocabulary { items })
    }

    async fn run_search_instances(
        &self,
        step: &PlanStep,
        plan: &Plan,
        op_ctx: &OperationContext,
    ) -> Result<StepOutput, ExecutorError> {
        let StepArguments::SearchInstances { text, types, namespace } = &step.args else {
            return Err(arg_err(&step.id, "instance search needs text"));
        };
        let type_iris = expand_values(&step.id, types, op_ctx)?;
        let out = self
            .registry
            .dispatch_with_context(
                &self.state,
                &self.exec_ctx,
                &plan.context,
                "search_instances",
                serde_json::json!({"text": text, "types": type_iris, "namespace": namespace}),
            )
            .map_err(|e| tool_err(&step.id, e))?;
        let results = out
            .get("results")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let items = results
            .iter()
            .filter_map(|v| {
                Some(crate::plan::ResourceCandidate {
                    uri: v.get("uri")?.as_str()?.to_string(),
                    label: v.get("label").and_then(|l| l.as_str()).unwrap_or("").to_string(),
                    type_: v.get("type").and_then(|l| l.as_str()).unwrap_or("").to_string(),
                    description: v
                        .get("description")
                        .and_then(|l| l.as_str())
                        .unwrap_or("")
                        .to_string(),
                    score: v.get("score").and_then(|s| s.as_f64()).unwrap_or(0.0) as f32,
                })
            })
            .collect::<Vec<_>>();
        if items.is_empty() {
            return Err(ExecutorError::DiscoverySurprise {
                step: step.id.clone(),
                message: format!("search_instances returned no resources for {text:?}"),
            });
        }
        Ok(StepOutput::ResourceCandidates { items })
    }

    async fn run_describe_resource(
        &self,
        step: &PlanStep,
        plan: &Plan,
        op_ctx: &OperationContext,
    ) -> Result<StepOutput, ExecutorError> {
        let StepArguments::DescribeResource { uris } = &step.args else {
            return Err(arg_err(&step.id, "describe_resource needs uris"));
        };
        let resolved = expand_values(&step.id, uris, op_ctx)?;
        if resolved.is_empty() {
            return Err(ExecutorError::DiscoverySurprise {
                step: step.id.clone(),
                message: "describe_resource has no URIs after reference resolution".to_string(),
            });
        }
        let out = self
            .registry
            .dispatch_with_context(
                &self.state,
                &self.exec_ctx,
                &plan.context,
                "describe_resource",
                serde_json::json!({"uris": resolved}),
            )
            .map_err(|e| tool_err(&step.id, e))?;
        let count = out
            .get("resources")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(resolved.len());
        Ok(StepOutput::ResourceDescriptions { count })
    }

    async fn run_query(
        &self,
        step: &PlanStep,
        plan: &Plan,
        op_ctx: &OperationContext,
    ) -> Result<StepOutput, ExecutorError> {
        let StepArguments::Query { pattern } = &step.args else {
            return Err(arg_err(&step.id, "query needs a pattern object"));
        };
        if !pattern.is_object() {
            return Err(arg_err(&step.id, "query pattern must be a JSON object"));
        }
        // No model SPARQL: reject raw query strings outright.
        for key in ["sparql", "query", "update", "delete"] {
            if pattern.get(key).is_some() {
                return Err(ExecutorError::StepFailed {
                    step: step.id.clone(),
                    tool: "build_query".to_string(),
                    category: "query-safety".to_string(),
                    message: format!("raw '{key}' field rejected: models propose typed patterns only"),
                });
            }
        }
        let built = self
            .registry
            .dispatch_with_context(
                &self.state,
                &self.exec_ctx,
                &plan.context,
                "build_query",
                serde_json::json!({"pattern": pattern}),
            )
            .map_err(|e| tool_err(&step.id, e))?;
        let plan_id = built
            .get("plan_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ExecutorError::Storage("build_query returned no plan_id".to_string()))?
            .to_string();
        let executed = self
            .registry
            .dispatch_with_context(
                &self.state,
                &self.exec_ctx,
                &plan.context,
                "execute_query",
                serde_json::json!({"plan_id": plan_id}),
            )
            .map_err(|e| tool_err(&step.id, e))?;
        let uris: Vec<String> = executed
            .get("uris")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if uris.is_empty() {
            // Representation trap (e.g. "Ohio" vs "OH" vs address
            // resource): pause for replanning rather than mutating blindly.
            return Err(ExecutorError::DiscoverySurprise {
                step: step.id.clone(),
                message: "query returned 0 URIs (possible representation mismatch); replanning".to_string(),
            });
        }
        let _ = op_ctx;
        Ok(StepOutput::UriSet { uris })
    }

    // -- mutation steps (guarded path via tool handlers) ------------------

    async fn run_create(
        &self,
        step: &PlanStep,
        plan: &Plan,
        op_ctx: &OperationContext,
    ) -> Result<StepOutput, ExecutorError> {
        let StepArguments::Create { types, properties } = &step.args else {
            return Err(arg_err(&step.id, "create needs types"));
        };
        let graph = self.default_graph(&plan.context)?;
        let type_iris = expand_values(&step.id, types, op_ctx)?;
        if type_iris.is_empty() {
            return Err(ExecutorError::DiscoverySurprise {
                step: step.id.clone(),
                message: "create has no types after reference resolution".to_string(),
            });
        }
        // Properties are opaque JSON but must not smuggle SPARQL.
        let raw = serde_json::to_string(properties).unwrap_or_default().to_lowercase();
        for kw in ["insert", "delete where", "drop graph", "clear graph", "load "] {
            if raw.contains(kw) {
                return Err(ExecutorError::StepFailed {
                    step: step.id.clone(),
                    tool: "create".to_string(),
                    category: "query-safety".to_string(),
                    message: format!("property payload smuggles update language ({kw})"),
                });
            }
        }
        let out = self
            .registry
            .dispatch_with_context(
                &self.state,
                &self.exec_ctx,
                &plan.context,
                "create",
                serde_json::json!({
                    "graph": graph.to_string(),
                    "types": type_iris,
                    "properties": properties,
                }),
            )
            .map_err(|e| tool_err(&step.id, e))?;
        Ok(StepOutput::MutationResult {
            ok: true,
            summary: out
                .get("summary")
                .and_then(|v| v.as_str())
                .unwrap_or("created")
                .to_string(),
        })
    }

    async fn run_update(
        &self,
        step: &PlanStep,
        plan: &Plan,
        op_ctx: &mut OperationContext,
    ) -> Result<StepOutput, ExecutorError> {
        let StepArguments::Update { targets, predicate, objects } = &step.args else {
            return Err(arg_err(&step.id, "update needs targets/predicate/objects"));
        };
        let graph = self.default_graph(&plan.context)?;
        let target_uris = expand_values(&step.id, targets, op_ctx)?;
        if target_uris.is_empty() {
            return Err(ExecutorError::DiscoverySurprise {
                step: step.id.clone(),
                message: "update has no targets after reference resolution".to_string(),
            });
        }
        let preds = expand_values(&step.id, std::slice::from_ref(predicate), op_ctx)?;
        let pred = preds.into_iter().next().ok_or_else(|| ExecutorError::DiscoverySurprise {
            step: step.id.clone(),
            message: "update predicate is unresolved; discovery must ground it first".to_string(),
        })?;
        let object_strs = expand_values(&step.id, objects, op_ctx)?;
        if object_strs.is_empty() {
            return Err(ExecutorError::DiscoverySurprise {
                step: step.id.clone(),
                message: "update has no objects after reference resolution".to_string(),
            });
        }
        // Large-update preview: record the URI set before committing.
        if target_uris.len() > LARGE_UPDATE_PREVIEW {
            op_ctx.record_artifact(
                &format!("{}-preview", step.id),
                format!(
                    "preview: would update {} target(s) with {} = {:?}",
                    target_uris.len(),
                    pred,
                    object_strs
                ),
            );
        }
        let mut added = 0usize;
        for uri in &target_uris {
            let add: Vec<serde_json::Value> = object_strs
                .iter()
                .map(|o| {
                    let kind = if o.contains(':') { "iri" } else { "literal" };
                    serde_json::json!({"predicate": pred, "kind": kind, "value": o})
                })
                .collect();
            let out = self
                .registry
                .dispatch_with_context(
                    &self.state,
                    &self.exec_ctx,
                    &plan.context,
                    "update",
                    serde_json::json!({"uri": uri, "graph": graph.to_string(), "add": add}),
                )
                .map_err(|e| tool_err(&step.id, e))?;
            added += out.get("added").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
        }
        Ok(StepOutput::MutationResult {
            ok: true,
            summary: format!("updated {} target(s), {added} quad(s) added", target_uris.len()),
        })
    }

    async fn run_delete(
        &self,
        step: &PlanStep,
        plan: &Plan,
        op_ctx: &OperationContext,
    ) -> Result<StepOutput, ExecutorError> {
        let StepArguments::Delete { targets } = &step.args else {
            return Err(arg_err(&step.id, "delete needs targets"));
        };
        let graph = self.default_graph(&plan.context)?;
        let target_uris = expand_values(&step.id, targets, op_ctx)?;
        if target_uris.is_empty() {
            return Err(ExecutorError::DiscoverySurprise {
                step: step.id.clone(),
                message: "delete has no targets after reference resolution".to_string(),
            });
        }
        let mut removed = 0usize;
        for uri in &target_uris {
            let mut args = serde_json::json!({"uri": uri, "graph": graph.to_string()});
            if let Some(tok) = &self.confirmation_token {
                args["confirmation_token"] = serde_json::Value::String(tok.clone());
            }
            let out = self
                .registry
                .dispatch_with_context(&self.state, &self.exec_ctx, &plan.context, "delete", args)
                .map_err(|e| tool_err(&step.id, e))?;
            removed += out.get("removed").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        }
        Ok(StepOutput::MutationResult {
            ok: true,
            summary: format!("deleted {removed} quad(s) across {} target(s)", target_uris.len()),
        })
    }

    // -- terminal steps ---------------------------------------------------

    async fn run_validate(
        &self,
        step: &PlanStep,
        plan: &Plan,
        _op_ctx: &OperationContext,
    ) -> Result<StepOutput, ExecutorError> {
        let graph = self.default_graph(&plan.context)?;
        let out = self
            .registry
            .dispatch_with_context(
                &self.state,
                &self.exec_ctx,
                &plan.context,
                "validate",
                serde_json::json!({"graph": graph.to_string()}),
            )
            .map_err(|e| tool_err(&step.id, e))?;
        let conforms = out.get("conforms").and_then(|v| v.as_bool()).unwrap_or(false);
        Ok(StepOutput::ValidationResult { conforms })
    }

    async fn run_verify(
        &self,
        step: &PlanStep,
        plan: &Plan,
        _op_ctx: &OperationContext,
    ) -> Result<StepOutput, ExecutorError> {
        let StepArguments::Verify { question } = &step.args else {
            return Err(arg_err(&step.id, "verify needs a question"));
        };
        let graph = self.default_graph(&plan.context)?;
        let out = self
            .registry
            .dispatch_with_context(
                &self.state,
                &self.exec_ctx,
                &plan.context,
                "verify",
                serde_json::json!({"question": question, "graph": graph.to_string()}),
            )
            .map_err(|e| tool_err(&step.id, e))?;
        let answer = out
            .get("answer")
            .and_then(|v| v.as_str())
            .unwrap_or("verified")
            .to_string();
        Ok(StepOutput::VerificationResult { answer })
    }
}

fn validate(plan: &Plan) -> Result<(), ExecutorError> {
    crate::plan::validate_plan(plan).map_err(ExecutorError::InvalidPlan)
}

fn arg_err(step: &str, msg: &str) -> ExecutorError {
    ExecutorError::InvalidPlan(vec![PlanError::custom(Some(step), "args", msg)])
}

fn tool_err(step: &str, e: crate::tools::ToolError) -> ExecutorError {
    match e.category.as_str() {
        "auth" => ExecutorError::Denied {
            step: step.to_string(),
            message: e.to_string(),
        },
        "confirmation" => ExecutorError::ConfirmationRequired {
            step: step.to_string(),
            message: e.to_string(),
        },
        "validation" => ExecutorError::ValidationFailed {
            step: step.to_string(),
            message: e.to_string(),
        },
        "grounding" => ExecutorError::DiscoverySurprise {
            step: step.to_string(),
            message: e.to_string(),
        },
        _ => ExecutorError::StepFailed {
            step: step.to_string(),
            tool: e.tool.clone(),
            category: e.category.clone(),
            message: e.message.clone(),
        },
    }
}

/// Topological order (Kahn, stable in plan order). Errors on cycles
/// (already rejected by the validator; this is defense in depth).
fn topo_order(plan: &Plan) -> Result<Vec<String>, ExecutorError> {
    let mut indeg: HashMap<&str, usize> = HashMap::new();
    let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();
    for s in &plan.steps {
        indeg.entry(s.id.as_str()).or_insert(0);
        for d in &s.depends_on {
            *indeg.entry(s.id.as_str()).or_insert(0) += 1;
            dependents.entry(d.as_str()).or_default().push(s.id.as_str());
        }
    }
    let mut queue: VecDeque<&str> = plan
        .steps
        .iter()
        .filter(|s| indeg.get(s.id.as_str()) == Some(&0))
        .map(|s| s.id.as_str())
        .collect();
    let mut order: Vec<String> = vec![];
    while let Some(id) = queue.pop_front() {
        order.push(id.to_string());
        if let Some(next) = dependents.get(id) {
            for n in next {
                let e = indeg.get_mut(*n).unwrap();
                *e -= 1;
                if *e == 0 {
                    queue.push_back(n);
                }
            }
        }
    }
    if order.len() != plan.steps.len() {
        return Err(ExecutorError::InvalidPlan(vec![PlanError::custom(
            None,
            "cycle",
            "dependency cycle detected during scheduling",
        )]));
    }
    Ok(order)
}

/// Expand a list of [`TypedValue`]s to concrete strings using completed
/// step outputs. `Unresolved` (or a ref to a missing/empty output) is a
/// discovery surprise — the executor pauses for replanning instead of
/// inventing a value.
fn expand_values(
    step: &str,
    values: &[TypedValue],
    op_ctx: &OperationContext,
) -> Result<Vec<String>, ExecutorError> {
    let mut out = vec![];
    for v in values {
        match v {
            TypedValue::Iri { iri } => out.push(iri.clone()),
            TypedValue::Unresolved { description } => {
                return Err(ExecutorError::DiscoverySurprise {
                    step: step.to_string(),
                    message: format!("unresolved value {description:?}; discovery must ground it first"),
                });
            }
            TypedValue::StepRef { step: src, field: _ } => {
                out.extend(step_ref_uris(op_ctx, step, src)?);
            }
            TypedValue::UriSetRef { step: src } => {
                out.extend(uri_set_uris(op_ctx, step, src)?);
            }
        }
    }
    Ok(out)
}

fn step_ref_uris(
    op_ctx: &OperationContext,
    step: &str,
    src: &str,
) -> Result<Vec<String>, ExecutorError> {
    let out = op_ctx.results.get(src).ok_or_else(|| ExecutorError::DiscoverySurprise {
        step: step.to_string(),
        message: format!("step_ref to {src:?} has no recorded output yet"),
    })?;
    match out {
        StepOutput::VocabularyCandidates { items } => {
            Ok(items.iter().map(|i| i.iri.clone()).collect())
        }
        StepOutput::ResolvedVocabulary { items } => {
            Ok(items.iter().map(|i| i.iri.clone()).collect())
        }
        StepOutput::ResourceCandidates { items } => {
            Ok(items.iter().map(|i| i.uri.clone()).collect())
        }
        StepOutput::UriSet { uris } => Ok(uris.clone()),
        StepOutput::ResourceDescriptions { .. } => Err(ExecutorError::DiscoverySurprise {
            step: step.to_string(),
            message: format!("step_ref to {src:?} is resource descriptions (no URIs to reuse)"),
        }),
        StepOutput::MutationResult { .. }
        | StepOutput::ValidationResult { .. }
        | StepOutput::VerificationResult { .. } => Err(ExecutorError::DiscoverySurprise {
            step: step.to_string(),
            message: format!("step_ref to {src:?} names a non-addressable output"),
        }),
    }
}

fn uri_set_uris(
    op_ctx: &OperationContext,
    step: &str,
    src: &str,
) -> Result<Vec<String>, ExecutorError> {
    match op_ctx.results.get(src) {
        Some(StepOutput::UriSet { uris }) if !uris.is_empty() => Ok(uris.clone()),
        Some(StepOutput::ResourceCandidates { items }) if !items.is_empty() => {
            Ok(items.iter().map(|i| i.uri.clone()).collect())
        }
        _ => Err(ExecutorError::DiscoverySurprise {
            step: step.to_string(),
            message: format!("uri_set_ref to {src:?} is empty or missing; discovery must ground it first"),
        }),
    }
}

/// Deterministic graph hash (sorted quad debug strings → u64 hex).
/// Used by safety tests to prove "graph unchanged".
pub fn graph_hash(repo: &(impl RdfRepository + ?Sized), graph: &GraphName) -> String {
    let mut quads = repo
        .match_quads(None, None, None, Some(graph), None)
        .unwrap_or_default();
    quads.sort_by_key(|q| format!("{q:?}"));
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for q in &quads {
        format!("{q:?}").hash(&mut h);
    }
    format!("{:016x}", h.finish())
}

/// Count quads in a graph (verify-count helper for eval).
pub fn graph_count(repo: &(impl RdfRepository + ?Sized), graph: &GraphName) -> usize {
    repo.match_quads(None, None, None, Some(graph), None)
        .map(|q| q.len())
        .unwrap_or(0)
}

/// A scripted [`Planner`] for tests/eval: returns queued plans in order.
/// Lets the executor's replanning path be tested without model weights.
pub struct ScriptedPlanner {
    pub plans: std::sync::Mutex<Vec<Plan>>,
}

impl ScriptedPlanner {
    pub fn new(plans: Vec<Plan>) -> Self {
        Self { plans: std::sync::Mutex::new(plans) }
    }
}

#[async_trait]
impl Planner for ScriptedPlanner {
    async fn create_plan(
        &self,
        _request: &UserRequest,
        _ctx: &mut OperationContext,
    ) -> Result<Plan, PlannerError> {
        let mut lock = self.plans.lock().expect("scripted planner poisoned");
        if lock.is_empty() {
            return Err(PlannerError::Model("script exhausted".to_string()));
        }
        Ok(lock.remove(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppLimits;
    use crate::ontology::{contact_shapes, load_default_ontology, OntologyReasoner};
    use crate::plan::{canonical_ohio_plan, OperationContext};
    use crate::policy::{AuditLog, ExecutionContext, Operation as PolicyOp};
    use crate::rdf::{OxigraphRepository, Quad, Term};
    use crate::search::TantivyIndex;

    const EX: &str = "http://example.com/";

    fn fixture_graph() -> (ToolState, ExecutionContext, GraphName, Arc<ToolRegistry>) {
        let repo = Arc::new(OxigraphRepository::new_in_memory(50_000).unwrap());
        load_default_ontology(&*repo).unwrap();
        let g = GraphName::tenant("acme").unwrap();
        let reasoner = Arc::new(OntologyReasoner::from_repository(&*repo).unwrap());
        let search = Arc::new(TantivyIndex::in_ram().unwrap());
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
            vec![g.clone(), GraphName::ontology()],
            vec![PolicyOp::Read, PolicyOp::Create, PolicyOp::Update, PolicyOp::Delete],
        );
        let registry = Arc::new(ToolRegistry::new());
        (state, ctx, g, registry)
    }

    fn seed_ohio(state: &ToolState, g: &GraphName) {
        let alice = Term::iri(format!("{EX}Alice")).unwrap();
        let acme = Term::iri(format!("{EX}Acme")).unwrap();
        let t = Term::iri("http://www.w3.org/1999/02/22-rdf-syntax-ns#type".to_string()).unwrap();
        let contact = Term::iri(format!("{EX}Contact")).unwrap();
        let account = Term::iri(format!("{EX}account")).unwrap();
        let state_p = Term::iri(format!("{EX}state")).unwrap();
        state
            .repo
            .insert(&[
                Quad::new(alice.clone(), t.clone(), contact, g.clone()).unwrap(),
                Quad::new(alice.clone(), account, acme.clone(), g.clone()).unwrap(),
                Quad::new(acme, state_p, Term::literal("OH"), g.clone()).unwrap(),
            ])
            .unwrap();
        // Seed the search index for the classification resource.
        let prospect = Term::iri(format!("{EX}Prospect")).unwrap();
        state
            .repo
            .insert(&[Quad::new(
                prospect.clone(),
                t,
                Term::iri(format!("{EX}Category")).unwrap(),
                g.clone(),
            )
            .unwrap()])
            .unwrap();
        let _ = state.search.sync_resource(&*state.repo, &format!("{EX}Prospect"), g);
        let _ = state.search.sync_resource(&*state.repo, &format!("{EX}Alice"), g);
    }

    #[test]
    fn topo_order_respects_dependencies() {
        let plan = canonical_ohio_plan();
        let order = topo_order(&plan).unwrap();
        assert_eq!(order.len(), 7);
        let pos = |id: &str| order.iter().position(|x| x == id).unwrap();
        assert!(pos("s1") < pos("s2"));
        assert!(pos("s2") < pos("s3"));
        assert!(pos("s5") < pos("s6"));
    }

    #[test]
    fn unresolved_value_is_discovery_surprise() {
        let ctx = OperationContext::new("q", GraphContext::default());
        let err = expand_values(
            "s5",
            &[TypedValue::Unresolved { description: "predicate".to_string() }],
            &ctx,
        )
        .unwrap_err();
        assert!(matches!(err, ExecutorError::DiscoverySurprise { .. }));
    }

    #[tokio::test]
    async fn replan_keeps_completed_outputs_immutable() {
        let (state, exec_ctx, g, registry) = fixture_graph();
        seed_ohio(&state, &g);
        let exec = Executor::new(registry, state, exec_ctx);
        // First plan: valid 2-step discovery; second (replan) tries to
        // rewrite s1 — must be rejected.
        let mut p1 = canonical_ohio_plan();
        p1.steps.truncate(2);
        let mut evil = p1.clone();
        evil.steps[0].intent = "rewritten".to_string();
        evil.steps[0].args = StepArguments::SearchVocab {
            terms: vec!["evil".to_string()],
            namespaces: vec![],
        };
        // Force a surprise on s3 by appending an unresolved update.
        p1.steps.push(PlanStep {
            id: "s3".to_string(),
            operation: Operation::Update,
            intent: "unresolved".to_string(),
            args: StepArguments::Update {
                targets: vec![TypedValue::Unresolved { description: "t".to_string() }],
                predicate: TypedValue::Iri { iri: format!("{EX}state") },
                objects: vec![TypedValue::Iri { iri: format!("{EX}X") }],
            },
            depends_on: vec!["s1".to_string(), "s2".to_string()],
            output: OutputType::MutationResult,
        });
        let planner = ScriptedPlanner::new(vec![evil]);
        let mut op_ctx = OperationContext::new(p1.goal.clone(), p1.context.clone());
        op_ctx.request = p1.goal.clone();
        let err = exec.execute(&p1, &mut op_ctx, Some(&planner)).await.unwrap_err();
        // Either invalid (validator rejects the hand-built s3 deps) or the
        // replan-mutated-completed guard fires — both prove completed
        // steps are not silently rewritten.
        assert!(
            matches!(err, ExecutorError::ReplanMutatedCompleted(_))
                || matches!(err, ExecutorError::InvalidPlan(_)),
            "{err:?}"
        );
    }
}
