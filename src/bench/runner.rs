//! Planning-only comparison harness (TESTING plan Phase T2).
//!
//! Compares the four planners (A/B/C/D) on the fixed 100-case benchmark
//! without executing anything: `create_plan` → `validate_plan` →
//! constraint check, with the repository untouched (zero-write gate).
//!
//! Keyless CI default: every run here uses deterministic scripted
//! substitutes through the real planner code paths —
//! A via [`MockModel`] tool-call scripts, B/C/D via a scripted
//! [`agentstream`] backend — so `cargo test` needs no key and no weights.
//! Live runs (`RUN_MODEL_TESTS=1`) reuse the same fixture + scoring with
//! real backends and write recordings the replay path can pin.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;

use crate::bench::case::{BenchmarkCase, Difficulty};
use crate::bench::dataset::{
    bench_shapes, build_adversarial, build_benchmark_ontology, build_instances, ns, BenchGraphs,
    Scale,
};
use crate::bench::metrics::RunMetrics;
use crate::bench::oracle::{
    check_plan_constraints, false_positives, mutation_accuracy, precision_recall,
};
use crate::bench::trace::{Trace, TraceEvent};
use crate::config::AppLimits;
use crate::executor::{graph_count, graph_hash, Executor, ExecutorError};
use crate::model::{MockModel, ModelCompletion, ToolCall};
use crate::ontology::OntologyReasoner;
use crate::plan::{
    Operation, OperationContext, OutputType, Plan, PlanStep, StepArguments, TypedValue, validate_plan,
};
use crate::planner::{
    AgentBackend, AgentStructuredPlanner, AgentToolPlanner, AgentTwoPassPlanner,
    NeedleSequentialPlanner, Planner, PlanningServices, UserRequest, check_prose_consistency,
};
use crate::policy::{AuditLog, CommitStatus, ExecutionContext, Operation as PolicyOp};
use crate::rdf::{GraphName, OxigraphRepository, Quad, Term};
use crate::search::TantivyIndex;
use crate::tools::{ToolRegistry, ToolState};

// ---------------------------------------------------------------------------
// Variants + ablations
// ---------------------------------------------------------------------------

/// The four planning architectures under comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Variant {
    A,
    B,
    C,
    D,
}

impl Variant {
    pub fn all() -> [Variant; 4] {
        [Variant::A, Variant::B, Variant::C, Variant::D]
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Variant::A => "A",
            Variant::B => "B",
            Variant::C => "C",
            Variant::D => "D",
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Variant::A => "A: Needle sequential",
            Variant::B => "B: agent tool loop",
            Variant::C => "C: structured single-shot",
            Variant::D => "D: two-pass NL + extraction",
        }
    }

    /// Deterministic model id used by the scripted substitutes, so request
    /// logs prove id propagation per variant.
    pub fn script_model_id(&self) -> &'static str {
        match self {
            Variant::A => "t2-bench-a",
            Variant::B => "t2-bench-b",
            Variant::C => "t2-bench-c",
            Variant::D => "t2-bench-d",
        }
    }
}

/// Ablation environments (TESTING_1 §13): pinpoints whether a failure is
/// discovery, query planning, or mutation planning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ablation {
    FullTools,
    PreResolvedVocab,
    PrecomputedQuery,
    PrecomputedPlan,
}

impl Ablation {
    pub fn all() -> [Ablation; 4] {
        [
            Ablation::FullTools,
            Ablation::PreResolvedVocab,
            Ablation::PrecomputedQuery,
            Ablation::PrecomputedPlan,
        ]
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Ablation::FullTools => "full-tools",
            Ablation::PreResolvedVocab => "pre-resolved-vocabulary",
            Ablation::PrecomputedQuery => "precomputed-query-results",
            Ablation::PrecomputedPlan => "precomputed-plan",
        }
    }

    /// Structural tool-list variation. The scripted harness synthesizes plans
    /// from `required_ops` (not from tool outputs), so every ablation still
    /// validates — the matrix proves the harness runs each environment;
    /// live runs (with real discovery) are where ablations diverge.
    pub fn apply(&self, case: &BenchmarkCase) -> BenchmarkCase {
        let mut c = case.clone();
        match self {
            Ablation::FullTools => {}
            Ablation::PreResolvedVocab => {
                c.tools.retain(|t| t != "search_vocabulary" && t != "describe_vocabulary");
            }
            Ablation::PrecomputedQuery => {
                c.tools.retain(|t| {
                    t != "search_instances"
                        && t != "query"
                        && t != "inspect_query"
                        && t != "build_query"
                        && t != "execute_query"
                });
            }
            Ablation::PrecomputedPlan => {
                c.tools.clear();
            }
        }
        c
    }
}

// ---------------------------------------------------------------------------
// Fixture: isolated bench store + planning services (read-only)
// ---------------------------------------------------------------------------

pub struct BenchFixture {
    pub services: PlanningServices,
    pub graphs: BenchGraphs,
    pub repo: Arc<OxigraphRepository>,
    pub audit: Arc<AuditLog>,
}

impl BenchFixture {
    pub fn tenant_a(&self) -> GraphName {
        self.graphs.tenant_a.clone()
    }

    pub fn hash_tenant_a(&self) -> String {
        graph_hash(&*self.repo, &self.graphs.tenant_a)
    }
}

/// Fresh isolated fixture: bench ontology + tiny instances + adversarial
/// pack, read-only execution context. Planning must never write.
pub fn build_fixture() -> BenchFixture {
    let repo = Arc::new(OxigraphRepository::new_in_memory(200_000).unwrap());
    let graphs = BenchGraphs::new();
    build_benchmark_ontology(&*repo).unwrap();
    build_instances(&*repo, &graphs, Scale::Tiny).unwrap();
    build_adversarial(&*repo, &graphs).unwrap();
    let reasoner = Arc::new(OntologyReasoner::from_repository(&*repo).unwrap());
    let search = Arc::new(TantivyIndex::in_ram().unwrap());
    let audit = Arc::new(AuditLog::in_memory());
    let state = ToolState::new(
        repo.clone(),
        search,
        reasoner,
        bench_shapes(),
        audit.clone(),
        AppLimits::default(),
    );
    let exec_ctx = ExecutionContext::new(
        "t2-bench",
        "bench",
        "bench",
        vec![graphs.tenant_a.clone(), GraphName::ontology()],
        vec![PolicyOp::Read],
    );
    let services = PlanningServices::new(Arc::new(ToolRegistry::new()), state, exec_ctx);
    BenchFixture {
        services,
        graphs,
        repo,
        audit,
    }
}

// ---------------------------------------------------------------------------
// Deterministic plan synthesis from a case's required_ops
// ---------------------------------------------------------------------------

fn request_terms(request: &str) -> Vec<String> {
    let mut terms: Vec<String> = request
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 2)
        .take(3)
        .map(|w| w.to_string())
        .collect();
    if terms.is_empty() {
        terms.push("contact".to_string());
    }
    terms
}

fn has_op(steps: &[PlanStep], op: Operation) -> bool {
    steps.iter().any(|s| s.operation == op)
}

fn step_id_for(steps: &[PlanStep], op: Operation) -> Option<String> {
    steps.iter().find(|s| s.operation == op).map(|s| s.id.clone())
}

