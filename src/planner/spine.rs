//! Phase 2 — spine-first planning: Rust owns the structure.
//!
//! A [`Spine`] is a fixed plan shape: ordered operations, dependency
//! edges, ref kinds, output types. The request classifier
//! ([`spine_for_request`]) picks one deterministically (no model
//! involved); per-field elicitation asks the model only for small
//! scalars (search terms, instance text) through tiny JSON schemas;
//! [`assemble`] builds the concrete [`Plan`] directly in Rust and runs
//! the unchanged validator. The model never emits a nested envelope.
//!
//! Requests no spine matches — and spine paths that fail anyway — fall
//! back loudly to the free-emission tool loop (`unmatched-request` /
//! `spine-failed` routing artifacts), which is the metric phase 3
//! shrinks.

use std::collections::HashSet;

use super::{
    strip_code_fences, AgentBackend, AgentToolPlanner, Planner, PlannerError, PlanningServices,
    UserRequest,
};
use crate::plan::{
    GraphContext, Operation, OperationContext, OutputType, Plan, PlanError, PlanStep, StepArguments,
    TypedValue,
};

/// Scalar slots the model fills for a spine. Everything structural
/// (steps, deps, refs, outputs) comes from the [`Spine`] template.
#[derive(Debug, Clone, Default)]
pub struct Slots {
    /// Terms for the first (always present) vocabulary search.
    pub terms: Vec<String>,
    /// Terms for the second vocabulary search (L10 only).
    pub terms_b: Vec<String>,
    /// Free text for the instance search (when the spine has one).
    pub text: String,
}

/// One step of a spine template: everything except scalar args.
pub struct SpineStep {
    pub id: &'static str,
    pub operation: Operation,
    pub intent: &'static str,
    pub depends_on: &'static [&'static str],
}

/// A fixed plan shape covering one request class.
pub struct Spine {
    /// Short name for routing artifacts and logs.
    pub name: &'static str,
    /// Ladder rung this spine covers (1–10).
    pub level: u8,
    /// All keywords must appear as whole words (lowercased request).
    /// Spines are tried in table order, most specific first.
    pub keywords: &'static [&'static str],
    pub needs_terms_b: bool,
    pub needs_text: bool,
    pub steps: &'static [SpineStep],
}

