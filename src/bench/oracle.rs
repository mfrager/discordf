//! Independent scoring oracles (TESTING plan Phase T1, §6).
//!
//! The oracles never consult the model: plan constraints come from the case,
//! URI sets from the dataset, and graph changes from before/after snapshots.

use std::collections::HashSet;

use super::case::BenchmarkCase;
use super::dataset::Snapshot;
use crate::plan::{Operation, Plan};

/// Canonical operation names accepted in `required_ops`. Aliases from
/// TESTING_1 §5 (`discover_vocabulary`, `construct_query`, `execute_query`)
/// map onto the canonical ops.
fn op_satisfied(plan: &Plan, required: &str) -> bool {
    let ops: Vec<Operation> = plan.steps.iter().map(|s| s.operation).collect();
    let has = |o: Operation| ops.contains(&o);
    match required {
        "search_vocabulary" => has(Operation::SearchVocabulary),
        "describe_vocabulary" => has(Operation::DescribeVocabulary),
        "search_instances" => has(Operation::SearchInstances),
        "describe_resource" => has(Operation::DescribeResource),
        "query" | "construct_query" | "execute_query" => has(Operation::Query),
        "create" => has(Operation::Create),
        "update" => has(Operation::Update),
        "delete" => has(Operation::Delete),
        "validate" => has(Operation::Validate),
        "verify" => has(Operation::Verify),
        "discover_vocabulary" => has(Operation::SearchVocabulary) || has(Operation::DescribeVocabulary),
        other => plan.steps.iter().any(|s| s.operation.as_str() == other),
    }
}

/// Missing required operations (empty = plan satisfies the case constraints).
pub fn check_plan_constraints(plan: &Plan, required: &[String]) -> Vec<String> {
    required
        .iter()
        .filter(|r| !op_satisfied(plan, r))
        .cloned()
        .collect()
}

/// Precision/recall of an actual URI set against the expected set.
/// Empty-expected + empty-actual scores (1,1); empty-expected + non-empty
/// actual scores precision 0 (recall 1: nothing was missed).
pub fn precision_recall(actual: &[String], expected: &[String]) -> (f32, f32) {
    let a: HashSet<&str> = actual.iter().map(String::as_str).collect();
    let e: HashSet<&str> = expected.iter().map(String::as_str).collect();
    if e.is_empty() {
        return (if a.is_empty() { 1.0 } else { 0.0 }, 1.0);
    }
    let tp = a.intersection(&e).count() as f32;
    let precision = if a.is_empty() { 1.0 } else { tp / a.len() as f32 };
    let recall = tp / e.len() as f32;
    (precision, recall)
}

/// Quad-level changes between two snapshots, as debug-string sets.
pub fn graph_changes(before: &Snapshot, after: &Snapshot) -> (HashSet<String>, HashSet<String>) {
    let b: HashSet<String> = before.quads.iter().map(|q| format!("{q:?}")).collect();
    let a: HashSet<String> = after.quads.iter().map(|q| format!("{q:?}")).collect();
    let added = a.difference(&b).cloned().collect();
    let removed = b.difference(&a).cloned().collect();
    (added, removed)
}

/// Mutation accuracy (TESTING_1 §6.2): `1 - |actual △ expected| /
/// |actual ∪ expected|` over change sets. Empty ∪ empty scores 1.0.
pub fn mutation_accuracy(actual: &HashSet<String>, expected: &HashSet<String>) -> f32 {
    let union: HashSet<&String> = actual.union(expected).collect();
    if union.is_empty() {
        return 1.0;
    }
    let symdiff = actual.symmetric_difference(expected).count() as f32;
    1.0 - symdiff / union.len() as f32
}

/// False-positive changes: actual changes outside the expected set. Penalized
/// separately and harder than misses (TESTING_1 §6.2).
pub fn false_positives(actual: &HashSet<String>, expected: &HashSet<String>) -> usize {
    actual.difference(expected).count()
}

/// Combined per-case score. All fields are computed, never model-reported.
#[derive(Debug, Clone, PartialEq)]
pub struct CaseScore {
    pub plan_failures: Vec<String>,
    pub precision: f32,
    pub recall: f32,
    pub mutation_accuracy: f32,
    pub false_positives: usize,
    pub graph_modified: bool,
    pub must_not_modify_violated: bool,
}

