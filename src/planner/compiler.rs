//! Phase 3 — request compiler: deterministic NL → Plan, LLM on demand.
//!
//! The compiler covers request classes where entity linking plus the
//! fixed phase-2 spine shapes fully determine the plan: classify the
//! request with the phase-2 table, link every content word against
//! the live ontology (the same substring rule the `search_vocabulary`
//! tool trusts), fill scalar slots with no model, ground
//! single-candidate mutation values, assemble with the unchanged
//! phase-2 [`assemble`](super::spine::assemble), validate with the
//! unchanged validator. Zero model calls on the happy path — this is
//! structural, not measured: [`compile_request`] takes no backend.
//!
//! What the compiler never does is guess. A slot with no confident
//! extraction becomes [`CompileOutcome::NeedsSlots`] (one bounded
//! per-field model call through [`CompilerPlanner`], same budget
//! shape as phase-2 elicitation); a request no spine covers, or a
//! plan that fails validation, becomes
//! [`CompileOutcome::Unmatched`] carrying a fate — either a named
//! next-compiler-rule candidate or accepted LLM planning. Steps the
//! compiler cannot ground keep their `unresolved` placeholders, and
//! the executor escalates on them exactly as before: loud, counted,
//! never silent.
//!
//! Query patterns are fixed single-triple templates
//! (`?x rdf:type <linked class>`) filled from linked entities — a
//! slot, not a query compiler. Rung qualifiers the template cannot
//! encode (the Ohio filter, stale-ness) are recorded as the next
//! rule candidate in the escalation ledger, which is how the
//! fallback shrinks over time by construction.

use std::collections::HashSet;

use super::{
    spine::{assemble, spine_for_request, Slots},
    strip_code_fences, AgentBackend, AgentToolPlanner, Planner, PlannerError, PlanningServices,
    UserRequest,
};
use crate::ontology::OntologyService;
use crate::plan::{GraphContext, OperationContext, Plan, StepArguments, TypedValue};
use crate::tools::vocabulary::ontology_subjects;

const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

/// Request scaffolding and spine verbs: never entity content.
const STOPWORDS: &[&str] = &[
    "the", "a", "an", "and", "or", "of", "for", "to", "in", "on", "as", "via", "with", "using",
    "use", "find", "list", "show", "give", "get", "all", "new", "their", "its", "is", "are",
    "what", "which", "how", "many", "much", "do", "does", "by", "from", "that", "this", "it",
    "be", "at", "no", "not", "create", "delete", "classify", "reconcile", "resolve", "describe",
    "record", "records",
];

/// Strip one trailing `s` only when it is a plural marker: the word is
/// long enough, and the letter before the `s` is a consonant other
/// than `s` (so `status` and `class` survive, `contacts` does not).
fn singular(word: &str) -> &str {
    if word.len() > 3 && word.ends_with('s') {
        let prev = word.as_bytes()[word.len() - 2] as char;
        if prev != 's' && !"aeiou".contains(prev) {
            return &word[..word.len() - 1];
        }
    }
    word
}

/// Lowercased content words in request order: alphanumeric split,
/// length ≥ 2 (keeps `OH`), stopwords dropped.
fn content_words(request: &str) -> Vec<String> {
    request
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 2 && !STOPWORDS.contains(w))
        .map(str::to_string)
        .collect()
}

/// Short name after the last `/` or `#`, lowercased.
fn short_local(iri: &str) -> String {
    iri.rsplit(['/', '#']).next().unwrap_or(iri).to_lowercase()
}

/// The kind of an ontology term, from its asserted `rdf:type` quads
/// first and the reasoner view second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntityKind {
    Class,
    Property,
    Individual,
    Term,
}

/// One request word resolved against the live ontology.
#[derive(Debug, Clone)]
pub struct LinkedEntity {
    /// Request word as written (lowercased).
    pub word: String,
    /// Singularized form used for vocabulary search terms.
    pub term: String,
    /// Linked ontology IRI.
    pub iri: String,
    pub kind: EntityKind,
}

fn term_kind(
    services: &PlanningServices,
    iri: &str,
    type_iris: &[String],
) -> EntityKind {
    for t in type_iris {
        let local = short_local(t);
        if local == "class" {
            return EntityKind::Class;
        }
        if local == "objectproperty"
            || local == "datatypeproperty"
            || local == "annotationproperty"
        {
            return EntityKind::Property;
        }
        if local == "concept" {
            return EntityKind::Individual;
        }
    }
    let r = &services.state.reasoner;
    if !r.superclasses(iri).is_empty() || !r.equivalents(iri).is_empty() {
        return EntityKind::Class;
    }
    if r.domain(iri).is_some() || r.range(iri).is_some() {
        return EntityKind::Property;
    }
    EntityKind::Term
}

