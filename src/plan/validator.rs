//! Plan validation (Plan 2 Phase 2): structural + dependency +
//! context + operational checks. Same validator for all four planners.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use super::{Operation, OutputType, Plan, StepArguments};

/// One validation failure. `step` is `None` for plan-level errors
/// (e.g. cycles, missing terminal validate/verify).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanError {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    pub code: String,
    pub message: String,
}

impl PlanError {
    /// Ad-hoc failure for errors detected outside full-plan validation
    /// (e.g. a single planning-tool call rejected during plan construction).
    pub fn custom(step: Option<&str>, code: &str, message: impl Into<String>) -> Self {
        Self {
            step: step.map(str::to_string),
            code: code.to_string(),
            message: message.into(),
        }
    }

    fn step(step: &str, code: &str, message: impl Into<String>) -> Self {
        Self {
            step: Some(step.to_string()),
            code: code.to_string(),
            message: message.into(),
        }
    }
    fn plan(code: &str, message: impl Into<String>) -> Self {
        Self {
            step: None,
            code: code.to_string(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.step {
            Some(s) => write!(f, "[{}:{}] {}", s, self.code, self.message),
            None => write!(f, "[plan:{}] {}", self.code, self.message),
        }
    }
}

/// Validate a plan. Returns `Ok(())` when the plan is well-formed;
/// otherwise returns every detected failure (no early exit).
pub fn validate_plan(plan: &Plan) -> Result<(), Vec<PlanError>> {
    let mut errors: Vec<PlanError> = vec![];

    // --- structural: ids unique, non-empty, intent present, args match op ---
    let mut seen: HashSet<&String> = HashSet::new();
    for s in &plan.steps {
        if s.id.trim().is_empty() {
            errors.push(PlanError::plan("id", "step id must not be empty"));
            continue;
        }
        if !seen.insert(&s.id) {
            errors.push(PlanError::step(&s.id, "duplicate", "duplicate step id"));
        }
        if s.intent.trim().is_empty() {
            errors.push(PlanError::step(&s.id, "intent", "intent must not be empty"));
        }
        if s.args.operation() != s.operation {
            errors.push(PlanError::step(
                &s.id,
                "args",
                format!(
                    "args kind {:?} does not match operation {:?}",
                    s.args.operation(),
                    s.operation
                ),
            ));
        }
        if s.output != OutputType::expected_for(s.operation) {
            errors.push(PlanError::step(
                &s.id,
                "output",
                format!(
                    "output {:?} does not match operation {:?} (expected {:?})",
                    s.output,
                    s.operation,
                    OutputType::expected_for(s.operation)
                ),
            ));
        }
        if s.depends_on.contains(&s.id) {
            errors.push(PlanError::step(
                &s.id,
                "dependency",
                "step cannot depend on itself",
            ));
        }
        // Extra arg-shape checks.
        check_args_shape(s, &mut errors);
    }
    if plan.steps.is_empty() {
        errors.push(PlanError::plan("empty", "plan has no steps"));
    }

    // --- dependency: refs exist + acyclic ---
    let ids: HashSet<&String> = plan.steps.iter().map(|s| &s.id).collect();
    for s in &plan.steps {
        for d in &s.depends_on {
            if !ids.contains(d) {
                errors.push(PlanError::step(
                    &s.id,
                    "dangling",
                    format!("depends on unknown step {d:?}"),
                ));
            }
        }
    }
    if let Some(cycle) = find_cycle(plan) {
        errors.push(PlanError::plan(
            "cycle",
            format!("dependency cycle detected: {}", cycle.join(" -> ")),
        ));
    }

    // --- dependency: producer-output -> consumer-input type match ---
    let by_id: HashMap<&String, &super::PlanStep> =
        plan.steps.iter().map(|s| (&s.id, s)).collect();
    for s in &plan.steps {
        // Skip type checks when refs dangle (already reported).
        if s.depends_on.iter().any(|d| !ids.contains(d)) {
            continue;
        }
        let dep_outputs: Vec<OutputType> = s
            .depends_on
            .iter()
            .filter_map(|d| by_id.get(d).map(|p| p.output))
            .collect();
        if let Err(msg) = check_dep_types(s.operation, &dep_outputs) {
            errors.push(PlanError::step(&s.id, "types", msg));
        }
        // StepRef/UriSetRef targets must name existing steps.
        check_ref_targets(s, &ids, &mut errors);
    }

    // --- context: dataset/graphs present ---
    if plan.context.dataset.trim().is_empty() {
        errors.push(PlanError::plan("context", "context.dataset must not be empty"));
    }

    // --- operational: mutations need discovery + terminal validate/verify ---
    check_operational(plan, &mut errors);

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn check_args_shape(s: &super::PlanStep, errors: &mut Vec<PlanError>) {
    match &s.args {
        StepArguments::SearchVocab { terms, .. } => {
            if terms.is_empty() || terms.iter().all(|t| t.trim().is_empty()) {
                errors.push(PlanError::step(&s.id, "args", "search needs at least one term"));
            }
        }
        StepArguments::SearchInstances { text, .. } => {
            if text.trim().is_empty() {
                errors.push(PlanError::step(&s.id, "args", "instance search needs text"));
            }
        }
        StepArguments::Verify { question } => {
            if question.trim().is_empty() {
                errors.push(PlanError::step(&s.id, "args", "verify needs a question"));
            }
        }
        StepArguments::Update { targets, .. } | StepArguments::Delete { targets } => {
            if targets.is_empty() {
                errors.push(PlanError::step(
                    &s.id,
                    "args",
                    "mutation needs at least one target",
                ));
            }
        }
        _ => {}
    }
}

/// Allowed producer outputs per consumer operation.
fn check_dep_types(op: Operation, dep_outputs: &[OutputType]) -> Result<(), String> {
    use OutputType as O;
    let has = |t: O| dep_outputs.contains(&t);
    let any_of = |ts: &[O]| ts.iter().any(|t| dep_outputs.contains(t));
    match op {
        Operation::SearchVocabulary => {
            if !dep_outputs.is_empty() {
                return Err("search_vocabulary takes no step dependencies".to_string());
            }
        }
        Operation::DescribeVocabulary => {
            if !has(O::VocabularyCandidates) {
                return Err(
                    "describe_vocabulary requires VocabularyCandidates (update consuming VocabularyCandidates as UriSet is a type error; describe first)"
                        .to_string(),
                );
            }
        }
        Operation::SearchInstances => {
            // Optional grounding on resolved vocabulary.
            if !dep_outputs.is_empty() && !has(O::ResolvedVocabulary) {
                return Err(
                    "search_instances dependencies must be ResolvedVocabulary".to_string(),
                );
            }
        }
        Operation::DescribeResource => {
            if !has(O::ResourceCandidates) {
                return Err("describe_resource requires ResourceCandidates".to_string());
            }
        }
        Operation::Query => {
            if !has(O::ResolvedVocabulary) {
                return Err("query requires ResolvedVocabulary".to_string());
            }
        }
        Operation::Create => {
            if !has(O::ResolvedVocabulary) {
                return Err("create requires ResolvedVocabulary".to_string());
            }
        }
        Operation::Update => {
            if !has(O::UriSet) {
                return Err(
                    "update requires a UriSet (update consuming VocabularyCandidates as UriSet is a type error)"
                        .to_string(),
                );
            }
            if !any_of(&[O::ResolvedVocabulary, O::ResourceCandidates]) {
                return Err(
                    "update requires ResolvedVocabulary or ResourceCandidates in addition to the UriSet"
                        .to_string(),
                );
            }
        }
        Operation::Delete => {
            if !has(O::UriSet) {
                return Err("delete requires a UriSet".to_string());
            }
        }
        Operation::Validate => {
            if !has(O::MutationResult) {
                return Err("validate requires a MutationResult".to_string());
            }
        }
        Operation::Verify => {
            if !any_of(&[O::MutationResult, O::ValidationResult]) {
                return Err("verify requires a MutationResult or ValidationResult".to_string());
            }
        }
    }
    Ok(())
}

fn check_ref_targets(
    s: &super::PlanStep,
    ids: &HashSet<&String>,
    errors: &mut Vec<PlanError>,
) {
    use super::TypedValue as T;
    let mut refs: Vec<&String> = vec![];
    match &s.args {
        StepArguments::DescribeVocab { iris } => {
            for v in iris {
                if let T::StepRef { step, .. } | T::UriSetRef { step } = v {
                    refs.push(step);
                }
            }
        }
        StepArguments::SearchInstances { types, .. } => {
            for v in types {
                if let T::StepRef { step, .. } | T::UriSetRef { step } = v {
                    refs.push(step);
                }
            }
        }
        StepArguments::DescribeResource { uris } => {
            for v in uris {
                if let T::StepRef { step, .. } | T::UriSetRef { step } = v {
                    refs.push(step);
                }
            }
        }
        StepArguments::Create { types, .. } => {
            for v in types {
                if let T::StepRef { step, .. } | T::UriSetRef { step } = v {
                    refs.push(step);
                }
            }
        }
        StepArguments::Update {
            targets,
            predicate,
            objects,
        } => {
            for v in targets.iter().chain(std::iter::once(predicate)).chain(objects.iter()) {
                if let T::StepRef { step, .. } | T::UriSetRef { step } = v {
                    refs.push(step);
                }
            }
        }
        StepArguments::Delete { targets } => {
            for v in targets {
                if let T::StepRef { step, .. } | T::UriSetRef { step } = v {
                    refs.push(step);
                }
            }
        }
        _ => {}
    }
    for r in refs {
        if !ids.contains(r) {
            errors.push(PlanError::step(
                &s.id,
                "dangling",
                format!("argument references unknown step {r:?}"),
            ));
        }
    }
}

fn find_cycle(plan: &Plan) -> Option<Vec<String>> {
    // DFS with color marks over owned ids (avoids borrow-lifetime issues).
    #[derive(PartialEq, Clone, Copy)]
    enum Mark {
        Temp,
        Perm,
    }
    let by_id: HashMap<String, &super::PlanStep> =
        plan.steps.iter().map(|s| (s.id.clone(), s)).collect();
    let mut marks: HashMap<String, Mark> = HashMap::new();
    let mut stack: Vec<String> = vec![];

    fn visit(
        id: &str,
        by_id: &HashMap<String, &super::PlanStep>,
        marks: &mut HashMap<String, Mark>,
        stack: &mut Vec<String>,
    ) -> Option<Vec<String>> {
        if marks.get(id) == Some(&Mark::Perm) {
            return None;
        }
        if marks.get(id) == Some(&Mark::Temp) {
            let pos = stack.iter().position(|x| x == id).unwrap_or(0);
            let mut cycle = stack[pos..].to_vec();
            cycle.push(id.to_string());
            return Some(cycle);
        }
        marks.insert(id.to_string(), Mark::Temp);
        stack.push(id.to_string());
        if let Some(step) = by_id.get(id) {
            for d in &step.depends_on {
                if by_id.contains_key(d.as_str()) {
                    if let Some(c) = visit(d, by_id, marks, stack) {
                        return Some(c);
                    }
                }
            }
        }
        stack.pop();
        marks.insert(id.to_string(), Mark::Perm);
        None
    }

    for s in &plan.steps {
        if !marks.contains_key(s.id.as_str()) {
            if let Some(c) = visit(&s.id, &by_id, &mut marks, &mut stack) {
                return Some(c);
            }
        }
    }
    None
}

fn check_operational(plan: &Plan, errors: &mut Vec<PlanError>) {
    let has_mutation = plan.steps.iter().any(|s| s.operation.is_mutation());
    if !has_mutation {
        return;
    }
    // Every mutation must transitively depend on discovery.
    let by_id: HashMap<&String, &super::PlanStep> =
        plan.steps.iter().map(|s| (&s.id, s)).collect();
    for s in plan.steps.iter().filter(|s| s.operation.is_mutation()) {
        if !transitively_reaches(s, &by_id, &|op| op.is_discovery()) {
            errors.push(PlanError::step(
                &s.id,
                "operational",
                "mutation must depend (directly or transitively) on discovery (search/describe/query)",
            ));
        }
    }
    // Terminal validate + verify after the last mutation.
    let last_mut = plan
        .steps
        .iter()
        .rposition(|s| s.operation.is_mutation());
    if let Some(idx) = last_mut {
        let tail = &plan.steps[idx + 1..];
        if !tail.iter().any(|s| s.operation == Operation::Validate) {
            errors.push(PlanError::plan(
                "operational",
                "plan with mutations must include validate after the last mutation",
            ));
        }
        if !tail.iter().any(|s| s.operation == Operation::Verify) {
            errors.push(PlanError::plan(
                "operational",
                "plan with mutations must include verify after the last mutation",
            ));
        }
    }
}

fn transitively_reaches(
    start: &super::PlanStep,
    by_id: &HashMap<&String, &super::PlanStep>,
    pred: &impl Fn(Operation) -> bool,
) -> bool {
    let mut seen: HashSet<String> = HashSet::new();
    let mut stack: Vec<String> = start.depends_on.clone();
    while let Some(id) = stack.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        if let Some(step) = by_id.get(&id) {
            if pred(step.operation) {
                return true;
            }
            stack.extend(step.depends_on.iter().cloned());
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::super::{canonical_ohio_plan, PlanStep, StepArguments, TypedValue};
    use super::*;

    #[test]
    fn valid_ohio_plan_passes() {
        assert!(validate_plan(&canonical_ohio_plan()).is_ok());
    }

    #[test]
    fn cycle_is_rejected() {
        let mut p = canonical_ohio_plan();
        // s1 -> s5 -> ... -> s1 cycle.
        p.steps[0].depends_on = vec!["s5".to_string()];
        let e = validate_plan(&p).unwrap_err();
        assert!(e.iter().any(|x| x.code == "cycle"), "{e:?}");
    }

    #[test]
    fn dangling_ref_is_rejected() {
        let mut p = canonical_ohio_plan();
        p.steps[4].depends_on = vec!["s99".to_string()];
        let e = validate_plan(&p).unwrap_err();
        assert!(e.iter().any(|x| x.code == "dangling"), "{e:?}");
    }

    #[test]
    fn type_mismatch_update_consuming_vocab_as_uriset_rejected() {
        let mut p = canonical_ohio_plan();
        // s5 (update) depending only on s1 (vocab) — no UriSet.
        p.steps[4].depends_on = vec!["s1".to_string()];
        p.steps[4].args = StepArguments::Update {
            targets: vec![TypedValue::StepRef {
                step: "s1".to_string(),
                field: "candidates".to_string(),
            }],
            predicate: TypedValue::unresolved("p"),
            objects: vec![],
        };
        let e = validate_plan(&p).unwrap_err();
        assert!(e.iter().any(|x| x.code == "types"), "{e:?}");
    }

    #[test]
    fn mutation_without_validate_verify_rejected() {
        let mut p = canonical_ohio_plan();
        p.steps.retain(|s| !matches!(s.operation, Operation::Validate | Operation::Verify));
        let e = validate_plan(&p).unwrap_err();
        assert!(
            e.iter().any(|x| x.code == "operational"),
            "{e:?}"
        );
    }

    #[test]
    fn mutation_without_discovery_rejected() {
        let mut p = canonical_ohio_plan();
        // Detach the update from discovery.
        let update = PlanStep {
            id: "s1".to_string(),
            operation: Operation::Update,
            intent: "rogue update".to_string(),
            args: StepArguments::Update {
                targets: vec![TypedValue::iri("http://example.com/Alice")],
                predicate: TypedValue::iri("http://example.com/name"),
                objects: vec![TypedValue::iri("http://example.com/Bob")],
            },
            depends_on: vec![],
            output: OutputType::MutationResult,
        };
        p.steps = vec![update];
        let e = validate_plan(&p).unwrap_err();
        assert!(e.iter().any(|x| x.code == "operational" || x.code == "types"), "{e:?}");
    }

    #[test]
    fn error_payload_shape() {
        let mut p = canonical_ohio_plan();
        p.steps[4].depends_on = vec!["s99".to_string()];
        let e = validate_plan(&p).unwrap_err();
        let v = serde_json::to_value(&e).unwrap();
        let first = &v.as_array().unwrap()[0];
        assert!(first.get("code").is_some());
        assert!(first.get("message").is_some());
    }
}
