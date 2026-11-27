//! Canonical plan representation (Plan 2 Phase 2).
//!
//! Planner-independent: all four Phase-4 planners (Needle sequential +
//! three `agentstream` planners) target these types. Rust owns the plan;
//! models propose, [`validator::validate_plan`] disposes.
//!
//! The plan is a DAG of operations over an explicit [`GraphContext`].
//! Steps refer to earlier outputs via [`TypedValue::StepRef`] /
//! [`TypedValue::UriSetRef`] — never via invented URI strings.

pub mod validator;

use std::collections::HashMap;

pub use validator::{validate_plan, PlanError};

use serde::{Deserialize, Serialize};

use crate::rdf::GraphName;

/// How much inference the executor may apply when resolving a plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum InferenceMode {
    #[default]
    None,
    Rdfs,
    OwlMicro,
}

/// Shared operational scope for a request. Replaces free-text graph scoping.
/// Every RDF tool accepts this, with optional per-call overrides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphContext {
    #[serde(default = "default_dataset")]
    pub dataset: String,
    #[serde(default)]
    pub graphs: Vec<GraphName>,
    /// prefix -> namespace IRI, e.g. `{"ex": "http://example.com/"}`.
    #[serde(default)]
    pub namespaces: HashMap<String, String>,
    #[serde(default)]
    pub ontology_refs: Vec<String>,
    #[serde(default)]
    pub inference: InferenceMode,
    #[serde(default)]
    pub tenant: Option<String>,
    /// Transaction/workspace identity, if any.
    #[serde(default)]
    pub tx: Option<String>,
}

fn default_dataset() -> String {
    "default".to_string()
}

impl Default for GraphContext {
    fn default() -> Self {
        Self {
            dataset: default_dataset(),
            graphs: vec![],
            namespaces: HashMap::new(),
            ontology_refs: vec![],
            inference: InferenceMode::None,
            tenant: None,
            tx: None,
        }
    }
}

impl GraphContext {
    /// Empty graphs = unconstrained (legacy callers keep working).
    pub fn allows_graph(&self, g: &GraphName) -> bool {
        if self.graphs.is_empty() {
            return true;
        }
        self.graphs.contains(g)
    }

    /// Empty namespaces = unconstrained.
    pub fn allows_namespace(&self, iri: &str) -> bool {
        if self.namespaces.is_empty() {
            return true;
        }
        self.namespaces.values().any(|ns| iri.starts_with(ns))
    }

    /// True when any requested namespace is outside the context allowlist.
    /// Empty context namespaces = unconstrained (never limited).
    pub fn scope_limited(&self, requested: &[String]) -> bool {
        if self.namespaces.is_empty() || requested.is_empty() {
            return false;
        }
        let allowed: Vec<&String> = self.namespaces.values().collect();
        requested
            .iter()
            .any(|r| !allowed.iter().any(|a| r == *a || r.starts_with(a.as_str())))
    }
}

/// Operations that may appear in a plan. Planning tools manipulate these;
/// the Phase-5 executor runs the RDF subset. Mutations never run during
/// planning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    SearchVocabulary,
    DescribeVocabulary,
    SearchInstances,
    DescribeResource,
    Query,
    Create,
    Update,
    Delete,
    Validate,
    Verify,
}

impl Operation {
    pub fn as_str(&self) -> &'static str {
        match self {
            Operation::SearchVocabulary => "search_vocabulary",
            Operation::DescribeVocabulary => "describe_vocabulary",
            Operation::SearchInstances => "search_instances",
            Operation::DescribeResource => "describe_resource",
            Operation::Query => "query",
            Operation::Create => "create",
            Operation::Update => "update",
            Operation::Delete => "delete",
            Operation::Validate => "validate",
            Operation::Verify => "verify",
        }
    }

    /// Discovery operations (read-only, allowed during planning).
    pub fn is_discovery(&self) -> bool {
        matches!(
            self,
            Operation::SearchVocabulary
                | Operation::DescribeVocabulary
                | Operation::SearchInstances
                | Operation::DescribeResource
                | Operation::Query
        )
    }

    /// Mutation operations (executor only, never during planning).
    pub fn is_mutation(&self) -> bool {
        matches!(
            self,
            Operation::Create | Operation::Update | Operation::Delete
        )
    }
}