const SPINES: &[Spine] = &[
    Spine {
        name: "reconcile",
        level: 10,
        keywords: &["reconcile"],
        needs_terms_b: true,
        needs_text: true,
        steps: &[
            SpineStep { id: "s1", operation: Operation::SearchVocabulary, intent: "discover contact and status vocabulary", depends_on: &[] },
            SpineStep { id: "s2", operation: Operation::SearchVocabulary, intent: "discover account and state vocabulary", depends_on: &[] },
            SpineStep { id: "s3", operation: Operation::DescribeVocabulary, intent: "resolve contact term domains", depends_on: &["s1"] },
            SpineStep { id: "s4", operation: Operation::DescribeVocabulary, intent: "resolve account term domains", depends_on: &["s2"] },
            SpineStep { id: "s5", operation: Operation::SearchInstances, intent: "find Ohio account instances", depends_on: &["s3"] },
            SpineStep { id: "s6", operation: Operation::Query, intent: "find contacts of Ohio accounts", depends_on: &["s4"] },
            SpineStep { id: "s7", operation: Operation::DescribeResource, intent: "describe the account instances", depends_on: &["s3", "s5"] },
            SpineStep { id: "s8", operation: Operation::Update, intent: "classify the contacts as prospects", depends_on: &["s6", "s4"] },
            SpineStep { id: "s9", operation: Operation::Validate, intent: "check the resulting graph", depends_on: &["s8"] },
            SpineStep { id: "s10", operation: Operation::Verify, intent: "count classified contacts", depends_on: &["s9"] },
        ],
    },
    Spine {
        name: "delete",
        level: 9,
        keywords: &["delete"],
        needs_terms_b: false,
        needs_text: true,
        steps: &[
            SpineStep { id: "s1", operation: Operation::SearchVocabulary, intent: "discover contact and status vocabulary", depends_on: &[] },
            SpineStep { id: "s2", operation: Operation::DescribeVocabulary, intent: "resolve term domains", depends_on: &["s1"] },
            SpineStep { id: "s3", operation: Operation::Query, intent: "find stale prospects", depends_on: &["s2"] },
            SpineStep { id: "s4", operation: Operation::SearchInstances, intent: "confirm stale instances", depends_on: &["s2"] },
            SpineStep { id: "s5", operation: Operation::Delete, intent: "remove the stale records", depends_on: &["s3", "s4"] },
            SpineStep { id: "s6", operation: Operation::Validate, intent: "check the resulting graph", depends_on: &["s5"] },
            SpineStep { id: "s7", operation: Operation::Verify, intent: "confirm the removals", depends_on: &["s6"] },
        ],
    },
    Spine {
        name: "create",
        level: 8,
        keywords: &["create"],
        needs_terms_b: false,
        needs_text: false,
        steps: &[
            SpineStep { id: "s1", operation: Operation::SearchVocabulary, intent: "discover contact vocabulary", depends_on: &[] },
            SpineStep { id: "s2", operation: Operation::DescribeVocabulary, intent: "resolve term domains", depends_on: &["s1"] },
            SpineStep { id: "s3", operation: Operation::Create, intent: "create the contact", depends_on: &["s2"] },
            SpineStep { id: "s4", operation: Operation::Validate, intent: "check the resulting graph", depends_on: &["s3"] },
            SpineStep { id: "s5", operation: Operation::Verify, intent: "confirm the created contact", depends_on: &["s4"] },
        ],
    },
    Spine {
        name: "classify",
        level: 7,
        keywords: &["classify"],
        needs_terms_b: false,
        needs_text: false,
        steps: &[
            SpineStep { id: "s1", operation: Operation::SearchVocabulary, intent: "discover contact and status vocabulary", depends_on: &[] },
            SpineStep { id: "s2", operation: Operation::DescribeVocabulary, intent: "resolve term domains", depends_on: &["s1"] },
            SpineStep { id: "s3", operation: Operation::Query, intent: "find matching contacts", depends_on: &["s2"] },
            SpineStep { id: "s4", operation: Operation::Update, intent: "classify contacts as prospects", depends_on: &["s2", "s3"] },
            SpineStep { id: "s5", operation: Operation::Validate, intent: "check the resulting graph", depends_on: &["s4"] },
            SpineStep { id: "s6", operation: Operation::Verify, intent: "count classified contacts", depends_on: &["s5"] },
        ],
    },
    Spine {
        name: "describe-instances",
        level: 6,
        keywords: &["describe", "instances"],
        needs_terms_b: false,
        needs_text: true,
        steps: &[
            SpineStep { id: "s1", operation: Operation::SearchVocabulary, intent: "discover contact and account vocabulary", depends_on: &[] },
            SpineStep { id: "s2", operation: Operation::DescribeVocabulary, intent: "resolve term domains", depends_on: &["s1"] },
            SpineStep { id: "s3", operation: Operation::SearchInstances, intent: "find instances grounded on resolved terms", depends_on: &["s2"] },
            SpineStep { id: "s4", operation: Operation::DescribeResource, intent: "describe the candidate instances", depends_on: &["s2", "s3"] },
        ],
    },
    Spine {
        name: "grounded-search",
        level: 5,
        keywords: &["instances", "resolved"],
        needs_terms_b: false,
        needs_text: true,
        steps: &[
            SpineStep { id: "s1", operation: Operation::SearchVocabulary, intent: "discover contact and account vocabulary", depends_on: &[] },
            SpineStep { id: "s2", operation: Operation::DescribeVocabulary, intent: "resolve term domains", depends_on: &["s1"] },
            SpineStep { id: "s3", operation: Operation::SearchInstances, intent: "find instances grounded on resolved terms", depends_on: &["s2"] },
        ],
    },
    Spine {
        name: "query",
        level: 4,
        keywords: &["ohio"],
        needs_terms_b: false,
        needs_text: false,
        steps: &[
            SpineStep { id: "s1", operation: Operation::SearchVocabulary, intent: "discover contact and state vocabulary", depends_on: &[] },
            SpineStep { id: "s2", operation: Operation::DescribeVocabulary, intent: "resolve term domains", depends_on: &["s1"] },
            SpineStep { id: "s3", operation: Operation::Query, intent: "find matching contacts", depends_on: &["s2"] },
        ],
    },
    Spine {
        name: "resolve",
        level: 3,
        keywords: &["resolve"],
        needs_terms_b: false,
        needs_text: false,
        steps: &[
            SpineStep { id: "s1", operation: Operation::SearchVocabulary, intent: "discover contact and status vocabulary", depends_on: &[] },
            SpineStep { id: "s2", operation: Operation::DescribeVocabulary, intent: "resolve term domains", depends_on: &["s1"] },
        ],
    },
    Spine {
        name: "dual-discovery",
        level: 2,
        keywords: &["vocabulary", "instances"],
        needs_terms_b: false,
        needs_text: true,
        steps: &[
            SpineStep { id: "s1", operation: Operation::SearchVocabulary, intent: "discover contact vocabulary", depends_on: &[] },
            SpineStep { id: "s2", operation: Operation::SearchInstances, intent: "find instances", depends_on: &[] },
        ],
    },
    Spine {
        name: "lookup",
        level: 1,
        keywords: &["list", "vocabulary"],
        needs_terms_b: false,
        needs_text: false,
        steps: &[
            SpineStep { id: "s1", operation: Operation::SearchVocabulary, intent: "discover contact vocabulary", depends_on: &[] },
        ],
    },
];

