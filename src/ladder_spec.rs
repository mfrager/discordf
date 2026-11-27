//! 10-step planning ladder spec (shared by `tests/ladder.rs` and the
//! `ladder_cli` binary).
//!
//! Rungs run from trivial to complex:
//! ```text
//! L1  1 step   single search_vocabulary (no deps)
//! L2  2 steps  two independent discoveries
//! L3  2 steps  chain: search -> describe
//! L4  3 steps  chain: search -> describe -> query
//! L5  3 steps  grounded instance search
//! L6  4 steps  diamond: describe_resource over two producers
//! L7  6 steps  full update spine (Ohio pattern)
//! L8  5 steps  create spine
//! L9  7 steps  delete spine with fan-in
//! L10 10 steps mixed: parallel discovery, diamonds, update, terminal gates
//! ```
//!
//! Every rung's shape satisfies `plan::validate_plan`: dependency type
//! rules, ref-target existence, and terminal validate/verify after the
//! last mutation.

/// One step of a ladder rung.
pub struct StepSpec {
    pub id: &'static str,
    pub operation: &'static str,
    pub intent: &'static str,
    pub args: serde_json::Value,
    pub depends_on: Vec<&'static str>,
    pub output: &'static str,
}

/// One ladder rung: a request plus its expected plan shape.
pub struct Rung {
    pub level: u8,
    pub request: &'static str,
    pub steps: Vec<StepSpec>,
}

fn search_vocab(terms: &[&str]) -> serde_json::Value {
    serde_json::json!({"kind": "search_vocab", "terms": terms})
}

fn describe_vocab(step: &str) -> serde_json::Value {
    serde_json::json!({"kind": "describe_vocab",
        "iris": [{"kind": "step_ref", "step": step, "field": "candidates"}]})
}

fn search_instances(text: &str) -> serde_json::Value {
    serde_json::json!({"kind": "search_instances", "text": text})
}

fn grounded_search(text: &str, step: &str) -> serde_json::Value {
    serde_json::json!({"kind": "search_instances", "text": text,
        "types": [{"kind": "step_ref", "step": step, "field": "candidates"}]})
}

fn describe_resource(step: &str) -> serde_json::Value {
    serde_json::json!({"kind": "describe_resource",
        "uris": [{"kind": "step_ref", "step": step, "field": "candidates"}]})
}

fn query() -> serde_json::Value {
    serde_json::json!({"kind": "query", "pattern": {}})
}

fn update(step: &str) -> serde_json::Value {
    serde_json::json!({"kind": "update",
        "targets": [{"kind": "uri_set_ref", "step": step}],
        "predicate": {"kind": "unresolved", "description": "status"},
        "objects": [{"kind": "unresolved", "description": "prospect"}]})
}

fn create() -> serde_json::Value {
    serde_json::json!({"kind": "create",
        "types": [{"kind": "unresolved", "description": "contact"}]})
}

fn delete(step: &str) -> serde_json::Value {
    serde_json::json!({"kind": "delete",
        "targets": [{"kind": "uri_set_ref", "step": step}]})
}

fn verify() -> serde_json::Value {
    serde_json::json!({"kind": "verify", "question": "How many resources were affected?"})
}