/// Declared output type of a plan step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum OutputType {
    VocabularyCandidates,
    ResolvedVocabulary,
    ResourceCandidates,
    ResourceDescriptions,
    UriSet,
    MutationResult,
    ValidationResult,
    VerificationResult,
}

impl OutputType {
    pub fn expected_for(op: Operation) -> Self {
        match op {
            Operation::SearchVocabulary => OutputType::VocabularyCandidates,
            Operation::DescribeVocabulary => OutputType::ResolvedVocabulary,
            Operation::SearchInstances => OutputType::ResourceCandidates,
            Operation::DescribeResource => OutputType::ResourceDescriptions,
            Operation::Query => OutputType::UriSet,
            Operation::Create | Operation::Update | Operation::Delete => {
                OutputType::MutationResult
            }
            Operation::Validate => OutputType::ValidationResult,
            Operation::Verify => OutputType::VerificationResult,
        }
    }
}

/// A value inside step arguments. Bare strings are never IRIs: use
/// [`TypedValue::Iri`] for known IRIs, [`TypedValue::Unresolved`] for
/// to-be-discovered terms, and [`TypedValue::StepRef`] /
/// [`TypedValue::UriSetRef`] for values produced by earlier steps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TypedValue {
    Iri { iri: String },
    Unresolved { description: String },
    StepRef { step: String, field: String },
    UriSetRef { step: String },
}

impl TypedValue {
    pub fn iri(s: impl Into<String>) -> Self {
        Self::Iri { iri: s.into() }
    }
    pub fn unresolved(s: impl Into<String>) -> Self {
        Self::Unresolved {
            description: s.into(),
        }
    }
}

/// Arguments for each operation. Tagged union — every operation has a
/// distinct shape so the model cannot smuggle arbitrary nested objects.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StepArguments {
    SearchVocab {
        terms: Vec<String>,
        #[serde(default)]
        namespaces: Vec<String>,
    },
    DescribeVocab {
        iris: Vec<TypedValue>,
    },
    SearchInstances {
        text: String,
        #[serde(default)]
        types: Vec<TypedValue>,
        #[serde(default)]
        namespace: Option<String>,
    },
    DescribeResource {
        uris: Vec<TypedValue>,
    },
    Query {
        /// Opaque until Phase 3 defines `QueryPattern`; validators only
        /// check presence + dependency linkage for now.
        pattern: serde_json::Value,
    },
    Create {
        types: Vec<TypedValue>,
        #[serde(default)]
        properties: Vec<serde_json::Value>,
    },
    Update {
        targets: Vec<TypedValue>,
        predicate: TypedValue,
        objects: Vec<TypedValue>,
    },
    Delete {
        targets: Vec<TypedValue>,
    },
    Validate {},
    Verify {
        question: String,
    },
}

impl StepArguments {
    /// Which operation this argument shape belongs to.
    pub fn operation(&self) -> Operation {
        match self {
            StepArguments::SearchVocab { .. } => Operation::SearchVocabulary,
            StepArguments::DescribeVocab { .. } => Operation::DescribeVocabulary,
            StepArguments::SearchInstances { .. } => Operation::SearchInstances,
            StepArguments::DescribeResource { .. } => Operation::DescribeResource,
            StepArguments::Query { .. } => Operation::Query,
            StepArguments::Create { .. } => Operation::Create,
            StepArguments::Update { .. } => Operation::Update,
            StepArguments::Delete { .. } => Operation::Delete,
            StepArguments::Validate { .. } => Operation::Validate,
            StepArguments::Verify { .. } => Operation::Verify,
        }
    }
}

/// One node of the plan DAG.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanStep {
    /// Step id, e.g. `s1`. Unique within the plan.
    pub id: String,
    pub operation: Operation,
    pub intent: String,
    pub args: StepArguments,
    #[serde(default)]
    pub depends_on: Vec<String>,
    pub output: OutputType,
}