/// Synthesize a validator-clean plan covering `case.expected.required_ops`.
///
/// `validate`/`verify` on read-only cases are *skipped* (they require a
/// `MutationResult` dependency, which a read-only plan cannot produce);
/// they surface as plan-constraint failures while schema validity passes.
/// Unknown op names are likewise left missing. Returns the plan plus the
/// required ops deliberately skipped.
pub fn synth_plan(case: &BenchmarkCase) -> (Plan, Vec<String>) {
    let mut steps: Vec<PlanStep> = vec![];
    let mut skipped: Vec<String> = vec![];
    let mut n: usize = 0;
    n += 1;
    let s1 = format!("s{n}");
    steps.push(PlanStep {
        id: s1.clone(),
        operation: Operation::SearchVocabulary,
        intent: format!("discover vocabulary for {}", case.id),
        args: StepArguments::SearchVocab {
            terms: request_terms(&case.request),
            namespaces: vec![],
        },
        depends_on: vec![],
        output: OutputType::VocabularyCandidates,
    });
    n += 1;
    let s2 = format!("s{n}");
    steps.push(PlanStep {
        id: s2.clone(),
        operation: Operation::DescribeVocabulary,
        intent: format!("resolve vocabulary for {}", case.id),
        args: StepArguments::DescribeVocab {
            iris: vec![TypedValue::StepRef {
                step: s1.clone(),
                field: "candidates".to_string(),
            }],
        },
        depends_on: vec![s1.clone()],
        output: OutputType::ResolvedVocabulary,
    });

    let ensure_search_instances = |steps: &mut Vec<PlanStep>, n: &mut usize| -> String {
        if let Some(id) = step_id_for(steps, Operation::SearchInstances) {
            return id;
        }
        *n += 1;
        let id = format!("s{n}");
        let s2id = steps
            .iter()
            .find(|s| s.operation == Operation::DescribeVocabulary)
            .map(|s| s.id.clone())
            .unwrap();
        steps.push(PlanStep {
            id: id.clone(),
            operation: Operation::SearchInstances,
            intent: format!("find candidate resources for {}", case.id),
            args: StepArguments::SearchInstances {
                text: case.request.chars().take(120).collect(),
                types: vec![],
                namespace: None,
            },
            depends_on: vec![s2id],
            output: OutputType::ResourceCandidates,
        });
        id
    };

    let ensure_query = |steps: &mut Vec<PlanStep>, n: &mut usize| -> String {
        if let Some(id) = step_id_for(steps, Operation::Query) {
            return id;
        }
        *n += 1;
        let id = format!("s{n}");
        let s2id = steps
            .iter()
            .find(|s| s.operation == Operation::DescribeVocabulary)
            .map(|s| s.id.clone())
            .unwrap();
        steps.push(PlanStep {
            id: id.clone(),
            operation: Operation::Query,
            intent: format!("select matching resources for {}", case.id),
            args: StepArguments::Query {
                pattern: serde_json::json!({"intent": case.id}),
            },
            depends_on: vec![s2id],
            output: OutputType::UriSet,
        });
        id
    };

    for req in &case.expected.required_ops {
        match req.as_str() {
            "search_vocabulary" | "discover_vocabulary" => {}
            "describe_vocabulary" => {}
            "search_instances" => {
                if !has_op(&steps, Operation::SearchInstances) {
                    let sid = ensure_search_instances(&mut steps, &mut n);
                    let _ = sid;
                }
            }
            "describe_resource" => {
                let search_id = ensure_search_instances(&mut steps, &mut n);
                if !has_op(&steps, Operation::DescribeResource) {
                    n += 1;
                    let id = format!("s{n}");
                    steps.push(PlanStep {
                        id: id.clone(),
                        operation: Operation::DescribeResource,
                        intent: format!("inspect resources for {}", case.id),
                        args: StepArguments::DescribeResource {
                            uris: vec![TypedValue::StepRef {
                                step: search_id.clone(),
                                field: "resource".to_string(),
                            }],
                        },
                        depends_on: vec![search_id],
                        output: OutputType::ResourceDescriptions,
                    });
                }
            }
            "query" | "construct_query" | "execute_query" => {
                ensure_query(&mut steps, &mut n);
            }
            "create" => {
                if !has_op(&steps, Operation::Create) {
                    n += 1;
                    let id = format!("s{n}");
                    let s2id = step_id_for(&steps, Operation::DescribeVocabulary).unwrap();
                    steps.push(PlanStep {
                        id: id.clone(),
                        operation: Operation::Create,
                        intent: format!("create resource for {}", case.id),
                        args: StepArguments::Create {
                            types: vec![TypedValue::Iri {
                                iri: ns::iri(ns::CRM, "Contact"),
                            }],
                            properties: vec![],
                        },
                        depends_on: vec![s2id],
                        output: OutputType::MutationResult,
                    });
                }
            }
            "update" => {
                if !has_op(&steps, Operation::Update) {
                    let q = ensure_query(&mut steps, &mut n);
                    let s2id = step_id_for(&steps, Operation::DescribeVocabulary).unwrap();
                    n += 1;
                    let id = format!("s{n}");
                    steps.push(PlanStep {
                        id: id.clone(),
                        operation: Operation::Update,
                        intent: format!("update matching resources for {}", case.id),
                        args: StepArguments::Update {
                            targets: vec![TypedValue::UriSetRef { step: q.clone() }],
                            predicate: TypedValue::Iri {
                                iri: ns::iri(ns::CRM, "name"),
                            },
                            objects: vec![TypedValue::Iri {
                                iri: ns::iri(ns::SALES, "ActiveStatus"),
                            }],
                        },
                        depends_on: vec![s2id, q],
                        output: OutputType::MutationResult,
                    });
                }
            }
            "delete" => {
                if !has_op(&steps, Operation::Delete) {
                    let q = ensure_query(&mut steps, &mut n);
                    n += 1;
                    let id = format!("s{n}");
                    steps.push(PlanStep {
                        id: id.clone(),
                        operation: Operation::Delete,
                        intent: format!("remove matching resources for {}", case.id),
                        args: StepArguments::Delete {
                            targets: vec![TypedValue::UriSetRef { step: q.clone() }],
                        },
                        depends_on: vec![q],
                        output: OutputType::MutationResult,
                    });
                }
            }
            "validate" => {
                if has_op(&steps, Operation::Validate) {
                    continue;
                }
                if let Some(mid) = steps
                    .iter()
                    .rev()
                    .find(|s| s.operation.is_mutation())
                    .map(|s| s.id.clone())
                {
                    n += 1;
                    let id = format!("s{n}");
                    steps.push(PlanStep {
                        id: id.clone(),
                        operation: Operation::Validate,
                        intent: format!("validate graph for {}", case.id),
                        args: StepArguments::Validate {},
                        depends_on: vec![mid],
                        output: OutputType::ValidationResult,
                    });
                } else {
                    skipped.push(req.clone());
                }
            }
            "verify" => {
                if has_op(&steps, Operation::Verify) {
                    continue;
                }
                let dep = step_id_for(&steps, Operation::Validate)
                    .or_else(|| {
                        steps
                            .iter()
                            .rev()
                            .find(|s| s.operation.is_mutation())
                            .map(|s| s.id.clone())
                    });
                if let Some(d) = dep {
                    n += 1;
                    let id = format!("s{n}");
                    steps.push(PlanStep {
                        id: id.clone(),
                        operation: Operation::Verify,
                        intent: format!("verify outcome for {}", case.id),
                        args: StepArguments::Verify {
                            question: format!("did {} succeed?", case.id),
                        },
                        depends_on: vec![d],
                        output: OutputType::VerificationResult,
                    });
                } else {
                    skipped.push(req.clone());
                }
            }
            _ => {
                skipped.push(req.clone());
            }
        }
    }

    // Operational rule: mutations need terminal validate + verify.
    if steps.iter().any(|s| s.operation.is_mutation()) {
        if !has_op(&steps, Operation::Validate) {
            let mid = steps
                .iter()
                .rev()
                .find(|s| s.operation.is_mutation())
                .map(|s| s.id.clone())
                .unwrap();
            n += 1;
            let id = format!("s{n}");
            steps.push(PlanStep {
                id: id.clone(),
                operation: Operation::Validate,
                intent: format!("validate graph for {}", case.id),
                args: StepArguments::Validate {},
                depends_on: vec![mid],
                output: OutputType::ValidationResult,
            });
        }
        if !has_op(&steps, Operation::Verify) {
            let dep = step_id_for(&steps, Operation::Validate).unwrap();
            n += 1;
            let id = format!("s{n}");
            steps.push(PlanStep {
                id: id.clone(),
                operation: Operation::Verify,
                intent: format!("verify outcome for {}", case.id),
                args: StepArguments::Verify {
                    question: format!("did {} succeed?", case.id),
                },
                depends_on: vec![dep],
                output: OutputType::VerificationResult,
            });
        }
    }

    (
        Plan {
            goal: case.request.clone(),
            context: case.context.clone(),
            steps,
            assumptions: vec![],
        },
        skipped,
    )
}

/// Numbered prose rendering of a plan for Option D pass 1, with explicit
/// `depends on step N` mentions the consistency checker parses.
pub fn prose_for_plan(plan: &Plan) -> String {
    let pos: HashMap<&str, usize> = plan
        .steps
        .iter()
        .enumerate()
        .map(|(i, s)| (s.id.as_str(), i + 1))
        .collect();
    plan.steps
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let deps = if s.depends_on.is_empty() {
                "no dependencies".to_string()
            } else {
                s.depends_on
                    .iter()
                    .filter_map(|d| pos.get(d.as_str()).map(|p| format!("depends on step {p}")))
                    .collect::<Vec<_>>()
                    .join(" and ")
            };
            format!(
                "{}. {} ({}); {deps}.",
                i + 1,
                s.intent,
                s.operation.as_str()
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// Scripted substitutes through the real planner code paths
// ---------------------------------------------------------------------------

fn output_name(o: OutputType) -> &'static str {
    match o {
        OutputType::VocabularyCandidates => "VocabularyCandidates",
        OutputType::ResolvedVocabulary => "ResolvedVocabulary",
        OutputType::ResourceCandidates => "ResourceCandidates",
        OutputType::ResourceDescriptions => "ResourceDescriptions",
        OutputType::UriSet => "UriSet",
        OutputType::MutationResult => "MutationResult",
        OutputType::ValidationResult => "ValidationResult",
        OutputType::VerificationResult => "VerificationResult",
    }
}

fn add_step_args(step: &PlanStep) -> serde_json::Value {
    serde_json::json!({
        "id": step.id,
        "operation": step.operation.as_str(),
        "intent": step.intent,
        "args": serde_json::to_value(&step.args).unwrap(),
        "depends_on": step.depends_on,
        "output": output_name(step.output),
    })
}

/// MockModel script for Option A: one `add_step` turn per step, then
/// `finish_plan`.
pub fn mock_model_for_plan(plan: &Plan) -> MockModel {
    let mut script: Vec<ModelCompletion> = plan
        .steps
        .iter()
        .enumerate()
        .map(|(i, s)| {
            ModelCompletion::ToolCalls(vec![ToolCall::new(
                "add_step",
                add_step_args(s),
                format!("t2-a-{i}"),
            )])
        })
        .collect();
    script.push(ModelCompletion::ToolCalls(vec![ToolCall::new(
        "finish_plan",
        serde_json::json!({"rationale": "scripted T2 plan", "assumptions": []}),
        "t2-a-finish".to_string(),
    )]));
    MockModel::new(script)
}

/// Tool-call turns for Option B's stub backend.
pub fn tool_turns_for_plan(plan: &Plan) -> Vec<Vec<(String, serde_json::Value)>> {
    let mut turns: Vec<Vec<(String, serde_json::Value)>> = plan
        .steps
        .iter()
        .map(|s| vec![("add_step".to_string(), add_step_args(s))])
        .collect();
    turns.push(vec![(
        "finish_plan".to_string(),
        serde_json::json!({"rationale": "scripted T2 plan", "assumptions": []}),
    )]);
    turns
}

pub fn plan_json_for_plan(plan: &Plan) -> String {
    serde_json::to_string(plan).unwrap()
}

/// Scripted OpenAI-compatible backend for B/C/D: queued texts or tool turns.
pub struct ScriptBackend {
    responses: Mutex<VecDeque<agentstream::backends::BackendResponse>>,
    seen_models: Mutex<Vec<String>>,
    seen_tool_counts: Mutex<Vec<usize>>,
}

impl ScriptBackend {
    pub fn with_texts(texts: Vec<String>) -> Self {
        Self {
            responses: Mutex::new(
                texts
                    .into_iter()
                    .map(|t| agentstream::backends::BackendResponse {
                        text: t,
                        finish_reason: Some("stop".to_string()),
                        ..Default::default()
                    })
                    .collect(),
            ),
            seen_models: Mutex::new(vec![]),
            seen_tool_counts: Mutex::new(vec![]),
        }
    }

    pub fn with_tool_turns(turns: Vec<Vec<(String, serde_json::Value)>>) -> Self {
        Self {
            responses: Mutex::new(
                turns
                    .into_iter()
                    .map(|calls| agentstream::backends::BackendResponse {
                        text: String::new(),
                        tool_calls: calls
                            .into_iter()
                            .enumerate()
                            .map(|(i, (name, input))| agentstream::backends::ToolCallData {
                                name,
                                input,
                                id: Some(format!("t2-call-{i}")),
                            })
                            .collect(),
                        finish_reason: Some("tool_calls".to_string()),
                        ..Default::default()
                    })
                    .collect(),
            ),
            seen_models: Mutex::new(vec![]),
            seen_tool_counts: Mutex::new(vec![]),
        }
    }

    pub fn models(&self) -> Vec<String> {
        self.seen_models.lock().unwrap().clone()
    }

    pub fn request_count(&self) -> usize {
        self.seen_models.lock().unwrap().len()
    }
}

#[async_trait]
impl agentstream::backends::LLMBackend for ScriptBackend {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn name(&self) -> &'static str {
        "t2-script"
    }
    async fn complete(
        &self,
        request: agentstream::backends::BackendRequest,
        _abort: Option<agentstream::backends::AbortSignal>,
    ) -> Result<agentstream::backends::BackendResponse, String> {
        self.seen_models.lock().unwrap().push(request.model.clone());
        self.seen_tool_counts.lock().unwrap().push(request.tools.len());
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| "t2 script exhausted".to_string())
    }
}

