//! Unified planner trait (Plan 2 Phase 4).
//!
//! Four planners build the same canonical [`Plan`] behind one interface:
//!
//! | Planner | Model | Style |
//! |---|---|---|
//! | [`NeedleSequentialPlanner`] (A) | Needle 3 via `cactus-rs` | sequential `complete()` tool loop |
//! | [`AgentToolPlanner`] (B) | generic LLM via `agentstream` | sequential `AgentStream` tool loop |
//! | [`AgentStructuredPlanner`] (C) | generic LLM via `agentstream` | single JSON structured output |
//! | [`AgentTwoPassPlanner`] (D) | generic LLM via `agentstream` | NL plan → structured extraction |
//!
//! A and B share the same planning-tool surface and the same in-memory
//! [`PlanningSession`] state machine; only the model turn differs
//! (`ModelAdapter::complete()` vs `AgentStream::send().with_tools()`).
//!
//! # Why there is no Python worker (plan §5)
//!
//! DESIGN_UPDATE_3 §2.4 sketches a small Python worker hosting Needle. It is
//! **not** needed here:
//!
//! - Needle 3 stays in-process via `cactus-rs` ([`NeedleSequentialPlanner`]
//!   drives it with `complete()`, never `run()`, so Rust inspects and
//!   validates every planning action before applying it — the application
//!   owns the loop, exactly as §2.4 requires).
//! - The generic LLM stays out-of-process behind its local OpenAI-compatible
//!   HTTP endpoint via `agentstream` ([`AgentBackend`]); Rust owns the turn
//!   loop there too (`AgentStream` turn loop for B, single/bounded calls for
//!   C/D).
//!
//! No separate worker process, no extra IPC: one Rust loop per planner.
//!
//! # Planning-time safety
//!
//! Planning tools mutate only the in-memory [`Plan`], never the repository.
//! The only repository access during planning is read-only discovery
//! (`search_vocabulary`, `describe_vocabulary`, `search_instances`,
//! `describe_resource`, `inspect_query`). Any `create` / `update` / `delete`
//! / `execute_query` / `build_query` (or legacy `search` / `read`) tool call
//! from a model is rejected with a typed error and never dispatched.

use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::llm::LlmConfig;
use crate::model::ModelAdapter;
use crate::plan::{
    GraphContext, Operation, OperationContext, OutputType, Plan, PlanError, PlanStep, StepArguments,
    TypedValue,
};
use crate::policy::ExecutionContext;
use crate::tools::{ToolDef, ToolRegistry, ToolState};

/// A user request to plan for. The authoritative request text also lives on
/// [`OperationContext::request`]; this is the planner-facing view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserRequest {
    pub text: String,
}

impl UserRequest {
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

/// Typed planner failure.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum PlannerError {
    /// Model transport / engine failure (includes "mock exhausted" in tests).
    #[error("model error: {0}")]
    Model(String),
    /// Malformed tool call or unparsable model output.
    #[error("schema error: {0}")]
    Schema(String),
    /// A plan (or single planning action) failed validation.
    #[error("plan validation failed: {0:?}")]
    Validation(Vec<PlanError>),
    /// Option D: prose plan and structured extraction disagree.
    #[error("consistency error: {0}")]
    Consistency(String),
    /// Turn / step / no-progress caps tripped (infinite-loop guard).
    #[error("planning budget exhausted: {0}")]
    Budget(String),
    /// The model asked for clarification; planning stops until answered.
    #[error("clarification needed: {0}")]
    Clarification(String),
}

/// Every planner — Needle or `agentstream`-backed — implements this.
/// The executor cannot tell which planner produced a [`Plan`].
#[async_trait]
pub trait Planner: Send + Sync {
    async fn create_plan(
        &self,
        request: &UserRequest,
        ctx: &mut OperationContext,
    ) -> Result<Plan, PlannerError>;
}

/// Caps for the interactive (A/B) planning loops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanCaps {
    /// Maximum steps in the in-memory plan (spec: 16).
    pub max_plan_steps: usize,
    /// Maximum model turns before the infinite-loop guard trips.
    pub max_turns: usize,
    /// Consecutive no-progress turns before aborting.
    pub max_no_progress: usize,
}

impl Default for PlanCaps {
    fn default() -> Self {
        Self {
            max_plan_steps: 16,
            max_turns: 32,
            max_no_progress: 3,
        }
    }
}

/// Bounded repair attempts for the single-shot (C/D) planners (spec: ≤2).
pub const MAX_REPAIR_RETRIES: usize = 2;

/// Read-only services a planning loop may call into. Everything is
/// reference-counted; planners own one and share it across turns.
#[derive(Clone)]
pub struct PlanningServices {
    pub registry: Arc<ToolRegistry>,
    pub state: ToolState,
    pub exec_ctx: ExecutionContext,
}

impl PlanningServices {
    pub fn new(registry: Arc<ToolRegistry>, state: ToolState, exec_ctx: ExecutionContext) -> Self {
        Self {
            registry,
            state,
            exec_ctx,
        }
    }
}

// ---------------------------------------------------------------------------
// Planning-tool surface (shared by A + B)
// ---------------------------------------------------------------------------

pub const TOOL_ADD_STEP: &str = "add_step";
pub const TOOL_REVISE_STEP: &str = "revise_step";
pub const TOOL_REMOVE_STEP: &str = "remove_step";
pub const TOOL_INSPECT_PLAN: &str = "inspect_plan";
pub const TOOL_FINISH_PLAN: &str = "finish_plan";
pub const TOOL_REQUEST_CLARIFICATION: &str = "request_clarification";

/// Read-only discovery tools callable during planning. Everything else that
/// touches the repository (`create`, `update`, `delete`, `execute_query`,
/// `build_query`, legacy `search`/`read`) is rejected at the planning
/// boundary.
pub const PLANNING_READ_TOOLS: &[&str] = &[
    "search_vocabulary",
    "describe_vocabulary",
    "search_instances",
    "describe_resource",
    "inspect_query",
];

const OPERATION_VALUES: &[&str] = &[
    "search_vocabulary",
    "describe_vocabulary",
    "search_instances",
    "describe_resource",
    "query",
    "create",
    "update",
    "delete",
    "validate",
    "verify",
];

const OUTPUT_VALUES: &[&str] = &[
    "VocabularyCandidates",
    "ResolvedVocabulary",
    "ResourceCandidates",
    "ResourceDescriptions",
    "UriSet",
    "MutationResult",
    "ValidationResult",
    "VerificationResult",
];

fn operation_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "required": ["id", "operation", "intent", "args", "output"],
        "properties": {
            "id": {"type": "string"},
            "operation": {"type": "string", "enum": OPERATION_VALUES},
            "intent": {"type": "string"},
            "args": {"type": "object"},
            "depends_on": {"type": "array", "items": {"type": "string"}},
            "output": {"type": "string", "enum": OUTPUT_VALUES}
        },
        "additionalProperties": false
    })
}