/// The canonical operational plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub goal: String,
    #[serde(default)]
    pub context: GraphContext,
    pub steps: Vec<PlanStep>,
    #[serde(default)]
    pub assumptions: Vec<String>,
}

/// A vocabulary candidate returned by `search_vocabulary`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VocabCandidate {
    pub iri: String,
    pub kind: String,
    pub label: String,
    #[serde(default)]
    pub comment: String,
}

/// A resolved vocabulary term returned by `describe_vocabulary`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedTerm {
    pub iri: String,
    pub kind: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub comment: String,
    #[serde(default)]
    pub domain: Option<String>,
    #[serde(default)]
    pub range: Option<String>,
}

/// An instance candidate returned by `search_instances`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourceCandidate {
    pub uri: String,
    #[serde(default)]
    pub label: String,
    #[serde(rename = "type", default)]
    pub type_: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub score: f32,
}

/// Typed outputs retained by [`OperationContext`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StepOutput {
    VocabularyCandidates { items: Vec<VocabCandidate> },
    ResolvedVocabulary { items: Vec<ResolvedTerm> },
    ResourceCandidates { items: Vec<ResourceCandidate> },
    ResourceDescriptions { count: usize },
    UriSet { uris: Vec<String> },
    MutationResult { ok: bool, summary: String },
    ValidationResult { conforms: bool },
    VerificationResult { answer: String },
}

impl StepOutput {
    pub fn output_type(&self) -> OutputType {
        match self {
            StepOutput::VocabularyCandidates { .. } => OutputType::VocabularyCandidates,
            StepOutput::ResolvedVocabulary { .. } => OutputType::ResolvedVocabulary,
            StepOutput::ResourceCandidates { .. } => OutputType::ResourceCandidates,
            StepOutput::ResourceDescriptions { .. } => OutputType::ResourceDescriptions,
            StepOutput::UriSet { .. } => OutputType::UriSet,
            StepOutput::MutationResult { .. } => OutputType::MutationResult,
            StepOutput::ValidationResult { .. } => OutputType::ValidationResult,
            StepOutput::VerificationResult { .. } => OutputType::VerificationResult,
        }
    }
}

/// Persistent request state. Owned and mutated only by Rust; the model
/// receives a compact rendering via [`OperationContext::status_summary`].
#[derive(Debug, Clone, Default)]
pub struct OperationContext {
    pub request: String,
    pub graph_context: GraphContext,
    pub namespaces: HashMap<String, String>,
    pub results: HashMap<String, StepOutput>,
    pub pending: Vec<String>,
    pub validation_errors: Vec<String>,
    /// Audit artifacts retained by planners (e.g. Option D's
    /// natural-language plan + structured extraction).
    pub artifacts: HashMap<String, String>,
}

impl OperationContext {
    pub fn new(request: impl Into<String>, graph_context: GraphContext) -> Self {
        Self {
            request: request.into(),
            graph_context,
            namespaces: HashMap::new(),
            results: HashMap::new(),
            pending: vec![],
            validation_errors: vec![],
            artifacts: HashMap::new(),
        }
    }

    /// Retain a planner audit artifact (Phase 4 Option D keeps both the
    /// natural-language plan and its structured extraction).
    pub fn record_artifact(&mut self, name: &str, content: impl Into<String>) {
        self.artifacts.insert(name.to_string(), content.into());
    }

    /// Fetch a previously recorded audit artifact, if any.
    pub fn artifact(&self, name: &str) -> Option<&str> {
        self.artifacts.get(name).map(String::as_str)
    }

    /// Record an `execute_query` result under its `QueryPlanId` as a typed
    /// `UriSet` output (Plan 2 Phase 3 §4). The executor / Phase-4 planners
    /// consume it via `TypedValue::StepRef { step: query_plan_id, .. }` or
    /// `TypedValue::UriSetRef { step: query_plan_id }` — never via raw URIs.
    pub fn record_query_result(&mut self, query_plan_id: &str, uris: Vec<String>) {
        let mut sorted = uris;
        sorted.sort();
        sorted.dedup();
        self.results.insert(
            query_plan_id.to_string(),
            StepOutput::UriSet { uris: sorted },
        );
    }