/// The full 10-rung ladder, ordered trivial → complex.
pub fn ladder() -> Vec<Rung> {
    vec![
        Rung {
            level: 1,
            request: "List the vocabulary terms for contacts.",
            steps: vec![StepSpec {
                id: "s1",
                operation: "search_vocabulary",
                intent: "discover contact vocabulary",
                args: search_vocab(&["contact"]),
                depends_on: vec![],
                output: "VocabularyCandidates",
            }],
        },
        Rung {
            level: 2,
            request: "Find contact vocabulary and search for Acme instances.",
            steps: vec![
                StepSpec {
                    id: "s1",
                    operation: "search_vocabulary",
                    intent: "discover contact vocabulary",
                    args: search_vocab(&["contact"]),
                    depends_on: vec![],
                    output: "VocabularyCandidates",
                },
                StepSpec {
                    id: "s2",
                    operation: "search_instances",
                    intent: "find Acme instances",
                    args: search_instances("Acme"),
                    depends_on: vec![],
                    output: "ResourceCandidates",
                },
            ],
        },
        Rung {
            level: 3,
            request: "Resolve the contact and status vocabulary terms.",
            steps: vec![
                StepSpec {
                    id: "s1",
                    operation: "search_vocabulary",
                    intent: "discover contact and status vocabulary",
                    args: search_vocab(&["contact", "status"]),
                    depends_on: vec![],
                    output: "VocabularyCandidates",
                },
                StepSpec {
                    id: "s2",
                    operation: "describe_vocabulary",
                    intent: "resolve term domains",
                    args: describe_vocab("s1"),
                    depends_on: vec!["s1"],
                    output: "ResolvedVocabulary",
                },
            ],
        },
        Rung {
            level: 4,
            request: "Find all contacts in Ohio.",
            steps: vec![
                StepSpec {
                    id: "s1",
                    operation: "search_vocabulary",
                    intent: "discover contact and state vocabulary",
                    args: search_vocab(&["contact", "state"]),
                    depends_on: vec![],
                    output: "VocabularyCandidates",
                },
                StepSpec {
                    id: "s2",
                    operation: "describe_vocabulary",
                    intent: "resolve term domains",
                    args: describe_vocab("s1"),
                    depends_on: vec!["s1"],
                    output: "ResolvedVocabulary",
                },
                StepSpec {
                    id: "s3",
                    operation: "query",
                    intent: "find matching contacts",
                    args: query(),
                    depends_on: vec!["s2"],
                    output: "UriSet",
                },
            ],
        },
        Rung {
            level: 5,
            request: "Find Acme-related instances using the resolved vocabulary.",
            steps: vec![
                StepSpec {
                    id: "s1",
                    operation: "search_vocabulary",
                    intent: "discover contact and account vocabulary",
                    args: search_vocab(&["contact", "account"]),
                    depends_on: vec![],
                    output: "VocabularyCandidates",
                },
                StepSpec {
                    id: "s2",
                    operation: "describe_vocabulary",
                    intent: "resolve term domains",
                    args: describe_vocab("s1"),
                    depends_on: vec!["s1"],
                    output: "ResolvedVocabulary",
                },
                StepSpec {
                    id: "s3",
                    operation: "search_instances",
                    intent: "find Acme instances grounded on resolved terms",
                    args: grounded_search("Acme", "s1"),
                    depends_on: vec!["s2"],
                    output: "ResourceCandidates",
                },
            ],
        },
        Rung {
            level: 6,
            request: "Describe the candidate Acme instances.",
            steps: vec![
                StepSpec {
                    id: "s1",
                    operation: "search_vocabulary",
                    intent: "discover contact and account vocabulary",
                    args: search_vocab(&["contact", "account"]),
                    depends_on: vec![],
                    output: "VocabularyCandidates",
                },
                StepSpec {
                    id: "s2",
                    operation: "describe_vocabulary",
                    intent: "resolve term domains",
                    args: describe_vocab("s1"),
                    depends_on: vec!["s1"],
                    output: "ResolvedVocabulary",
                },
                StepSpec {
                    id: "s3",
                    operation: "search_instances",
                    intent: "find Acme instances grounded on resolved terms",
                    args: grounded_search("Acme", "s1"),
                    depends_on: vec!["s2"],
                    output: "ResourceCandidates",
                },
                StepSpec {
                    id: "s4",
                    operation: "describe_resource",
                    intent: "describe the candidate instances",
                    args: describe_resource("s3"),
                    depends_on: vec!["s2", "s3"],
                    output: "ResourceDescriptions",
                },
            ],
        },
        Rung {
            level: 7,
            request: "Classify Ohio contacts as prospects.",
            steps: vec![
                StepSpec {
                    id: "s1",
                    operation: "search_vocabulary",
                    intent: "discover contact and status vocabulary",
                    args: search_vocab(&["contact", "status"]),
                    depends_on: vec![],
                    output: "VocabularyCandidates",
                },
                StepSpec {
                    id: "s2",
                    operation: "describe_vocabulary",
                    intent: "resolve term domains",
                    args: describe_vocab("s1"),
                    depends_on: vec!["s1"],
                    output: "ResolvedVocabulary",
                },
                StepSpec {
                    id: "s3",
                    operation: "query",
                    intent: "find matching contacts",
                    args: query(),
                    depends_on: vec!["s2"],
                    output: "UriSet",
                },
                StepSpec {
                    id: "s4",
                    operation: "update",
                    intent: "classify contacts as prospects",
                    args: update("s3"),
                    depends_on: vec!["s2", "s3"],
                    output: "MutationResult",
                },
                StepSpec {
                    id: "s5",
                    operation: "validate",
                    intent: "check the resulting graph",
                    args: serde_json::json!({"kind": "validate"}),
                    depends_on: vec!["s4"],
                    output: "ValidationResult",
                },
                StepSpec {
                    id: "s6",
                    operation: "verify",
                    intent: "count classified contacts",
                    args: verify(),
                    depends_on: vec!["s5"],
                    output: "VerificationResult",
                },
            ],
        },
        Rung {
            level: 8,
            request: "Create a new contact record.",
            steps: vec![
                StepSpec {
                    id: "s1",
                    operation: "search_vocabulary",
                    intent: "discover contact vocabulary",
                    args: search_vocab(&["contact"]),
                    depends_on: vec![],
                    output: "VocabularyCandidates",
                },
                StepSpec {
                    id: "s2",
                    operation: "describe_vocabulary",
                    intent: "resolve term domains",
                    args: describe_vocab("s1"),
                    depends_on: vec!["s1"],
                    output: "ResolvedVocabulary",
                },
                StepSpec {
                    id: "s3",
                    operation: "create",
                    intent: "create the contact",
                    args: create(),
                    depends_on: vec!["s2"],
                    output: "MutationResult",
                },
                StepSpec {
                    id: "s4",
                    operation: "validate",
                    intent: "check the resulting graph",
                    args: serde_json::json!({"kind": "validate"}),
                    depends_on: vec!["s3"],
                    output: "ValidationResult",
                },
                StepSpec {
                    id: "s5",
                    operation: "verify",
                    intent: "confirm the created contact",
                    args: verify(),
                    depends_on: vec!["s4"],
                    output: "VerificationResult",
                },
            ],
        },
        Rung {
            level: 9,
            request: "Delete stale prospect records.",
            steps: vec![
                StepSpec {
                    id: "s1",
                    operation: "search_vocabulary",
                    intent: "discover contact and status vocabulary",
                    args: search_vocab(&["contact", "status"]),
                    depends_on: vec![],
                    output: "VocabularyCandidates",
                },
                StepSpec {
                    id: "s2",
                    operation: "describe_vocabulary",
                    intent: "resolve term domains",
                    args: describe_vocab("s1"),
                    depends_on: vec!["s1"],
                    output: "ResolvedVocabulary",
                },
                StepSpec {
                    id: "s3",
                    operation: "query",
                    intent: "find stale prospects",
                    args: query(),
                    depends_on: vec!["s2"],
                    output: "UriSet",
                },
                StepSpec {
                    id: "s4",
                    operation: "search_instances",
                    intent: "confirm stale instances",
                    args: grounded_search("stale prospect", "s1"),
                    depends_on: vec!["s2"],
                    output: "ResourceCandidates",
                },
                StepSpec {
                    id: "s5",
                    operation: "delete",
                    intent: "remove the stale records",
                    args: delete("s3"),
                    depends_on: vec!["s3", "s4"],
                    output: "MutationResult",
                },
                StepSpec {
                    id: "s6",
                    operation: "validate",
                    intent: "check the resulting graph",
                    args: serde_json::json!({"kind": "validate"}),
                    depends_on: vec!["s5"],
                    output: "ValidationResult",
                },
                StepSpec {
                    id: "s7",
                    operation: "verify",
                    intent: "confirm the removals",
                    args: verify(),
                    depends_on: vec!["s6"],
                    output: "VerificationResult",
                },
            ],
        },
        Rung {
            level: 10,
            request: "Reconcile Ohio accounts and classify their contacts.",
            steps: vec![
                StepSpec {
                    id: "s1",
                    operation: "search_vocabulary",
                    intent: "discover contact and status vocabulary",
                    args: search_vocab(&["contact", "status"]),
                    depends_on: vec![],
                    output: "VocabularyCandidates",
                },
                StepSpec {
                    id: "s2",
                    operation: "search_vocabulary",
                    intent: "discover account and state vocabulary",
                    args: search_vocab(&["account", "state"]),
                    depends_on: vec![],
                    output: "VocabularyCandidates",
                },
                StepSpec {
                    id: "s3",
                    operation: "describe_vocabulary",
                    intent: "resolve contact term domains",
                    args: describe_vocab("s1"),
                    depends_on: vec!["s1"],
                    output: "ResolvedVocabulary",
                },
                StepSpec {
                    id: "s4",
                    operation: "describe_vocabulary",
                    intent: "resolve account term domains",
                    args: describe_vocab("s2"),
                    depends_on: vec!["s2"],
                    output: "ResolvedVocabulary",
                },
                StepSpec {
                    id: "s5",
                    operation: "search_instances",
                    intent: "find Ohio account instances",
                    args: grounded_search("Ohio account", "s2"),
                    depends_on: vec!["s3"],
                    output: "ResourceCandidates",
                },
                StepSpec {
                    id: "s6",
                    operation: "query",
                    intent: "find contacts of Ohio accounts",
                    args: query(),
                    depends_on: vec!["s4"],
                    output: "UriSet",
                },
                StepSpec {
                    id: "s7",
                    operation: "describe_resource",
                    intent: "describe the account instances",
                    args: describe_resource("s5"),
                    depends_on: vec!["s3", "s5"],
                    output: "ResourceDescriptions",
                },
                StepSpec {
                    id: "s8",
                    operation: "update",
                    intent: "classify the contacts as prospects",
                    args: update("s6"),
                    depends_on: vec!["s6", "s4"],
                    output: "MutationResult",
                },
                StepSpec {
                    id: "s9",
                    operation: "validate",
                    intent: "check the resulting graph",
                    args: serde_json::json!({"kind": "validate"}),
                    depends_on: vec!["s8"],
                    output: "ValidationResult",
                },
                StepSpec {
                    id: "s10",
                    operation: "verify",
                    intent: "count classified contacts",
                    args: verify(),
                    depends_on: vec!["s9"],
                    output: "VerificationResult",
                },
            ],
        },
    ]
}

/// Full-plan JSON for a rung (the C/D scripted payload shape).
pub fn rung_plan_json(rung: &Rung) -> String {
    let steps: Vec<serde_json::Value> = rung
        .steps
        .iter()
        .map(|s| {
            serde_json::json!({
                "id": s.id, "operation": s.operation, "intent": s.intent,
                "args": s.args, "depends_on": s.depends_on, "output": s.output
            })
        })
        .collect();
    serde_json::json!({"goal": rung.request, "steps": steps}).to_string()
}

fn step_number(id: &str) -> usize {
    id.trim_start_matches('s').parse().unwrap_or(0)
}

/// Numbered NL prose where every structured dependency is stated
/// explicitly (`depends on step N`), which is what D's
/// `check_prose_consistency` enforces.
pub fn rung_prose(rung: &Rung) -> String {
    rung.steps
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let mut line = format!("{}. {}", i + 1, s.intent);
            for d in &s.depends_on {
                line.push_str(&format!("; depends on step {}", step_number(d)));
            }
            line.push('.');
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}
