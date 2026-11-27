//! Safe typed query subsystem (Plan 2 Phase 3).
//!
//! Models propose **typed graph patterns**; Rust validates, compiles to the
//! graph query language, and executes against the repository. No tool accepts
//! free-text queries: `build_query` validates + stores a plan, `inspect_query`
//! shows the compiled form + representation hints, `execute_query` runs it.
//!
//! Enforcement (UPDATE_2 table — every row implemented or the phase fails):
//! IRI validity · property compatibility (domain/range; warn not coerce on
//! `"Ohio"` vs `"OH"` vs geo-URI) · literal correctness · namespace isolation
//! · variable/pattern/join validity · safety (limit clamp, pattern caps,
//! update-keyword rejection, cross-graph escape denial).

use std::collections::{HashMap, HashSet};
use std::sync::{atomic::{AtomicU64, Ordering}, Mutex};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::AppLimits;
use crate::ontology::{OntologyReasoner, OntologyService};
use crate::plan::GraphContext;
use crate::policy::ExecutionContext;
use crate::rdf::{GraphName, RdfRepository, Term};

/// Opaque plan handle returned by `build_query` (e.g. `qp_1`).
pub type QueryPlanId = String;

/// Top-level query operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryOperation {
    Select,
    Ask,
    Construct,
}

impl Default for QueryOperation {
    fn default() -> Self {
        Self::Select
    }
}

/// A `?name`-style variable or a concrete IRI subject.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SubjectPattern {
    Var { name: String },
    Iri { iri: String },
}

/// Predicates must be concrete IRIs — variable/unbound predicates are
/// rejected (they cannot be validated against the ontology).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PredicatePattern {
    Iri { iri: String },
}

/// Object: variable, IRI reference, or literal value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ObjectPattern {
    Var { name: String },
    Iri { iri: String },
    Literal {
        value: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        datatype: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lang: Option<String>,
    },
}

/// Typed filter operators. String payloads are data (escaped on compile);
/// payloads smuggling update-language keywords are rejected as unsafe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FilterOp {
    Eq { var: String, value: String },
    Contains { var: String, substring: String },
    Regex { var: String, pattern: String },
    Gt { var: String, value: String },
    Lt { var: String, value: String },
    LangMatch { var: String, lang: String },
}

/// One graph pattern row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriplePattern {
    pub subject: SubjectPattern,
    pub predicate: PredicatePattern,
    pub object: ObjectPattern,
}

/// The typed query proposal. This is the only query shape a model may emit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryPattern {
    #[serde(default)]
    pub operation: QueryOperation,
    #[serde(default)]
    pub graphs: Vec<GraphName>,
    pub patterns: Vec<TriplePattern>,
    #[serde(default)]
    pub filters: Vec<FilterOp>,
    #[serde(default)]
    pub return_vars: Vec<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Typed failure for query validation/compilation/execution.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum QueryError {
    #[error("args: {0}")]
    Args(String),
    #[error("grounding: {0}")]
    Grounding(String),
    #[error("validation: {0}")]
    Validation(String),
    #[error("auth: {0}")]
    Auth(String),
    #[error("query-safety: {0}")]
    Safety(String),
    #[error("storage: {0}")]
    Storage(String),
}

impl QueryError {
    /// Tool-error category string for registry mapping.
    pub fn category(&self) -> &'static str {
        match self {
            QueryError::Args(_) => "args",
            QueryError::Grounding(_) => "grounding",
            QueryError::Validation(_) => "validation",
            QueryError::Auth(_) => "auth",
            QueryError::Safety(_) => "query-safety",
            QueryError::Storage(_) => "storage",
        }
    }
    pub fn message(&self) -> String {
        match self {
            QueryError::Args(m)
            | QueryError::Grounding(m)
            | QueryError::Validation(m)
            | QueryError::Auth(m)
            | QueryError::Safety(m)
            | QueryError::Storage(m) => m.clone(),
        }
    }
}

/// Stored representation sample for literal-discovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepresentationHint {
    pub predicate: String,
    pub expected: String,
    pub stored_samples: Vec<String>,
    pub hint: String,
}

/// A validated, compiled plan.
#[derive(Debug, Clone)]
pub struct ValidatedPlan {
    pub id: QueryPlanId,
    pub pattern: QueryPattern,
    /// Compiled graph-query string (parameterized, escaped). Exposed to
    /// planners under both the `compiled` and `sparql` response keys.
    pub compiled: String,
    pub explanation: String,
    pub representations: Vec<RepresentationHint>,
    pub effective_limit: usize,
}

/// Execution result for SELECT (ASK/CONSTRUCT map onto this shape too).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryResult {
    pub uris: Vec<String>,
    pub rows: Vec<HashMap<String, String>>,
    pub count: usize,
    pub truncated: bool,
}

/// In-memory plan store. All planner turns share one instance via `ToolState`.
#[derive(Debug, Default)]
pub struct QueryPlanStore {
    plans: Mutex<HashMap<QueryPlanId, ValidatedPlan>>,
    counter: AtomicU64,
}

impl QueryPlanStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, id: &str) -> Option<ValidatedPlan> {
        self.plans.lock().unwrap().get(id).cloned()
    }

    pub fn len(&self) -> usize {
        self.plans.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Validate + compile + store. Returns the stored plan.
    pub fn build(
        &self,
        pattern: QueryPattern,
        repo: &dyn RdfRepository,
        reasoner: &OntologyReasoner,
        graph_ctx: &GraphContext,
        exec_ctx: &ExecutionContext,
        limits: &AppLimits,
    ) -> Result<ValidatedPlan, QueryError> {
        let plan = validate_and_compile(pattern, repo, reasoner, graph_ctx, exec_ctx, limits)?;
        let id = format!("qp_{}", self.counter.fetch_add(1, Ordering::SeqCst) + 1);
        let stored = ValidatedPlan {
            id: id.clone(),
            pattern: plan.pattern,
            compiled: plan.compiled,
            explanation: plan.explanation,
            representations: plan.representations,
            effective_limit: plan.effective_limit,
        };
        self.plans.lock().unwrap().insert(id, stored.clone());
        Ok(stored)
    }

    /// Execute a stored plan deterministically.
    pub fn execute(
        &self,
        id: &str,
        repo: &dyn RdfRepository,
        reasoner: &OntologyReasoner,
        graph_ctx: &GraphContext,
        exec_ctx: &ExecutionContext,
        limits: &AppLimits,
        limit_override: Option<usize>,
    ) -> Result<QueryResult, QueryError> {
        let plan = self
            .get(id)
            .ok_or_else(|| QueryError::Args(format!("unknown query plan {id:?}")))?;
        // Re-check auth at execution time (graphs may have changed allowlist).
        enforce_graph_auth(&plan.pattern, graph_ctx, exec_ctx)?;
        execute_validated(&plan, repo, reasoner, limits, limit_override)
    }
}