    /// URIs previously recorded for a `QueryPlanId` (if any).
    pub fn query_uris(&self, query_plan_id: &str) -> Option<&[String]> {
        match self.results.get(query_plan_id) {
            Some(StepOutput::UriSet { uris }) => Some(uris),
            _ => None,
        }
    }

    /// Compact status prompt for the next planner turn. Never dumps history,
    /// full graphs, or raw tool payloads.
    pub fn status_summary(&self, plan: &Plan) -> String {
        let mut done: Vec<String> = vec![];
        let mut pending: Vec<String> = vec![];
        for s in &plan.steps {
            if self.results.contains_key(&s.id) {
                done.push(format!("{}: {} done", s.id, s.operation.as_str()));
            } else {
                pending.push(format!("{}: {}", s.id, s.operation.as_str()));
            }
        }
        let mut counts: HashMap<String, usize> = HashMap::new();
        for out in self.results.values() {
            let n = match out {
                StepOutput::VocabularyCandidates { items } => items.len(),
                StepOutput::ResolvedVocabulary { items } => items.len(),
                StepOutput::ResourceCandidates { items } => items.len(),
                StepOutput::UriSet { uris } => uris.len(),
                _ => 1,
            };
            *counts.entry(format!("{:?}", out.output_type())).or_default() += n;
        }
        format!(
            "User objective:\n{}\n\nCurrent plan:\n{}\n\nPlan status:\n{} steps, {} done, {} pending.\nNo RDF operations executed beyond recorded results.\nResolved outputs: {:?}.\nRemaining: {}.\nContinue constructing the plan; call finish_plan only when all required operations and dependencies are represented.",
            self.request,
            plan.steps
                .iter()
                .map(|s| format!(
                    "{}: {} — {} (depends on {:?})",
                    s.id,
                    s.operation.as_str(),
                    s.intent,
                    s.depends_on
                ))
                .collect::<Vec<_>>()
                .join("\n"),
            plan.steps.len(),
            done.len(),
            pending.len(),
            counts,
            if pending.is_empty() {
                "nothing; consider finish_plan".to_string()
            } else {
                format!("still needed: {}", pending.join(", "))
            }
        )
    }
}