/// Planning-tool definitions in the existing [`ToolDef`] (Needle) style.
pub fn planning_tool_defs() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: TOOL_ADD_STEP,
            description: "Add one operation to the in-memory plan. Args: id (e.g. s1), operation, intent, args (typed per operation), depends_on (existing step ids, never inferred), output. read-only intent; never executes RDF ops.",
            schema: operation_schema(),
        },
        ToolDef {
            name: TOOL_REVISE_STEP,
            description: "Revise an unexecuted plan step (operation, intent, args, depends_on, output). Rejected when the step already produced a recorded result, or when other steps depend on the removed fields.",
            schema: serde_json::json!({
                "type": "object",
                "required": ["id"],
                "properties": {
                    "id": {"type": "string"},
                    "operation": {"type": "string", "enum": OPERATION_VALUES},
                    "intent": {"type": "string"},
                    "args": {"type": "object"},
                    "depends_on": {"type": "array", "items": {"type": "string"}},
                    "output": {"type": "string", "enum": OUTPUT_VALUES}
                },
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: TOOL_REMOVE_STEP,
            description: "Remove a plan step. Rejected when other steps depend on it (would dangle references).",
            schema: serde_json::json!({
                "type": "object",
                "required": ["id"],
                "properties": {"id": {"type": "string"}},
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: TOOL_INSPECT_PLAN,
            description: "Return the current plan, its dependency edges, and unresolved step references. No arguments.",
            schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: TOOL_FINISH_PLAN,
            description: "Terminate planning. Runs full plan validation; on failure returns the validation errors so the plan can be repaired instead of accepted. Args: rationale (string), assumptions (array of strings).",
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "rationale": {"type": "string"},
                    "assumptions": {"type": "array", "items": {"type": "string"}}
                },
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: TOOL_REQUEST_CLARIFICATION,
            description: "Stop planning with a question for the user. Use only for genuinely ambiguous requests, never merely because the ontology has not been searched yet. Args: question (string).",
            schema: serde_json::json!({
                "type": "object",
                "required": ["question"],
                "properties": {"question": {"type": "string"}},
                "additionalProperties": false
            }),
        },
    ]
}

/// Planning-tool definitions as `agentstream` tools for Option B
/// (`AgentStream::send(...).with_tools(...)`).
pub fn planning_tools_for_agent() -> Vec<agentstream::tools::Tool> {
    planning_tool_defs()
        .into_iter()
        .map(|d| {
            agentstream::tools::Tool::new(d.name.to_string(), d.description.to_string())
                .with_parameters(d.schema.clone())
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Prompt builders (shared by B/C/D)
// ---------------------------------------------------------------------------

fn graph_context_summary(graph_ctx: &GraphContext) -> String {
    let graphs = if graph_ctx.graphs.is_empty() {
        "(unconstrained)".to_string()
    } else {
        graph_ctx
            .graphs
            .iter()
            .map(|g| g.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let namespaces = if graph_ctx.namespaces.is_empty() {
        "(unconstrained)".to_string()
    } else {
        let mut pairs: Vec<String> = graph_ctx
            .namespaces
            .iter()
            .map(|(p, iri)| format!("{p}={iri}"))
            .collect();
        pairs.sort();
        pairs.join(", ")
    };
    format!(
        "dataset: {}; graphs: {}; namespaces: {}; inference: {:?}",
        graph_ctx.dataset, graphs, namespaces, graph_ctx.inference
    )
}

/// System prompt shared by the three `agentstream` planners: role +
/// operations + rules + [`GraphContext`] summary.
pub fn planner_system_prompt(graph_ctx: &GraphContext) -> String {
    format!(
        "You are an operational planner for an RDF knowledge graph. You build a plan; a separate executor runs it.\n\
        \nAvailable plan operations: search_vocabulary (find ontology terms), describe_vocabulary (resolve term details), \
        search_instances (find resources), describe_resource (inspect resources), query (typed graph pattern over discovered vocabulary), \
        create / update / delete (executor only, planned but never run now), validate (SHACL check after mutations), verify (independent re-query proof).\n\
        \nRules:\n\
        1. Build the plan with add_step / revise_step / remove_step; inspect with inspect_plan; end with finish_plan.\n\
        2. Every step that consumes another step's output must list it in depends_on explicitly; dependencies are never inferred.\n\
        3. Reference prior outputs only via step_ref {{kind: step_ref, step, field}} or uri_set_ref {{kind: uri_set_ref, step}}; never invent URIs. Unknown values stay unresolved.\n\
        4. Mutations run only in the executor. Plans with mutations must discover first and end with validate then verify.\n\
        5. You may call read-only discovery (search_vocabulary, describe_vocabulary, search_instances, describe_resource, inspect_query) while planning. \
        Never call create, update, delete, execute_query, or build_query during planning.\n\
        6. Flag unknown value representations (literal vs IRI, e.g. \"Ohio\" vs \"OH\") instead of assuming one.\n\
        \nGraph context:\n{}",
        graph_context_summary(graph_ctx)
    )
}

/// Per-turn user message for the interactive planners: objective + compact
/// [`OperationContext`] rendering (never full history).
pub fn planning_turn_prompt(
    request: &UserRequest,
    ctx: &OperationContext,
    plan: &Plan,
    last_results: &[String],
) -> String {
    let results = if last_results.is_empty() {
        "No tool calls yet.".to_string()
    } else {
        last_results.join("\n")
    };
    format!(
        "User objective:\n{}\n\n{}\n\nLast tool results:\n{}\n\nContinue constructing the plan; call finish_plan only when all required operations and dependencies are represented.",
        request.text,
        ctx.status_summary(plan),
        results
    )
}

/// Single-shot prompt for Option C.
pub fn structured_request_message(request: &UserRequest, ctx: &OperationContext) -> String {
    format!(
        "{}\n\nUser objective:\n{}\n\nGraph context:\n{}\n\nReturn the complete operational plan as a single JSON object matching the provided schema (goal, steps with id/operation/intent/args/depends_on/output, assumptions). \
        Use step_ref / uri_set_ref for values produced by earlier steps; never invent URIs.",
        planner_system_prompt(&ctx.graph_context),
        request.text,
        graph_context_summary(&ctx.graph_context)
    )
}

/// First-pass prompt for Option D.
pub fn nl_plan_request_message(request: &UserRequest, ctx: &OperationContext) -> String {
    format!(
        "{}\n\nUser objective:\n{}\n\nWrite the operational plan as numbered prose steps (1., 2., ...). For each step state its purpose, the operation it needs, \
        what it depends on (\"depends on step N\"), and which value representations are still unknown and must be discovered. \
        Do not emit JSON, SPARQL, or direct graph mutations.",
        planner_system_prompt(&ctx.graph_context),
        request.text
    )
}

/// Second-pass extraction prompt for Option D.
pub fn extraction_request_message(prose: &str) -> String {
    format!(
        "Encode the following natural-language operational plan into the provided plan schema. Encode, do not improve: \
        preserve every numbered step in order as s1..sN, preserve every stated dependency, do not add or drop operations, \
        and represent unknown values as unresolved or step references.\n\nNatural-language plan:\n{prose}"
    )
}

// ---------------------------------------------------------------------------
// In-memory planning session (shared by A + B)
// ---------------------------------------------------------------------------

/// Argument shapes for the planning tools.
#[derive(Debug, Deserialize)]
struct AddStepArgs {
    id: String,
    operation: Operation,
    intent: String,
    args: StepArguments,
    #[serde(default)]
    depends_on: Vec<String>,
    output: OutputType,
}

#[derive(Debug, Deserialize)]
struct ReviseStepArgs {
    id: String,
    operation: Option<Operation>,
    intent: Option<String>,
    args: Option<StepArguments>,
    depends_on: Option<Vec<String>>,
    output: Option<OutputType>,
}

#[derive(Debug, Deserialize)]
struct RemoveStepArgs {
    id: String,
}

#[derive(Debug, Deserialize, Default)]
struct FinishPlanArgs {
    // Parsed (part of the tool schema) but intentionally unused: the
    // completion signal is the validated plan itself.
    #[serde(default, rename = "rationale")]
    _rationale: Option<String>,
    #[serde(default)]
    assumptions: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct ClarifyArgs {
    question: String,
}

/// Outcome of applying one model turn's tool calls, in order.
#[derive(Debug, Default)]
struct TurnApplication {
    /// Compact per-call feedback lines for the next prompt.
    results: Vec<String>,
    /// Set when `finish_plan` validated successfully (stop applying calls).
    finished: Option<Plan>,
}

fn plan_digest(plan: &Plan) -> u64 {
    let s = serde_json::to_string(&plan.steps).unwrap_or_default();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Mutable plan under construction plus loop-guard bookkeeping.
pub struct PlanningSession {
    pub plan: Plan,
    pub turns: usize,
    no_progress_streak: usize,
    last_digest: u64,
}

impl PlanningSession {
    pub fn new(request: &UserRequest, ctx: &OperationContext) -> Self {
        let plan = Plan {
            goal: request.text.clone(),
            context: ctx.graph_context.clone(),
            steps: vec![],
            assumptions: vec![],
        };
        let last_digest = plan_digest(&plan);
        Self {
            plan,
            turns: 0,
            no_progress_streak: 0,
            last_digest,
        }
    }

    fn ids(&self) -> Vec<String> {
        self.plan.steps.iter().map(|s| s.id.clone()).collect()
    }

    fn note_progress(&mut self, caps: &PlanCaps) -> Result<(), PlannerError> {
        let digest = plan_digest(&self.plan);
        if digest == self.last_digest {
            self.no_progress_streak += 1;
        } else {
            self.no_progress_streak = 0;
            self.last_digest = digest;
        }
        if self.no_progress_streak >= caps.max_no_progress {
            return Err(PlannerError::Budget(format!(
                "no progress for {} consecutive turns (duplicate or rejected actions)",
                self.no_progress_streak
            )));
        }
        Ok(())
    }
}

fn validation_err(step: Option<&str>, code: &str, msg: impl Into<String>) -> PlannerError {
    PlannerError::Validation(vec![PlanError::custom(step, code, msg)])
}

fn check_step_consistency(
    step_id: &str,
    operation: Operation,
    args: &StepArguments,
    output: OutputType,
    depends_on: &[String],
    existing: &[String],
) -> Result<(), PlannerError> {
    if step_id.trim().is_empty() {
        return Err(PlannerError::Schema("step id must not be empty".to_string()));
    }
    if args.operation() != operation {
        return Err(PlannerError::Schema(format!(
            "args kind {:?} does not match operation {operation:?}",
            args.operation()
        )));
    }
    if output != OutputType::expected_for(operation) {
        return Err(validation_err(
            Some(step_id),
            "output",
            format!("output {output:?} does not match operation {operation:?}"),
        ));
    }
    for d in depends_on {
        if !existing.contains(d) {
            return Err(validation_err(
                Some(step_id),
                "dependency",
                format!("depends on unknown step {d:?}; dependencies are never inferred"),
            ));
        }
        if d == step_id {
            return Err(validation_err(
                Some(step_id),
                "dependency",
                "step cannot depend on itself",
            ));
        }
    }
    // Typed references (step_ref / uri_set_ref) must also resolve to
    // existing steps at add time — never inferred, never forward.
    for r in ref_targets_in_args(args) {
        if !existing.contains(&r) {
            return Err(validation_err(
                Some(step_id),
                "dangling",
                format!("argument references unknown step {r:?}; dependencies are never inferred"),
            ));
        }
    }
    Ok(())
}

fn apply_add_step(
    session: &mut PlanningSession,
    caps: &PlanCaps,
    args: serde_json::Value,
) -> Result<serde_json::Value, PlannerError> {
    let a: AddStepArgs = serde_json::from_value(args)
        .map_err(|e| PlannerError::Schema(format!("invalid add_step args: {e}")))?;
    if session.plan.steps.iter().any(|s| s.id == a.id) {
        return Err(validation_err(
            Some(&a.id),
            "duplicate",
            format!("step id {:?} already exists", a.id),
        ));
    }
    if session.plan.steps.len() >= caps.max_plan_steps {
        return Err(PlannerError::Budget(format!(
            "plan step cap ({}) reached; revise or finish instead",
            caps.max_plan_steps
        )));
    }
    // Duplicate-operation detection: same operation + same args as an
    // existing step adds no information.
    if session
        .plan
        .steps
        .iter()
        .any(|s| s.operation == a.operation && s.args == a.args)
    {
        return Err(validation_err(
            Some(&a.id),
            "duplicate",
            "an identical operation step already exists; revise it instead of duplicating",
        ));
    }
    let existing = session.ids();
    check_step_consistency(&a.id, a.operation, &a.args, a.output, &a.depends_on, &existing)?;
    session.plan.steps.push(PlanStep {
        id: a.id.clone(),
        operation: a.operation,
        intent: a.intent,
        args: a.args,
        depends_on: a.depends_on,
        output: a.output,
    });
    Ok(serde_json::json!({"added": a.id}))
}

fn apply_revise_step(
    session: &mut PlanningSession,
    ctx: &OperationContext,
    args: serde_json::Value,
) -> Result<serde_json::Value, PlannerError> {
    let a: ReviseStepArgs = serde_json::from_value(args)
        .map_err(|e| PlannerError::Schema(format!("invalid revise_step args: {e}")))?;
    let pos = session
        .plan
        .steps
        .iter()
        .position(|s| s.id == a.id)
        .ok_or_else(|| validation_err(Some(&a.id), "args", format!("unknown step {:?}", a.id)))?;
    if ctx.results.contains_key(&a.id) {
        return Err(validation_err(
            Some(&a.id),
            "frozen",
            "step already has a recorded result and is immutable; add a replacement step instead",
        ));
    }
    let mut next = session.plan.steps[pos].clone();
    if let Some(op) = a.operation {
        next.operation = op;
    }
    if let Some(intent) = a.intent {
        next.intent = intent;
    }
    if let Some(args) = a.args {
        next.args = args;
    }
    if let Some(deps) = a.depends_on {
        next.depends_on = deps;
    }
    if let Some(out) = a.output {
        next.output = out;
    }
    let existing: Vec<String> = session
        .ids()
        .into_iter()
        .filter(|id| id != &next.id)
        .collect();
    check_step_consistency(
        &next.id,
        next.operation,
        &next.args,
        next.output,
        &next.depends_on,
        &existing,
    )?;
    session.plan.steps[pos] = next;
    Ok(serde_json::json!({"revised": a.id}))
}

fn apply_remove_step(
    session: &mut PlanningSession,
    args: serde_json::Value,
) -> Result<serde_json::Value, PlannerError> {
    let a: RemoveStepArgs = serde_json::from_value(args)
        .map_err(|e| PlannerError::Schema(format!("invalid remove_step args: {e}")))?;
    let pos = session
        .plan
        .steps
        .iter()
        .position(|s| s.id == a.id)
        .ok_or_else(|| validation_err(Some(&a.id), "args", format!("unknown step {:?}", a.id)))?;
    let dependents: Vec<String> = session
        .plan
        .steps
        .iter()
        .filter(|s| s.depends_on.contains(&a.id))
        .map(|s| s.id.clone())
        .collect();
    if !dependents.is_empty() {
        return Err(validation_err(
            Some(&a.id),
            "dependency",
            format!("cannot remove: steps {dependents:?} depend on it"),
        ));
    }
    session.plan.steps.remove(pos);
    Ok(serde_json::json!({"removed": a.id}))
}

fn inspect_plan_value(session: &PlanningSession) -> serde_json::Value {
    let unresolved: Vec<serde_json::Value> = session
        .plan
        .steps
        .iter()
        .flat_map(|s| refs_in_args(&s.args).into_iter().map(move |r| (s.id.clone(), r)))
        .map(|(step, reference)| serde_json::json!({"step": step, "reference": reference}))
        .collect();
    serde_json::json!({
        "steps": session.plan.steps,
        "depends_on": session.plan.steps.iter().map(|s| serde_json::json!({"id": s.id, "depends_on": s.depends_on})).collect::<Vec<_>>(),
        "unresolved": unresolved,
    })
}

fn refs_in_args(args: &StepArguments) -> Vec<String> {
    let mut out = vec![];
    let mut push = |v: &TypedValue| match v {
        TypedValue::StepRef { step, field } => out.push(format!("step_ref {step}.{field}")),
        TypedValue::UriSetRef { step } => out.push(format!("uri_set_ref {step}")),
        TypedValue::Unresolved { description } => out.push(format!("unresolved {description}")),
        TypedValue::Iri { .. } => {}
    };
    match args {
        StepArguments::DescribeVocab { iris } => iris.iter().for_each(&mut push),
        StepArguments::SearchInstances { types, .. } => types.iter().for_each(&mut push),
        StepArguments::DescribeResource { uris } => uris.iter().for_each(&mut push),
        StepArguments::Create { types, .. } => types.iter().for_each(&mut push),
        StepArguments::Update {
            targets,
            predicate,
            objects,
        } => targets
            .iter()
            .chain(std::iter::once(predicate))
            .chain(objects.iter())
            .for_each(&mut push),
        StepArguments::Delete { targets } => targets.iter().for_each(&mut push),
        _ => {}
    }
    out
}

/// Step ids referenced by `step_ref` / `uri_set_ref` values in step args.
/// Used to reject dangling references at add time (never inferred). The
/// opaque `query.pattern` object is intentionally skipped here — it carries
/// no `TypedValue`s.
fn ref_targets_in_args(args: &StepArguments) -> Vec<String> {
    let mut out = vec![];
    let mut push = |v: &TypedValue| {
        if let TypedValue::StepRef { step, .. } | TypedValue::UriSetRef { step } = v {
            out.push(step.clone());
        }
    };
    match args {
        StepArguments::DescribeVocab { iris } => iris.iter().for_each(&mut push),
        StepArguments::SearchInstances { types, .. } => types.iter().for_each(&mut push),
        StepArguments::DescribeResource { uris } => uris.iter().for_each(&mut push),
        StepArguments::Create { types, .. } => types.iter().for_each(&mut push),
        StepArguments::Update {
            targets,
            predicate,
            objects,
        } => targets
            .iter()
            .chain(std::iter::once(predicate))
            .chain(objects.iter())
            .for_each(&mut push),
        StepArguments::Delete { targets } => targets.iter().for_each(&mut push),
        _ => {}
    }
    out
}

/// Outcome of a single planning-tool or discovery call.
#[derive(Debug)]
enum CallOutcome {
    /// Feedback value for the next prompt (discovery results included).
    Value(serde_json::Value),
    /// `finish_plan` validated: planning is done.
    Finished(Plan),
}

/// Apply one planning-tool or read-only discovery call.
fn apply_call(
    session: &mut PlanningSession,
    services: &PlanningServices,
    ctx: &OperationContext,
    caps: &PlanCaps,
    name: &str,
    args: serde_json::Value,
) -> Result<CallOutcome, PlannerError> {
    match name {
        TOOL_ADD_STEP => Ok(CallOutcome::Value(apply_add_step(session, caps, args)?)),
        TOOL_REVISE_STEP => Ok(CallOutcome::Value(apply_revise_step(session, ctx, args)?)),
        TOOL_REMOVE_STEP => Ok(CallOutcome::Value(apply_remove_step(session, args)?)),
        TOOL_INSPECT_PLAN => Ok(CallOutcome::Value(inspect_plan_value(session))),
        TOOL_FINISH_PLAN => {
            let a: FinishPlanArgs = serde_json::from_value(args)
                .map_err(|e| PlannerError::Schema(format!("invalid finish_plan args: {e}")))?;
            session.plan.assumptions = a.assumptions.unwrap_or_default();
            match crate::plan::validate_plan(&session.plan) {
                Ok(()) => Ok(CallOutcome::Finished(session.plan.clone())),
                Err(errs) => Err(PlannerError::Validation(errs)),
            }
        }
        TOOL_REQUEST_CLARIFICATION => {
            let a: ClarifyArgs = serde_json::from_value(args)
                .map_err(|e| PlannerError::Schema(format!("invalid request_clarification args: {e}")))?;
            Err(PlannerError::Clarification(a.question))
        }
        _ if PLANNING_READ_TOOLS.contains(&name) => {
            let out = services
                .registry
                .dispatch_with_context(
                    &services.state,
                    &services.exec_ctx,
                    &ctx.graph_context,
                    name,
                    args,
                )
                .map_err(|e| {
                    PlannerError::Model(format!("discovery tool {name} failed: [{e}]"))
                })?;
            // Discovery results feed back into the next prompt (truncated
            // there) so the model can ground its next planning decision.
            Ok(CallOutcome::Value(out))
        }
        _ => Err(PlannerError::Schema(format!(
            "tool '{name}' is not available during planning; use add_step/revise_step/remove_step/inspect_plan/finish_plan plus read-only discovery ({})",
            PLANNING_READ_TOOLS.join(", ")
        ))),
    }
}

fn ok_line(name: &str, value: &serde_json::Value) -> String {
    let short = truncate_json(value);
    format!("ok {name}: {short}")
}

fn err_line(name: &str, err: &PlannerError) -> String {
    format!("ERR {name}: {err}")
}

fn truncate_json(v: &serde_json::Value) -> String {
    let s = serde_json::to_string(v).unwrap_or_default();
    if s.len() > 600 {
        format!("{}…", &s[..600])
    } else {
        s
    }
}

/// Apply one turn's tool calls order-sensitively. A `finish_plan` that fails
/// validation is per-call feedback (the model may repair), not terminal.
fn apply_turn(
    session: &mut PlanningSession,
    services: &PlanningServices,
    ctx: &OperationContext,
    caps: &PlanCaps,
    calls: &[(String, serde_json::Value)],
) -> Result<TurnApplication, PlannerError> {
    let mut app = TurnApplication::default();
    for (name, args) in calls {
        match apply_call(session, services, ctx, caps, name, args.clone()) {
            Ok(CallOutcome::Value(v)) => {
                app.results.push(ok_line(name, &v));
            }
            Ok(CallOutcome::Finished(plan)) => {
                app.results.push(format!("ok {name}: plan validated"));
                app.finished = Some(plan);
                break;
            }
            Err(PlannerError::Clarification(q)) => {
                return Err(PlannerError::Clarification(q));
            }
            Err(e) => {
                app.results.push(err_line(name, &e));
            }
        }
    }
    Ok(app)
}

/// Attempt to finish when the model stops emitting tool calls: validate the
/// current plan; success ends planning, failure becomes feedback.
fn attempt_implicit_finish(
    session: &mut PlanningSession,
) -> Result<Option<Plan>, PlannerError> {
    match crate::plan::validate_plan(&session.plan) {
        Ok(()) => Ok(Some(session.plan.clone())),
        Err(errs) => Err(PlannerError::Validation(errs)),
    }
}

// ---------------------------------------------------------------------------
// Option A — Needle sequential planner (cactus-rs `complete()` loop)
// ---------------------------------------------------------------------------

/// Option A: Needle 3 as a sequential planning-tool caller.
///
/// Holds its [`ModelAdapter`] behind a mutex (the adapter needs `&mut`, the
/// [`Planner`] trait takes `&self`). Production wires the live Needle
/// adapter; tests use `MockModel`.
pub struct NeedleSequentialPlanner {
    model: Mutex<Box<dyn ModelAdapter>>,
    services: PlanningServices,
    pub caps: PlanCaps,
}

impl NeedleSequentialPlanner {
    pub fn new(model: Box<dyn ModelAdapter>, services: PlanningServices) -> Self {
        Self {
            model: Mutex::new(model),
            services,
            caps: PlanCaps::default(),
        }
    }

    pub fn with_caps(mut self, caps: PlanCaps) -> Self {
        self.caps = caps;
        self
    }

    /// Synchronous core (Needle's `complete()` is blocking).
    pub fn create_plan_sync(
        &self,
        request: &UserRequest,
        ctx: &mut OperationContext,
    ) -> Result<Plan, PlannerError> {
        let mut session = PlanningSession::new(request, ctx);
        let system = planner_system_prompt(&ctx.graph_context);
        let mut input = format!(
            "{system}\n\n{}",
            planning_turn_prompt(request, ctx, &session.plan, &[])
        );
        loop {
            session.turns += 1;
            if session.turns > self.caps.max_turns {
                return Err(PlannerError::Budget(format!(
                    "turn cap ({}) reached without finish_plan",
                    self.caps.max_turns
                )));
            }
            let completion = self
                .model
                .lock()
                .expect("planner model lock poisoned")
                .complete(&input)
                .map_err(|e| PlannerError::Model(e.to_string()))?;
            let calls: Vec<(String, serde_json::Value)> = match completion {
                crate::model::ModelCompletion::ToolCalls(calls) => calls
                    .into_iter()
                    .map(|c| (c.name, c.args))
                    .collect(),
                crate::model::ModelCompletion::Done(text) => {
                    if text.trim().is_empty() {
                        return Err(PlannerError::Model(
                            "model stopped with empty output before finish_plan".to_string(),
                        ));
                    }
                    match attempt_implicit_finish(&mut session) {
                        Ok(Some(plan)) => return Ok(plan),
                        Ok(None) => unreachable!(),
                        Err(e) => {
                            session.note_progress(&self.caps)?;
                            input = format!(
                                "{system}\n\n{}\n\nModel stopped saying {text:?}, but: {e}. Keep planning.",
                                planning_turn_prompt(request, ctx, &session.plan, &[])
                            );
                            continue;
                        }
                    }
                }
            };
            if calls.is_empty() {
                match attempt_implicit_finish(&mut session) {
                    Ok(Some(plan)) => return Ok(plan),
                    Ok(None) => unreachable!(),
                    Err(e) => {
                        session.note_progress(&self.caps)?;
                        input = format!(
                            "{system}\n\n{}\n\nNo tool calls emitted, but: {e}. Keep planning.",
                            planning_turn_prompt(request, ctx, &session.plan, &[])
                        );
                        continue;
                    }
                }
            }
            // Submit results back through the adapter (mirrors the agent
            // loop; the live Needle adapter ignores this by design).
            let snapshot: Vec<(String, serde_json::Value)> = calls.clone();
            match apply_turn(&mut session, &self.services, ctx, &self.caps, &snapshot) {
                Ok(app) => {
                    if let Some(plan) = app.finished {
                        return Ok(plan);
                    }
                    let _ = self
                        .model
                        .lock()
                        .expect("planner model lock poisoned")
                        .submit_tool_results(&[]);
                    session.note_progress(&self.caps)?;
                    input = format!(
                        "{system}\n\n{}",
                        planning_turn_prompt(request, ctx, &session.plan, &app.results)
                    );
                }
                Err(PlannerError::Clarification(q)) => {
                    return Err(PlannerError::Clarification(q));
                }
                Err(e) => {
                    // apply_turn only returns terminal Clarification; any
                    // other error is unexpected — surface it.
                    return Err(e);
                }
            }
        }
    }
}

#[async_trait]
impl Planner for NeedleSequentialPlanner {
    async fn create_plan(
        &self,
        request: &UserRequest,
        ctx: &mut OperationContext,
    ) -> Result<Plan, PlannerError> {
        self.create_plan_sync(request, ctx)
    }
}

// ---------------------------------------------------------------------------
// agentstream-backed planners (B/C/D)
// ---------------------------------------------------------------------------

/// Model + backend handle shared by the three `agentstream` planners.
/// The model id always comes from `OPENAI_MODEL` (via [`LlmConfig`); tests
/// pass an explicit id) so evaluation logs prove which local model ran.
#[derive(Clone)]
pub struct AgentBackend {
    pub backend: Arc<dyn agentstream::backends::LLMBackend>,
    pub model: String,
    pub max_tokens: u32,
}

impl AgentBackend {
    pub fn new(backend: Arc<dyn agentstream::backends::LLMBackend>, model: impl Into<String>) -> Self {
        Self {
            backend,
            model: model.into(),
            max_tokens: 2048,
        }
    }

    pub fn from_config(config: &LlmConfig, backend: Arc<dyn agentstream::backends::LLMBackend>) -> Self {
        Self::new(backend, config.model.clone())
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// Fresh turn state. Callers set the system prompt, then `send(...)`.
    /// Interactive loops call `clear_history()` before each turn so every
    /// request carries the compact [`OperationContext`] rendering rather
    /// than an ever-growing transcript (`clear_history` keeps the system
    /// message, which is re-sent separately by `agentstream`).
    pub fn stream(&self) -> agentstream::agent::AgentStream {
        crate::llm::build_agent(self.backend.clone(), &self.model, self.max_tokens)
    }
}

/// Option B: generic LLM as a sequential planning-tool caller.
///
/// Same tools and session machine as Option A; turns go through
/// `AgentStream::send(...).with_tools(...)`. Multi-call responses are
/// applied order-sensitively; `depends_on` is enforced, never inferred.
pub struct AgentToolPlanner {
    backend: AgentBackend,
    services: PlanningServices,
    pub caps: PlanCaps,
}

impl AgentToolPlanner {
    pub fn new(backend: AgentBackend, services: PlanningServices) -> Self {
        Self {
            backend,
            services,
            caps: PlanCaps::default(),
        }
    }

    pub fn with_caps(mut self, caps: PlanCaps) -> Self {
        self.caps = caps;
        self
    }
}

#[async_trait]
impl Planner for AgentToolPlanner {
    async fn create_plan(
        &self,
        request: &UserRequest,
        ctx: &mut OperationContext,
    ) -> Result<Plan, PlannerError> {
        let mut session = PlanningSession::new(request, ctx);
        let system = planner_system_prompt(&ctx.graph_context);
        let tools = planning_tools_for_agent();
        let mut agent = self.backend.stream();
        agent.set_system(&system);
        let mut message = planning_turn_prompt(request, ctx, &session.plan, &[]);
        loop {
            session.turns += 1;
            if session.turns > self.caps.max_turns {
                return Err(PlannerError::Budget(format!(
                    "turn cap ({}) reached without finish_plan",
                    self.caps.max_turns
                )));
            }
            // Compact-context discipline: rebuild from current state.
            agent.clear_history();
            let resp = agent
                .send(&message)
                .with_tools(tools.clone())
                .await
                .map_err(|e| PlannerError::Model(e))?;
            let calls: Vec<(String, serde_json::Value)> = resp
                .tool_calls
                .iter()
                .map(|c| (c.name.clone(), c.input.clone()))
                .collect();
            if calls.is_empty() {
                match attempt_implicit_finish(&mut session) {
                    Ok(Some(plan)) => return Ok(plan),
                    Ok(None) => unreachable!(),
                    Err(e) => {
                        session.note_progress(&self.caps)?;
                        message = format!(
                            "{}\n\nNo tool calls emitted (model said {text:?}), but: {e}. Keep planning.",
                            planning_turn_prompt(request, ctx, &session.plan, &[]),
                            text = truncate_text(&resp.text, 200),
                        );
                        continue;
                    }
                }
            }
            match apply_turn(&mut session, &self.services, ctx, &self.caps, &calls) {
                Ok(app) => {
                    if let Some(plan) = app.finished {
                        return Ok(plan);
                    }
                    session.note_progress(&self.caps)?;
                    message = planning_turn_prompt(request, ctx, &session.plan, &app.results);
                }
                Err(PlannerError::Clarification(q)) => {
                    return Err(PlannerError::Clarification(q));
                }
                Err(e) => return Err(e),
            }
        }
    }
}

fn truncate_text(s: &str, max: usize) -> String {
    if s.len() > max {
        format!("{}…", &s[..max])
    } else {
        s.to_string()
    }
}

// ---------------------------------------------------------------------------
// Structured output (shared by C + second pass of D)
// ---------------------------------------------------------------------------

/// JSON Schema sent with `with_schema(..., Some("operational_plan"))`.
/// Structural detail is best-effort for the provider; strictness lives in
/// Rust (`serde` parse + [`crate::plan::validate_plan`]).
pub fn plan_schema_json() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "required": ["goal", "steps"],
        "properties": {
            "goal": {"type": "string"},
            "assumptions": {"type": "array", "items": {"type": "string"}},
            "context": {"type": "object"},
            "steps": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["id", "operation", "intent", "args", "output"],
                    "properties": {
                        "id": {"type": "string"},
                        "operation": {"type": "string", "enum": OPERATION_VALUES},
                        "intent": {"type": "string"},
                        "args": {"type": "object"},
                        "depends_on": {"type": "array", "items": {"type": "string"}},
                        "output": {"type": "string", "enum": OUTPUT_VALUES}
                    },
                    "additionalProperties": false
                }
            }
        },
        "additionalProperties": false
    })
}

fn strip_code_fences(text: &str) -> &str {
    let t = text.trim();
    let t = t.strip_prefix("```json").or_else(|| t.strip_prefix("```")).unwrap_or(t);
    let t = t.strip_suffix("```").unwrap_or(t);
    t.trim()
}

/// Parse model text into a [`Plan`]: accept the object directly or wrapped
/// under the `operational_plan` schema name. Rust owns the context and the
/// goal fallback.
pub fn parse_plan_json(
    text: &str,
    request: &UserRequest,
    ctx: &OperationContext,
) -> Result<Plan, PlannerError> {
    let raw = strip_code_fences(text);
    let value: serde_json::Value = serde_json::from_str(raw)
        .map_err(|e| PlannerError::Schema(format!("response is not JSON: {e}")))?;
    let inner = match &value {
        serde_json::Value::Object(map) if map.contains_key("operational_plan") => {
            map.get("operational_plan").cloned().unwrap_or(value)
        }
        _ => value,
    };
    let mut plan: Plan = serde_json::from_value(inner)
        .map_err(|e| PlannerError::Schema(format!("response does not match the plan schema: {e}")))?;
    // Rust owns the context; the goal must not be blank.
    plan.context = ctx.graph_context.clone();
    if plan.goal.trim().is_empty() {
        plan.goal = request.text.clone();
    }
    Ok(plan)
}

fn validation_feedback(errs: &[PlanError]) -> String {
    errs.iter()
        .map(|e| match &e.step {
            Some(s) => format!("[{s}:{}] {}", e.code, e.message),
            None => format!("[plan:{}] {}", e.code, e.message),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// One structured-output turn with bounded repair (≤ [`MAX_REPAIR_RETRIES`]).
async fn generate_structured_plan(
    agent: &mut agentstream::agent::AgentStream,
    first_message: String,
    request: &UserRequest,
    ctx: &OperationContext,
    max_repairs: usize,
) -> Result<Plan, PlannerError> {
    let mut message = first_message;
    for attempt in 0..=max_repairs {
        let resp = agent
            .send(&message)
            .with_schema(plan_schema_json(), Some("operational_plan".to_string()))
            .await
            .map_err(|e| PlannerError::Model(e))?;
        match parse_plan_json(&resp.text, request, ctx) {
            Ok(plan) => match crate::plan::validate_plan(&plan) {
                Ok(()) => return Ok(plan),
                Err(errs) => {
                    if attempt == max_repairs {
                        return Err(PlannerError::Validation(errs));
                    }
                    message = format!(
                        "The previous plan was invalid:\n{}\n\nPrevious output:\n{}\n\nRegenerate the complete corrected plan as a single JSON object.",
                        validation_feedback(&errs),
                        truncate_text(&resp.text, 2000)
                    );
                }
            },
            Err(PlannerError::Schema(msg)) => {
                if attempt == max_repairs {
                    return Err(PlannerError::Schema(msg));
                }
                message = format!(
                    "The previous output was not a valid plan ({msg}). Regenerate the complete plan as a single JSON object.\n\nPrevious output:\n{}",
                    truncate_text(&resp.text, 2000)
                );
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!("repair loop always returns")
}

/// Option C: single structured-output turn + bounded repair.
///
/// Baseline planner: one model call, one schema, full validation. Repair is
/// bounded (≤2 retries with validator error feedback), never open-ended.
pub struct AgentStructuredPlanner {
    backend: AgentBackend,
    pub max_repairs: usize,
}

impl AgentStructuredPlanner {
    pub fn new(backend: AgentBackend) -> Self {
        Self {
            backend,
            max_repairs: MAX_REPAIR_RETRIES,
        }
    }

    pub fn with_max_repairs(mut self, n: usize) -> Self {
        self.max_repairs = n;
        self
    }
}

#[async_trait]
impl Planner for AgentStructuredPlanner {
    async fn create_plan(
        &self,
        request: &UserRequest,
        ctx: &mut OperationContext,
    ) -> Result<Plan, PlannerError> {
        let mut agent = self.backend.stream();
        agent.set_system(&planner_system_prompt(&ctx.graph_context));
        let message = structured_request_message(request, ctx);
        generate_structured_plan(&mut agent, message, request, ctx, self.max_repairs).await
    }
}

// ---------------------------------------------------------------------------
// Option D — two-pass NL plan → structured extraction
// ---------------------------------------------------------------------------

/// A numbered prose step extracted from a first-pass plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProseStep {
    pub number: usize,
    pub text: String,
}

/// Parse `1. ...` / `Step 2: ...` items out of a first-pass plan.
/// Continuation lines attach to the current step.
pub fn parse_numbered_prose(prose: &str) -> Vec<ProseStep> {
    let head =
        regex::Regex::new(r"(?i)^\s*(?:step\s+)?(\d+)\s*[.):\-]\s*(.+?)\s*$").expect("prose regex");
    let mut steps: Vec<ProseStep> = vec![];
    for line in prose.lines() {
        if let Some(caps) = head.captures(line) {
            let number: usize = caps[1].parse().unwrap_or(steps.len() + 1);
            steps.push(ProseStep {
                number,
                text: caps[2].to_string(),
            });
        } else if !line.trim().is_empty() {
            if let Some(last) = steps.last_mut() {
                last.text.push(' ');
                last.text.push_str(line.trim());
            }
        }
    }
    steps
}

/// Dependency mentions (`depends on step N`, `after step N`) inside prose.
fn prose_dep_mentions(text: &str) -> Vec<usize> {
    let re = regex::Regex::new(
        r"(?i)(?:depends?\s+(?:on|upon)|after|following)\s+(?:step\s+)?(\d+)",
    )
    .expect("dep regex");
    re.captures_iter(text)
        .filter_map(|c| c[1].parse::<usize>().ok())
        .collect()
}

/// Consistency check between the retained prose plan and its structured
/// extraction: every prose step (positional 1..N ↔ `plan.steps[i-1]`) must
/// exist, and every stated dependency must be preserved. Failures name the
/// offending prose step.
pub fn check_prose_consistency(prose: &str, plan: &Plan) -> Result<(), PlannerError> {
    let prose_steps = parse_numbered_prose(prose);
    if prose_steps.is_empty() {
        return Err(PlannerError::Consistency(
            "first pass produced no numbered steps; the prompt requires 'number each step'"
                .to_string(),
        ));
    }
    if plan.steps.len() != prose_steps.len() {
        if plan.steps.len() < prose_steps.len() {
            // Id-based diagnosis: the extractor must emit s1..sN in order,
            // so the first position whose id diverges names the dropped
            // prose step. Fall back to the first unmapped position when the
            // extractor renumbered its ids.
            let mut missing = &prose_steps[plan.steps.len()];
            for (i, p) in prose_steps.iter().enumerate() {
                let expected = format!("s{}", p.number);
                if plan.steps.get(i).map(|s| s.id.as_str()) != Some(expected.as_str()) {
                    missing = p;
                    break;
                }
            }
            return Err(PlannerError::Consistency(format!(
                "structured plan omits prose step {} ('{}'); encode, do not improve",
                missing.number,
                truncate_text(&missing.text, 80)
            )));
        }
        let extra = &plan.steps[prose_steps.len()];
        return Err(PlannerError::Consistency(format!(
            "structured step '{}' has no prose counterpart at position {}; encode, do not improve",
            extra.id,
            prose_steps.len() + 1
        )));
    }
    for (i, prose_step) in prose_steps.iter().enumerate() {
        for dep in prose_dep_mentions(&prose_step.text) {
            if dep < 1 || dep > prose_steps.len() {
                return Err(PlannerError::Consistency(format!(
                    "prose step {} references unknown step {dep}",
                    prose_step.number
                )));
            }
            let expected_id = &plan.steps[dep - 1].id;
            if !plan.steps[i].depends_on.contains(expected_id) {
                return Err(PlannerError::Consistency(format!(
                    "prose step {} depends on step {dep}, but structured step '{}' lacks that dependency",
                    prose_step.number, plan.steps[i].id
                )));
            }
        }
    }
    Ok(())
}

/// Option D: unconstrained NL plan first, then schema-constrained extraction
/// with an "encode, do not improve" instruction. Both artifacts are retained
/// on the [`OperationContext`] for audit.
pub struct AgentTwoPassPlanner {
    backend: AgentBackend,
    pub max_repairs: usize,
}

impl AgentTwoPassPlanner {
    pub fn new(backend: AgentBackend) -> Self {
        Self {
            backend,
            max_repairs: MAX_REPAIR_RETRIES,
        }
    }

    pub fn with_max_repairs(mut self, n: usize) -> Self {
        self.max_repairs = n;
        self
    }
}

#[async_trait]
impl Planner for AgentTwoPassPlanner {
    async fn create_plan(
        &self,
        request: &UserRequest,
        ctx: &mut OperationContext,
    ) -> Result<Plan, PlannerError> {
        let mut agent = self.backend.stream();
        agent.set_system(&planner_system_prompt(&ctx.graph_context));
        // Pass 1: unconstrained NL plan.
        let prose = agent
            .send(&nl_plan_request_message(request, ctx))
            .await
            .map_err(|e| PlannerError::Model(e))?
            .text;
        if prose.trim().is_empty() {
            return Err(PlannerError::Model(
                "first pass returned an empty natural-language plan".to_string(),
            ));
        }
        ctx.record_artifact("nl_plan", prose.clone());
        // Pass 2: structured extraction ("encode, do not improve").
        let plan = generate_structured_plan(
            &mut agent,
            extraction_request_message(&prose),
            request,
            ctx,
            self.max_repairs,
        )
        .await?;
        check_prose_consistency(&prose, &plan)?;
        ctx.record_artifact(
            "structured_plan",
            serde_json::to_string(&plan).unwrap_or_default(),
        );
        Ok(plan)
    }
}

// ---------------------------------------------------------------------------
// Read-only planning guard (used by tests to assert repo immutability)
// ---------------------------------------------------------------------------

/// Names the model must never be able to invoke during planning.
pub fn forbidden_during_planning(name: &str) -> bool {
    matches!(
        name,
        "create" | "update" | "delete" | "execute_query" | "build_query" | "search" | "read"
    ) || !is_known_planning_tool(name)
}

fn is_known_planning_tool(name: &str) -> bool {
    matches!(
        name,
        TOOL_ADD_STEP
            | TOOL_REVISE_STEP
            | TOOL_REMOVE_STEP
            | TOOL_INSPECT_PLAN
            | TOOL_FINISH_PLAN
            | TOOL_REQUEST_CLARIFICATION
    ) || PLANNING_READ_TOOLS.contains(&name)
}

// ---------------------------------------------------------------------------
// Tests (mock backends only — no weights required)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    use agentstream::backends::{BackendRequest, BackendResponse, LLMBackend};

    // -- Mock LLM backend ---------------------------------------------------

    struct ScriptBackend {
        responses: Mutex<VecDeque<BackendResponse>>,
        seen_models: Mutex<Vec<String>>,
        seen_tool_schemas: Mutex<Vec<usize>>,
    }

    impl ScriptBackend {
        fn with_texts(texts: Vec<&str>) -> Self {
            Self {
                responses: Mutex::new(
                    texts
                        .into_iter()
                        .map(|t| BackendResponse {
                            text: t.to_string(),
                            finish_reason: Some("stop".to_string()),
                            ..BackendResponse::default()
                        })
                        .collect(),
                ),
                seen_models: Mutex::new(vec![]),
                seen_tool_schemas: Mutex::new(vec![]),
            }
        }

        fn with_tool_turns(turns: Vec<Vec<(String, serde_json::Value)>>) -> Self {
            Self {
                responses: Mutex::new(
                    turns
                        .into_iter()
                        .map(|calls| BackendResponse {
                            text: String::new(),
                            tool_calls: calls
                                .into_iter()
                                .enumerate()
                                .map(|(i, (name, input))| {
                                    agentstream::backends::ToolCallData {
                                        name,
                                        input,
                                        id: Some(format!("call-{i}")),
                                    }
                                })
                                .collect(),
                            finish_reason: Some("tool_calls".to_string()),
                            ..BackendResponse::default()
                        })
                        .collect(),
                ),
                seen_models: Mutex::new(vec![]),
                seen_tool_schemas: Mutex::new(vec![]),
            }
        }

        fn models(&self) -> Vec<String> {
            self.seen_models.lock().unwrap().clone()
        }

        fn request_count(&self) -> usize {
            self.seen_models.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl LLMBackend for ScriptBackend {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn name(&self) -> &'static str {
            "script"
        }

        async fn complete(
            &self,
            request: BackendRequest,
            _abort: Option<agentstream::backends::AbortSignal>,
        ) -> Result<BackendResponse, String> {
            self.seen_models.lock().unwrap().push(request.model.clone());
            self.seen_tool_schemas
                .lock()
                .unwrap()
                .push(request.tools.len());
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| "script exhausted".to_string())
        }
    }

    fn script_backend(texts: Vec<&str>) -> (Arc<ScriptBackend>, AgentBackend) {
        let script = Arc::new(ScriptBackend::with_texts(texts));
        let backend: Arc<dyn LLMBackend> = script.clone();
        (script, AgentBackend::new(backend, "test-local-model"))
    }

    fn tool_script_backend(
        turns: Vec<Vec<(String, serde_json::Value)>>,
    ) -> (Arc<ScriptBackend>, AgentBackend) {
        let script = Arc::new(ScriptBackend::with_tool_turns(turns));
        let backend: Arc<dyn LLMBackend> = script.clone();
        (script, AgentBackend::new(backend, "test-local-model"))
    }

    // -- Fixtures ------------------------------------------------------------

    fn fixture_services() -> (PlanningServices, GraphContext) {
        use crate::config::AppLimits;
        use crate::ontology::{contact_shapes, load_default_ontology, OntologyReasoner};
        use crate::policy::{AuditLog, Operation as PolicyOp};
        use crate::rdf::OxigraphRepository;
        use crate::search::TantivyIndex;

        let repo = Arc::new(OxigraphRepository::new_in_memory(50_000).unwrap());
        load_default_ontology(&*repo).unwrap();
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
        let graph = crate::rdf::GraphName::tenant("acme").unwrap();
        let exec_ctx = ExecutionContext::new(
            "req-1",
            "alice",
            "acme",
            vec![graph.clone(), crate::rdf::GraphName::ontology()],
            vec![PolicyOp::Read],
        );
        let graph_ctx = GraphContext {
            graphs: vec![graph],
            namespaces: [("ex".to_string(), "http://example.com/".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let services = PlanningServices::new(Arc::new(ToolRegistry::new()), state, exec_ctx);
        (services, graph_ctx)
    }

    fn op_ctx(graph_ctx: GraphContext) -> OperationContext {
        OperationContext::new("Classify Ohio contacts", graph_ctx)
    }

    fn add_step_call(
        id: &str,
        operation: &str,
        intent: &str,
        args: serde_json::Value,
        depends_on: Vec<&str>,
        output: &str,
    ) -> (String, serde_json::Value) {
        (
            TOOL_ADD_STEP.to_string(),
            serde_json::json!({
                "id": id,
                "operation": operation,
                "intent": intent,
                "args": args,
                "depends_on": depends_on,
                "output": output
            }),
        )
    }

    fn search_vocab_args(terms: Vec<&str>) -> serde_json::Value {
        serde_json::json!({"kind": "search_vocab", "terms": terms})
    }

    fn describe_vocab_args() -> serde_json::Value {
        serde_json::json!({"kind": "describe_vocab", "iris": [{"kind": "step_ref", "step": "s1", "field": "candidates"}]})
    }

    // -- Option A ------------------------------------------------------------

    #[test]
    fn needle_loop_builds_ohio_dag() {
        use crate::model::{MockModel, ModelCompletion, ToolCall};
        let (services, graph_ctx) = fixture_services();
        let mut ctx = op_ctx(graph_ctx);
        let request = UserRequest::new("Classify Ohio contacts");
        let script = vec![
            ModelCompletion::ToolCalls(vec![ToolCall::new(
                TOOL_ADD_STEP,
                serde_json::json!({
                    "id": "s1", "operation": "search_vocabulary",
                    "intent": "discover contact/account vocabulary",
                    "args": search_vocab_args(vec!["contact", "account"]),
                    "depends_on": [], "output": "VocabularyCandidates"
                }),
                "c1",
            )]),
            ModelCompletion::ToolCalls(vec![ToolCall::new(
                TOOL_ADD_STEP,
                serde_json::json!({
                    "id": "s2", "operation": "describe_vocabulary",
                    "intent": "resolve terms",
                    "args": describe_vocab_args(),
                    "depends_on": ["s1"], "output": "ResolvedVocabulary"
                }),
                "c2",
            )]),
            ModelCompletion::ToolCalls(vec![ToolCall::new(
                TOOL_ADD_STEP,
                serde_json::json!({
                    "id": "s3", "operation": "query",
                    "intent": "find Ohio contacts",
                    "args": {"kind": "query", "pattern": {}},
                    "depends_on": ["s2"], "output": "UriSet"
                }),
                "c3",
            )]),
            ModelCompletion::ToolCalls(vec![ToolCall::new(
                TOOL_FINISH_PLAN,
                serde_json::json!({"rationale": "discovery then query"}),
                "c4",
            )]),
        ];
        let planner =
            NeedleSequentialPlanner::new(Box::new(MockModel::new(script)), services.clone());
        let plan = planner.create_plan_sync(&request, &mut ctx).unwrap();
        assert_eq!(plan.steps.len(), 3);
        assert_eq!(plan.steps[2].depends_on, vec!["s2".to_string()]);
        crate::plan::validate_plan(&plan).unwrap();
    }

    #[test]
    fn needle_premature_finish_rejected_by_validator() {
        let (services, graph_ctx) = fixture_services();
        let _ = &services;
        let ctx = op_ctx(graph_ctx);
        let request = UserRequest::new("Classify Ohio contacts");
        let mut session = PlanningSession::new(&request, &ctx);
        // Empty plan: finish must be rejected, not silently accepted.
        let err = apply_call(
            &mut session,
            &fixture_services().0,
            &ctx,
            &PlanCaps::default(),
            TOOL_FINISH_PLAN,
            serde_json::json!({}),
        )
        .unwrap_err();
        assert!(matches!(err, PlannerError::Validation(_)), "{err:?}");
    }

    #[test]
    fn needle_duplicate_and_loop_guards_trip() {
        let (services, graph_ctx) = fixture_services();
        let ctx = op_ctx(graph_ctx);
        let request = UserRequest::new("Classify Ohio contacts");
        let mut session = PlanningSession::new(&request, &ctx);
        let caps = PlanCaps::default();
        let args = serde_json::json!({
            "id": "s1", "operation": "search_vocabulary",
            "intent": "discover",
            "args": search_vocab_args(vec!["contact"]),
            "depends_on": [], "output": "VocabularyCandidates"
        });
        apply_call(&mut session, &services, &ctx, &caps, TOOL_ADD_STEP, args.clone())
            .unwrap();
        // Same id twice → duplicate rejection.
        let err = apply_call(&mut session, &services, &ctx, &caps, TOOL_ADD_STEP, args)
            .unwrap_err();
        assert!(matches!(err, PlannerError::Validation(_)), "{err:?}");

        // Infinite loop: the model repeats a duplicate forever → Budget.
        use crate::model::{MockModel, ModelCompletion, ToolCall};
        let dup = serde_json::json!({
            "id": "s1", "operation": "search_vocabulary",
            "intent": "discover",
            "args": search_vocab_args(vec!["contact"]),
            "depends_on": [], "output": "VocabularyCandidates"
        });
        let script: Vec<ModelCompletion> = (0..10)
            .map(|i| {
                ModelCompletion::ToolCalls(vec![ToolCall::new(
                    TOOL_ADD_STEP,
                    dup.clone(),
                    format!("c{i}"),
                )])
            })
            .collect();
        let planner = NeedleSequentialPlanner::new(Box::new(MockModel::new(script)), services)
            .with_caps(PlanCaps {
                max_turns: 10,
                max_no_progress: 3,
                ..PlanCaps::default()
            });
        let mut ctx2 = op_ctx(GraphContext::default());
        let err = planner.create_plan_sync(&request, &mut ctx2).unwrap_err();
        assert!(matches!(err, PlannerError::Budget(_)), "{err:?}");
    }

    // -- Option B ------------------------------------------------------------

    #[tokio::test]
    async fn agent_tool_loop_grounds_deps_and_rejects_missing_dep() {
        let (services, graph_ctx) = fixture_services();
        let mut ctx = op_ctx(graph_ctx);
        let request = UserRequest::new("Classify Ohio contacts");
        // Turn 1: read-only discovery. Turn 2: two ordered planning calls —
        // a grounded describe step plus an update missing its UriSet dep.
        let (_script, backend) = tool_script_backend(vec![
            vec![(
                "search_vocabulary".to_string(),
                serde_json::json!({"terms": ["contact"]}),
            )],
            vec![
                add_step_call(
                    "s1",
                    "search_vocabulary",
                    "discover contact/account vocabulary",
                    search_vocab_args(vec!["contact", "account"]),
                    vec![],
                    "VocabularyCandidates",
                ),
                (
                    TOOL_ADD_STEP.to_string(),
                    serde_json::json!({
                        "id": "sX", "operation": "update",
                        "intent": "update without grounding",
                        "args": {"kind": "update",
                                "targets": [{"kind": "uri_set_ref", "step": "s9"}],
                                "predicate": {"kind": "unresolved", "description": "status"},
                                "objects": [{"kind": "unresolved", "description": "prospect"}]},
                        "depends_on": ["s1"], "output": "MutationResult"
                    }),
                ),
            ],
            vec![(
                TOOL_FINISH_PLAN.to_string(),
                serde_json::json!({"rationale": "grounded discovery plan"}),
            )],
        ]);
        let planner = AgentToolPlanner::new(backend, services);
        let plan = planner.create_plan(&request, &mut ctx).await.unwrap();
        // The grounded step landed; the missing-dep update was rejected even
        // though both calls arrived in one response.
        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.steps[0].id, "s1");
        crate::plan::validate_plan(&plan).unwrap();
    }

    #[tokio::test]
    async fn agent_tool_loop_sends_tools_and_model_id() {
        let (services, graph_ctx) = fixture_services();
        let mut ctx = op_ctx(graph_ctx);
        let request = UserRequest::new("hi");
        let (script, backend) = tool_script_backend(vec![vec![(
            TOOL_FINISH_PLAN.to_string(),
            serde_json::json!({}),
        )]]);
        // Empty plan fails validation → Budget via exhausted script; what
        // matters here is what the backend observed (two turns: the finish
        // attempt plus its repair retry).
        let planner = AgentToolPlanner::new(backend, services).with_caps(PlanCaps {
            max_turns: 2,
            ..PlanCaps::default()
        });
        let _ = planner.create_plan(&request, &mut ctx).await;
        assert_eq!(
            script.models(),
            vec!["test-local-model".to_string(), "test-local-model".to_string()]
        );
        // Tool schemas were sent with the first turn.
        let counts = script.seen_tool_schemas.lock().unwrap().clone();
        assert!(!counts.is_empty() && counts[0] > 0);
        for tool in planning_tools_for_agent() {
            let schema = tool.schema();
            assert_eq!(schema.get("type").and_then(|v| v.as_str()), Some("object"));
        }
    }

    // -- Option C ------------------------------------------------------------

    #[tokio::test]
    async fn structured_planner_parses_and_validates() {
        let (services, _graph_ctx) = fixture_services();
        let _ = &services;
        let graph_ctx = GraphContext {
            graphs: vec![crate::rdf::GraphName::tenant("acme").unwrap()],
            ..Default::default()
        };
        let mut ctx = op_ctx(graph_ctx);
        let request = UserRequest::new("Classify Ohio contacts");
        let plan_json = serde_json::to_value(crate::plan::canonical_ohio_plan()).unwrap();
        let (_script, backend) = script_backend(vec![&plan_json.to_string()]);
        let planner = AgentStructuredPlanner::new(backend);
        let plan = planner.create_plan(&request, &mut ctx).await.unwrap();
        assert_eq!(plan.steps.len(), 7);
        // Rust owns the context: the canonical fixture's context is replaced.
        assert_eq!(plan.context.graphs, ctx.graph_context.graphs);
        crate::plan::validate_plan(&plan).unwrap();
    }

    #[tokio::test]
    async fn structured_planner_repairs_malformed_json() {
        let (_services, graph_ctx) = fixture_services();
        let mut ctx = op_ctx(graph_ctx);
        let request = UserRequest::new("Classify Ohio contacts");
        let plan_json = serde_json::json!({
            "goal": "Classify Ohio contacts",
            "steps": [
                {"id": "s1", "operation": "search_vocabulary", "intent": "discover",
                 "args": {"kind": "search_vocab", "terms": ["contact"]},
                 "depends_on": [], "output": "VocabularyCandidates"}
            ]
        });
        let (script, backend) =
            script_backend(vec!["{not json", &plan_json.to_string()]);
        let planner = AgentStructuredPlanner::new(backend);
        let plan = planner.create_plan(&request, &mut ctx).await.unwrap();
        assert_eq!(plan.steps.len(), 1);
        // One failure + one repair = two model calls.
        assert_eq!(script.request_count(), 2);
    }

    #[tokio::test]
    async fn structured_planner_unrepairable_returns_validation() {
        let (_services, graph_ctx) = fixture_services();
        let mut ctx = op_ctx(graph_ctx);
        let request = UserRequest::new("Classify Ohio contacts");
        // Dangling dependency can never validate.
        let bad = serde_json::json!({
            "goal": "Classify Ohio contacts",
            "steps": [
                {"id": "s1", "operation": "describe_vocabulary", "intent": "resolve",
                 "args": {"kind": "describe_vocab", "iris": []},
                 "depends_on": ["s0"], "output": "ResolvedVocabulary"}
            ]
        });
        let bad_text = bad.to_string();
        let (script, backend) =
            script_backend(vec![&bad_text, &bad_text, &bad_text, &bad_text]);
        let planner = AgentStructuredPlanner::new(backend);
        let err = planner.create_plan(&request, &mut ctx).await.unwrap_err();
        assert!(matches!(err, PlannerError::Validation(_)), "{err:?}");
        // Initial attempt + ≤2 retries.
        assert_eq!(script.request_count(), 3);
    }

    // -- Option D ------------------------------------------------------------

    const D_PROSE: &str = "\
1. Search the vocabulary for contact and account terms.\n\
2. Describe the discovered terms to resolve their domains; depends on step 1.\n\
3. Search for an existing prospect classification resource.\n\
4. Update the matching contacts; depends on step 2 and depends on step 3.";

    fn d_plan_json(drop_index: Option<usize>, drop_dep: bool) -> String {
        let mut steps = vec![
            serde_json::json!({"id": "s1", "operation": "search_vocabulary", "intent": "discover",
                 "args": {"kind": "search_vocab", "terms": ["contact"]},
                 "depends_on": [], "output": "VocabularyCandidates"}),
            serde_json::json!({"id": "s2", "operation": "describe_vocabulary", "intent": "resolve",
                 "args": {"kind": "describe_vocab", "iris": [{"kind": "step_ref", "step": "s1", "field": "candidates"}]},
                 "depends_on": ["s1"], "output": "ResolvedVocabulary"}),
            serde_json::json!({"id": "s3", "operation": "search_instances", "intent": "find classification",
                 "args": {"kind": "search_instances", "text": "prospect"},
                 "depends_on": ["s2"], "output": "ResourceCandidates"}),
            serde_json::json!({"id": "s4", "operation": "update", "intent": "classify",
                 "args": {"kind": "update",
                         "targets": [{"kind": "uri_set_ref", "step": "s9"}],
                         "predicate": {"kind": "unresolved", "description": "status"},
                         "objects": [{"kind": "unresolved", "description": "prospect"}]},
                 "depends_on": ["s2", "s3"], "output": "MutationResult"}),
        ];
        if let Some(i) = drop_index {
            steps.remove(i);
        }
        if drop_dep {
            if let Some(s4) = steps.last_mut() {
                s4["depends_on"] = serde_json::json!(["s2"]);
            }
        }
        serde_json::json!({"goal": "Classify Ohio contacts", "steps": steps}).to_string()
    }

    #[tokio::test]
    async fn two_pass_planner_keeps_both_artifacts() {
        let (_services, graph_ctx) = fixture_services();
        let mut ctx = op_ctx(graph_ctx);
        let request = UserRequest::new("Classify Ohio contacts");
        // Six prose steps; the structured extraction mirrors them 1:1 and
        // satisfies full plan validation (discovery → query → update →
        // validate → verify).
        let prose = "1. Search the vocabulary for contact and status terms.";
        let _ = prose;
        let plan_text = d_plan_json(None, false);
        let _ = &plan_text;
        let ok_text = serde_json::json!({
            "goal": "Classify Ohio contacts",
            "steps": [
                {"id": "s1", "operation": "search_vocabulary", "intent": "discover terms",
                 "args": {"kind": "search_vocab", "terms": ["contact", "status"]},
                 "depends_on": [], "output": "VocabularyCandidates"},
                {"id": "s2", "operation": "describe_vocabulary", "intent": "resolve domains",
                 "args": {"kind": "describe_vocab", "iris": [{"kind": "step_ref", "step": "s1", "field": "candidates"}]},
                 "depends_on": ["s1"], "output": "ResolvedVocabulary"},
                {"id": "s3", "operation": "query", "intent": "find matching contacts",
                 "args": {"kind": "query", "pattern": {}},
                 "depends_on": ["s2"], "output": "UriSet"},
                {"id": "s4", "operation": "update", "intent": "classify contacts",
                 "args": {"kind": "update",
                         "targets": [{"kind": "uri_set_ref", "step": "s3"}],
                         "predicate": {"kind": "unresolved", "description": "status"},
                         "objects": [{"kind": "unresolved", "description": "prospect"}]},
                 "depends_on": ["s2", "s3"], "output": "MutationResult"},
                {"id": "s5", "operation": "validate", "intent": "check graph",
                 "args": {"kind": "validate"},
                 "depends_on": ["s4"], "output": "ValidationResult"},
                {"id": "s6", "operation": "verify", "intent": "count classified",
                 "args": {"kind": "verify", "question": "How many contacts were classified?"},
                 "depends_on": ["s5"], "output": "VerificationResult"}
            ]
        })
        .to_string();
        let prose = "1. Search the vocabulary for contact and status terms.\n\
            2. Describe the discovered terms to resolve their domains; depends on step 1.\n\
            3. Query for the matching contacts; depends on step 2.\n\
            4. Update the matching contacts with the prospect classification; depends on step 2 and depends on step 3.\n\
            5. Validate the resulting graph; depends on step 4.\n\
            6. Verify the number of classified contacts; depends on step 5.";
        let (_s2, backend2) = script_backend(vec![prose, ok_text.as_str()]);
        let planner2 = AgentTwoPassPlanner::new(backend2);
        // Full D path: validation passes (query grounds the update, and the
        // mutation tail ends in validate + verify), so both artifacts land.
        let plan = planner2.create_plan(&request, &mut ctx).await.unwrap();
        assert_eq!(plan.steps.len(), 6);
        crate::plan::validate_plan(&plan).unwrap();
        assert_eq!(ctx.artifact("nl_plan"), Some(prose));
    }

    #[test]
    fn two_pass_consistency_names_omitted_step() {
        let plan: Plan = serde_json::from_value(
            serde_json::from_str::<serde_json::Value>(&d_plan_json(Some(2), false)).unwrap(),
        )
        .unwrap();
        // d_plan_json index 2 is the instance-search step ("Search for an
        // existing prospect classification resource").
        let err = check_prose_consistency(D_PROSE, &plan).unwrap_err();
        assert!(matches!(err, PlannerError::Consistency(_)), "{err:?}");
        let msg = err.to_string();
        assert!(msg.contains('3'), "{msg}");
        assert!(msg.contains("prospect classification"), "{msg}");
    }

    #[test]
    fn two_pass_consistency_names_dropped_dependency() {
        let plan: Plan = serde_json::from_value(
            serde_json::from_str::<serde_json::Value>(&d_plan_json(None, true)).unwrap(),
        )
        .unwrap();
        let err = check_prose_consistency(D_PROSE, &plan).unwrap_err();
        assert!(matches!(err, PlannerError::Consistency(_)), "{err:?}");
        let msg = err.to_string();
        assert!(msg.contains('4') && msg.contains('3'), "{msg}");
    }

    // -- Planning never mutates the repo -------------------------------------

    #[test]
    fn planning_loop_never_mutates_repo() {
        use crate::model::{MockModel, ModelCompletion, ToolCall};
        use crate::rdf::{Quad, RdfError, RdfRepository, TripleDelta};

        struct SpyRepo {
            inner: crate::rdf::OxigraphRepository,
            mutations: Mutex<usize>,
        }

        impl RdfRepository for SpyRepo {
            fn insert(&self, quads: &[Quad]) -> Result<TripleDelta, RdfError> {
                *self.mutations.lock().unwrap() += 1;
                self.inner.insert(quads)
            }
            fn remove(&self, quads: &[Quad]) -> Result<TripleDelta, RdfError> {
                *self.mutations.lock().unwrap() += 1;
                self.inner.remove(quads)
            }
            fn apply_atomic(
                &self,
                to_add: &[Quad],
                to_remove: &[Quad],
            ) -> Result<TripleDelta, RdfError> {
                *self.mutations.lock().unwrap() += 1;
                self.inner.apply_atomic(to_add, to_remove)
            }
            fn match_quads(
                &self,
                subject: Option<&crate::rdf::Term>,
                predicate: Option<&crate::rdf::Term>,
                object: Option<&crate::rdf::Term>,
                graph: Option<&crate::rdf::GraphName>,
                limit: Option<usize>,
            ) -> Result<Vec<Quad>, RdfError> {
                self.inner.match_quads(subject, predicate, object, graph, limit)
            }
            fn delete_subject(
                &self,
                subject: &crate::rdf::Term,
                graph: &crate::rdf::GraphName,
            ) -> Result<TripleDelta, RdfError> {
                *self.mutations.lock().unwrap() += 1;
                self.inner.delete_subject(subject, graph)
            }
            fn list_graphs(&self) -> Result<Vec<crate::rdf::GraphName>, RdfError> {
                self.inner.list_graphs()
            }
            fn len(&self) -> Result<usize, RdfError> {
                self.inner.len()
            }
            fn contains(&self, quad: &Quad) -> Result<bool, RdfError> {
                self.inner.contains(quad)
            }
        }

        use crate::config::AppLimits;
        use crate::ontology::{contact_shapes, load_default_ontology, OntologyReasoner};
        use crate::policy::{AuditLog, Operation as PolicyOp};
        use crate::search::TantivyIndex;

        let spy = Arc::new(SpyRepo {
            inner: crate::rdf::OxigraphRepository::new_in_memory(50_000).unwrap(),
            mutations: Mutex::new(0),
        });
        load_default_ontology(&*spy as &dyn RdfRepository).unwrap();
        // Fixture setup done: the planning loop itself must not mutate.
        *spy.mutations.lock().unwrap() = 0;
        let repo_dyn: Arc<dyn RdfRepository> = spy.clone();
        let state = ToolState::new(
            repo_dyn,
            Arc::new(TantivyIndex::in_ram().unwrap()),
            Arc::new(OntologyReasoner::from_repository(&*spy as &dyn RdfRepository).expect("reasoner")),
            contact_shapes(),
            Arc::new(AuditLog::in_memory()),
            AppLimits::default(),
        );
        let graph = crate::rdf::GraphName::tenant("acme").unwrap();
        let exec_ctx = ExecutionContext::new(
            "req-1",
            "alice",
            "acme",
            vec![graph.clone(), crate::rdf::GraphName::ontology()],
            vec![PolicyOp::Read],
        );
        let services = PlanningServices::new(Arc::new(ToolRegistry::new()), state, exec_ctx);
        let graph_ctx = GraphContext {
            graphs: vec![graph],
            ..Default::default()
        };
        let mut ctx = op_ctx(graph_ctx);
        let request = UserRequest::new("Classify Ohio contacts");

        // The model tries a forbidden mutation mid-planning, then plans
        // properly and finishes. The mutation must be rejected, never run.
        let script = vec![
            ModelCompletion::ToolCalls(vec![ToolCall::new(
                "create",
                serde_json::json!({"graph": "urn:needle:tenant:acme"}),
                "c0",
            )]),
            ModelCompletion::ToolCalls(vec![ToolCall::new(
                TOOL_ADD_STEP,
                serde_json::json!({
                    "id": "s1", "operation": "search_vocabulary",
                    "intent": "discover",
                    "args": search_vocab_args(vec!["contact"]),
                    "depends_on": [], "output": "VocabularyCandidates"
                }),
                "c1",
            )]),
            ModelCompletion::ToolCalls(vec![ToolCall::new(
                TOOL_FINISH_PLAN,
                serde_json::json!({}),
                "c2",
            )]),
        ];
        let planner = NeedleSequentialPlanner::new(Box::new(MockModel::new(script)), services);
        let plan = planner.create_plan_sync(&request, &mut ctx).unwrap();
        assert_eq!(plan.steps.len(), 1);
        assert_eq!(*spy.mutations.lock().unwrap(), 0);
    }

    #[test]
    fn forbidden_surface_covers_mutations_and_legacy_tools() {
        for name in [
            "create",
            "update",
            "delete",
            "execute_query",
            "build_query",
            "search",
            "read",
            "drop_graph",
        ] {
            assert!(forbidden_during_planning(name), "{name}");
        }
        for name in [
            TOOL_ADD_STEP,
            TOOL_FINISH_PLAN,
            "search_vocabulary",
            "inspect_query",
        ] {
            assert!(!forbidden_during_planning(name), "{name}");
        }
    }

    #[test]
    fn plan_schema_is_well_formed() {
        let schema = plan_schema_json();
        assert_eq!(schema["type"], "object");
        let steps = &schema["properties"]["steps"]["items"];
        assert!(steps["required"].as_array().unwrap().contains(&serde_json::json!("id")));
        let ops = steps["properties"]["operation"]["enum"].as_array().unwrap();
        assert!(ops.contains(&serde_json::json!("query")));
        let outs = steps["properties"]["output"]["enum"].as_array().unwrap();
        assert!(outs.contains(&serde_json::json!("UriSet")));
    }

    #[test]
    fn prompt_builders_stay_compact() {
        let graph_ctx = GraphContext {
            dataset: "crm".to_string(),
            graphs: vec![crate::rdf::GraphName::tenant("acme").unwrap()],
            ..Default::default()
        };
        let system = planner_system_prompt(&graph_ctx);
        assert!(system.contains("depends_on"));
        assert!(system.contains("crm"));
        let ctx = op_ctx(graph_ctx);
        let request = UserRequest::new("goal");
        let session = PlanningSession::new(&request, &ctx);
        let turn = planning_turn_prompt(&request, &ctx, &session.plan, &[]);
        assert!(turn.contains("goal"));
        assert!(!turn.contains("quad"));
        let _ = VecDeque::<u8>::new();
    }
}