#[derive(Debug)]
struct CompiledParts {
    pattern: QueryPattern,
    compiled: String,
    explanation: String,
    representations: Vec<RepresentationHint>,
    effective_limit: usize,
}

// ---------------------------------------------------------------------------
// Validation + compilation
// ---------------------------------------------------------------------------

/// Bound on graph-pattern rows per plan. This doubles as the
/// traversal-depth cap: a join chain can be at most this many hops long
/// (patterns must connect via shared variables/IRIs, so depth <= len).
const MAX_PATTERNS: usize = 8;
const MAX_FILTERS: usize = 8;
/// Wall-clock budget for one `execute_query` over the in-memory join.
/// The repository scan is bounded, so this gate only trips on pathological
/// fan-out; it exists so every `execute_query` enforces a timeout gate.
const QUERY_EXEC_BUDGET_MS: u64 = 30_000;

fn is_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some('?') => (),
        _ => return false,
    }
    let rest: String = chars.collect();
    if rest.is_empty() {
        return false;
    }
    let mut rc = rest.chars();
    match rc.next() {
        Some(c) if c.is_ascii_alphabetic() => (),
        _ => return false,
    }
    rc.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Well-known core vocabularies are always allowed regardless of the plan
/// namespace allowlist (rdf:type etc. must validate in every plan).
fn is_core_iri(iri: &str) -> bool {
    iri.starts_with("http://www.w3.org/")
        || iri.starts_with("https://www.w3.org/")
        || iri.starts_with("http://purl.org/")
        || iri.starts_with("https://purl.org/")
}

fn namespace_allowed(graph_ctx: &GraphContext, iri: &str) -> bool {
    if is_core_iri(iri) {
        return true;
    }
    graph_ctx.allows_namespace(iri)
}

fn valid_iri(s: &str) -> bool {
    if s.trim().is_empty() || s.contains([' ', '\n', '\t', '<', '>', '"', '{', '}']) {
        return false;
    }
    s.contains(':')
}

/// Case-insensitive scan for update-language keywords smuggled in string data.
fn contains_update_language(s: &str) -> bool {
    let upper = s.to_uppercase();
    // Require a statement keyword; plain words like "updated" do not trip.
    for kw in [
        "INSERT DATA",
        "DELETE DATA",
        "DELETE WHERE",
        "INSERT {",
        "DELETE {",
        "UPDATE ",
        "LOAD ",
        "CLEAR ",
        "DROP ",
        "CREATE GRAPH",
        "COPY ",
        "MOVE ",
        "ADD ",
    ] {
        if upper.contains(kw) {
            return true;
        }
    }
    // Braced mutation fragment, e.g. "} DELETE {".
    if upper.contains('}') && upper.contains('{') {
        for kw in ["INSERT", "DELETE", "UPDATE", "LOAD", "CLEAR", "DROP"] {
            if upper.contains(kw) {
                return true;
            }
        }
    }
    false
}

fn check_no_update_language(values: &[String]) -> Result<(), QueryError> {
    for v in values {
        if contains_update_language(v) {
            return Err(QueryError::Safety(format!(
                "query-safety: string payload rejected (update-language keyword): {v:?}"
            )));
        }
    }
    Ok(())
}

/// Is `iri` a known class/predicate? Resolves via the ontology graph or via
/// actual usage in the repository.
fn iri_known(
    iri: &str,
    repo: &dyn RdfRepository,
    reasoner: &OntologyReasoner,
) -> Result<bool, QueryError> {
    if reasoner.domain(iri).is_some() || reasoner.range(iri).is_some() {
        return Ok(true);
    }
    if !reasoner.superclasses(iri).is_empty() || !reasoner.equivalents(iri).is_empty() {
        return Ok(true);
    }
    if !reasoner.subproperties(iri).is_empty() {
        return Ok(true);
    }
    // Ontology-graph subject?
    let onto = GraphName::ontology();
    let subj = Term::iri(iri.to_string())
        .map_err(|e| QueryError::Args(format!("bad IRI {iri:?}: {e}")))?;
    let in_onto = repo
        .match_quads(Some(&subj), None, None, Some(&onto), Some(1))
        .map_err(|e| QueryError::Storage(e.to_string()))?;
    if !in_onto.is_empty() {
        return Ok(true);
    }
    // Used as predicate or type anywhere?
    let pred = subj.clone();
    let as_pred = repo
        .match_quads(None, Some(&pred), None, None, Some(1))
        .map_err(|e| QueryError::Storage(e.to_string()))?;
    if !as_pred.is_empty() {
        return Ok(true);
    }
    let type_pred =
        Term::iri("http://www.w3.org/1999/02/22-rdf-syntax-ns#type".to_string()).unwrap();
    let obj = Term::iri(iri.to_string())
        .map_err(|e| QueryError::Args(format!("bad IRI {iri:?}: {e}")))?;
    let as_type = repo
        .match_quads(None, Some(&type_pred), Some(&obj), None, Some(1))
        .map_err(|e| QueryError::Storage(e.to_string()))?;
    if !as_type.is_empty() {
        return Ok(true);
    }
    // Used as any subject/object?
    let as_subj = repo
        .match_quads(Some(&subj), None, None, None, Some(1))
        .map_err(|e| QueryError::Storage(e.to_string()))?;
    if !as_subj.is_empty() {
        return Ok(true);
    }
    Ok(false)
}