/// Canonical Ohio-contacts plan from DESIGN_UPDATE_3 §1.1 (7 steps).
/// Used by validator tests and the golden snapshot.
pub fn canonical_ohio_plan() -> Plan {
    Plan {
        goal: "Classify contacts at Ohio accounts as prospects".to_string(),
        context: GraphContext {
            dataset: "crm".to_string(),
            graphs: vec![GraphName::Named("crm:production".to_string())],
            ..Default::default()
        },
        assumptions: vec![],
        steps: vec![
            PlanStep {
                id: "s1".to_string(),
                operation: Operation::SearchVocabulary,
                intent: "Discover contact, account, location and classification vocabulary"
                    .to_string(),
                args: StepArguments::SearchVocab {
                    terms: vec![
                        "contact".to_string(),
                        "account".to_string(),
                        "location".to_string(),
                    ],
                    namespaces: vec![],
                },
                depends_on: vec![],
                output: OutputType::VocabularyCandidates,
            },
            PlanStep {
                id: "s2".to_string(),
                operation: Operation::DescribeVocabulary,
                intent: "Resolve the relevant classes and relationships".to_string(),
                args: StepArguments::DescribeVocab {
                    iris: vec![TypedValue::StepRef {
                        step: "s1".to_string(),
                        field: "candidates".to_string(),
                    }],
                },
                depends_on: vec!["s1".to_string()],
                output: OutputType::ResolvedVocabulary,
            },
            PlanStep {
                id: "s3".to_string(),
                operation: Operation::Query,
                intent: "Find contacts whose associated accounts are in Ohio".to_string(),
                args: StepArguments::Query {
                    pattern: serde_json::json!({"intent": "contacts-at-ohio-accounts"}),
                },
                depends_on: vec!["s2".to_string()],
                output: OutputType::UriSet,
            },
            PlanStep {
                id: "s4".to_string(),
                operation: Operation::SearchInstances,
                intent: "Find the prospective customer classification resource".to_string(),
                args: StepArguments::SearchInstances {
                    text: "prospective customer".to_string(),
                    types: vec![],
                    namespace: None,
                },
                depends_on: vec!["s2".to_string()],
                output: OutputType::ResourceCandidates,
            },
            PlanStep {
                id: "s5".to_string(),
                operation: Operation::Update,
                intent: "Assign the prospect classification to matching contacts".to_string(),
                args: StepArguments::Update {
                    targets: vec![TypedValue::UriSetRef {
                        step: "s3".to_string(),
                    }],
                    predicate: TypedValue::Unresolved {
                        description: "customer classification property".to_string(),
                    },
                    objects: vec![TypedValue::StepRef {
                        step: "s4".to_string(),
                        field: "resource".to_string(),
                    }],
                },
                depends_on: vec!["s3".to_string(), "s4".to_string()],
                output: OutputType::MutationResult,
            },
            PlanStep {
                id: "s6".to_string(),
                operation: Operation::Validate,
                intent: "Validate the updated graph".to_string(),
                args: StepArguments::Validate {},
                depends_on: vec!["s5".to_string()],
                output: OutputType::ValidationResult,
            },
            PlanStep {
                id: "s7".to_string(),
                operation: Operation::Verify,
                intent: "Verify the final number of classified contacts".to_string(),
                args: StepArguments::Verify {
                    question: "how many contacts were classified?".to_string(),
                },
                depends_on: vec!["s5".to_string()],
                output: OutputType::VerificationResult,
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graph_context_allows_and_limits() {
        let empty = GraphContext::default();
        assert!(empty.allows_graph(&GraphName::tenant("acme").unwrap()));
        assert!(empty.allows_namespace("http://example.com/Contact"));
        assert!(!empty.scope_limited(&["http://example.com/".to_string()]));

        let scoped = GraphContext {
            graphs: vec![GraphName::tenant("acme").unwrap()],
            namespaces: [("ex".to_string(), "http://example.com/".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        assert!(scoped.allows_graph(&GraphName::tenant("acme").unwrap()));
        assert!(!scoped.allows_graph(&GraphName::tenant("other").unwrap()));
        assert!(scoped.allows_namespace("http://example.com/Contact"));
        assert!(!scoped.allows_namespace("http://other.com/X"));
        assert!(scoped.scope_limited(&["http://other.com/".to_string()]));
        assert!(!scoped.scope_limited(&["http://example.com/".to_string()]));
    }

    #[test]
    fn canonical_plan_validates() {
        let plan = canonical_ohio_plan();
        assert!(validate_plan(&plan).is_ok(), "{:?}", validate_plan(&plan));
        assert_eq!(plan.steps.len(), 7);
    }

    #[test]
    fn canonical_plan_snapshot_shape() {
        let plan = canonical_ohio_plan();
        let v = serde_json::to_value(&plan).unwrap();
        // Golden shape (not byte-exact): 7 steps, expected ops/outputs.
        let steps = v.get("steps").unwrap().as_array().unwrap();
        assert_eq!(steps.len(), 7);
        let ops: Vec<&str> = steps
            .iter()
            .map(|s| s.get("operation").unwrap().as_str().unwrap())
            .collect();
        assert_eq!(
            ops,
            vec![
                "search_vocabulary",
                "describe_vocabulary",
                "query",
                "search_instances",
                "update",
                "validate",
                "verify"
            ]
        );
        // Round-trip.
        let back: Plan = serde_json::from_value(v).unwrap();
        assert_eq!(back, plan);
    }

    #[test]
    fn status_summary_is_compact() {
        let plan = canonical_ohio_plan();
        let mut ctx = OperationContext::new(plan.goal.clone(), plan.context.clone());
        let s = ctx.status_summary(&plan);
        assert!(s.contains("Classify contacts"));
        assert!(s.contains("s1"));
        // No history dump: summary is bounded.
        assert!(s.len() < 4000, "len={}", s.len());
        ctx.results.insert(
            "s1".to_string(),
            StepOutput::VocabularyCandidates { items: vec![] },
        );
        let s2 = ctx.status_summary(&plan);
        assert!(s2.contains("1 done"));
    }
}