fn script_agent_backend(
    model: &str,
    responses: ScriptResponses,
) -> (Arc<ScriptBackend>, AgentBackend) {
    let script = Arc::new(match responses {
        ScriptResponses::Texts(t) => ScriptBackend::with_texts(t),
        ScriptResponses::Turns(t) => ScriptBackend::with_tool_turns(t),
    });
    let backend: Arc<dyn agentstream::backends::LLMBackend> = script.clone();
    (script, AgentBackend::new(backend, model))
}

enum ScriptResponses {
    Texts(Vec<String>),
    Turns(Vec<Vec<(String, serde_json::Value)>>),
}

/// Build the four scripted planners for one case. Each drives its real
/// planner loop; only the model bytes are scripted from `synth_plan`.
pub struct ScriptedVariants {
    pub plan: Plan,
    pub skipped: Vec<String>,
    pub prose: String,
}

impl ScriptedVariants {
    /// Build from an explicit plan (T3 executable plans); same scripted
    /// bytes per variant as [`ScriptedVariants::for_case`].
    pub fn for_plan(plan: Plan) -> Self {
        let prose = prose_for_plan(&plan);
        Self { plan, skipped: vec![], prose }
    }

    pub fn for_case(case: &BenchmarkCase) -> Self {
        let (plan, skipped) = synth_plan(case);
        let prose = prose_for_plan(&plan);
        Self {
            plan,
            skipped,
            prose,
        }
    }

    pub fn planner_a(&self, services: &PlanningServices) -> NeedleSequentialPlanner {
        NeedleSequentialPlanner::new(
            Box::new(mock_model_for_plan(&self.plan)),
            services.clone(),
        )
    }

    pub fn planner_b(
        &self,
        services: &PlanningServices,
    ) -> (AgentToolPlanner, Arc<ScriptBackend>) {
        let (script, backend) = script_agent_backend(
            Variant::B.script_model_id(),
            ScriptResponses::Turns(tool_turns_for_plan(&self.plan)),
        );
        (AgentToolPlanner::new(backend, services.clone()), script)
    }

    pub fn planner_c(&self) -> (AgentStructuredPlanner, Arc<ScriptBackend>) {
        let (script, backend) = script_agent_backend(
            Variant::C.script_model_id(),
            ScriptResponses::Texts(vec![plan_json_for_plan(&self.plan)]),
        );
        (AgentStructuredPlanner::new(backend), script)
    }

    pub fn planner_d(&self) -> (AgentTwoPassPlanner, Arc<ScriptBackend>) {
        let (script, backend) = script_agent_backend(
            Variant::D.script_model_id(),
            ScriptResponses::Texts(vec![
                self.prose.clone(),
                plan_json_for_plan(&self.plan),
            ]),
        );
        (AgentTwoPassPlanner::new(backend), script)
    }
}

/// Planner-to-executor join for a prebuilt executable plan (T5 tests):
/// scripted bytes per variant through the real planner loop; the produced
/// plan must equal the canned executable plan.
pub async fn join_for_test(
    variant: Variant,
    sv: &ScriptedVariants,
    exec_plan: &Plan,
    fixture: &ExecFixture,
) -> Plan {
    let mut ctx = OperationContext::new(exec_plan.goal.clone(), exec_plan.context.clone());
    let req = UserRequest::new(exec_plan.goal.clone());
    let produced = match variant {
        Variant::A => {
            let p = sv.planner_a(&fixture.services);
            p.create_plan(&req, &mut ctx).await.expect("scripted A plans")
        }
        Variant::B => {
            let (p, _s) = sv.planner_b(&fixture.services);
            p.create_plan(&req, &mut ctx).await.expect("scripted B plans")
        }
        Variant::C => {
            let (p, _s) = sv.planner_c();
            p.create_plan(&req, &mut ctx).await.expect("scripted C plans")
        }
        Variant::D => {
            let (p, _s) = sv.planner_d();
            p.create_plan(&req, &mut ctx).await.expect("scripted D plans")
        }
    };
    assert_eq!(produced, *exec_plan, "variant {} join", variant.as_str());
    produced
}

// ---------------------------------------------------------------------------
// Planning-only run + metrics
// ---------------------------------------------------------------------------

/// Per-run planning scores. Execution fields stay unset (T3 owns them).
#[derive(Debug, Clone)]
pub struct PlanningScores {
    pub schema_valid: bool,
    pub validation_errors: Vec<String>,
    pub plan_failures: Vec<String>,
    pub skipped_required: Vec<String>,
    pub dependency_ok: bool,
    pub termination_ok: bool,
    pub vocab_ok: bool,
    pub query_ok: bool,
    pub steps: usize,
    pub consistency_ok: Option<bool>,
}

impl PlanningScores {
    pub fn complete(&self) -> bool {
        self.plan_failures.is_empty()
    }
}

fn is_dep_error(code: &str) -> bool {
    matches!(code, "dependency" | "dangling" | "cycle" | "types")
}

/// One planning-only benchmark run.
#[derive(Debug, Clone)]
pub struct PlanningRun {
    pub case_id: String,
    pub variant: Variant,
    pub ablation: Ablation,
    pub plan: Option<Plan>,
    pub planner_error: Option<String>,
    pub scores: PlanningScores,
    pub trace: Trace,
    pub latency_ms: u64,
    pub audit_added: usize,
    pub zero_writes: bool,
    pub model_ids: Vec<String>,
    pub turns: usize,
}

impl PlanningRun {
    pub fn to_run_metrics(&self) -> RunMetrics {
        RunMetrics {
            success: self.scores.schema_valid && self.scores.complete(),
            plan_complete: self.scores.complete(),
            vocab_ok: self.scores.vocab_ok,
            query_ok: self.scores.query_ok,
            mutation_ok: true,
            scope_ok: self.zero_writes,
            recovery_ok: true,
            termination_ok: self.scores.termination_ok,
            model_calls: self.turns,
            tool_calls: if matches!(self.variant, Variant::A | Variant::B) {
                self.scores.steps
            } else {
                0
            },
            invalid_tool_calls: 0,
            schema_failures: if self.scores.schema_valid { 0 } else { 1 },
            planning_latency_ms: self.latency_ms,
            execution_latency_ms: 0,
            replans: 0,
            tokens_in: None,
            tokens_out: None,
        }
    }
}

fn vocab_required(case: &BenchmarkCase) -> bool {
    case.expected.required_ops.iter().any(|o| {
        o == "search_vocabulary" || o == "describe_vocabulary" || o == "discover_vocabulary"
    })
}

fn query_required(case: &BenchmarkCase) -> bool {
    case.expected.required_ops.iter().any(|o| {
        o == "query" || o == "construct_query" || o == "execute_query"
    })
}