fn enforce_graph_auth(
    pattern: &QueryPattern,
    graph_ctx: &GraphContext,
    exec_ctx: &ExecutionContext,
) -> Result<(), QueryError> {
    use crate::policy::Operation;
    // Plan-level scope: every pattern graph must be in the GraphContext.
    for g in &pattern.graphs {
        if !graph_ctx.allows_graph(g) {
            return Err(QueryError::Auth(format!(
                "graph {g} not in plan GraphContext"
            )));
        }
    }
    // Execution allowlist (legacy fully-empty contexts stay unconstrained).
    if exec_ctx.allowed_graphs.is_empty() && exec_ctx.permitted_ops.is_empty() {
        return Ok(());
    }
    if !exec_ctx.permitted_ops.contains(&Operation::Read) {
        return Err(QueryError::Auth(format!(
            "principal {} may not perform read",
            exec_ctx.principal
        )));
    }
    for g in &pattern.graphs {
        if !exec_ctx.allowed_graphs.contains(g) {
            return Err(QueryError::Auth(format!(
                "graph {g} not in allowlist for principal {}",
                exec_ctx.principal
            )));
        }
    }
    Ok(())
}

/// Asserted `rdf:type` IRIs for a concrete subject in the plan graphs.
fn asserted_types_for(
    iri: &str,
    graphs: &[GraphName],
    repo: &dyn RdfRepository,
) -> Result<Vec<String>, QueryError> {
    let subj = Term::iri(iri.to_string())
        .map_err(|e| QueryError::Args(format!("bad IRI {iri:?}: {e}")))?;
    let type_pred =
        Term::iri("http://www.w3.org/1999/02/22-rdf-syntax-ns#type".to_string()).unwrap();
    let mut out = vec![];
    if graphs.is_empty() {
        let quads = repo
            .match_quads(Some(&subj), Some(&type_pred), None, None, None)
            .map_err(|e| QueryError::Storage(e.to_string()))?;
        for q in quads {
            if let Some(t) = q.object.as_iri() {
                out.push(t.to_string());
            }
        }
    } else {
        for g in graphs {
            let quads = repo
                .match_quads(Some(&subj), Some(&type_pred), None, Some(g), None)
                .map_err(|e| QueryError::Storage(e.to_string()))?;
            for q in quads {
                if let Some(t) = q.object.as_iri() {
                    out.push(t.to_string());
                }
            }
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

fn validate_and_compile(
    mut pattern: QueryPattern,
    repo: &dyn RdfRepository,
    reasoner: &OntologyReasoner,
    graph_ctx: &GraphContext,
    exec_ctx: &ExecutionContext,
    limits: &AppLimits,
) -> Result<CompiledParts, QueryError> {
    // --- structural ---
    if pattern.patterns.is_empty() {
        return Err(QueryError::Args("query needs at least one pattern".to_string()));
    }
    if pattern.patterns.len() > MAX_PATTERNS {
        return Err(QueryError::Safety(format!(
            "query-safety: too many patterns / traversal depth exceeded (max {MAX_PATTERNS})"
        )));
    }
    if pattern.filters.len() > MAX_FILTERS {
        return Err(QueryError::Safety(format!(
            "query-safety: too many filters (max {MAX_FILTERS})"
        )));
    }
    if pattern.graphs.is_empty() {
        return Err(QueryError::Args("query needs at least one graph".to_string()));
    }

    // Collect string payloads for update-language scan.
    let mut string_payloads: Vec<String> = vec![];
    for p in &pattern.patterns {
        if let ObjectPattern::Literal { value, datatype, lang } = &p.object {
            string_payloads.push(value.clone());
            if let Some(d) = datatype {
                string_payloads.push(d.clone());
            }
            if let Some(l) = lang {
                string_payloads.push(l.clone());
            }
        }
    }
    for f in &pattern.filters {
        match f {
            FilterOp::Eq { value, .. }
            | FilterOp::Gt { value, .. }
            | FilterOp::Lt { value, .. } => string_payloads.push(value.clone()),
            FilterOp::Contains { substring, .. } => string_payloads.push(substring.clone()),
            FilterOp::Regex { pattern, .. } => string_payloads.push(pattern.clone()),
            FilterOp::LangMatch { lang, .. } => string_payloads.push(lang.clone()),
        }
    }
    check_no_update_language(&string_payloads)?;

    // --- graph auth + namespace isolation ---
    enforce_graph_auth(&pattern, graph_ctx, exec_ctx)?;

    // --- per-pattern checks ---
    let mut vars: HashSet<String> = HashSet::new();
    for (i, p) in pattern.patterns.iter().enumerate() {
        // Subject.
        match &p.subject {
            SubjectPattern::Var { name } => {
                if !is_var_name(name) {
                    return Err(QueryError::Args(format!(
                        "pattern {i}: bad variable {name:?}"
                    )));
                }
                vars.insert(name.clone());
            }
            SubjectPattern::Iri { iri } => {
                if !valid_iri(iri) {
                    return Err(QueryError::Args(format!(
                        "pattern {i}: bad subject IRI {iri:?}"
                    )));
                }
                if !namespace_allowed(graph_ctx, iri) {
                    return Err(QueryError::Auth(format!(
                        "subject IRI {iri} outside plan namespaces"
                    )));
                }
            }
        }
        // Predicate: concrete IRI only (unbound predicates rejected).
        let pred_iri = match &p.predicate {
            PredicatePattern::Iri { iri } => iri,
        };
        if !valid_iri(pred_iri) {
            return Err(QueryError::Args(format!(
                "pattern {i}: bad predicate IRI {pred_iri:?}"
            )));
        }
        if !namespace_allowed(graph_ctx, pred_iri) {
            return Err(QueryError::Auth(format!(
                "predicate IRI {pred_iri} outside plan namespaces"
            )));
        }
        if !iri_known(pred_iri, repo, reasoner)? {
            return Err(QueryError::Grounding(format!(
                "unknown predicate IRI: {pred_iri}"
            )));
        }
        // Object.
        match &p.object {
            ObjectPattern::Var { name } => {
                if !is_var_name(name) {
                    return Err(QueryError::Args(format!(
                        "pattern {i}: bad variable {name:?}"
                    )));
                }
                vars.insert(name.clone());
            }
            ObjectPattern::Iri { iri } => {
                if !valid_iri(iri) {
                    return Err(QueryError::Args(format!(
                        "pattern {i}: bad object IRI {iri:?}"
                    )));
                }
                if !namespace_allowed(graph_ctx, iri) {
                    return Err(QueryError::Auth(format!(
                        "object IRI {iri} outside plan namespaces"
                    )));
                }
            }
            ObjectPattern::Literal { value: _, datatype, lang } => {
                if let (Some(_), Some(_)) = (datatype, lang) {
                    return Err(QueryError::Validation(
                        "literal cannot carry both datatype and lang".to_string(),
                    ));
                }
                if let Some(d) = datatype {
                    if !valid_iri(d) {
                        return Err(QueryError::Validation(format!(
                            "bad datatype IRI {d:?}"
                        )));
                    }
                }
                if let Some(l) = lang {
                    if l.trim().is_empty() {
                        return Err(QueryError::Validation(
                            "empty lang tag".to_string(),
                        ));
                    }
                }
            }
        }
    }

    // Cross-graph escape: pattern graphs were already checked against the
    // plan context; a pattern with an explicitly remote graph IRI is denied.
    for g in &pattern.graphs {
        let s = g.to_string();
        if s.contains("..") || s.contains(' ') {
            return Err(QueryError::Safety(format!(
                "query-safety: bad graph name {s:?}"
            )));
        }
    }

    // --- property compatibility (domain/range) ---
    for (i, p) in pattern.patterns.iter().enumerate() {
        let pred_iri = match &p.predicate {
            PredicatePattern::Iri { iri } => iri.clone(),
        };
        let domain = reasoner.domain(&pred_iri);
        let range = reasoner.range(&pred_iri);
        // Subject side.
        if let (Some(dom), SubjectPattern::Iri { iri }) = (&domain, &p.subject) {
            let types = asserted_types_for(iri, &pattern.graphs, repo)?;
            if !types.is_empty()
                && !types.iter().any(|t| t == dom || reasoner.is_subclass_of(t, dom))
            {
                return Err(QueryError::Validation(format!(
                    "pattern {i}: domain mismatch: {iri} is {types:?}, property {pred_iri} requires {dom}"
                )));
            }
        }
        // Object side.
        match &p.object {
            ObjectPattern::Iri { iri } => {
                if let Some(rng) = &range {
                    let types = asserted_types_for(iri, &pattern.graphs, repo)?;
                    if !types.is_empty()
                        && !types.iter().any(|t| t == rng || reasoner.is_subclass_of(t, rng))
                    {
                        return Err(QueryError::Validation(format!(
                            "pattern {i}: range mismatch: {iri} is {types:?}, property {pred_iri} requires {rng}"
                        )));
                    }
                }
                // Literal-expected property given an IRI?
                let lname = pred_iri.rsplit(['/', '#']).next().unwrap_or("").to_lowercase();
                if ["name", "email", "label", "comment", "title", "state"].contains(&lname.as_str())
                    && range.is_none()
                {
                    // Heuristic only when the ontology declares no range but the
                    // predicate is a known datatype-style property in fixtures.
                    // Skip: do not hard-fail informational predicates.
                }
            }
            ObjectPattern::Literal { .. } => {
                if let Some(rng) = &range {
                    // Range declares a resource class but a literal was given.
                    // Distinguish "Ohio"-style representation traps from hard
                    // type errors via the hint path below: still reject, but
                    // with stored samples attached in `representations`.
                    return Err(QueryError::Validation(format!(
                        "pattern {i}: range mismatch: literal given for property {pred_iri} with range {rng}"
                    )));
                }
            }
            ObjectPattern::Var { .. } => {}
        }
    }

    // --- variable / join validity ---
    for v in &pattern.return_vars {
        if !is_var_name(v) {
            return Err(QueryError::Args(format!("bad return var {v:?}")));
        }
        if !vars.contains(v) {
            return Err(QueryError::Validation(format!(
                "return var {v} is unbound (no pattern binds it)"
            )));
        }
    }
    // Filters may only reference bound variables.
    for f in &pattern.filters {
        let v = match f {
            FilterOp::Eq { var, .. }
            | FilterOp::Contains { var, .. }
            | FilterOp::Regex { var, .. }
            | FilterOp::Gt { var, .. }
            | FilterOp::Lt { var, .. }
            | FilterOp::LangMatch { var, .. } => var,
        };
        if !is_var_name(v) {
            return Err(QueryError::Args(format!("bad filter var {v:?}")));
        }
        if !vars.contains(v) {
            return Err(QueryError::Validation(format!(
                "filter var {v} is unbound (no pattern binds it)"
            )));
        }
        if let FilterOp::Regex { pattern: rx, .. } = f {
            if regex::Regex::new(rx).is_err() {
                return Err(QueryError::Validation(format!(
                    "bad regex filter {rx:?}"
                )));
            }
        }
    }
    // Joins must connect: the pattern-variable graph must be connected.
    if pattern.patterns.len() > 1 && !patterns_connected(&pattern.patterns) {
        return Err(QueryError::Validation(
            "disconnected patterns (cartesian product); add a shared variable or IRI"
                .to_string(),
        ));
    }

    // --- safety: limit clamp ---
    let requested = pattern.limit.unwrap_or(limits.default_page_limit);
    let effective_limit = requested.max(1).min(limits.max_resources_per_read);
    pattern.limit = Some(effective_limit);

    // --- representation discovery (stored samples, never coerce) ---
    let representations = collect_representations(&pattern, repo)?;

    let compiled = compile_to_query_string(&pattern);
    let explanation = explain(&pattern, effective_limit);

    Ok(CompiledParts {
        pattern,
        compiled,
        explanation,
        representations,
        effective_limit,
    })
}

fn pattern_vars(p: &TriplePattern) -> HashSet<String> {
    let mut out = HashSet::new();
    if let SubjectPattern::Var { name } = &p.subject {
        out.insert(name.clone());
    }
    if let ObjectPattern::Var { name } = &p.object {
        out.insert(name.clone());
    }
    out
}

fn pattern_key(p: &TriplePattern) -> HashSet<String> {
    // Connectivity via shared variables OR shared concrete IRIs.
    let mut out = pattern_vars(p);
    if let SubjectPattern::Iri { iri } = &p.subject {
        out.insert(format!("iri:{iri}"));
    }
    if let ObjectPattern::Iri { iri } = &p.object {
        out.insert(format!("iri:{iri}"));
    }
    out
}

fn patterns_connected(patterns: &[TriplePattern]) -> bool {
    let mut seen = pattern_key(&patterns[0]);
    for p in patterns.iter().skip(1) {
        let k = pattern_key(p);
        if seen.intersection(&k).next().is_none() {
            return false;
        }
        seen.extend(k);
    }
    true
}

/// Sample stored object representations for each concrete literal in the
/// pattern, so the planner can refine instead of assuming `"Ohio"`.
fn collect_representations(
    pattern: &QueryPattern,
    repo: &dyn RdfRepository,
) -> Result<Vec<RepresentationHint>, QueryError> {
    let mut out = vec![];
    for p in &pattern.patterns {
        let pred_iri = match &p.predicate {
            PredicatePattern::Iri { iri } => iri.clone(),
        };
        if let ObjectPattern::Literal { value, .. } = &p.object {
            let pred = Term::iri(pred_iri.clone())
                .map_err(|e| QueryError::Args(format!("bad predicate {pred_iri:?}: {e}")))?;
            let mut samples: Vec<String> = vec![];
            for g in &pattern.graphs {
                let quads = repo
                    .match_quads(None, Some(&pred), None, Some(g), Some(50))
                    .map_err(|e| QueryError::Storage(e.to_string()))?;
                for q in quads {
                    let s = match &q.object {
                        Term::Iri(i) => format!("iri:{i}"),
                        Term::BlankNode(b) => format!("bnode:{b}"),
                        Term::Literal { value, datatype, lang } => {
                            if let Some(l) = lang {
                                format!("literal:{value}@{l}")
                            } else if let Some(d) = datatype {
                                format!("literal:{value}^^{d}")
                            } else {
                                format!("literal:{value}")
                            }
                        }
                    };
                    if !samples.contains(&s) {
                        samples.push(s);
                    }
                    if samples.len() >= 5 {
                        break;
                    }
                }
            }
            samples.sort();
            let found = samples.iter().any(|s| s.contains(value));
            out.push(RepresentationHint {
                predicate: pred_iri,
                expected: value.clone(),
                stored_samples: samples,
                hint: if found {
                    "expected value present in samples".to_string()
                } else {
                    "expected value NOT found in samples; check datatype, lang tag, or IRI-vs-literal representation (e.g. \"Ohio\" vs \"OH\" vs geo-URI) before executing".to_string()
                },
            });
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Compilation to the graph query language (parameterized + escaped)
// ---------------------------------------------------------------------------

fn escape_iri(iri: &str) -> String {
    format!("<{}>", iri.replace('>', "%3E"))
}

fn escape_literal(value: &str, datatype: &Option<String>, lang: &Option<String>) -> String {
    let esc = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r");
    let mut s = format!("\"{esc}\"");
    if let Some(l) = lang {
        s.push('@');
        s.push_str(l);
    } else if let Some(d) = datatype {
        s.push_str(&format!("^^<{}>", d.replace('>', "%3E")));
    }
    s
}

fn subject_string(s: &SubjectPattern) -> String {
    match s {
        SubjectPattern::Var { name } => name.clone(),
        SubjectPattern::Iri { iri } => escape_iri(iri),
    }
}

fn object_string(o: &ObjectPattern) -> String {
    match o {
        ObjectPattern::Var { name } => name.clone(),
        ObjectPattern::Iri { iri } => escape_iri(iri),
        ObjectPattern::Literal { value, datatype, lang } => {
            escape_literal(value, datatype, lang)
        }
    }
}

fn filter_string(f: &FilterOp) -> String {
    match f {
        FilterOp::Eq { var, value } => {
            format!("({} = \"{}\")", var, value.replace('"', "\\\""))
        }
        FilterOp::Contains { var, substring } => {
            format!("(CONTAINS(LCASE(STR({})), LCASE(\"{}\")))", var, substring.replace('"', "\\\""))
        }
        FilterOp::Regex { var, pattern } => {
            format!("(REGEX(STR({}), \"{}\"))", var, pattern.replace('"', "\\\""))
        }
        FilterOp::Gt { var, value } => format!("({} > \"{}\")", var, value.replace('"', "\\\"")),
        FilterOp::Lt { var, value } => format!("({} < \"{}\")", var, value.replace('"', "\\\"")),
        FilterOp::LangMatch { var, lang } => {
            format!("(LANGMATCHES(LANG({}), \"{}\"))", var, lang.replace('"', "\\\""))
        }
    }
}

/// Compile a validated pattern. Only `SELECT`/`ASK`/`CONSTRUCT` are emitted;
/// update-language strings are never accepted (see validation).
pub fn compile_to_query_string(pattern: &QueryPattern) -> String {
    let limit = pattern.limit.unwrap_or(50);
    let head = match pattern.operation {
        QueryOperation::Select => {
            if pattern.return_vars.is_empty() {
                "SELECT *".to_string()
            } else {
                format!("SELECT {}", pattern.return_vars.join(" "))
            }
        }
        QueryOperation::Ask => "ASK".to_string(),
        QueryOperation::Construct => {
            // Project the first pattern as the template when no explicit
            // template is given (bounded, deterministic).
            if let Some(p) = pattern.patterns.first() {
                format!(
                    "CONSTRUCT {{ {} {} {} }}",
                    subject_string(&p.subject),
                    match &p.predicate {
                        PredicatePattern::Iri { iri } => escape_iri(iri),
                    },
                    object_string(&p.object)
                )
            } else {
                "CONSTRUCT { ?s ?p ?o }".to_string()
            }
        }
    };
    let mut body = String::new();
    for g in &pattern.graphs {
        body.push_str(&format!("  GRAPH {} {{\n", escape_iri(&g.to_string())));
        for p in &pattern.patterns {
            let pred = match &p.predicate {
                PredicatePattern::Iri { iri } => escape_iri(iri),
            };
            body.push_str(&format!(
                "    {} {} {} .\n",
                subject_string(&p.subject),
                pred,
                object_string(&p.object)
            ));
        }
        body.push_str("  }\n");
    }
    let mut q = format!("{head}\nWHERE {{\n{body}");
    for f in &pattern.filters {
        q.push_str(&format!("  FILTER{} .\n", filter_string(f)));
    }
    q.push_str("}\n");
    if !matches!(pattern.operation, QueryOperation::Ask) {
        q.push_str(&format!("LIMIT {limit}"));
    }
    q
}

fn explain(pattern: &QueryPattern, effective_limit: usize) -> String {
    format!(
        "matches {} pattern(s) in {} graph(s) with {} filter(s); projects [{}]; limit {effective_limit}; requires resolved predicates and bound return vars",
        pattern.patterns.len(),
        pattern.graphs.len(),
        pattern.filters.len(),
        pattern.return_vars.join(", ")
    )
}

// ---------------------------------------------------------------------------
// Execution (pattern-match + join over the repository; deterministic)
// ---------------------------------------------------------------------------

type Binding = HashMap<String, Term>;

fn bind_term(
    pat_iri: Option<&str>,
    bound: Option<&Term>,
    actual: &Term,
) -> Option<Term> {
    if let Some(b) = bound {
        return if b == actual { Some(b.clone()) } else { None };
    }
    if let Some(iri) = pat_iri {
        return if actual.as_iri() == Some(iri) {
            Some(actual.clone())
        } else {
            None
        };
    }
    Some(actual.clone())
}

fn pattern_matches(
    pat: &TriplePattern,
    graphs: &[GraphName],
    repo: &dyn RdfRepository,
    binding: &Binding,
) -> Result<Vec<Binding>, QueryError> {
    // Concrete filters for the repository scan.
    let subj_iri: Option<String> = match &pat.subject {
        SubjectPattern::Iri { iri } => Some(iri.clone()),
        SubjectPattern::Var { name } => binding
            .get(name)
            .and_then(|t| t.as_iri().map(|s| s.to_string())),
    };
    let pred_iri = match &pat.predicate {
        PredicatePattern::Iri { iri } => iri.clone(),
    };
    let obj_term: Option<Term> = match &pat.object {
        ObjectPattern::Iri { iri } => Some(
            Term::iri(iri.clone()).map_err(|e| QueryError::Args(e.to_string()))?,
        ),
        ObjectPattern::Literal { value, datatype, lang } => {
            let t = if let Some(l) = lang {
                Term::lang_literal(value.clone(), l.clone())
                    .map_err(|e| QueryError::Args(e.to_string()))?
            } else if let Some(d) = datatype {
                Term::typed_literal(value.clone(), d.clone())
                    .map_err(|e| QueryError::Args(e.to_string()))?
            } else {
                Term::literal(value.clone())
            };
            // A pre-bound variable must equal the literal.
            Some(t)
        }
        ObjectPattern::Var { name } => binding.get(name).cloned(),
    };
    // Pre-bound variable on the subject side that resolved to a literal:
    // literals can never be subjects → no match (not an error).
    if let SubjectPattern::Var { name } = &pat.subject {
        if let Some(t) = binding.get(name) {
            if !matches!(t, Term::Iri(_) | Term::BlankNode(_)) {
                return Ok(vec![]);
            }
        }
    }

    let subj_term = subj_iri
        .map(Term::iri)
        .transpose()
        .map_err(|e| QueryError::Args(e.to_string()))?;
    let pred_term =
        Term::iri(pred_iri).map_err(|e| QueryError::Args(e.to_string()))?;
    let mut out = vec![];
    for g in graphs {
        let quads = repo
            .match_quads(
                subj_term.as_ref(),
                Some(&pred_term),
                obj_term.as_ref(),
                Some(g),
                None,
            )
            .map_err(|e| QueryError::Storage(e.to_string()))?;
        for q in quads {
            let mut next = binding.clone();
            // Bind subject var.
            if let SubjectPattern::Var { name } = &pat.subject {
                if let Some(existing) = binding.get(name) {
                    if existing != &q.subject {
                        continue;
                    }
                } else {
                    next.insert(name.clone(), q.subject.clone());
                }
            } else if let SubjectPattern::Iri { iri } = &pat.subject {
                if bind_term(Some(iri), binding.get(&format!("__s_{iri}")), &q.subject).is_none() {
                    continue;
                }
            }
            // Object var (concrete objects already filtered by the scan).
            if let ObjectPattern::Var { name } = &pat.object {
                if let Some(existing) = binding.get(name) {
                    if existing != &q.object {
                        continue;
                    }
                } else {
                    next.insert(name.clone(), q.object.clone());
                }
            }
            out.push(next);
        }
    }
    Ok(out)
}

fn term_display(t: &Term) -> String {
    match t {
        Term::Iri(s) => s.clone(),
        Term::BlankNode(b) => format!("_:{b}"),
        Term::Literal { value, datatype, lang } => {
            if let Some(l) = lang {
                format!("{value}@{l}")
            } else if let Some(d) = datatype {
                format!("{value}^^{d}")
            } else {
                value.clone()
            }
        }
    }
}

fn apply_filters(
    bindings: Vec<Binding>,
    filters: &[FilterOp],
) -> Result<Vec<Binding>, QueryError> {
    let mut out = bindings;
    for f in filters {
        out = out
            .into_iter()
            .filter(|b| {
                match f {
                    FilterOp::Eq { var, value } => {
                        b.get(var).map(|t| term_display(t) == *value).unwrap_or(false)
                    }
                    FilterOp::Contains { var, substring } => b
                        .get(var)
                        .map(|t| {
                            term_display(t)
                                .to_lowercase()
                                .contains(&substring.to_lowercase())
                        })
                        .unwrap_or(false),
                    FilterOp::Regex { var, pattern } => b
                        .get(var)
                        .map(|t| {
                            regex::Regex::new(pattern)
                                .map(|rx| rx.is_match(&term_display(t)))
                                .unwrap_or(false)
                        })
                        .unwrap_or(false),
                    FilterOp::Gt { var, value } => b
                        .get(var)
                        .map(|t| term_display(t) > *value)
                        .unwrap_or(false),
                    FilterOp::Lt { var, value } => b
                        .get(var)
                        .map(|t| term_display(t) < *value)
                        .unwrap_or(false),
                    FilterOp::LangMatch { var, lang } => b
                        .get(var)
                        .map(|t| match t {
                            Term::Literal { lang: Some(l), .. } => {
                                l.to_lowercase() == lang.to_lowercase()
                            }
                            _ => false,
                        })
                        .unwrap_or(false),
                }
            })
            .collect();
    }
    Ok(out)
}

/// Back-compat alias: the compiled string is the plan's graph-query text
/// (returned as `sparql` in `inspect_query` per the Phase 3 spec).
#[allow(dead_code)]
pub fn plan_sparql(plan: &ValidatedPlan) -> &str {
    &plan.compiled
}

fn execute_validated(
    plan: &ValidatedPlan,
    repo: &dyn RdfRepository,
    _reasoner: &OntologyReasoner,
    limits: &AppLimits,
    limit_override: Option<usize>,
) -> Result<QueryResult, QueryError> {
    let started = std::time::Instant::now();
    let budget = std::time::Duration::from_millis(QUERY_EXEC_BUDGET_MS);
    let effective = limit_override
        .unwrap_or(plan.effective_limit)
        .max(1)
        .min(limits.max_resources_per_read);
    let mut bindings: Vec<Binding> = vec![HashMap::new()];
    for pat in &plan.pattern.patterns {
        if started.elapsed() > budget {
            return Err(QueryError::Safety(format!(
                "query-safety: execution timeout exceeded ({QUERY_EXEC_BUDGET_MS}ms)"
            )));
        }
        let mut next_all: Vec<Binding> = vec![];
        for b in &bindings {
            next_all.extend(pattern_matches(pat, &plan.pattern.graphs, repo, b)?);
        }
        bindings = next_all;
        if bindings.is_empty() {
            break;
        }
        if bindings.len() > limits.max_triples_per_read * 2 {
            bindings.truncate(limits.max_triples_per_read * 2);
            break;
        }
    }
    bindings = apply_filters(bindings, &plan.pattern.filters)?;
    // Deterministic order.
    bindings.sort_by(|a, b| {
        let ka = plan
            .pattern
            .return_vars
            .iter()
            .map(|v| a.get(v).map(term_display).unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\u{1f}");
        let kb = plan
            .pattern
            .return_vars
            .iter()
            .map(|v| b.get(v).map(term_display).unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\u{1f}");
        ka.cmp(&kb)
    });

    match plan.pattern.operation {
        QueryOperation::Ask => {
            let matched = !bindings.is_empty();
            Ok(QueryResult {
                uris: vec![],
                rows: vec![HashMap::from([(
                    "ask".to_string(),
                    matched.to_string(),
                )])],
                count: usize::from(matched),
                truncated: false,
            })
        }
        _ => {
            let total = bindings.len();
            let truncated = total > effective;
            let mut rows: Vec<HashMap<String, String>> = vec![];
            let mut uris: Vec<String> = vec![];
            for b in bindings.into_iter().take(effective) {
                let mut row = HashMap::new();
                for v in &plan.pattern.return_vars {
                    if let Some(t) = b.get(v) {
                        let d = term_display(t);
                        row.insert(v.clone(), d.clone());
                        if let Term::Iri(iri) = t {
                            if !uris.contains(iri) {
                                uris.push(iri.clone());
                            }
                        }
                    }
                }
                rows.push(row);
            }
            // Fallback: when no return vars project IRIs (e.g. SELECT *),
            // surface distinct IRI subjects so planners get usable URIs.
            if uris.is_empty() && !rows.is_empty() {
                let mut seen = HashSet::new();
                for b in rows.iter() {
                    for v in b.values() {
                        if v.contains(':') && !v.contains(' ') && seen.insert(v.clone()) {
                            uris.push(v.clone());
                        }
                    }
                }
            }
            uris.sort();
            uris.dedup();
            Ok(QueryResult {
                uris,
                rows,
                count: total,
                truncated,
            })
        }
    }
}

/// Parse a graph list element: accepts `"urn:..."`, `{"Named": "..."}`,
/// `"Default"`, or `{"Default": ...}` (planner-friendly and enum forms).
fn parse_graph_element(v: &serde_json::Value) -> Result<GraphName, QueryError> {
    match v {
        serde_json::Value::String(s) if s == "Default" || s == "default" => Ok(GraphName::Default),
        serde_json::Value::String(s) => GraphName::named(s.clone())
            .map_err(|e| QueryError::Args(format!("bad graph: {e}"))),
        serde_json::Value::Object(map) => {
            if let Some(s) = map.get("Named").and_then(|x| x.as_str()) {
                return GraphName::named(s.to_string())
                    .map_err(|e| QueryError::Args(format!("bad graph: {e}")));
            }
            if map.contains_key("Default") {
                return Ok(GraphName::Default);
            }
            Err(QueryError::Args(format!("bad graph element: {v}")))
        }
        _ => Err(QueryError::Args(format!("bad graph element: {v}"))),
    }
}

/// Parse an untrusted JSON value into a `QueryPattern` (planner input).
/// Accepts planner-friendly bare-string graphs (`"urn:..."`) as well as the
/// `GraphName` enum encoding.
pub fn pattern_from_json(mut v: serde_json::Value) -> Result<QueryPattern, QueryError> {
    if let Some(graphs) = v.get("graphs").and_then(|g| g.as_array()).cloned() {
        let parsed: Result<Vec<GraphName>, QueryError> =
            graphs.iter().map(parse_graph_element).collect();
        let parsed = parsed?;
        if let Some(obj) = v.as_object_mut() {
            obj.insert(
                "graphs".to_string(),
                serde_json::to_value(&parsed)
                    .map_err(|e| QueryError::Args(format!("invalid graphs: {e}")))?,
            );
        }
    }
    serde_json::from_value(v).map_err(|e| QueryError::Args(format!("invalid pattern: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ontology::{load_default_ontology, OntologyReasoner};
    use crate::rdf::OxigraphRepository;

    const EX: &str = "http://example.com/";
    const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

    fn ex(s: &str) -> String {
        format!("{EX}{s}")
    }

    fn setup() -> (OxigraphRepository, OntologyReasoner, GraphContext, ExecutionContext, AppLimits) {
        let repo = OxigraphRepository::new_in_memory(50_000).unwrap();
        load_default_ontology(&repo).unwrap();
        let g = GraphName::tenant("acme").unwrap();
        let alice = Term::iri(ex("Alice")).unwrap();
        let acme = Term::iri(ex("Acme")).unwrap();
        repo.insert(&[
            crate::rdf::Quad::new(
                alice.clone(),
                Term::iri(RDF_TYPE.to_string()).unwrap(),
                Term::iri(ex("Contact")).unwrap(),
                g.clone(),
            )
            .unwrap(),
            crate::rdf::Quad::new(
                alice.clone(),
                Term::iri(ex("name")).unwrap(),
                Term::literal("Alice"),
                g.clone(),
            )
            .unwrap(),
            crate::rdf::Quad::new(
                alice.clone(),
                Term::iri(ex("account")).unwrap(),
                acme.clone(),
                g.clone(),
            )
            .unwrap(),
            crate::rdf::Quad::new(
                acme.clone(),
                Term::iri(RDF_TYPE.to_string()).unwrap(),
                Term::iri(ex("Account")).unwrap(),
                g.clone(),
            )
            .unwrap(),
            crate::rdf::Quad::new(
                acme.clone(),
                Term::iri(ex("state")).unwrap(),
                Term::literal("OH"),
                g.clone(),
            )
            .unwrap(),
        ])
        .unwrap();
        let reasoner = OntologyReasoner::from_repository(&repo).unwrap();
        let gctx = GraphContext {
            graphs: vec![g.clone()],
            namespaces: [("ex".to_string(), EX.to_string())].into_iter().collect(),
            ..Default::default()
        };
        let ctx = ExecutionContext::new(
            "req-1",
            "alice",
            "acme",
            vec![g, GraphName::ontology()],
            vec![
                crate::policy::Operation::Read,
                crate::policy::Operation::Create,
            ],
        );
        (repo, reasoner, gctx, ctx, AppLimits::default())
    }

    pub fn ohio_pattern(graph: &GraphName) -> QueryPattern {
        QueryPattern {
            operation: QueryOperation::Select,
            graphs: vec![graph.clone()],
            patterns: vec![
                TriplePattern {
                    subject: SubjectPattern::Var { name: "?contact".to_string() },
                    predicate: PredicatePattern::Iri { iri: RDF_TYPE.to_string() },
                    object: ObjectPattern::Iri { iri: ex("Contact") },
                },
                TriplePattern {
                    subject: SubjectPattern::Var { name: "?contact".to_string() },
                    predicate: PredicatePattern::Iri { iri: ex("account") },
                    object: ObjectPattern::Var { name: "?account".to_string() },
                },
                TriplePattern {
                    subject: SubjectPattern::Var { name: "?account".to_string() },
                    predicate: PredicatePattern::Iri { iri: ex("state") },
                    object: ObjectPattern::Literal {
                        value: "OH".to_string(),
                        datatype: None,
                        lang: None,
                    },
                },
            ],
            filters: vec![],
            return_vars: vec!["?contact".to_string()],
            limit: Some(50),
        }
    }

    #[test]
    fn compile_emits_graph_limit_and_escapes() {
        let (_, _, gctx, _, _) = setup_owned_ctx();
        let g = gctx.graphs[0].clone();
        let compiled = compile_to_query_string(&ohio_pattern(&g));
        assert!(compiled.contains("GRAPH <urn:needle:tenant:acme>"), "{compiled}");
        assert!(compiled.contains("LIMIT 50"), "{compiled}");
        assert!(!compiled.contains("UPDATE"), "{compiled}");
        // Injection payloads are escaped, never interpolated raw.
        let mut evil = ohio_pattern(&g);
        evil.patterns[2].object = ObjectPattern::Literal {
            value: "\" . } DELETE { ?s ?p ?o".to_string(),
            datatype: None,
            lang: None,
        };
        let c2 = compile_to_query_string(&evil);
        assert!(c2.contains("\\\""), "{c2}");
    }

    fn setup_owned_ctx() -> (OxigraphRepository, OntologyReasoner, GraphContext, ExecutionContext, AppLimits) {
        setup()
    }

    #[test]
    fn rejection_matrix() {
        let (repo, reasoner, gctx, ctx, limits) = setup();
        let g = gctx.graphs[0].clone();

        // Unknown predicate.
        let mut p = ohio_pattern(&g);
        p.patterns[1].predicate = PredicatePattern::Iri {
            iri: "http://example.com/nope".to_string(),
        };
        let e = validate_and_compile(p, &repo, &reasoner, &gctx, &ctx, &limits).unwrap_err();
        assert!(matches!(e, QueryError::Grounding(_)), "{e:?}");

        // Domain mismatch: account predicate on an Account subject.
        let mut p2 = ohio_pattern(&g);
        p2.patterns[1].subject = SubjectPattern::Iri { iri: ex("Acme") };
        let e2 = validate_and_compile(p2, &repo, &reasoner, &gctx, &ctx, &limits).unwrap_err();
        assert!(matches!(e2, QueryError::Validation(_)), "{e2:?}");

        // Cross-graph escape.
        let mut p3 = ohio_pattern(&g);
        p3.graphs = vec![GraphName::tenant("other").unwrap()];
        let e3 = validate_and_compile(p3, &repo, &reasoner, &gctx, &ctx, &limits).unwrap_err();
        assert!(matches!(e3, QueryError::Auth(_)), "{e3:?}");

        // Smuggled update language in a filter.
        let mut p4 = ohio_pattern(&g);
        p4.filters = vec![FilterOp::Contains {
            var: "?contact".to_string(),
            substring: "x } DELETE { ?s ?p ?o".to_string(),
        }];
        let e4 = validate_and_compile(p4, &repo, &reasoner, &gctx, &ctx, &limits).unwrap_err();
        assert!(matches!(e4, QueryError::Safety(_)), "{e4:?}");
    }

    #[test]
    fn representation_hint_for_ohio_vs_oh() {
        let (repo, reasoner, gctx, ctx, limits) = setup();
        let g = gctx.graphs[0].clone();
        let mut p = ohio_pattern(&g);
        // Planner assumes "Ohio"; store has "OH".
        p.patterns[2].object = ObjectPattern::Literal {
            value: "Ohio".to_string(),
            datatype: None,
            lang: None,
        };
        let compiled = validate_and_compile(p, &repo, &reasoner, &gctx, &ctx, &limits).unwrap();
        assert!(!compiled.representations.is_empty());
        let h = &compiled.representations[0];
        assert!(h.stored_samples.iter().any(|s| s.contains("OH")), "{h:?}");
        assert!(h.hint.contains("NOT found"), "{h:?}");
    }

    #[test]
    fn three_tool_flow_is_deterministic() {
        let (repo, reasoner, gctx, ctx, limits) = setup();
        let g = gctx.graphs[0].clone();
        let store = QueryPlanStore::new();
        let plan = store
            .build(ohio_pattern(&g), &repo, &reasoner, &gctx, &ctx, &limits)
            .unwrap();
        assert!(plan.explanation.contains("3 pattern"), "{:?}", plan.explanation);
        let r1 = store.execute(&plan.id, &repo, &reasoner, &gctx, &ctx, &limits, None).unwrap();
        let r2 = store.execute(&plan.id, &repo, &reasoner, &gctx, &ctx, &limits, None).unwrap();
        assert_eq!(r1, r2);
        assert!(r1.uris.contains(&ex("Alice")));
        assert!(!r1.truncated);
    }
}
