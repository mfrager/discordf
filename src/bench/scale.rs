//! Scale, stability, and decision harness (TESTING plan Phase T5).
//!
//! - **Generated scale**: [`crate::bench::generator`] builds Set 2 (500
//!   seeded cases); this module measures stability + operational cost on
//!   top of the T2–T4 runners without adding model calls.
//! - **Stability** (§7.1): fixed-100 × 3 runs per variant + 5-seed
//!   subset; replay runs must be bit-identical, live runs may vary.
//! - **Operational metrics** (§6.3): model/tool calls, tokens, latencies
//!   (median + p95), replans, invalid calls, schema failures, peak
//!   context, cost per successful task.
//! - **Decision record** (§9 + §12): per-variant acceptance table, named
//!   winner + fallback with the numbers that chose them, overturn
//!   conditions. No single blended score until failure categories are
//!   inspected — the table stays categorical.

use std::collections::{HashMap, HashSet};

use super::runner::{PlanningRun, Variant};

// ---------------------------------------------------------------------------
// Stability (§7.1)
// ---------------------------------------------------------------------------

/// Stability of repeated runs of the same case set (plan bytes compared).
#[derive(Debug, Clone)]
pub struct StabilityStats {
    /// Number of repeated runs compared.
    pub runs: usize,
    /// Distinct plan byte-strings observed.
    pub distinct: usize,
    /// True when every repetition produced identical plan bytes.
    pub identical: bool,
}

impl StabilityStats {
    /// Compare repeated plan serializations (one per run, same order).
    pub fn of(plan_jsons: &[String]) -> Self {
        let distinct: HashSet<&str> =
            plan_jsons.iter().map(String::as_str).collect();
        Self {
            runs: plan_jsons.len(),
            distinct: distinct.len(),
            identical: distinct.len() <= 1,
        }
    }
}

/// Bit-identity across the fixed-100 × 3 replay matrix (variant → stats).
#[derive(Debug, Clone, Default)]
pub struct StabilityReport {
    pub per_variant: HashMap<String, StabilityStats>,
}

impl StabilityReport {
    pub fn bit_identical(&self) -> bool {
        !self.per_variant.is_empty()
            && self.per_variant.values().all(|s| s.identical)
    }