/// Run one planning-only trial: plan, validate, constraint-check. The repo
/// must be untouched afterwards (zero-write gate).
pub async fn run_planning_only(
    planner: &dyn Planner,
    case: &BenchmarkCase,
    fixture: &BenchFixture,
    variant: Variant,
    ablation: Ablation,
) -> PlanningRun {
    let mut ctx = OperationContext::new(case.request.clone(), case.context.clone());
    let mut trace = Trace::new();
    trace.record(TraceEvent::RequestReceived {
        request: case.request.clone(),
    });
    trace.record(TraceEvent::PlanningStarted {
        variant: variant.as_str().to_string(),
    });
    let hash_before = fixture.hash_tenant_a();
    let audit_before = fixture.audit.len();
    let start = Instant::now();
    let request = UserRequest::new(case.request.clone());
    let result = planner.create_plan(&request, &mut ctx).await;
    let latency_ms = start.elapsed().as_millis() as u64;

    let hash_after = fixture.hash_tenant_a();
    let audit_after = fixture.audit.len();
    let zero_writes = hash_before == hash_after && audit_before == audit_after;

    match result {
        Ok(plan) => {
            let steps = plan.steps.len();
            trace.record(TraceEvent::PlanCompleted { steps });
            let validation = validate_plan(&plan);
            let schema_valid = validation.is_ok();
            let validation_errors: Vec<String> = match &validation {
                Ok(()) => vec![],
                Err(errs) => {
                    trace.record(TraceEvent::SemanticValidation {
                        errors: errs.len(),
                    });
                    errs.iter().map(|e| format!("{e}")).collect()
                }
            };
            let dependency_ok = match &validation {
                Ok(()) => true,
                Err(errs) => !errs.iter().any(|e| is_dep_error(&e.code)),
            };
            let plan_failures = check_plan_constraints(&plan, &case.expected.required_ops);
            let vocab_ok = if vocab_required(case) {
                plan.steps.iter().any(|s| {
                    s.operation == Operation::SearchVocabulary
                        || s.operation == Operation::DescribeVocabulary
                })
            } else {
                true
            };
            let query_ok = if query_required(case) {
                plan.steps.iter().any(|s| s.operation == Operation::Query)
            } else {
                true
            };
            // Termination (per-variant rules): a returned plan proves
            // A-finish_plan / B-stop-vs-call / C-single-output; D additionally
            // needs prose→structure preservation from the retained artifacts.
            let ids_unique = {
                let mut seen = HashSet::new();
                plan.steps.iter().all(|s| seen.insert(s.id.clone()))
            };
            let base_term = !plan.steps.is_empty() && steps <= 16 && ids_unique;
            let consistency_ok = if variant == Variant::D {
                match ctx.artifact("nl_plan") {
                    Some(prose) => Some(check_prose_consistency(prose, &plan).is_ok()),
                    None => Some(false),
                }
            } else {
                None
            };
            let termination_ok = match variant {
                Variant::D => base_term && consistency_ok == Some(true),
                _ => base_term,
            };
            trace.record(TraceEvent::Finished {
                outcome: if schema_valid && plan_failures.is_empty() {
                    "planning-ok".to_string()
                } else {
                    "planning-issues".to_string()
                },
            });
            maybe_write_recording(variant, &case.id, Some(&plan), None);
            PlanningRun {
                case_id: case.id.clone(),
                variant,
                ablation,
                plan: Some(plan),
                planner_error: None,
                scores: PlanningScores {
                    schema_valid,
                    validation_errors,
                    plan_failures,
                    skipped_required: vec![],
                    dependency_ok,
                    termination_ok,
                    vocab_ok,
                    query_ok,
                    steps,
                    consistency_ok,
                },
                trace,
                latency_ms,
                audit_added: audit_after - audit_before,
                zero_writes,
                model_ids: vec![variant.script_model_id().to_string()],
                turns: script_turns(variant, steps),
            }
        }
        Err(e) => {
            let msg = e.to_string();
            trace.record(TraceEvent::Finished {
                outcome: format!("planner-error: {msg}"),
            });
            maybe_write_recording(variant, &case.id, None, Some(&msg));
            PlanningRun {
                case_id: case.id.clone(),
                variant,
                ablation,
                plan: None,
                planner_error: Some(msg),
                scores: PlanningScores {
                    schema_valid: false,
                    validation_errors: vec!["planner-error".to_string()],
                    plan_failures: case.expected.required_ops.clone(),
                    skipped_required: vec![],
                    dependency_ok: false,
                    termination_ok: false,
                    vocab_ok: false,
                    query_ok: false,
                    steps: 0,
                    consistency_ok: if variant == Variant::D { Some(false) } else { None },
                },
                trace,
                latency_ms,
                audit_added: audit_after - audit_before,
                zero_writes,
                model_ids: vec![variant.script_model_id().to_string()],
                turns: 0,
            }
        }
    }
}

fn script_turns(variant: Variant, steps: usize) -> usize {
    match variant {
        Variant::A | Variant::B => steps + 1,
        Variant::C => 1,
        Variant::D => 2,
    }
}

// ---------------------------------------------------------------------------
// Aggregates + report
// ---------------------------------------------------------------------------

/// Aggregate planning scores for one variant × case set.
#[derive(Debug, Clone, Default)]
pub struct PlanningAggregate {
    pub n: usize,
    pub schema_rate: f64,
    pub complete_rate: f64,
    pub dependency_rate: f64,
    pub termination_rate: f64,
    pub vocab_rate: f64,
    pub query_rate: f64,
    pub zero_write_rate: f64,
    pub avg_steps: f64,
    pub avg_latency_ms: f64,
}

impl PlanningAggregate {
    pub fn from_runs(runs: &[PlanningRun]) -> Self {
        let n = runs.len();
        if n == 0 {
            return Self::default();
        }
        let rate = |f: fn(&PlanningRun) -> bool| {
            runs.iter().filter(|r| f(r)).count() as f64 / n as f64
        };
        Self {
            n,
            schema_rate: rate(|r| r.scores.schema_valid),
            complete_rate: rate(|r| r.scores.complete()),
            dependency_rate: rate(|r| r.scores.dependency_ok),
            termination_rate: rate(|r| r.scores.termination_ok),
            vocab_rate: rate(|r| r.scores.vocab_ok),
            query_rate: rate(|r| r.scores.query_ok),
            zero_write_rate: rate(|r| r.zero_writes),
            avg_steps: runs.iter().map(|r| r.scores.steps).sum::<usize>() as f64 / n as f64,
            avg_latency_ms: runs.iter().map(|r| r.latency_ms).sum::<u64>() as f64 / n as f64,
        }
    }

    /// Non-adversarial subset (Difficulty::Adversarial excluded) for the
    /// ≥80% schema-validity floor.
    pub fn from_runs_non_adversarial(runs: &[PlanningRun], cases: &[BenchmarkCase]) -> Self {
        let adv: HashSet<&str> = cases
            .iter()
            .filter(|c| c.difficulty == Difficulty::Adversarial)
            .map(|c| c.id.as_str())
            .collect();
        let kept: Vec<PlanningRun> = runs
            .iter()
            .filter(|r| !adv.contains(r.case_id.as_str()))
            .cloned()
            .collect();
        Self::from_runs(&kept)
    }
}

/// TESTING_1 §9-style planning comparison table (A–D columns).
pub fn render_planning_table(by_variant: &HashMap<String, PlanningAggregate>) -> String {
    let cell = |v: Option<&PlanningAggregate>, f: &dyn Fn(&PlanningAggregate) -> String| {
        v.map(f).unwrap_or_else(|| "—".to_string())
    };
    let pct = |r: f64| format!("{:.0}%", r * 100.0);
    let mut out =
        String::from("| Metric | A: Needle | B: Tools | C: Structured | D: Two-pass |\n");
    out.push_str("| --- | --- | --- | --- | --- |\n");
    let row = |name: &str, f: &dyn Fn(&PlanningAggregate) -> String| {
        format!(
            "| {name} | {} | {} | {} | {} |\n",
            cell(by_variant.get("A"), f),
            cell(by_variant.get("B"), f),
            cell(by_variant.get("C"), f),
            cell(by_variant.get("D"), f),
        )
    };
    out.push_str(&row("Schema validity", &|a| pct(a.schema_rate)));
    out.push_str(&row("Completeness", &|a| pct(a.complete_rate)));
    out.push_str(&row("Dependency accuracy", &|a| pct(a.dependency_rate)));
    out.push_str(&row("Termination accuracy", &|a| pct(a.termination_rate)));
    out.push_str(&row("Vocabulary accuracy", &|a| pct(a.vocab_rate)));
    out.push_str(&row("Query-intent accuracy", &|a| pct(a.query_rate)));
    out.push_str(&row("Zero writes", &|a| pct(a.zero_write_rate)));
    out.push_str(&row("Avg. steps", &|a| format!("{:.1}", a.avg_steps)));
    out.push_str(&row("Avg. latency (ms)", &|a| format!("{:.1}", a.avg_latency_ms)));
    out
}