fn asserted_types(services: &PlanningServices, iri: &str) -> Vec<String> {
    let subject = match crate::rdf::Term::iri(iri.to_string()) {
        Ok(t) => t,
        Err(_) => return vec![],
    };
    let pred = match crate::rdf::Term::iri(RDF_TYPE.to_string()) {
        Ok(t) => t,
        Err(_) => return vec![],
    };
    services
        .state
        .repo
        .match_quads(
            Some(&subject),
            Some(&pred),
            None,
            Some(&crate::rdf::GraphName::ontology()),
            None,
        )
        .map(|quads| {
            quads
                .iter()
                .filter_map(|q| q.object.as_iri().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// Deterministic entity linking: every content word against every
/// ontology subject local, rank exact (0) < singularized (1) <
/// substring (2), lexicographic IRI tiebreak. Multiple IRIs may link
/// one word (equivalent classes, legacy duplicates) — all are kept;
/// grounding rules below only fire on single candidates.
pub fn link_request(
    request: &str,
    services: &PlanningServices,
) -> Result<Vec<LinkedEntity>, String> {
    let subjects = ontology_subjects(&services.state).map_err(|e| e.to_string())?;
    let mut locals: Vec<(String, String)> = subjects
        .into_iter()
        .map(|iri| (short_local(&iri), iri))
        .collect();
    locals.sort();
    let mut out: Vec<LinkedEntity> = vec![];
    let mut seen: HashSet<(String, String)> = HashSet::new();
    for word in content_words(request) {
        let sing = singular(&word).to_string();
        let mut hits: Vec<(u8, String)> = vec![];
        for (local, iri) in &locals {
            let rank = if *local == word {
                0
            } else if *local == sing && sing != word {
                1
            } else if local.contains(&word as &str) || word.contains(local as &str) {
                2
            } else {
                continue;
            };
            hits.push((rank, iri.clone()));
        }
        hits.sort();
        hits.truncate(4);
        for (rank, iri) in hits {
            if !seen.insert((word.clone(), iri.clone())) {
                continue;
            }
            let term = if rank == 1 { sing.clone() } else { word.clone() };
            let types = asserted_types(services, &iri);
            out.push(LinkedEntity {
                word: word.clone(),
                term,
                iri: iri.clone(),
                kind: term_kind(services, &iri, &types),
            });
        }
    }
    Ok(out)
}

/// First quoted span (double quotes, else single quotes — an
/// unmatched apostrophe never closes), else the first capitalized
/// word after position 0 (skips the leading verb: `Find`,
/// `Classify`). `Acme-related` splits on the hyphen, yielding `Acme`.
pub fn extract_text(request: &str) -> Option<String> {
    for quote in [b'"', b'\''] {
        let bytes = request.as_bytes();
        let mut first: Option<usize> = None;
        for (i, b) in bytes.iter().enumerate() {
            if *b == quote {
                if let Some(s) = first {
                    let span: String = request[s + 1..i].trim().to_string();
                    if !span.is_empty() {
                        return Some(span);
                    }
                    first = None;
                } else {
                    first = Some(i);
                }
            }
        }
    }
    let words: Vec<&str> = request
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    for w in words.iter().skip(1) {
        let mut chars = w.chars();
        if let Some(c) = chars.next() {
            if c.is_ascii_uppercase() && w.len() > 1 {
                return Some(w.to_string());
            }
        }
    }
    None
}

/// A slot the compiler could not fill deterministically.
#[derive(Debug, Clone)]
pub enum MissingSlot {
    /// No linked word for a required vocabulary search.
    Terms,
    /// No confident instance-search text (needs `needs_text` spine or
    /// the SHACL-required create label).
    Text,
    /// Several properties fit the update; the model must pick one.
    PredicateChoice { candidates: Vec<String> },
}

impl MissingSlot {
    pub fn name(&self) -> &'static str {
        match self {
            MissingSlot::Terms => "terms",
            MissingSlot::Text => "text",
            MissingSlot::PredicateChoice { .. } => "predicate",
        }
    }

    /// The question carries the exact JSON shape inline: the 1B models
    /// ignore `response_format` scaffolding, so the shape must be in
    /// the prose (same lesson as the phase-1 few-shot and the phase-2
    /// elicitation prompts).
    pub fn question(&self, request: &str) -> String {
        match self {
            MissingSlot::Terms => format!(
                "List the vocabulary search terms (single words or short phrases) for this request. Reply with ONLY a JSON object like {{\"terms\": [\"contact\"]}} \u{2014} no prose, no fences.\n\nRequest: {request}"
            ),
            MissingSlot::Text => format!(
                "Give the short instance-search text for this request (e.g. a name like \"Acme\"). Reply with ONLY a JSON object like {{\"text\": \"Acme\"}} \u{2014} no prose, no fences.\n\nRequest: {request}"
            ),
            MissingSlot::PredicateChoice { candidates } => format!(
                "Which property should the update write? Reply with ONLY a JSON object like {{\"predicate\": \"{}\"}} \u{2014} no prose, no fences. Candidates: {}. Request: {request}",
                candidates.first().cloned().unwrap_or_default(),
                candidates.join(", ")
            ),
        }
    }
}

/// Single-candidate mutation grounding from linked entities. Anything
/// ambiguous stays empty here (the plan keeps `unresolved`, or the
/// request routes to [`MissingSlot::PredicateChoice`]).
#[derive(Debug, Clone, Default)]
pub struct MutationGrounding {
    /// First linked class: the `?x rdf:type` query template filler.
    pub query_class: Option<String>,
    /// Linked classes for the create step.
    pub create_types: Vec<String>,
    /// (SHACL-required predicate, label text) pairs for create.
    pub create_props: Vec<(String, String)>,
    /// Single property whose domain is a linked class and whose range
    /// accepts a linked value.
    pub update_predicate: Option<String>,
    /// Linked values in the predicate range.
    pub update_objects: Vec<String>,
    /// Several properties fit: genuine ambiguity for the model.
    pub predicate_candidates: Vec<String>,
}

fn linked_classes(links: &[LinkedEntity]) -> Vec<String> {
    let mut out = vec![];
    for l in links {
        if l.kind == EntityKind::Class && !out.contains(&l.iri) {
            out.push(l.iri.clone());
        }
    }
    out
}

/// Properties with a declared domain in `classes`, with their ranges.
fn domain_properties(services: &PlanningServices, classes: &[String]) -> Vec<(String, String)> {
    let subjects = match ontology_subjects(&services.state) {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    let mut iris: Vec<String> = subjects.into_iter().collect();
    iris.sort();
    let mut out = vec![];
    for iri in iris {
        let types = asserted_types(services, &iri);
        if term_kind(services, &iri, &types) != EntityKind::Property {
            continue;
        }
        let r = &services.state.reasoner;
        if let Some(domain) = r.domain(&iri) {
            if classes.contains(&domain) {
                if let Some(range) = r.range(&iri) {
                    out.push((iri, range));
                }
            }
        }
    }
    out
}

/// Types asserted for an individual (range matching for update values).
fn value_types(services: &PlanningServices, iri: &str) -> Vec<String> {
    asserted_types(services, iri)
}

/// SHACL-required property paths (`min_count >= 1`) for a class, in
/// shape order. Values come from the instance label text.
fn required_properties(services: &PlanningServices, class_iri: &str) -> Vec<String> {
    let mut out = vec![];
    for shape in &services.state.shapes {
        if shape.target_class == class_iri {
            for c in &shape.constraints {
                if c.min_count >= 1 {
                    out.push(c.path.clone());
                }
            }
        }
    }
    out
}

pub fn ground_mutation(
    level: u8,
    links: &[LinkedEntity],
    services: &PlanningServices,
    text: Option<&str>,
) -> MutationGrounding {
    let mut g = MutationGrounding::default();
    let classes = linked_classes(links);
    g.query_class = classes.first().cloned();
    if level == 8 {
        g.create_types = classes.clone();
        if let (Some(label), Some(class)) = (text, classes.first()) {
            for path in required_properties(services, class) {
                g.create_props.push((path, label.to_string()));
            }
        }
    }
    if level == 7 || level == 10 {
        let props = domain_properties(services, &classes);
        let mut fitting: Vec<(String, Vec<String>)> = vec![];
        for (prop, range) in props {
            let mut objs: Vec<String> = links
                .iter()
                // Values are individuals typed by the range; a linked
                // class that merely shares the range IRI is not a value.
                .filter(|l| {
                    l.kind == EntityKind::Individual
                        && value_types(services, &l.iri).contains(&range)
                })
                .map(|l| l.iri.clone())
                .collect();
            objs.sort();
            objs.dedup();
            if !objs.is_empty() {
                fitting.push((prop, objs));
            }
        }
        fitting.sort();
        match fitting.len() {
            1 => {
                g.update_predicate = Some(fitting[0].0.clone());
                g.update_objects = fitting[0].1.clone();
            }
            n if n > 1 => {
                g.predicate_candidates = fitting.into_iter().map(|(p, _)| p).collect();
            }
            _ => {}
        }
    }
    g
}

/// Fixed single-triple template: every instance of the linked class.
/// Graphs come from the plan context (instance graphs, never the
/// ontology graph). Qualifiers the template cannot encode (the Ohio
/// filter, stale-ness) are the documented next-rule candidates —
/// the shape matches the ladder rungs, which encode them no better.
fn grounded_pattern(graph_ctx: &GraphContext, class_iri: &str) -> Option<serde_json::Value> {
    let graphs: Vec<String> = graph_ctx
        .graphs
        .iter()
        .filter(|g| !g.is_ontology())
        .map(|g| g.to_string())
        .collect();
    if graphs.is_empty() {
        return None;
    }
    Some(serde_json::json!({
        "graphs": graphs,
        "return_vars": ["?x"],
        "patterns": [{
            "subject": {"kind": "var", "name": "?x"},
            "predicate": {"kind": "iri", "iri": RDF_TYPE},
            "object": {"kind": "iri", "iri": class_iri},
        }],
    }))
}

fn apply_grounding(plan: &mut Plan, g: &MutationGrounding, graph_ctx: &GraphContext) {
    for step in &mut plan.steps {
        match &mut step.args {
            StepArguments::Query { pattern } => {
                if let Some(class) = &g.query_class {
                    if let Some(p) = grounded_pattern(graph_ctx, class) {
                        *pattern = p;
                    }
                }
            }
            StepArguments::Create { types, properties } => {
                if !g.create_types.is_empty() {
                    *types = g.create_types.iter().map(|iri| TypedValue::iri(iri)).collect();
                }
                for (pred, value) in &g.create_props {
                    properties.push(serde_json::json!({
                        "predicate": pred,
                        "kind": "literal",
                        "value": value,
                    }));
                }
            }
            StepArguments::Update { predicate, objects, .. } => {
                if let Some(p) = &g.update_predicate {
                    *predicate = TypedValue::iri(p);
                }
                if !g.update_objects.is_empty() {
                    *objects = g.update_objects.iter().map(|iri| TypedValue::iri(iri)).collect();
                }
            }
            _ => {}
        }
    }
}

/// The compiler verdict for one request.
#[derive(Debug)]
pub enum CompileOutcome {
    /// Assembled and validated with zero model calls.
    Compiled {
        plan: Plan,
        spine: &'static str,
        level: u8,
    },
    /// A spine matched but slots are missing; the planner may ask
    /// the model for exactly these fields (bounded, per-field).
    NeedsSlots {
        spine: &'static str,
        level: u8,
        slots: Slots,
        missing: Vec<MissingSlot>,
        question: String,
    },
    /// No spine, or linking/assembly failed: escalate loudly.
    /// The `reason` doubles as the next-rule decision input:
    /// `no-spine` → accepted LLM planning; anything else → a
    /// compiler-rule candidate named by the reason.
    Unmatched {
        reason: String,
    },
}

/// Deterministic compile: classify → link → extract → ground →
/// assemble → validate. Takes no backend: the happy path provably
/// makes zero model calls.
pub fn compile_request(
    text: &str,
    services: &PlanningServices,
    graph_ctx: &GraphContext,
) -> CompileOutcome {
    let Some(spine) = spine_for_request(text) else {
        return CompileOutcome::Unmatched {
            reason: "no-spine".to_string(),
        };
    };
    let links = match link_request(text, services) {
        Ok(l) => l,
        Err(e) => {
            return CompileOutcome::Unmatched {
                reason: format!("linking-failed: {e}"),
            }
        }
    };
    // One term per word: links arrive rank-sorted per word, so the
    // first term wins (exact `account` beats substring `accounts`).
    let mut terms: Vec<String> = vec![];
    let mut seen_words: HashSet<String> = HashSet::new();
    for l in &links {
        if seen_words.insert(l.word.clone()) && !terms.contains(&l.term) {
            terms.push(l.term.clone());
        }
    }
    // Grounding check: every term must hit the ontology under the
    // same substring rule the search tool uses.
    let subjects = match ontology_subjects(&services.state) {
        Ok(s) => s,
        Err(e) => {
            return CompileOutcome::Unmatched {
                reason: format!("linking-failed: {e}"),
            }
        }
    };
    let locals: Vec<String> = subjects.iter().map(|iri| short_local(iri)).collect();
    terms.retain(|t| locals.iter().any(|l| l.contains(t as &str)));
    // Extract the text up front: a terms gap must not drop a confident
    // label the model should not have to re-supply.
    let text_slot = extract_text(text);
    if terms.is_empty() {
        return CompileOutcome::NeedsSlots {
            spine: spine.name,
            level: spine.level,
            slots: Slots {
                terms: vec![],
                terms_b: vec![],
                text: text_slot.unwrap_or_default(),
            },
            missing: vec![MissingSlot::Terms],
            question: MissingSlot::Terms.question(text),
        };
    }
    // Instance-search text, plus the create label: SHACL-required
    // properties cannot be guessed, so an unlabeled create is a
    // question, not a plan.
    let text_required = spine.needs_text || spine.level == 8;
    if text_required && text_slot.is_none() {
        return CompileOutcome::NeedsSlots {
            spine: spine.name,
            level: spine.level,
            slots: Slots {
                terms: terms.clone(),
                terms_b: terms.clone(),
                text: String::new(),
            },
            missing: vec![MissingSlot::Text],
            question: MissingSlot::Text.question(text),
        };
    }
    let slots = Slots {
        terms: terms.clone(),
        terms_b: terms.clone(),
        text: text_slot.clone().unwrap_or_default(),
    };
    let grounding = ground_mutation(spine.level, &links, services, text_slot.as_deref());
    if !grounding.predicate_candidates.is_empty() {
        let missing = MissingSlot::PredicateChoice {
            candidates: grounding.predicate_candidates.clone(),
        };
        return CompileOutcome::NeedsSlots {
            spine: spine.name,
            level: spine.level,
            slots,
            missing: vec![missing.clone()],
            question: missing.question(text),
        };
    }
    let mut plan = match assemble(spine, &slots, text, graph_ctx) {
        Ok(p) => p,
        Err(e) => {
            return CompileOutcome::Unmatched {
                reason: format!("template-invalid: {e:?}"),
            }
        }
    };
    apply_grounding(&mut plan, &grounding, graph_ctx);
    if let Err(e) = crate::plan::validate_plan(&plan) {
        return CompileOutcome::Unmatched {
            reason: format!("grounding-invalid: {e:?}"),
        };
    }
    CompileOutcome::Compiled {
        plan,
        spine: spine.name,
        level: spine.level,
    }
}

/// One compiled request for the corpus report.
pub struct CorpusRow {
    pub id: String,
    pub request: String,
    pub outcome: String,
    pub fate: String,
}

/// Headline metric: how much traffic the compiler owns.
pub struct CorpusSummary {
    pub total: usize,
    pub compiled: usize,
    pub needs_slots: usize,
    pub unmatched: usize,
    pub unmatched_rate: f64,
}

fn fate_for(outcome: &CompileOutcome) -> String {
    match outcome {
        CompileOutcome::Compiled { .. } => "compiler-default".to_string(),
        CompileOutcome::NeedsSlots { spine, missing, .. } => {
            format!("rule-candidate:{spine}:{}", missing[0].name())
        }
        CompileOutcome::Unmatched { reason } if reason == "no-spine" => {
            "accepted-llm".to_string()
        }
        CompileOutcome::Unmatched { reason } => {
            let rule: String = reason
                .chars()
                .take_while(|c| *c != ':' && *c != ' ')
                .collect();
            format!("rule-candidate:{rule}")
        }
    }
}

/// Compile a whole corpus keylessly: every row carries its fate, so
/// the unmatched rate and the next-rule backlog are published, not
/// asserted away.
pub fn compile_corpus(
    cases: &[(String, String)],
    services: &PlanningServices,
    graph_ctx: &GraphContext,
) -> (Vec<CorpusRow>, CorpusSummary) {
    let mut rows = vec![];
    let (mut compiled, mut needs_slots, mut unmatched) = (0, 0, 0);
    for (id, request) in cases {
        let outcome = compile_request(request, services, graph_ctx);
        let label = match &outcome {
            CompileOutcome::Compiled { spine, level, .. } => {
                compiled += 1;
                format!("compiled:{spine}:L{level}")
            }
            CompileOutcome::NeedsSlots { spine, missing, .. } => {
                needs_slots += 1;
                let fields: Vec<&str> = missing.iter().map(|m| m.name()).collect();
                format!("needs-slots:{spine}:{}", fields.join("+"))
            }
            CompileOutcome::Unmatched { reason } => {
                unmatched += 1;
                format!("unmatched:{reason}")
            }
        };
        rows.push(CorpusRow {
            id: id.clone(),
            request: request.clone(),
            outcome: label,
            fate: fate_for(&outcome),
        });
    }
    let total = cases.len();
    let summary = CorpusSummary {
        total,
        compiled,
        needs_slots,
        unmatched,
        unmatched_rate: if total == 0 {
            0.0
        } else {
            unmatched as f64 / total as f64
        },
    };
    (rows, summary)
}

/// Per-call compiler accounting, written to the `compiler_stats`
/// artifact (same role as the phase-1 `planner_stats` artifact).
#[derive(Debug, Clone, Default)]
pub struct CompilerStats {
    pub compiled: bool,
    pub spine: String,
    pub model_slot_calls: u32,
    pub escalated: bool,
    pub reason: String,
}

impl CompilerStats {
    fn artifact(&self) -> String {
        serde_json::json!({
            "compiled": self.compiled,
            "spine": self.spine,
            "model_slot_calls": self.model_slot_calls,
            "escalated": self.escalated,
            "reason": self.reason,
        })
        .to_string()
    }
}

/// Phase-3 planner: compile first; ask the model only for missing
/// scalars (bounded, per-field); escalate to the B tool loop loudly
/// when nothing compiles. The escalation keeps no separate fallback
/// set: B is the retained fallback (only loop interleaving discovery
/// with execution state); A/C/D are retired from planning duty (see
/// OPERATIONS.md) but their code and tests stay.
pub struct CompilerPlanner {
    backend: AgentBackend,
    services: PlanningServices,
}

impl CompilerPlanner {
    pub fn new(backend: AgentBackend, services: PlanningServices) -> Self {
        Self { backend, services }
    }

    async fn elicit_json(
        &self,
        prompt: &str,
        schema: serde_json::Value,
    ) -> Result<serde_json::Value, PlannerError> {
        let mut agent = self.backend.stream();
        agent.set_system("You answer with only the requested JSON object. No prose, no fences.");
        let mut message = prompt.to_string();
        let mut last_raw = String::new();
        // One attempt plus one repair: a field failure burns one cheap
        // turn, never a whole plan.
        for _ in 0..=1 {
            let resp = agent
                .send(&message)
                .with_schema(schema.clone(), Some("slots".to_string()))
                .await
                .map_err(PlannerError::Model)?;
            let raw = strip_code_fences(&resp.text);
            match serde_json::from_str::<serde_json::Value>(raw) {
                Ok(v) => return Ok(v),
                Err(e) => {
                    last_raw = raw.to_string();
                    message = format!(
                        "That was not valid JSON ({e}). Reply with ONLY the JSON object, no prose."
                    );
                }
            }
        }
        Err(PlannerError::Schema(format!(
            "slot elicitation did not produce JSON (last reply: {})",
            last_raw.chars().take(200).collect::<String>()
        )))
    }

    async fn fill_missing(
        &self,
        request: &UserRequest,
        slots: &mut Slots,
        missing: &[MissingSlot],
    ) -> Result<u32, PlannerError> {
        let mut calls = 0u32;
        for m in missing {
            match m {
                MissingSlot::Terms => {
                    let schema = serde_json::json!({
                        "type": "object",
                        "required": ["terms"],
                        "properties": {"terms": {"type": "array", "items": {"type": "string"}, "minItems": 1}},
                        "additionalProperties": false
                    });
                    let v = self.elicit_json(&m.question(&request.text), schema).await?;
                    calls += 1;
                    let terms: Vec<String> = v
                        .get("terms")
                        .and_then(|t| t.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| x.as_str())
                                .map(|s| s.trim().to_lowercase())
                                .filter(|s| !s.is_empty())
                                .collect()
                        })
                        .unwrap_or_default();
                    if terms.is_empty() {
                        return Err(PlannerError::Schema("elicited terms are empty".to_string()));
                    }
                    slots.terms = terms.clone();
                    slots.terms_b = terms;
                }
                MissingSlot::Text => {
                    let schema = serde_json::json!({
                        "type": "object",
                        "required": ["text"],
                        "properties": {"text": {"type": "string", "minLength": 1}},
                        "additionalProperties": false
                    });
                    let v = self.elicit_json(&m.question(&request.text), schema).await?;
                    calls += 1;
                    let text = v
                        .get("text")
                        .and_then(|t| t.as_str())
                        .map(|s| s.trim().to_string())
                        .unwrap_or_default();
                    if text.is_empty() {
                        return Err(PlannerError::Schema("elicited text is empty".to_string()));
                    }
                    slots.text = text;
                }
                MissingSlot::PredicateChoice { candidates } => {
                    let schema = serde_json::json!({
                        "type": "object",
                        "required": ["predicate"],
                        "properties": {"predicate": {"type": "string", "enum": candidates}},
                        "additionalProperties": false
                    });
                    let v = self.elicit_json(&m.question(&request.text), schema).await?;
                    calls += 1;
                    let picked = v
                        .get("predicate")
                        .and_then(|p| p.as_str())
                        .map(|s| s.to_string())
                        .unwrap_or_default();
                    if !candidates.contains(&picked) {
                        return Err(PlannerError::Schema(
                            "elicited predicate is not a candidate".to_string(),
                        ));
                    }
                    // The choice is recorded for the re-ground step
                    // below; the slot itself stays structural.
                    slots.text = format!("{}|predicate={picked}", slots.text);
                }
            }
        }
        Ok(calls)
    }
}

#[async_trait::async_trait]
impl Planner for CompilerPlanner {
    async fn create_plan(
        &self,
        request: &UserRequest,
        ctx: &mut OperationContext,
    ) -> Result<Plan, PlannerError> {
        let outcome = compile_request(&request.text, &self.services, &ctx.graph_context);
        match outcome {
            CompileOutcome::Compiled { plan, spine, .. } => {
                ctx.artifacts.insert(
                    "routing".to_string(),
                    format!("compiler:{spine}"),
                );
                ctx.artifacts.insert(
                    "compiler_stats".to_string(),
                    CompilerStats {
                        compiled: true,
                        spine: spine.to_string(),
                        ..Default::default()
                    }
                    .artifact(),
                );
                Ok(plan)
            }
            CompileOutcome::NeedsSlots {
                spine,
                level,
                mut slots,
                missing,
                question,
            } => {
                let predicate_pick = missing.iter().find_map(|m| match m {
                    MissingSlot::PredicateChoice { .. } => Some(true),
                    _ => None,
                });
                let calls = self.fill_missing(request, &mut slots, &missing).await.map_err(
                    |e| match e {
                        PlannerError::Schema(_) => PlannerError::Clarification(question.clone()),
                        other => other,
                    },
                )?;
                // Re-ground with any elicited values (a picked
                // predicate arrives smuggled in the text slot; split it
                // back out before assembly).
                let mut picked_predicate: Option<String> = None;
                if predicate_pick.is_some() {
                    if let Some((text, pick)) = slots.text.rsplit_once("|predicate=") {
                        picked_predicate = Some(pick.to_string());
                        slots.text = text.to_string();
                    }
                }
                let links = link_request(&request.text, &self.services)
                    .map_err(|e| PlannerError::Schema(format!("re-link failed: {e}")))?;
                let mut grounding =
                    ground_mutation(level, &links, &self.services, Some(slots.text.as_str()).filter(|s| !s.is_empty()));
                if let Some(p) = picked_predicate {
                    grounding.update_predicate = Some(p);
                    grounding.predicate_candidates = vec![];
                    // Objects for the picked predicate: values whose
                    // type the predicate range accepts.
                    let range = self.services.state.reasoner.range(
                        grounding.update_predicate.as_deref().unwrap_or_default(),
                    );
                    if let Some(range) = range {
                        let mut objs: Vec<String> = links
                            .iter()
                            .filter(|l| {
                                l.kind == EntityKind::Individual
                                    && value_types(&self.services, &l.iri).contains(&range)
                                    || l.kind == EntityKind::Class && l.iri == range
                            })
                            .map(|l| l.iri.clone())
                            .collect();
                        objs.sort();
                        objs.dedup();
                        grounding.update_objects = objs;
                    }
                }
                let spine_ref = spine_for_request(&request.text).ok_or_else(|| {
                    PlannerError::Schema("spine vanished between compile and fill".to_string())
                })?;
                let mut plan =
                    assemble(spine_ref, &slots, &request.text, &ctx.graph_context)
                        .map_err(PlannerError::Validation)?;
                apply_grounding(&mut plan, &grounding, &ctx.graph_context);
                crate::plan::validate_plan(&plan).map_err(PlannerError::Validation)?;
                ctx.artifacts.insert(
                    "routing".to_string(),
                    format!("compiler-slots:{spine}"),
                );
                ctx.artifacts.insert(
                    "compiler_stats".to_string(),
                    CompilerStats {
                        compiled: true,
                        spine: spine.to_string(),
                        model_slot_calls: calls,
                        ..Default::default()
                    }
                    .artifact(),
                );
                Ok(plan)
            }
            CompileOutcome::Unmatched { reason } => {
                ctx.artifacts.insert(
                    "routing".to_string(),
                    format!("compiler-escalated:{reason}"),
                );
                ctx.artifacts.insert(
                    "compiler_stats".to_string(),
                    CompilerStats {
                        escalated: true,
                        reason: reason.clone(),
                        ..Default::default()
                    }
                    .artifact(),
                );
                // Loud escalation to the retained fallback: the B tool
                // loop, the only loop interleaving discovery with
                // execution state.
                AgentToolPlanner::new(self.backend.clone(), self.services.clone())
                    .create_plan(request, ctx)
                    .await
            }
        }
    }
}

/// Decision attached to one escalated request: the next compiler
/// rule to write, or an explicit acceptance of LLM planning.
pub fn escalation_decision(outcome: &CompileOutcome) -> String {
    fate_for(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use crate::bench::{dataset::Scale, runner::build_exec_fixture};
    use crate::ontology::load_default_ontology;
    use crate::policy::{AuditLog, ExecutionContext, Operation as PolicyOp};
    use crate::rdf::{GraphName, OxigraphRepository, Term};
    use std::sync::Arc;

    fn bench_services() -> (PlanningServices, GraphContext) {
        let fx = build_exec_fixture(Scale::Tiny);
        let graph_ctx = GraphContext {
            graphs: vec![fx.graphs.tenant_a.clone()],
            namespaces: [
                ("ex".to_string(), "http://bench.example/ex#".to_string()),
                ("crm".to_string(), "http://bench.example/crm#".to_string()),
                ("sales".to_string(), "http://bench.example/sales#".to_string()),
                ("geo".to_string(), "http://bench.example/geo#".to_string()),
            ]
            .into_iter()
            .collect::<HashMap<String, String>>(),
            ..Default::default()
        };
        let _ = &fx.graphs.tenant_b;
        // Keep the repo alive through the fixture services; drop the rest.
        (fx.services, graph_ctx)
    }

    #[test]
    fn linker_resolves_contact_and_status_individual() {
        let (services, _) = bench_services();
        let links = link_request("Classify Ohio accounts as prospects.", &services).unwrap();
        // Noise links exist (substring recall: `accountstatus` matches
        // `accounts`); assertions target the best-rank link per word.
        let class_link = links.iter().find(|l| {
            l.word == "accounts" && l.kind == EntityKind::Class && l.term == "account"
        });
        assert!(class_link.is_some());
        assert!(class_link.unwrap().iri.ends_with("/crm/Account"));
        let value_link = links.iter().find(|l| {
            l.word == "prospects" && l.kind == EntityKind::Individual
        });
        assert!(value_link.is_some());
        assert!(value_link.unwrap().iri.ends_with("/sales/ProspectStatus"));
    }

    #[test]
    fn linker_drops_unlinked_words_quietly() {
        let (services, _) = bench_services();
        let links = link_request("Find all contacts", &services).unwrap();
        assert!(links.iter().any(|l| l.word == "contacts"));
        assert!(links.iter().all(|l| l.word != "all" && l.word != "find"));
    }

    #[test]
    fn text_extraction_prefers_quotes_then_capitals() {
        assert_eq!(
            extract_text("Delete the 'Account 1' records."),
            Some("Account 1".to_string())
        );
        assert_eq!(
            extract_text("Find contact vocabulary and search for Acme instances."),
            Some("Acme".to_string())
        );
        assert_eq!(extract_text("Delete stale prospect records."), None);
    }

    #[test]
    fn default_ontology_links_contact_but_no_prospect() {
        let repo = Arc::new(OxigraphRepository::new_in_memory(50_000).unwrap());
        load_default_ontology(&*repo).unwrap();
        let reasoner = Arc::new(crate::ontology::OntologyReasoner::from_repository(&*repo).unwrap());
        let search = Arc::new(crate::search::TantivyIndex::in_ram().unwrap());
        let audit = Arc::new(AuditLog::in_memory());
        let state = crate::tools::ToolState::new(
            repo,
            search,
            reasoner,
            crate::ontology::contact_shapes(),
            audit,
            crate::config::AppLimits::default(),
        );
        let graph = GraphName::tenant("acme").unwrap();
        let exec_ctx = ExecutionContext::new(
            "req-compiler",
            "alice",
            "acme",
            vec![graph, GraphName::ontology()],
            vec![PolicyOp::Read],
        );
        let services = PlanningServices::new(Arc::new(crate::tools::ToolRegistry::new()), state, exec_ctx);
        let links = link_request("Classify Ohio contacts as prospects.", &services).unwrap();
        assert!(links.iter().any(|l| l.word == "contacts" && l.kind == EntityKind::Class));
        // No status vocabulary on the default ontology: nothing to
        // ground an update predicate with — the loud boundary.
        assert!(links.iter().all(|l| l.word != "prospects"));
        let classes = linked_classes(&links);
        assert_eq!(classes.len(), 1);
        let g = ground_mutation(7, &links, &services, None);
        assert!(g.update_predicate.is_none());
        assert!(g.predicate_candidates.is_empty());
    }
}