impl CaseScore {
    /// Planning-only scoring: constraints + URI-set quality.
    pub fn planning_only(case: &BenchmarkCase, plan: &Plan, actual_uris: &[String]) -> Self {
        let plan_failures = check_plan_constraints(plan, &case.expected.required_ops);
        let (precision, recall) = precision_recall(actual_uris, &case.expected.expected_uris);
        Self {
            plan_failures,
            precision,
            recall,
            mutation_accuracy: 1.0,
            false_positives: 0,
            graph_modified: false,
            must_not_modify_violated: false,
        }
    }

    /// Full-execution scoring: adds graph-diff scoring against the expected
    /// change set. `expected_changes` is the oracle change set (added ∪
    /// removed debug strings); empty when the case forbids modification.
    pub fn full_execution(
        case: &BenchmarkCase,
        plan: &Plan,
        actual_uris: &[String],
        before: &Snapshot,
        after: &Snapshot,
        expected_changes: &HashSet<String>,
    ) -> Self {
        let mut s = Self::planning_only(case, plan, actual_uris);
        let (added, removed) = graph_changes(before, after);
        let actual: HashSet<String> = added.union(&removed).cloned().collect();
        s.graph_modified = !actual.is_empty();
        s.mutation_accuracy = mutation_accuracy(&actual, expected_changes);
        s.false_positives = false_positives(&actual, expected_changes);
        s.must_not_modify_violated = case.expected.must_not_modify_graph && s.graph_modified;
        s
    }

    pub fn plan_ok(&self) -> bool {
        self.plan_failures.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{OutputType, PlanStep, StepArguments, TypedValue};

    fn plan_with(ops: &[Operation]) -> Plan {
        Plan {
            goal: "g".to_string(),
            context: Default::default(),
            assumptions: vec![],
            steps: ops
                .iter()
                .enumerate()
                .map(|(i, op)| PlanStep {
                    id: format!("s{}", i + 1),
                    operation: *op,
                    intent: "t".to_string(),
                    args: match op {
                        Operation::SearchVocabulary => StepArguments::SearchVocab {
                            terms: vec!["x".to_string()],
                            namespaces: vec![],
                        },
                        _ => StepArguments::Verify { question: "q".to_string() },
                    },
                    depends_on: vec![],
                    output: OutputType::expected_for(*op),
                })
                .collect(),
        }
    }

    #[test]
    fn constraints_catch_missing_ops() {
        let p = plan_with(&[Operation::SearchVocabulary]);
        assert!(check_plan_constraints(&p, &["search_vocabulary".to_string()]).is_empty());
        assert!(check_plan_constraints(&p, &["discover_vocabulary".to_string()]).is_empty());
        let missing = check_plan_constraints(&p, &["query".to_string(), "construct_query".to_string()]);
        assert_eq!(missing.len(), 2);
        // Unused import guard: TypedValue referenced via plan types only.
        let _ = TypedValue::Iri { iri: "x".to_string() };
    }

    #[test]
    fn pr_edge_cases() {
        assert_eq!(precision_recall(&[], &[]), (1.0, 1.0));
        assert_eq!(precision_recall(&["a".to_string()], &[]), (0.0, 1.0));
        let (p, r) = precision_recall(
            &["a".to_string(), "b".to_string()],
            &["a".to_string(), "c".to_string()],
        );
        assert!((p - 0.5).abs() < 1e-6);
        assert!((r - 0.5).abs() < 1e-6);
    }

    #[test]
    fn mutation_metric_behaviour() {
        let (a, e) = (HashSet::new(), HashSet::new());
        assert_eq!(mutation_accuracy(&a, &e), 1.0);
        let a: HashSet<String> = ["x".to_string()].into_iter().collect();
        let e: HashSet<String> = ["x".to_string(), "y".to_string()].into_iter().collect();
        assert!((mutation_accuracy(&a, &e) - 0.5).abs() < 1e-6);
        assert_eq!(false_positives(&a, &e), 0);
        let extra: HashSet<String> =
            ["x".to_string(), "y".to_string(), "z".to_string()].into_iter().collect();
        assert_eq!(false_positives(&extra, &e), 1);
    }
}