/// Ablation matrix (rows) × variant (columns): schema-validity pass/fail.
pub fn render_ablation_matrix(
    results: &HashMap<(String, String), bool>,
) -> String {
    let mut out = String::from("| Ablation | A | B | C | D |\n");
    out.push_str("| --- | --- | --- | --- |\n");
    for ab in Ablation::all() {
        let mark = |v: Variant| {
            results
                .get(&(ab.as_str().to_string(), v.as_str().to_string()))
                .map(|ok| if *ok { "pass" } else { "FAIL" }.to_string())
                .unwrap_or_else(|| "—".to_string())
        };
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            ab.as_str(),
            mark(Variant::A),
            mark(Variant::B),
            mark(Variant::C),
            mark(Variant::D),
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// Recordings (proxy pattern, same as real_backends): RECORD=1 writes,
// default CI replays the scripted synthesis deterministically.
// ---------------------------------------------------------------------------

pub fn recording_path(variant: Variant, case_id: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("recordings")
        .join("t2")
        .join(variant.as_str().to_lowercase())
        .join(format!("{case_id}.json"))
}

fn maybe_write_recording(variant: Variant, case_id: &str, plan: Option<&Plan>, error: Option<&str>) {
    if std::env::var("RECORD").map(|v| v == "1").unwrap_or(false) {
        let path = recording_path(variant, case_id);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let value = serde_json::json!({
            "case_id": case_id,
            "variant": variant.as_str(),
            "plan": plan,
            "error": error,
        });
        let _ = std::fs::write(&path, serde_json::to_string_pretty(&value).unwrap());
    }
}

/// Load a recorded plan for replay, if present.
pub fn load_recorded_plan(variant: Variant, case_id: &str) -> Option<Plan> {
    let path = recording_path(variant, case_id);
    let raw = std::fs::read_to_string(&path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    serde_json::from_value(value.get("plan")?.clone()).ok()
}

// ---------------------------------------------------------------------------
// Live-backend helper (opt-in: RUN_MODEL_TESTS=1)
// ---------------------------------------------------------------------------

/// Keyless local backend from the environment (`OPENAI_BASE_URL` /
/// `OPENAI_MODEL`, no key), mirroring `tests/real_backends.rs`.
pub fn live_backend_from_env(model_override: Option<&str>) -> AgentBackend {
    let base = std::env::var("OPENAI_BASE_URL").unwrap_or_else(|_| "http://localhost:8080".to_string());
    let model = model_override
        .map(str::to_string)
        .or_else(|| std::env::var("OPENAI_MODEL").ok())
        .unwrap_or_else(|| "default-local".to_string());
    // Capped at 4092 per turn: the 1B model's reasoning fills whatever
    // context it gets, so bigger budgets only buy longer waits.
    // HTTP timeout stays 1800 s so a full 4092-token trickle still lands.
    let cfg = crate::llm::config_from_vars(Some(&base), Some(&model), None, Some("1800"))
        .expect("live T2 backend config (keyless local)");
    AgentBackend::new(crate::llm::build_backend(&cfg), cfg.model.clone()).with_max_tokens(4092)
}

/// True when live-model runs are requested.
pub fn live_requested() -> bool {
    std::env::var("RUN_MODEL_TESTS").map(|v| v == "1").unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Phase T3 — full execution (final-state correctness)
// ---------------------------------------------------------------------------

const RDF_TYPE_T3: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

/// Write-enabled isolated fixture: same bench dataset as [`build_fixture`]
/// but the execution context permits reads + all mutation classes on the
/// tenant graphs. Every run × variant gets a FRESH fixture (snapshot
/// isolation); cross-run leakage fails the scope gate by construction.
pub struct ExecFixture {
    pub services: PlanningServices,
    pub graphs: BenchGraphs,
    pub repo: Arc<OxigraphRepository>,
    pub audit: Arc<AuditLog>,
    pub registry: Arc<ToolRegistry>,
}

/// Fresh isolated execution fixture at the given instance scale. Tiny is
/// the CI default; Standard backs the bulk-100 / over-limit tiers.
///
/// Read caps are raised above `AppLimits::default` (50 resources) so the
/// 100-tier fits in one query; the safety invariants (25-preview
/// threshold, confirmation gate, authz, SHACL) are untouched.
pub fn build_exec_fixture(scale: Scale) -> ExecFixture {
    let repo = Arc::new(OxigraphRepository::new_in_memory(2_000_000).unwrap());
    let limits = AppLimits {
        max_resources_per_read: 2000,
        max_triples_per_read: 20_000,
        default_page_limit: 2000,
        ..AppLimits::default()
    };
    let graphs = BenchGraphs::new();
    build_benchmark_ontology(&*repo).unwrap();
    build_instances(&*repo, &graphs, scale).unwrap();
    build_adversarial(&*repo, &graphs).unwrap();
    let reasoner = Arc::new(OntologyReasoner::from_repository(&*repo).unwrap());
    let search = Arc::new(TantivyIndex::in_ram().unwrap());
    let audit = Arc::new(AuditLog::in_memory());
    let registry = Arc::new(ToolRegistry::new());
    let state = ToolState::new(
        repo.clone(),
        search,
        reasoner,
        bench_shapes(),
        audit.clone(),
        limits,
    );
    let exec_ctx = ExecutionContext::new(
        "t3-bench",
        "bench",
        "bench",
        vec![
            graphs.tenant_a.clone(),
            graphs.legacy.clone(),
            GraphName::ontology(),
        ],
        vec![
            PolicyOp::Read,
            PolicyOp::Create,
            PolicyOp::Update,
            PolicyOp::Delete,
        ],
    );
    let services = PlanningServices::new(registry.clone(), state, exec_ctx);
    ExecFixture { services, graphs, repo, audit, registry }
}

impl ExecFixture {
    /// Per-graph content hashes for every benchmark graph (scope gate).
    pub fn snapshot_all(&self) -> HashMap<String, String> {
        let mut m = HashMap::new();
        for g in [
            &self.graphs.ontology,
            &self.graphs.tenant_a,
            &self.graphs.tenant_b,
            &self.graphs.import_vocab,
            &self.graphs.legacy,
            &self.graphs.inference,
            &self.graphs.tx,
        ] {
            m.insert(g.to_string(), graph_hash(&*self.repo, g));
        }
        m
    }

    pub fn audit_len(&self) -> usize {
        self.audit.len()
    }
}

/// Real (executable) query shapes. Unlike T2's `{"intent": ...}` stub
/// patterns, every variant here must compile via `build_query` and run via
/// `execute_query` against the bench dataset.
#[derive(Debug, Clone)]
pub enum ExecQueryKind {
    /// All `crm:Contact` subjects in the plan graphs (10 on tiny).
    AllContacts,
    /// All `crm:Account` subjects (3 on tiny).
    AllAccounts,
    /// Contacts `crm:worksFor` an account with direct `geo:stateName "OH"`.
    OhioContacts,
    /// Accounts with direct `geo:stateName "OH"` literal.
    OhioAccounts,
    /// `test:Trap` subjects (adversarial decoys, SHACL-unconstrained).
    TrapKind,
    /// `legacy:customer` subjects in the legacy graph.
    LegacyCustomers,
    /// Every typed subject (`?s rdf:type ?o`) — the poisoned broad scope.
    MatchAllTyped,
    /// Matches nothing (`geo:stateName "ZZ"`) — the empty-scope tier.
    EmptyZZ,
    /// Exactly one contact by literal name (`crm:name` literal match).
    SingleContactName(String),
    /// One exact `legacy:customer` subject pinned by an `Eq` filter on
    /// `?s` (scenario C migrates a created subject without touching the
    // dirty seed subject).
    ExactLegacySubject(String),
}

fn var(name: &str) -> serde_json::Value {
    serde_json::json!({"kind": "var", "name": name})
}

fn iri_pat(iri: &str) -> serde_json::Value {
    serde_json::json!({"kind": "iri", "iri": iri})
}

fn lit_pat(value: &str) -> serde_json::Value {
    serde_json::json!({"kind": "literal", "value": value})
}

fn triple(sub: serde_json::Value, pred: &str, obj: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "subject": sub,
        "predicate": iri_pat(pred),
        "object": obj,
    })
}

/// Build a real typed query pattern for `kind` scoped to `graphs`.
pub fn exec_query_pattern(kind: &ExecQueryKind, graphs: &[GraphName]) -> serde_json::Value {
    use crate::bench::dataset::ns as bns;
    let gs: Vec<String> = graphs.iter().map(|g| g.to_string()).collect();
    let (patterns, return_vars, limit, filters): (
        Vec<serde_json::Value>,
        Vec<String>,
        Option<usize>,
        Vec<serde_json::Value>,
    ) = match kind {
        ExecQueryKind::AllContacts => (
            vec![triple(var("?s"), RDF_TYPE_T3, iri_pat(&bns::iri(bns::CRM, "Contact")))],
            vec!["?s".to_string()],
            None,
            vec![],
        ),
        ExecQueryKind::AllAccounts => (
            vec![triple(var("?s"), RDF_TYPE_T3, iri_pat(&bns::iri(bns::CRM, "Account")))],
            vec!["?s".to_string()],
            None,
            vec![],
        ),
        ExecQueryKind::OhioContacts => (
            vec![
                triple(var("?c"), &bns::iri(bns::CRM, "worksFor"), var("?a")),
                triple(var("?a"), &bns::iri(bns::GEO, "stateName"), lit_pat("OH")),
            ],
            vec!["?c".to_string()],
            None,
            vec![],
        ),
        ExecQueryKind::OhioAccounts => (
            vec![triple(
                var("?a"),
                &bns::iri(bns::GEO, "stateName"),
                lit_pat("OH"),
            )],
            vec!["?a".to_string()],
            None,
            vec![],
        ),
        ExecQueryKind::TrapKind => (
            vec![triple(var("?s"), RDF_TYPE_T3, iri_pat(&bns::iri(bns::TEST, "Trap")))],
            vec!["?s".to_string()],
            None,
            vec![],
        ),
        ExecQueryKind::LegacyCustomers => (
            vec![triple(var("?s"), RDF_TYPE_T3, iri_pat(&bns::iri(bns::LEGACY, "customer")))],
            vec!["?s".to_string()],
            None,
            vec![],
        ),
        ExecQueryKind::MatchAllTyped => (
            vec![triple(var("?s"), RDF_TYPE_T3, var("?o"))],
            vec!["?s".to_string()],
            None,
            vec![],
        ),
        ExecQueryKind::EmptyZZ => (
            vec![triple(
                var("?a"),
                &bns::iri(bns::GEO, "stateName"),
                lit_pat("ZZ"),
            )],
            vec!["?a".to_string()],
            None,
            vec![],
        ),
        ExecQueryKind::SingleContactName(name) => (
            vec![
                triple(var("?s"), RDF_TYPE_T3, iri_pat(&bns::iri(bns::CRM, "Contact"))),
                triple(var("?s"), &bns::iri(bns::CRM, "name"), lit_pat(name)),
            ],
            vec!["?s".to_string()],
            None,
            vec![],
        ),
        ExecQueryKind::ExactLegacySubject(uri) => (
            vec![triple(var("?s"), RDF_TYPE_T3, iri_pat(&bns::iri(bns::LEGACY, "customer")))],
            vec!["?s".to_string()],
            None,
            vec![serde_json::json!({"kind": "eq", "var": "?s", "value": uri})],
        ),
    };
    let mut v = serde_json::json!({
        "operation": "select",
        "graphs": gs,
        "patterns": patterns,
        "filters": filters,
        "return_vars": return_vars,
    });
    if let Some(n) = limit {
        v["limit"] = serde_json::json!(n);
    }
    v
}

/// Independent re-query through the tool registry (never the executor): the
/// oracle path for expected URI sets, post-`verify` agreement, and the
/// scenario-F stale-subset check.
pub async fn independent_query_uris(
    fixture: &ExecFixture,
    pattern: &serde_json::Value,
    graph_ctx: &crate::plan::GraphContext,
) -> Result<Vec<String>, String> {
    let built = fixture
        .registry
        .dispatch_with_context(
            &fixture.services.state,
            &fixture.services.exec_ctx,
            graph_ctx,
            "build_query",
            serde_json::json!({"pattern": pattern}),
        )
        .map_err(|e| e.to_string())?;
    let plan_id = built
        .get("plan_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "build_query returned no plan_id".to_string())?;
    let executed = fixture
        .registry
        .dispatch_with_context(
            &fixture.services.state,
            &fixture.services.exec_ctx,
            graph_ctx,
            "execute_query",
            serde_json::json!({"plan_id": plan_id}),
        )
        .map_err(|e| e.to_string())?;
    let mut uris: Vec<String> = executed
        .get("uris")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    uris.sort();
    uris.dedup();
    Ok(uris)
}

/// Independent `verify` count for the target graph.
pub fn independent_verify_count(
    fixture: &ExecFixture,
    graph_ctx: &crate::plan::GraphContext,
    graph: &GraphName,
) -> Result<u64, String> {
    let out = fixture
        .registry
        .dispatch_with_context(
            &fixture.services.state,
            &fixture.services.exec_ctx,
            graph_ctx,
            "verify",
            serde_json::json!({"question": "t3 verify-count", "graph": graph.to_string()}),
        )
        .map_err(|e| e.to_string())?;
    out.get("quad_count")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| "verify returned no quad_count".to_string())
}

/// The mutation a T3 executable plan performs (oracle-side expectation).
#[derive(Debug, Clone)]
pub enum ExpectedMutation {
    /// Add `(target, predicate, object)` per target URI (add-only update).
    UpdateProbe { predicate: String, object: String },
    /// Remove every quad whose subject is a target URI.
    DeleteSubjects,
    /// Create one `type_iri` resource carrying `name` (auto URI).
    CreateProbe { type_iri: String, name: String },
    /// No mutation: execution must leave every graph untouched.
    ReadOnly,
}

/// Full-execution trial descriptor: which query feeds the mutation, what
/// the mutation is, which graph is the target, and whether the destructive
/// gate gets a confirmation token.
pub struct ExecExpectation {
    pub query: ExecQueryKind,
    pub mutation: ExpectedMutation,
    pub target_graph: GraphName,
    pub confirm_token: Option<String>,
    pub limit: Option<usize>,
}

impl ExecExpectation {
    pub fn update(query: ExecQueryKind, target: GraphName, predicate: String, object: String) -> Self {
        Self { query, mutation: ExpectedMutation::UpdateProbe { predicate, object }, target_graph: target, confirm_token: None, limit: None }
    }

    pub fn with_limit(mut self, n: usize) -> Self {
        self.limit = Some(n);
        self
    }

    pub fn with_confirm(mut self, token: &str) -> Self {
        self.confirm_token = Some(token.to_string());
        self
    }
}

fn pattern_with_limit(mut pattern: serde_json::Value, limit: Option<usize>) -> serde_json::Value {
    if let Some(n) = limit {
        pattern["limit"] = serde_json::json!(n);
    }
    pattern
}

/// Build an executable plan for `expectation`: discovery spine (safe
/// terms) → real typed query → mutation (if any) → validate + verify.
/// Read-only expectations stop after the query (a `validate`/`verify`
/// without a `MutationResult` dep would fail validation — T2 §2 lesson).
pub fn exec_plan_for(label: &str, graphs: &[GraphName], expectation: &ExecExpectation) -> Plan {
    use crate::bench::dataset::ns as bns;
    let ctx = crate::plan::GraphContext {
        graphs: graphs.to_vec(),
        namespaces: [
            ("ex".to_string(), bns::EX.to_string()),
            ("crm".to_string(), bns::CRM.to_string()),
            ("geo".to_string(), bns::GEO.to_string()),
            ("sales".to_string(), bns::SALES.to_string()),
            ("event".to_string(), bns::EVENT.to_string()),
            ("org".to_string(), bns::ORG.to_string()),
            ("legacy".to_string(), bns::LEGACY.to_string()),
            ("test".to_string(), bns::TEST.to_string()),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let mut steps: Vec<PlanStep> = vec![];
    steps.push(PlanStep {
        id: "s1".to_string(),
        operation: Operation::SearchVocabulary,
        intent: format!("discover vocabulary for {label}"),
        args: StepArguments::SearchVocab {
            terms: vec!["contact".to_string(), "account".to_string()],
            namespaces: vec![],
        },
        depends_on: vec![],
        output: OutputType::VocabularyCandidates,
    });
    steps.push(PlanStep {
        id: "s2".to_string(),
        operation: Operation::DescribeVocabulary,
        intent: format!("resolve vocabulary for {label}"),
        args: StepArguments::DescribeVocab {
            iris: vec![TypedValue::StepRef { step: "s1".to_string(), field: "candidates".to_string() }],
        },
        depends_on: vec!["s1".to_string()],
        output: OutputType::ResolvedVocabulary,
    });
    let pattern = pattern_with_limit(
        exec_query_pattern(&expectation.query, graphs),
        expectation.limit,
    );
    steps.push(PlanStep {
        id: "s3".to_string(),
        operation: Operation::Query,
        intent: format!("select targets for {label}"),
        args: StepArguments::Query { pattern },
        depends_on: vec!["s2".to_string()],
        output: OutputType::UriSet,
    });
    match &expectation.mutation {
        ExpectedMutation::ReadOnly => {}
        ExpectedMutation::UpdateProbe { predicate, object } => {
            steps.push(PlanStep {
                id: "s4".to_string(),
                operation: Operation::Update,
                intent: format!("apply probe update for {label}"),
                args: StepArguments::Update {
                    targets: vec![TypedValue::UriSetRef { step: "s3".to_string() }],
                    predicate: TypedValue::Iri { iri: predicate.clone() },
                    objects: vec![TypedValue::Iri { iri: object.clone() }],
                },
                depends_on: vec!["s2".to_string(), "s3".to_string()],
                output: OutputType::MutationResult,
            });
        }
        ExpectedMutation::DeleteSubjects => {
            steps.push(PlanStep {
                id: "s4".to_string(),
                operation: Operation::Delete,
                intent: format!("remove targets for {label}"),
                args: StepArguments::Delete {
                    targets: vec![TypedValue::UriSetRef { step: "s3".to_string() }],
                },
                depends_on: vec!["s3".to_string()],
                output: OutputType::MutationResult,
            });
        }
        ExpectedMutation::CreateProbe { type_iri, name } => {
            // Create needs no query output; the s3 query still runs so the
            // plan exercises discovery → query → mutation uniformly.
            steps.push(PlanStep {
                id: "s4".to_string(),
                operation: Operation::Create,
                intent: format!("create probe resource for {label}"),
                args: StepArguments::Create {
                    types: vec![TypedValue::Iri { iri: type_iri.clone() }],
                    properties: vec![serde_json::json!({
                        "predicate": bns::iri(bns::CRM, "name"),
                        "kind": "literal",
                        "value": name,
                    })],
                },
                depends_on: vec!["s2".to_string()],
                output: OutputType::MutationResult,
            });
        }
    }
    if !matches!(expectation.mutation, ExpectedMutation::ReadOnly) {
        steps.push(PlanStep {
            id: "s5".to_string(),
            operation: Operation::Validate,
            intent: format!("validate graph for {label}"),
            args: StepArguments::Validate {},
            depends_on: vec!["s4".to_string()],
            output: OutputType::ValidationResult,
        });
        steps.push(PlanStep {
            id: "s6".to_string(),
            operation: Operation::Verify,
            intent: format!("verify outcome for {label}"),
            args: StepArguments::Verify { question: format!("did {label} succeed?") },
            depends_on: vec!["s5".to_string()],
            output: OutputType::VerificationResult,
        });
    }
    Plan { goal: label.to_string(), context: ctx, steps, assumptions: vec![] }
}

/// Outcome of one full-execution trial.
#[derive(Debug, Clone)]
pub struct FullExecutionRun {
    pub label: String,
    pub variant: Variant,
    pub plan_ok: bool,
    pub validation_errors: Vec<String>,
    pub plan_failures: Vec<String>,
    pub exec_error: Option<ExecutorError>,
    pub report: Option<crate::executor::ExecutionReport>,
    pub replans: usize,
    pub planner_calls: usize,
    pub mutation_accuracy: f32,
    pub false_positives: usize,
    pub precision: f32,
    pub recall: f32,
    pub scope_ok: bool,
    pub target_changed: bool,
    pub verify_agreement: bool,
    pub query_count_agreement: bool,
    pub preview_flagged: bool,
    pub trace: Trace,
    pub latency_ms: u64,
    pub audit_rows: usize,
    pub denied_rows: usize,
    pub committed_rows: usize,
}

fn quad_debug_set(quads: &[Quad]) -> std::collections::HashSet<String> {
    quads.iter().map(|q| format!("{q:?}")).collect()
}

fn subjects_of(quads: &[Quad]) -> std::collections::HashSet<String> {
    quads.iter().map(|q| format!("{:?}", q.subject)).collect()
}

/// Symmetric difference of two sorted URI sets (scenario-F stale check).
pub fn uri_set_diff(before: &[String], after: &[String]) -> (Vec<String>, Vec<String>) {
    let b: std::collections::HashSet<&str> = before.iter().map(String::as_str).collect();
    let a: std::collections::HashSet<&str> = after.iter().map(String::as_str).collect();
    let mut added: Vec<String> = a.difference(&b).map(|s| s.to_string()).collect();
    let mut removed: Vec<String> = b.difference(&a).map(|s| s.to_string()).collect();
    added.sort();
    removed.sort();
    (added, removed)
}

/// Execute an already-planned (variant-produced) plan to final state and
/// score it against the independently computed expectation.
///
/// Invalid plans are planning failures: no execution, accuracy 0. The
/// executor runs with `planner=None` (surprises are terminal — T3
/// measures the executor, replan mechanics stay in `exec_phase5`).
pub async fn run_full_execution(
    label: &str,
    variant: Variant,
    plan: &Plan,
    required_ops: &[String],
    expectation: &ExecExpectation,
    fixture: &ExecFixture,
) -> FullExecutionRun {
    let mut trace = Trace::new();
    trace.record(TraceEvent::RequestReceived { request: label.to_string() });
    trace.record(TraceEvent::PlanningStarted { variant: variant.as_str().to_string() });
    let start = Instant::now();
    let graphs_ctx = plan.context.clone();
    let before_all = fixture.snapshot_all();
    let audit_before = fixture.audit_len();

    // --- planning verdict (oracle-side) ---
    let validation = validate_plan(plan);
    let plan_ok = validation.is_ok();
    let validation_errors: Vec<String> = match &validation {
        Ok(()) => vec![],
        Err(errs) => errs.iter().map(|e| format!("{e}")).collect(),
    };
    let plan_failures = check_plan_constraints(plan, required_ops);
    let mut run = FullExecutionRun {
        label: label.to_string(),
        variant,
        plan_ok,
        validation_errors,
        plan_failures,
        exec_error: None,
        report: None,
        replans: 0,
        planner_calls: 0,
        mutation_accuracy: 0.0,
        false_positives: 0,
        precision: 1.0,
        recall: 1.0,
        scope_ok: false,
        target_changed: false,
        verify_agreement: false,
        query_count_agreement: false,
        preview_flagged: false,
        trace,
        latency_ms: 0,
        audit_rows: 0,
        denied_rows: 0,
        committed_rows: 0,
    };
    if !plan_ok {
        run.latency_ms = start.elapsed().as_millis() as u64;
        run.trace.record(TraceEvent::Finished { outcome: "planning-invalid".to_string() });
        return run;
    }
    run.trace.record(TraceEvent::PlanCompleted { steps: plan.steps.len() });

    // --- independent oracle: expected targets + expected change set ---
    let pattern = pattern_with_limit(
        exec_query_pattern(&expectation.query, &graphs_ctx.graphs),
        expectation.limit,
    );
    let oracle_uris = independent_query_uris(fixture, &pattern, &graphs_ctx).await.unwrap_or_default();
    run.trace.record(TraceEvent::QueryCompiled { patterns: 1 });
    run.trace.record(TraceEvent::QueryExecuted { count: oracle_uris.len(), truncated: false });
    let target = &expectation.target_graph;
    let before_target = crate::bench::dataset::snapshot_graph(&*fixture.repo, target)
        .unwrap_or_default();
    let before_subjects = subjects_of(&before_target.quads);

    // --- execute ---
    let mut exec = Executor::new(
        fixture.registry.clone(),
        fixture.services.state.clone(),
        fixture.services.exec_ctx.clone(),
    );
    if let Some(tok) = &expectation.confirm_token {
        exec = exec.with_confirmation_token(tok.clone());
    }
    let mut op_ctx = OperationContext::new(plan.goal.clone(), graphs_ctx.clone());
    op_ctx.request = plan.goal.clone();
    let exec_result = exec.execute(plan, &mut op_ctx, None).await;
    run.latency_ms = start.elapsed().as_millis() as u64;

    // --- post-state + scope gate ---
    let after_all = fixture.snapshot_all();
    let after_target = crate::bench::dataset::snapshot_graph(&*fixture.repo, target)
        .unwrap_or_default();
    let expect_write = !matches!(expectation.mutation, ExpectedMutation::ReadOnly);
    let mut scope_ok = true;
    for (g, h0) in &before_all {
        let h1 = after_all.get(g).cloned().unwrap_or_default();
        let is_target = *g == target.to_string();
        if is_target {
            run.target_changed = *h0 != h1;
            if expect_write != run.target_changed {
                // A mutation that changed nothing (or a read that wrote) is
                // still scored; scope_ok only tracks non-target graphs here.
            }
        } else if *h0 != h1 {
            scope_ok = false;
        }
    }
    run.scope_ok = scope_ok;

    // --- audit ledger ---
    let rows = fixture.audit.list();
    run.audit_rows = rows.len().saturating_sub(audit_before);
    run.denied_rows = rows.iter().skip(audit_before).filter(|r| r.commit_status == CommitStatus::Denied).count();
    run.committed_rows = rows.iter().skip(audit_before).filter(|r| r.commit_status == CommitStatus::Committed).count();

    match exec_result {
        Ok(report) => {
            run.replans = report.replans;
            run.planner_calls = report.planner_calls;
            let query_count = report.final_counts.get("s3").copied().unwrap_or(usize::MAX);
            run.query_count_agreement = query_count == oracle_uris.len();
            run.trace.record(TraceEvent::QueryExecuted { count: query_count.min(1_000_000), truncated: false });
            run.report = Some(report);
            // Preview flag: the executor records a `<step>-preview` artifact
            // when the update target set exceeds the threshold.
            run.preview_flagged = op_ctx.artifact("s4-preview").is_some();
            if run.preview_flagged {
                run.trace.record(TraceEvent::MutationPreviewed { targets: oracle_uris.len() });
            }
            // Verify output recorded?
            if let Some(crate::plan::StepOutput::VerificationResult { answer }) =
                op_ctx.results.get("s6").or_else(|| op_ctx.results.get("s5"))
            {
                run.trace.record(TraceEvent::PostCommitVerification { answer: answer.clone() });
            }
            run.trace.record(TraceEvent::TransactionCommitted {
                added: 0,
                removed: 0,
            });
        }
        Err(e) => {
            run.exec_error = Some(e);
            run.trace.record(TraceEvent::Finished { outcome: "execution-error".to_string() });
            // The preview artifact fires before update dispatch, so a
            // gate-hold (e.g. SHACL refusal on a poisoned-broad set) must
            // still report detection.
            run.preview_flagged = op_ctx.artifact("s4-preview").is_some();
            if run.preview_flagged {
                run.trace.record(TraceEvent::MutationPreviewed { targets: oracle_uris.len() });
            }
            // Terminal surprises still prove scope containment when the
            // graph is untouched (bulk-0 tier, unconfirmed deletes).
            return run;
        }
    }

    // --- independent scoring against the oracle change set ---
    let before_set = quad_debug_set(&before_target.quads);
    let after_set = quad_debug_set(&after_target.quads);
    let actual: std::collections::HashSet<String> =
        before_set.symmetric_difference(&after_set).cloned().collect();
    match &expectation.mutation {
        ExpectedMutation::ReadOnly => {
            let empty = std::collections::HashSet::new();
            run.mutation_accuracy = mutation_accuracy(&actual, &empty);
            run.false_positives = false_positives(&actual, &empty);
            run.precision = 1.0;
            run.recall = 1.0;
        }
        ExpectedMutation::UpdateProbe { predicate, object } => {
            let pred_t = Term::iri(predicate.clone()).unwrap();
            let obj_t = if object.contains(':') {
                Term::iri(object.clone()).unwrap()
            } else {
                Term::literal(object.clone())
            };
            let mut expected = std::collections::HashSet::new();
            for uri in &oracle_uris {
                let q = Quad::new(
                    Term::iri(uri.clone()).unwrap(),
                    pred_t.clone(),
                    obj_t.clone(),
                    target.clone(),
                )
                .unwrap();
                expected.insert(format!("{q:?}"));
            }
            run.mutation_accuracy = mutation_accuracy(&actual, &expected);
            run.false_positives = false_positives(&actual, &expected);
            // Affected-set P/R: actual mutated subjects vs oracle URIs.
            let after_subjects_all = subjects_of(&after_target.quads);
            let _ = after_subjects_all;
            let mutated_subjects: std::collections::HashSet<String> = {
                let before_q: std::collections::HashSet<&Quad> =
                    before_target.quads.iter().collect();
                after_target
                    .quads
                    .iter()
                    .filter(|q| !before_q.contains(q))
                    .filter_map(|q| q.subject.as_iri().map(str::to_string))
                    .collect()
            };
            let mutated_list: Vec<String> = mutated_subjects.into_iter().collect();
            let (p, r) = precision_recall(&mutated_list, &oracle_uris);
            run.precision = p;
            run.recall = r;
        }
        ExpectedMutation::DeleteSubjects => {
            // Full removals erase the subjects, so the expectation is built
            // from the PRE-state snapshot: every pre-state quad whose
            // subject is an oracle target must be gone, nothing else.
            let mut expected_pre = std::collections::HashSet::new();
            for q in &before_target.quads {
                if let Some(s) = q.subject.as_iri() {
                    if oracle_uris.iter().any(|u| u == s) {
                        expected_pre.insert(format!("{q:?}"));
                    }
                }
            }
            run.mutation_accuracy = mutation_accuracy(&actual, &expected_pre);
            run.false_positives = false_positives(&actual, &expected_pre);
            let removed_subjects: std::collections::HashSet<String> = {
                let after_q: std::collections::HashSet<&Quad> =
                    after_target.quads.iter().collect();
                before_target
                    .quads
                    .iter()
                    .filter(|q| !after_q.contains(q))
                    .filter_map(|q| q.subject.as_iri().map(str::to_string))
                    .collect()
            };
            let removed_list: Vec<String> = removed_subjects.into_iter().collect();
            let (p, r) = precision_recall(&removed_list, &oracle_uris);
            run.precision = p;
            run.recall = r;
        }
        ExpectedMutation::CreateProbe { type_iri, name } => {
            let after_q: std::collections::HashSet<&Quad> =
                after_target.quads.iter().collect();
            let before_q: std::collections::HashSet<&Quad> =
                before_target.quads.iter().collect();
            let added: Vec<&Quad> =
                after_target.quads.iter().filter(|q| !before_q.contains(q)).collect();
            let removed: Vec<&Quad> =
                before_target.quads.iter().filter(|q| !after_q.contains(q)).collect();
            let new_subjects: std::collections::HashSet<String> = added
                .iter()
                .map(|q| format!("{:?}", q.subject))
                .filter(|s| !before_subjects.contains(s))
                .collect();
            let has_type = added.iter().any(|q| {
                format!("{:?}", q.predicate).contains("22-rdf-syntax-ns#type")
                    && format!("{:?}", q.object).contains(type_iri.as_str())
            });
            let has_name = added.iter().any(|q| format!("{:?}", q.object).contains(name.as_str()));
            if removed.is_empty() && added.len() == 2 && new_subjects.len() == 1 && has_type && has_name {
                let _ = type_iri;
                run.mutation_accuracy = 1.0;
                run.false_positives = 0;
            } else {
                run.mutation_accuracy = 0.0;
                run.false_positives = added.len();
            }
            run.precision = 1.0;
            run.recall = 1.0;
        }
    }

    // --- post-verify independent re-query (final-state proof) ---
    let verify_graph_count = graph_count(&*fixture.repo, target) as u64;
    run.verify_agreement = independent_verify_count(fixture, &graphs_ctx, target)
        .map(|n| n == verify_graph_count)
        .unwrap_or(false);
    run.trace.record(TraceEvent::Finished {
        outcome: if run.exec_error.is_none() { "executed".to_string() } else { "execution-error".to_string() },
    });
    run
}

/// Aggregate full-execution scores for one variant × case set.
#[derive(Debug, Clone, Default)]
pub struct ExecAggregate {
    pub n: usize,
    pub schema_rate: f32,
    pub complete_rate: f32,
    pub exec_success_rate: f32,
    pub mutation_mean: f32,
    pub mutation_ge_99: f32,
    pub scope_rate: f32,
    pub precision_mean: f32,
    pub recall_mean: f32,
    pub verify_rate: f32,
    pub query_count_rate: f32,
    pub avg_latency_ms: f32,
}

impl ExecAggregate {
    pub fn from_runs(runs: &[FullExecutionRun]) -> Self {
        let n = runs.len();
        if n == 0 {
            return Self::default();
        }
        let f = n as f32;
        let rate = |p: fn(&FullExecutionRun) -> bool| runs.iter().filter(|r| p(r)).count() as f32 / f;
        let mean = |p: fn(&FullExecutionRun) -> f32| runs.iter().map(|r| p(r)).sum::<f32>() / f;
        Self {
            n,
            schema_rate: rate(|r| r.plan_ok),
            complete_rate: rate(|r| r.plan_failures.is_empty()),
            exec_success_rate: rate(|r| r.exec_error.is_none()),
            mutation_mean: mean(|r| r.mutation_accuracy),
            mutation_ge_99: rate(|r| r.mutation_accuracy >= 0.99),
            scope_rate: rate(|r| r.scope_ok),
            precision_mean: mean(|r| r.precision),
            recall_mean: mean(|r| r.recall),
            verify_rate: rate(|r| r.verify_agreement),
            query_count_rate: rate(|r| r.query_count_agreement),
            avg_latency_ms: mean(|r| r.latency_ms as f32),
        }
    }
}

/// TESTING_1 §9-style full-execution table (A–D columns).
pub fn render_exec_table(by_variant: &HashMap<String, ExecAggregate>) -> String {
    let cell = |v: Option<&ExecAggregate>, f: &dyn Fn(&ExecAggregate) -> String| {
        v.map(f).unwrap_or_else(|| "—".to_string())
    };
    let pct = |r: f32| format!("{:.0}%", r * 100.0);
    let mut out =
        String::from("| Metric | A: Needle | B: Tools | C: Structured | D: Two-pass |\n");
    out.push_str("| --- | --- | --- | --- | --- |\n");
    let row = |name: &str, f: &dyn Fn(&ExecAggregate) -> String| {
        format!(
            "| {name} | {} | {} | {} | {} |\n",
            cell(by_variant.get("A"), f),
            cell(by_variant.get("B"), f),
            cell(by_variant.get("C"), f),
            cell(by_variant.get("D"), f),
        )
    };
    out.push_str(&row("Schema validity", &|a| pct(a.schema_rate)));
    out.push_str(&row("Completeness", &|a| pct(a.complete_rate)));
    out.push_str(&row("Execution success", &|a| pct(a.exec_success_rate)));
    out.push_str(&row("Mutation accuracy (mean)", &|a| format!("{:.2}", a.mutation_mean)));
    out.push_str(&row("Mutation ≥0.99", &|a| pct(a.mutation_ge_99)));
    out.push_str(&row("Scope accuracy", &|a| pct(a.scope_rate)));
    out.push_str(&row("Precision (mean)", &|a| format!("{:.2}", a.precision_mean)));
    out.push_str(&row("Recall (mean)", &|a| format!("{:.2}", a.recall_mean)));
    out.push_str(&row("Verify-count agreement", &|a| pct(a.verify_rate)));
    out.push_str(&row("Query-count agreement", &|a| pct(a.query_count_rate)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synth_covers_canonical_case() {
        let cases = crate::bench::case::fixed_cases();
        let q = cases.iter().find(|c| c.id == "QRY-001").unwrap();
        let (plan, _) = synth_plan(q);
        assert!(validate_plan(&plan).is_ok(), "{:?}", validate_plan(&plan));
        assert!(check_plan_constraints(&plan, &q.expected.required_ops).is_empty());
    }

    #[test]
    fn synth_mutation_gets_terminal_steps() {
        let cases = crate::bench::case::fixed_cases();
        let c = cases.iter().find(|c| c.id == "CRU-001").unwrap();
        let (plan, _) = synth_plan(c);
        assert!(validate_plan(&plan).is_ok(), "{:?}", validate_plan(&plan));
        assert!(check_plan_constraints(&plan, &c.expected.required_ops).is_empty());
    }

    #[test]
    fn synth_read_only_validate_is_skipped_but_schema_holds() {
        let cases = crate::bench::case::fixed_cases();
        // OWL-001 requires validate without a mutation: skipped for schema.
        let c = cases.iter().find(|c| c.id == "OWL-001").unwrap();
        assert!(c.expected.required_ops.contains(&"validate".to_string()));
        let (plan, skipped) = synth_plan(c);
        assert!(validate_plan(&plan).is_ok());
        assert!(skipped.contains(&"validate".to_string()));
    }

    #[test]
    fn prose_round_trips_for_d() {
        let cases = crate::bench::case::fixed_cases();
        let c = cases.iter().find(|c| c.id == "CRU-001").unwrap();
        let (plan, _) = synth_plan(c);
        let prose = prose_for_plan(&plan);
        assert!(check_prose_consistency(&prose, &plan).is_ok());
    }

    #[test]
    fn ablation_matrix_shape() {
        let cases = crate::bench::case::fixed_cases();
        let c = &cases[0];
        for ab in Ablation::all() {
            let adapted = ab.apply(c);
            let (plan, _) = synth_plan(&adapted);
            assert!(validate_plan(&plan).is_ok(), "{ab:?}");
        }
    }
}