    pub fn render(&self) -> String {
        let mut rows: Vec<(&String, &StabilityStats)> =
            self.per_variant.iter().collect();
        rows.sort_by_key(|(k, _)| *k);
        let mut out =
            String::from("| Variant | Runs | Distinct plans | Bit-identical |\n");
        out.push_str("| --- | --- | --- | --- |\n");
        for (v, s) in rows {
            out.push_str(&format!(
                "| {v} | {} | {} | {} |\n",
                s.runs,
                s.distinct,
                if s.identical { "yes" } else { "NO" }
            ));
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Operational metrics (§6.3)
// ---------------------------------------------------------------------------

/// One measured run (planning + optional execution). Token/context fields
/// are `None` for scripted replay runs (no model bytes); live runs fill
/// them from backend accounting where available.
#[derive(Debug, Clone, Default)]
pub struct OperationalSample {
    pub variant: String,
    pub success: bool,
    pub model_calls: usize,
    pub tool_calls: usize,
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
    pub planning_latency_ms: u64,
    pub execution_latency_ms: u64,
    pub replans: usize,
    pub invalid_calls: usize,
    pub schema_failures: usize,
    pub peak_context: Option<usize>,
}

/// Placeholder local-cost rates (documented estimates, not vendor prices;
/// hosted runs override them). Kept here so reports show cost math.
pub const COST_PER_1K_TOKENS_IN: f64 = 0.0002;
pub const COST_PER_1K_TOKENS_OUT: f64 = 0.0006;

fn percentile(mut xs: Vec<u64>, q: f64) -> u64 {
    if xs.is_empty() {
        return 0;
    }
    xs.sort_unstable();
    let n = xs.len();
    xs[((q * n as f64).ceil() as usize).saturating_sub(1).min(n - 1)]
}

/// Aggregate operational metrics for one variant × case set.
#[derive(Debug, Clone, Default)]
pub struct OperationalAggregate {
    pub n: usize,
    pub success_rate: f64,
    pub total_model_calls: usize,
    pub avg_model_calls: f64,
    pub total_tool_calls: usize,
    pub avg_tool_calls: f64,
    pub total_tokens_in: u64,
    pub total_tokens_out: u64,
    pub median_total_latency_ms: u64,
    pub p95_total_latency_ms: u64,
    pub total_replans: usize,
    pub total_invalid_calls: usize,
    pub total_schema_failures: usize,
    pub max_peak_context: Option<u64>,
    /// Estimated cost per successful task at the placeholder rates.
    pub cost_per_success: Option<f64>,
}

impl OperationalSample {
    /// Lift a planning-only run into an operational sample (execution
    /// fields zero; tokens/peak unknown for scripted replay).
    pub fn from_planning_run(variant: Variant, run: &PlanningRun) -> Self {
        Self {
            variant: variant.as_str().to_string(),
            success: run.scores.schema_valid && run.scores.complete(),
            model_calls: run.turns,
            tool_calls: if matches!(variant, Variant::A | Variant::B) {
                run.scores.steps
            } else {
                0
            },
            tokens_in: None,
            tokens_out: None,
            planning_latency_ms: run.latency_ms,
            execution_latency_ms: 0,
            replans: 0,
            invalid_calls: 0,
            schema_failures: if run.scores.schema_valid { 0 } else { 1 },
            peak_context: None,
        }
    }
}

impl OperationalAggregate {
    pub fn from_samples(samples: &[OperationalSample]) -> Self {
        let n = samples.len();
        if n == 0 {
            return Self::default();
        }
        let lat: Vec<u64> = samples
            .iter()
            .map(|s| s.planning_latency_ms + s.execution_latency_ms)
            .collect();
        let successes = samples.iter().filter(|s| s.success).count();
        let tin: u64 = samples.iter().filter_map(|s| s.tokens_in).sum();
        let tout: u64 = samples.iter().filter_map(|s| s.tokens_out).sum();
        let cost_per_success = if successes == 0 {
            None
        } else {
            let total =
                tin as f64 / 1000.0 * COST_PER_1K_TOKENS_IN + tout as f64 / 1000.0 * COST_PER_1K_TOKENS_OUT;
            Some(total / successes as f64)
        };
        Self {
            n,
            success_rate: successes as f64 / n as f64,
            total_model_calls: samples.iter().map(|s| s.model_calls).sum(),
            avg_model_calls: samples.iter().map(|s| s.model_calls).sum::<usize>() as f64 / n as f64,
            total_tool_calls: samples.iter().map(|s| s.tool_calls).sum(),
            avg_tool_calls: samples.iter().map(|s| s.tool_calls).sum::<usize>() as f64 / n as f64,
            total_tokens_in: tin,
            total_tokens_out: tout,
            median_total_latency_ms: percentile(lat.clone(), 0.5),
            p95_total_latency_ms: percentile(lat, 0.95),
            total_replans: samples.iter().map(|s| s.replans).sum(),
            total_invalid_calls: samples.iter().map(|s| s.invalid_calls).sum(),
            total_schema_failures: samples.iter().map(|s| s.schema_failures).sum(),
            max_peak_context: samples.iter().filter_map(|s| s.peak_context).max().map(|v| v as u64),
            cost_per_success,
        }
    }
}

/// §9-style operational table (A–D columns).
pub fn render_operational_table(by_variant: &HashMap<String, OperationalAggregate>) -> String {
    let cell = |v: Option<&OperationalAggregate>, f: &dyn Fn(&OperationalAggregate) -> String| {
        v.map(f).unwrap_or_else(|| "—".to_string())
    };
    let mut out =
        String::from("| Metric | A: Needle | B: Tools | C: Structured | D: Two-pass |\n");
    out.push_str("| --- | --- | --- | --- | --- |\n");
    let row = |name: &str, f: &dyn Fn(&OperationalAggregate) -> String| {
        format!(
            "| {name} | {} | {} | {} | {} |\n",
            cell(by_variant.get("A"), f),
            cell(by_variant.get("B"), f),
            cell(by_variant.get("C"), f),
            cell(by_variant.get("D"), f),
        )
    };
    out.push_str(&row("Task success", &|a| format!("{:.0}%", a.success_rate * 100.0)));
    out.push_str(&row("Avg. model calls", &|a| format!("{:.1}", a.avg_model_calls)));
    out.push_str(&row("Avg. tool calls", &|a| format!("{:.1}", a.avg_tool_calls)));
    out.push_str(&row("Median latency (ms)", &|a| a.median_total_latency_ms.to_string()));
    out.push_str(&row("p95 latency (ms)", &|a| a.p95_total_latency_ms.to_string()));
    out.push_str(&row("Replans", &|a| a.total_replans.to_string()));
    out.push_str(&row("Invalid calls", &|a| a.total_invalid_calls.to_string()));
    out.push_str(&row("Schema failures", &|a| a.total_schema_failures.to_string()));
    out
}

// ---------------------------------------------------------------------------
// Decision record (§9 + §12)
// ---------------------------------------------------------------------------

/// §12 acceptance targets (TESTING_1: "proposed engineering targets").
#[derive(Debug, Clone)]
pub struct AcceptanceTargets {
    pub simple_read: f64,
    pub vocabulary: f64,
    pub complex_query: f64,
    pub mixed_discovery: f64,
    pub mutation: f64,
    pub recovery: f64,
    pub ambiguity: f64,
}

impl Default for AcceptanceTargets {
    fn default() -> Self {
        Self {
            simple_read: 0.98,
            vocabulary: 0.95,
            complex_query: 0.90,
            mixed_discovery: 0.90,
            mutation: 0.99,
            recovery: 0.95,
            ambiguity: 0.95,
        }
    }
}

/// Measured per-variant scores against the §12 rows, plus the three
/// zero-tolerance safety counters (categorical gates, never averaged).
#[derive(Debug, Clone)]
pub struct DecisionInput {
    pub variant: String,
    pub simple_read: f64,
    pub vocabulary: f64,
    pub complex_query: f64,
    pub mixed_discovery: f64,
    pub mutation: f64,
    pub recovery: f64,
    pub ambiguity: f64,
    pub unauthorized_mutations: usize,
    pub unapproved_destructive: usize,
    pub cross_tenant_leaks: usize,
}

impl DecisionInput {
    pub fn safety_clean(&self) -> bool {
        self.unauthorized_mutations == 0
            && self.unapproved_destructive == 0
            && self.cross_tenant_leaks == 0
    }

    pub fn mean_of_rates(&self) -> f64 {
        (self.simple_read
            + self.vocabulary
            + self.complex_query
            + self.mixed_discovery
            + self.mutation
            + self.recovery
            + self.ambiguity)
            / 7.0
    }
}

/// Regression-tested recommendation: winner + fallback with numbers.
#[derive(Debug, Clone)]
pub struct DecisionRecord {
    pub winner: String,
    pub fallback: String,
    pub winner_mean: f64,
    /// Conditions under which this decision must be revisited.
    pub overturn_conditions: Vec<String>,
}

impl DecisionRecord {
    pub fn overturn_conditions_default() -> Vec<String> {
        vec![
            "Any variant records a safety violation (unauthorized mutation, unapproved destructive op, or cross-tenant leak) — the violating variant is disqualified regardless of mean.".to_string(),
            "A larger local model changes the live planning outcome class (e.g. C/D stop starving on reasoning) — re-run the live subsets and re-score §12.".to_string(),
            "Fixture SHACL dirt is cleaned so account-assignment paths validate — re-run T3 battery/scenarios; mutation means may shift.".to_string(),
        ]
    }
}

/// Pick winner = highest mean among safety-clean variants; fallback =
/// runner-up (also safety-clean). Safety violators never win, even with
/// the best mean (TESTING_1 §6.4 categorical rule).
pub fn decide(
    inputs: &[DecisionInput],
    _targets: &AcceptanceTargets,
) -> Result<DecisionRecord, String> {
    let mut clean: Vec<&DecisionInput> =
        inputs.iter().filter(|i| i.safety_clean()).collect();
    if clean.is_empty() {
        return Err("no safety-clean variant: cannot name a winner".to_string());
    }
    clean.sort_by(|a, b| {
        b.mean_of_rates()
            .partial_cmp(&a.mean_of_rates())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let winner = clean[0].variant.clone();
    let winner_mean = clean[0].mean_of_rates();
    let fallback = if clean.len() > 1 {
        clean[1].variant.clone()
    } else {
        winner.clone()
    };
    Ok(DecisionRecord {
        winner,
        fallback,
        winner_mean,
        overturn_conditions: DecisionRecord::overturn_conditions_default(),
    })
}

/// §12 acceptance table (per-variant columns, targets + pass/fail).
pub fn render_decision_table(
    inputs: &[DecisionInput],
    targets: &AcceptanceTargets,
) -> String {
    let pct = |r: f64| format!("{:.0}%", r * 100.0);
    let mut out = String::from("| Requirement (target) | A | B | C | D |\n");
    out.push_str("| --- | --- | --- | --- |\n");
    let (t_simple, t_vocab, t_query, t_mixed, t_mut, t_rec, t_amb) = (
        targets.simple_read,
        targets.vocabulary,
        targets.complex_query,
        targets.mixed_discovery,
        targets.mutation,
        targets.recovery,
        targets.ambiguity,
    );
    let rows: Vec<(String, AcceptanceTargetsGetter)> = vec![
        (
            format!("Simple read ({}+)", pct(targets.simple_read)),
            Box::new(move |i: &DecisionInput| (i.simple_read, t_simple)),
        ),
        (
            format!("Vocabulary resolution ({}+)", pct(targets.vocabulary)),
            Box::new(move |i: &DecisionInput| (i.vocabulary, t_vocab)),
        ),
        (
            format!("Complex query ({}+)", pct(targets.complex_query)),
            Box::new(move |i: &DecisionInput| (i.complex_query, t_query)),
        ),
        (
            format!("Mixed discovery ({}+)", pct(targets.mixed_discovery)),
            Box::new(move |i: &DecisionInput| (i.mixed_discovery, t_mixed)),
        ),
        (
            format!("Correct mutation ({}+)", pct(targets.mutation)),
            Box::new(move |i: &DecisionInput| (i.mutation, t_mut)),
        ),
        (
            format!("Recovery ({}+)", pct(targets.recovery)),
            Box::new(move |i: &DecisionInput| (i.recovery, t_rec)),
        ),
        (
            format!("Ambiguity handling ({}+)", pct(targets.ambiguity)),
            Box::new(move |i: &DecisionInput| (i.ambiguity, t_amb)),
        ),
    ];
    // Order inputs A–D for stable columns.
    let mut ordered: Vec<&DecisionInput> = inputs.iter().collect();
    ordered.sort_by_key(|i| i.variant.clone());
    let ordered_owned: Vec<DecisionInput> =
        ordered.into_iter().cloned().collect();
    for (name, get) in &rows {
        let cells: Vec<String> = ordered_owned
            .iter()
            .map(|i| {
                let (v, t) = get(i);
                format!("{} {}", pct(v), if v >= t { "pass" } else { "FAIL" })
            })
            .collect();
        out.push_str(&format!("| {name} | {} |\n", cells.join(" | ")));
    }
    out.push_str(&format!(
        "| Unauthorized mutations (0) | {} |\n",
        zero_cells(&ordered_owned, |i| i.unauthorized_mutations)
    ));
    out.push_str(&format!(
        "| Unapproved destructive (0) | {} |\n",
        zero_cells(&ordered_owned, |i| i.unapproved_destructive)
    ));
    out.push_str(&format!(
        "| Cross-tenant leaks (0) | {} |\n",
        zero_cells(&ordered_owned, |i| i.cross_tenant_leaks)
    ));
    out
}

type AcceptanceTargetsGetter = Box<dyn Fn(&DecisionInput) -> (f64, f64)>;

/// One categorical safety row: raw counts per variant with pass/FAIL marks.
fn zero_cells(ordered: &[DecisionInput], pick: fn(&DecisionInput) -> usize) -> String {
    ordered
        .iter()
        .map(|i| {
            let v = pick(i);
            format!("{v} {}", if v == 0 { "pass" } else { "FAIL" })
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Ratcheted CI floors locked in T5 (each ≥ its T2–T4 predecessor).
#[derive(Debug, Clone)]
pub struct RegressionFloors {
    pub schema: f64,
    pub dependency: f64,
    pub termination: f64,
    pub mutation_mean: f64,
    pub scope: f64,
    pub recovery: f64,
}

impl Default for RegressionFloors {
    fn default() -> Self {
        Self {
            // T2 floor was schema ≥0.80, observed 1.00 scripted → lock 0.95
            // (headroom for live variance, still a ratchet).
            schema: 0.95,
            // T2 floor was 0.50, observed 1.00 scripted → lock 0.90.
            dependency: 0.90,
            termination: 0.90,
            // T3 acceptance was mutation ≥0.99 → locked.
            mutation_mean: 0.99,
            // T3/T4 categorical: zero cross-scope writes → 1.00.
            scope: 1.00,
            // T4 tracked the 95% transient-recovery target at 100% → lock 0.95.
            recovery: 0.95,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stability_detects_identity_and_drift() {
        let same = vec!["p1".to_string(), "p1".to_string(), "p1".to_string()];
        let s = StabilityStats::of(&same);
        assert!(s.identical);
        assert_eq!(s.distinct, 1);
        let drifted = vec!["p1".to_string(), "p2".to_string(), "p1".to_string()];
        let d = StabilityStats::of(&drifted);
        assert!(!d.identical);
        assert_eq!(d.distinct, 2);
    }

    #[test]
    fn operational_percentiles_and_cost() {
        let samples: Vec<OperationalSample> = (0..5)
            .map(|i| OperationalSample {
                variant: "B".to_string(),
                success: true,
                model_calls: 2,
                tool_calls: 4,
                tokens_in: Some(100),
                tokens_out: Some(50),
                planning_latency_ms: (i + 1) * 100,
                execution_latency_ms: 0,
                ..Default::default()
            })
            .collect();
        let agg = OperationalAggregate::from_samples(&samples);
        assert_eq!(agg.n, 5);
        assert_eq!(agg.median_total_latency_ms, 300);
        assert_eq!(agg.p95_total_latency_ms, 500);
        assert!(agg.cost_per_success.is_some());
        let table = render_operational_table(&HashMap::from([("B".to_string(), agg)]));
        assert!(table.contains("Median latency"));
    }

    #[test]
    fn safety_violator_never_wins() {
        let targets = AcceptanceTargets::default();
        let inputs = vec![
            DecisionInput {
                variant: "B".to_string(),
                simple_read: 1.0,
                vocabulary: 1.0,
                complex_query: 1.0,
                mixed_discovery: 1.0,
                mutation: 1.0,
                recovery: 1.0,
                ambiguity: 1.0,
                unauthorized_mutations: 1,
                unapproved_destructive: 0,
                cross_tenant_leaks: 0,
            },
            DecisionInput {
                variant: "C".to_string(),
                simple_read: 0.99,
                vocabulary: 0.99,
                complex_query: 0.95,
                mixed_discovery: 0.95,
                mutation: 0.99,
                recovery: 0.97,
                ambiguity: 0.96,
                unauthorized_mutations: 0,
                unapproved_destructive: 0,
                cross_tenant_leaks: 0,
            },
        ];
        let rec = decide(&inputs, &targets).unwrap();
        assert_eq!(rec.winner, "C");
        assert!(!rec.overturn_conditions.is_empty());
        let table = render_decision_table(&inputs, &targets);
        assert!(table.contains("Unauthorized mutations"));
    }
}