fn request_words(request: &str) -> HashSet<String> {
    request
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

/// Deterministic request → spine classifier. Whole-word conjunctions,
/// table order (most specific first). Returns `None` for anything no
/// spine covers — the honest fallback signal, never a guess.
pub fn spine_for_request(request: &str) -> Option<&'static Spine> {
    let words = request_words(request);
    SPINES.iter().find(|s| s.keywords.iter().all(|k| words.contains(*k)))
}

/// All spines, for coverage reporting.
pub fn all_spines() -> Vec<&'static Spine> {
    SPINES.iter().collect()
}

fn step_ref(step: &str) -> TypedValue {
    TypedValue::StepRef { step: step.to_string(), field: "candidates".to_string() }
}

fn uri_set(step: &str) -> TypedValue {
    TypedValue::UriSetRef { step: step.to_string() }
}

/// Build the concrete plan for `spine` from elicited `slots`.
/// Shapes mirror the ladder rungs; validation runs unchanged — a
/// failure here is a template bug, and the caller falls back loudly.
pub fn assemble(
    spine: &Spine,
    slots: &Slots,
    goal: &str,
    graph_ctx: &GraphContext,
) -> Result<Plan, Vec<PlanError>> {
    let query_pattern = || serde_json::json!({});
    let mut steps: Vec<PlanStep> = vec![];
    let push = |steps: &mut Vec<PlanStep>, spec: &SpineStep, args: StepArguments| {
        steps.push(PlanStep {
            id: spec.id.to_string(),
            operation: spec.operation,
            intent: spec.intent.to_string(),
            args,
            depends_on: spec.depends_on.iter().map(|s| s.to_string()).collect(),
            output: OutputType::expected_for(spec.operation),
        });
    };
    match spine.level {
        1 => push(&mut steps, &spine.steps[0], StepArguments::SearchVocab {
            terms: slots.terms.clone(), namespaces: vec![],
        }),
        2 => {
            push(&mut steps, &spine.steps[0], StepArguments::SearchVocab {
                terms: slots.terms.clone(), namespaces: vec![],
            });
            push(&mut steps, &spine.steps[1], StepArguments::SearchInstances {
                text: slots.text.clone(), types: vec![], namespace: None,
            });
        }
        3 => {
            push(&mut steps, &spine.steps[0], StepArguments::SearchVocab {
                terms: slots.terms.clone(), namespaces: vec![],
            });
            push(&mut steps, &spine.steps[1], StepArguments::DescribeVocab {
                iris: vec![step_ref("s1")],
            });
        }
        4 => {
            push(&mut steps, &spine.steps[0], StepArguments::SearchVocab {
                terms: slots.terms.clone(), namespaces: vec![],
            });
            push(&mut steps, &spine.steps[1], StepArguments::DescribeVocab {
                iris: vec![step_ref("s1")],
            });
            push(&mut steps, &spine.steps[2], StepArguments::Query {
                pattern: query_pattern(),
            });
        }
        5 => {
            push(&mut steps, &spine.steps[0], StepArguments::SearchVocab {
                terms: slots.terms.clone(), namespaces: vec![],
            });
            push(&mut steps, &spine.steps[1], StepArguments::DescribeVocab {
                iris: vec![step_ref("s1")],
            });
            push(&mut steps, &spine.steps[2], StepArguments::SearchInstances {
                text: slots.text.clone(),
                types: vec![step_ref("s1")],
                namespace: None,
            });
        }
        6 => {
            push(&mut steps, &spine.steps[0], StepArguments::SearchVocab {
                terms: slots.terms.clone(), namespaces: vec![],
            });
            push(&mut steps, &spine.steps[1], StepArguments::DescribeVocab {
                iris: vec![step_ref("s1")],
            });
            push(&mut steps, &spine.steps[2], StepArguments::SearchInstances {
                text: slots.text.clone(),
                types: vec![step_ref("s1")],
                namespace: None,
            });
            push(&mut steps, &spine.steps[3], StepArguments::DescribeResource {
                uris: vec![step_ref("s3")],
            });
        }
        7 => {
            push(&mut steps, &spine.steps[0], StepArguments::SearchVocab {
                terms: slots.terms.clone(), namespaces: vec![],
            });
            push(&mut steps, &spine.steps[1], StepArguments::DescribeVocab {
                iris: vec![step_ref("s1")],
            });
            push(&mut steps, &spine.steps[2], StepArguments::Query {
                pattern: query_pattern(),
            });
            push(&mut steps, &spine.steps[3], StepArguments::Update {
                targets: vec![uri_set("s3")],
                predicate: TypedValue::unresolved("status"),
                objects: vec![TypedValue::unresolved("prospect")],
            });
            push(&mut steps, &spine.steps[4], StepArguments::Validate {});
            push(&mut steps, &spine.steps[5], StepArguments::Verify {
                question: "How many resources were affected?".to_string(),
            });
        }
        8 => {
            push(&mut steps, &spine.steps[0], StepArguments::SearchVocab {
                terms: slots.terms.clone(), namespaces: vec![],
            });
            push(&mut steps, &spine.steps[1], StepArguments::DescribeVocab {
                iris: vec![step_ref("s1")],
            });
            push(&mut steps, &spine.steps[2], StepArguments::Create {
                types: vec![TypedValue::unresolved("contact")],
                properties: vec![],
            });
            push(&mut steps, &spine.steps[3], StepArguments::Validate {});
            push(&mut steps, &spine.steps[4], StepArguments::Verify {
                question: "How many resources were affected?".to_string(),
            });
        }
        9 => {
            push(&mut steps, &spine.steps[0], StepArguments::SearchVocab {
                terms: slots.terms.clone(), namespaces: vec![],
            });
            push(&mut steps, &spine.steps[1], StepArguments::DescribeVocab {
                iris: vec![step_ref("s1")],
            });
            push(&mut steps, &spine.steps[2], StepArguments::Query {
                pattern: query_pattern(),
            });
            push(&mut steps, &spine.steps[3], StepArguments::SearchInstances {
                text: slots.text.clone(),
                types: vec![step_ref("s1")],
                namespace: None,
            });
            push(&mut steps, &spine.steps[4], StepArguments::Delete {
                targets: vec![uri_set("s3")],
            });
            push(&mut steps, &spine.steps[5], StepArguments::Validate {});
            push(&mut steps, &spine.steps[6], StepArguments::Verify {
                question: "How many resources were affected?".to_string(),
            });
        }
        _ => {
            // Level 10: parallel discovery, diamonds, update, terminal gates.
            push(&mut steps, &spine.steps[0], StepArguments::SearchVocab {
                terms: slots.terms.clone(), namespaces: vec![],
            });
            push(&mut steps, &spine.steps[1], StepArguments::SearchVocab {
                terms: slots.terms_b.clone(), namespaces: vec![],
            });
            push(&mut steps, &spine.steps[2], StepArguments::DescribeVocab {
                iris: vec![step_ref("s1")],
            });
            push(&mut steps, &spine.steps[3], StepArguments::DescribeVocab {
                iris: vec![step_ref("s2")],
            });
            push(&mut steps, &spine.steps[4], StepArguments::SearchInstances {
                text: slots.text.clone(),
                types: vec![step_ref("s2")],
                namespace: None,
            });
            push(&mut steps, &spine.steps[5], StepArguments::Query {
                pattern: query_pattern(),
            });
            push(&mut steps, &spine.steps[6], StepArguments::DescribeResource {
                uris: vec![step_ref("s5")],
            });
            push(&mut steps, &spine.steps[7], StepArguments::Update {
                targets: vec![uri_set("s6")],
                predicate: TypedValue::unresolved("status"),
                objects: vec![TypedValue::unresolved("prospect")],
            });
            push(&mut steps, &spine.steps[8], StepArguments::Validate {});
            push(&mut steps, &spine.steps[9], StepArguments::Verify {
                question: "How many resources were affected?".to_string(),
            });
        }
    }
    let plan = Plan {
        goal: goal.to_string(),
        context: graph_ctx.clone(),
        steps,
        assumptions: vec![],
    };
    crate::plan::validate_plan(&plan).map(|_| plan)
}

/// Spine-first planner: classify → elicit scalars → assemble.
/// Anything uncovered or failed falls back loudly to the tool loop.
pub struct SpinePlanner {
    backend: AgentBackend,
    services: PlanningServices,
    pub max_elicitation_repairs: usize,
}

impl SpinePlanner {
    pub fn new(backend: AgentBackend, services: PlanningServices) -> Self {
        Self { backend, services, max_elicitation_repairs: 1 }
    }

    async fn elicit_terms(
        &self,
        request: &UserRequest,
        label: &str,
    ) -> Result<Vec<String>, PlannerError> {
        let prompt = format!(
            "List the vocabulary search terms (single words or short phrases) for this request. \
            Reply with ONLY a JSON object like {{\"terms\": [\"contact\"]}} — no prose, no fences.\n\n\
            Request: {}\nWhich {}?",
            request.text, label
        );
        let schema = serde_json::json!({
            "type": "object",
            "required": ["terms"],
            "properties": {"terms": {"type": "array", "items": {"type": "string"}, "minItems": 1}},
            "additionalProperties": false
        });
        let value = self.elicit(&prompt, schema).await?;
        let terms: Vec<String> = value
            .get("terms")
            .and_then(|t| t.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        if terms.is_empty() {
            return Err(PlannerError::Schema("elicited terms are empty".to_string()));
        }
        Ok(terms)
    }

    async fn elicit_text(&self, request: &UserRequest) -> Result<String, PlannerError> {
        let prompt = format!(
            "Give the short instance-search text for this request (e.g. a name like \"Acme\"). \
            Reply with ONLY a JSON object like {{\"text\": \"Acme\"}} — no prose, no fences.\n\n\
            Request: {}",
            request.text
        );
        let schema = serde_json::json!({
            "type": "object",
            "required": ["text"],
            "properties": {"text": {"type": "string", "minLength": 1}},
            "additionalProperties": false
        });
        let value = self.elicit(&prompt, schema).await?;
        let text = value
            .get("text")
            .and_then(|t| t.as_str())
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        if text.is_empty() {
            return Err(PlannerError::Schema("elicited text is empty".to_string()));
        }
        Ok(text)
    }

    async fn elicit(
        &self,
        prompt: &str,
        schema: serde_json::Value,
    ) -> Result<serde_json::Value, PlannerError> {
        let mut agent = self.backend.stream();
        agent.set_system("You answer with only the requested JSON object. No prose, no fences.");
        let mut message = prompt.to_string();
        for _ in 0..=self.max_elicitation_repairs {
            let resp = agent
                .send(&message)
                .with_schema(schema.clone(), Some("slots".to_string()))
                .await
                .map_err(PlannerError::Model)?;
            let raw = strip_code_fences(&resp.text);
            match serde_json::from_str::<serde_json::Value>(raw) {
                Ok(v) => return Ok(v),
                Err(e) => {
                    message = format!(
                        "That was not valid JSON ({e}). Reply with ONLY the JSON object, no prose."
                    );
                }
            }
        }
        Err(PlannerError::Schema("elicitation did not produce JSON".to_string()))
    }

    async fn create_plan_spine(
        &self,
        spine: &Spine,
        request: &UserRequest,
        ctx: &mut OperationContext,
    ) -> Result<Plan, PlannerError> {
        let terms = self.elicit_terms(request, "vocabulary search terms").await?;
        let terms_b = if spine.needs_terms_b {
            self.elicit_terms(request, "terms for the second vocabulary search").await?
        } else {
            vec![]
        };
        let text = if spine.needs_text {
            self.elicit_text(request).await?
        } else {
            String::new()
        };
        let slots = Slots { terms, terms_b, text };
        assemble(spine, &slots, &request.text, &ctx.graph_context)
            .map_err(PlannerError::Validation)
    }
}

#[async_trait::async_trait]
impl Planner for SpinePlanner {
    async fn create_plan(
        &self,
        request: &UserRequest,
        ctx: &mut OperationContext,
    ) -> Result<Plan, PlannerError> {
        let Some(spine) = spine_for_request(&request.text) else {
            ctx.artifacts.insert(
                "routing".to_string(),
                "unmatched-request".to_string(),
            );
            return AgentToolPlanner::new(self.backend.clone(), self.services.clone())
                .create_plan(request, ctx)
                .await;
        };
        match self.create_plan_spine(spine, request, ctx).await {
            Ok(plan) => {
                ctx.artifacts.insert(
                    "routing".to_string(),
                    format!("spine:{}", spine.name),
                );
                Ok(plan)
            }
            Err(e) => {
                ctx.artifacts.insert(
                    "routing".to_string(),
                    format!("spine-failed:{}", truncate(&format!("{e:?}"), 160)),
                );
                AgentToolPlanner::new(self.backend.clone(), self.services.clone())
                    .create_plan(request, ctx)
                    .await
            }
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() > max {
        format!("{}…", &s[..max])
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph_ctx() -> GraphContext {
        GraphContext {
            graphs: vec![],
            namespaces: [("ex".to_string(), "http://example.com/".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        }
    }

    fn slots() -> Slots {
        Slots {
            terms: vec!["contact".to_string(), "status".to_string()],
            terms_b: vec!["account".to_string(), "state".to_string()],
            text: "Acme".to_string(),
        }
    }

    #[test]
    fn every_spine_assembles_a_validating_plan() {
        let ctx = graph_ctx();
        let slots = slots();
        for spine in all_spines() {
            let plan = assemble(spine, &slots, "test goal", &ctx)
                .unwrap_or_else(|e| panic!("spine {} must assemble+validate: {e:?}", spine.name));
            assert_eq!(plan.steps.len() as u8, spine.steps.len() as u8, "{}", spine.name);
            assert_eq!(plan.goal, "test goal");
        }
    }

    #[test]
    fn classifier_covers_ladder_rungs_exactly() {
        let cases = [
            ("List the vocabulary terms for contacts.", 1),
            ("Find contact vocabulary and search for Acme instances.", 2),
            ("Resolve the contact and status vocabulary terms.", 3),
            ("Find all contacts in Ohio.", 4),
            ("Find Acme-related instances using the resolved vocabulary.", 5),
            ("Describe the candidate Acme instances.", 6),
            ("Classify Ohio contacts as prospects.", 7),
            ("Create a new contact record.", 8),
            ("Delete stale prospect records.", 9),
            ("Reconcile Ohio accounts and classify their contacts.", 10),
        ];
        for (request, level) in cases {
            let spine = spine_for_request(request)
                .unwrap_or_else(|| panic!("rung L{level} must match a spine"));
            assert_eq!(spine.level, level, "request: {request}");
        }
    }

    #[test]
    fn classifier_orders_confusable_pairs() {
        // L10 contains "classify" but must win over L7; L7 contains
        // "Ohio" but must win over L4; L5 contains "resolve" only as
        // "resolved" (whole-word matching keeps it distinct from L3).
        let s = spine_for_request("Reconcile Ohio accounts and classify their contacts.").unwrap();
        assert_eq!(s.level, 10);
        let s = spine_for_request("Classify Ohio contacts as prospects.").unwrap();
        assert_eq!(s.level, 7);
        let s = spine_for_request("Find Acme-related instances using the resolved vocabulary.").unwrap();
        assert_eq!(s.level, 5);
    }

    #[test]
    fn classifier_returns_none_outside_coverage() {
        assert!(spine_for_request("Change a contact literal property").is_none());
        assert!(spine_for_request("Find all contacts").is_none());
        assert!(spine_for_request("do something unclassifiable xyzzy").is_none());
    }
}
