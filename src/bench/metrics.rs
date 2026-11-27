//! Metrics + comparative report tables (TESTING plan Phase T1, §6 + §9).
//!
//! Primary metrics (§6.1), operational metrics (§6.3), and the per-variant
//! comparison table (§9). Safety (§6.4) is categorical — counted, never
//! averaged — and enforced by gates in T4, not by this aggregator.

use std::collections::HashMap;

/// Per-run measurements. Planning-only runs leave execution fields zero.
#[derive(Debug, Clone, Default)]
pub struct RunMetrics {
    pub success: bool,
    pub plan_complete: bool,
    pub vocab_ok: bool,
    pub query_ok: bool,
    pub mutation_ok: bool,
    pub scope_ok: bool,
    pub recovery_ok: bool,
    pub termination_ok: bool,
    pub model_calls: usize,
    pub tool_calls: usize,
    pub invalid_tool_calls: usize,
    pub schema_failures: usize,
    pub planning_latency_ms: u64,
    pub execution_latency_ms: u64,
    pub replans: usize,
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
}

/// Aggregate over a set of runs (one variant × one case set).
#[derive(Debug, Clone, Default)]
pub struct Aggregate {
    pub n: usize,
    pub success_rate: f64,
    pub plan_complete_rate: f64,
    pub vocab_rate: f64,
    pub query_rate: f64,
    pub mutation_rate: f64,
    pub scope_rate: f64,
    pub recovery_rate: f64,
    pub termination_rate: f64,
    pub avg_model_calls: f64,
    pub avg_tool_calls: f64,
    pub median_total_latency_ms: u64,
    pub p95_total_latency_ms: u64,
}

impl Aggregate {
    pub fn from_runs(runs: &[RunMetrics]) -> Self {
        let n = runs.len();
        if n == 0 {
            return Self::default();
        }
        let rate = |f: fn(&RunMetrics) -> bool| runs.iter().filter(|r| f(r)).count() as f64 / n as f64;
        let mut lat: Vec<u64> = runs.iter().map(|r| r.planning_latency_ms + r.execution_latency_ms).collect();
        lat.sort_unstable();
        let pick = |q: f64| lat[((q * n as f64).ceil() as usize).saturating_sub(1).min(n - 1)];
        Self {
            n,
            success_rate: rate(|r| r.success),
            plan_complete_rate: rate(|r| r.plan_complete),
            vocab_rate: rate(|r| r.vocab_ok),
            query_rate: rate(|r| r.query_ok),
            mutation_rate: rate(|r| r.mutation_ok),
            scope_rate: rate(|r| r.scope_ok),
            recovery_rate: rate(|r| r.recovery_ok),
            termination_rate: rate(|r| r.termination_ok),
            avg_model_calls: runs.iter().map(|r| r.model_calls).sum::<usize>() as f64 / n as f64,
            avg_tool_calls: runs.iter().map(|r| r.tool_calls).sum::<usize>() as f64 / n as f64,
            median_total_latency_ms: pick(0.5),
            p95_total_latency_ms: pick(0.95),
        }
    }
}

/// Render the TESTING_1 §9 comparison table for variants A–D.
pub fn render_report_table(by_variant: &HashMap<String, Aggregate>) -> String {
    let cell = |v: Option<&Aggregate>, f: &dyn Fn(&Aggregate) -> String| {
        v.map(f).unwrap_or_else(|| "—".to_string())
    };
    let pct = |r: f64| format!("{:.0}%", r * 100.0);
    let mut out = String::from("| Metric | A: Needle | B: Tools | C: Structured | D: Two-pass |\n");
    out.push_str("| --- | --- | --- | --- | --- |\n");
    let row = |name: &str, f: &dyn Fn(&Aggregate) -> String| {
        format!(
            "| {name} | {} | {} | {} | {} |\n",
            cell(by_variant.get("A"), f),
            cell(by_variant.get("B"), f),
            cell(by_variant.get("C"), f),
            cell(by_variant.get("D"), f),
        )
    };
    out.push_str(&row("Task success", &|a| pct(a.success_rate)));
    out.push_str(&row("Plan completeness", &|a| pct(a.plan_complete_rate)));
    out.push_str(&row("Vocabulary accuracy", &|a| pct(a.vocab_rate)));
    out.push_str(&row("Query accuracy", &|a| pct(a.query_rate)));
    out.push_str(&row("Mutation accuracy", &|a| pct(a.mutation_rate)));
    out.push_str(&row("Recovery success", &|a| pct(a.recovery_rate)));
    out.push_str(&row("Avg. model calls", &|a| format!("{:.1}", a.avg_model_calls)));
    out.push_str(&row("Avg. tool calls", &|a| format!("{:.1}", a.avg_tool_calls)));
    out.push_str(&row("Median latency (ms)", &|a| a.median_total_latency_ms.to_string()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregate_and_table() {
        let runs = vec![
            RunMetrics { success: true, plan_complete: true, model_calls: 2, tool_calls: 4, planning_latency_ms: 100, ..Default::default() },
            RunMetrics { success: false, plan_complete: false, model_calls: 4, tool_calls: 8, planning_latency_ms: 300, ..Default::default() },
        ];
        let agg = Aggregate::from_runs(&runs);
        assert_eq!(agg.n, 2);
        assert!((agg.success_rate - 0.5).abs() < 1e-9);
        assert!((agg.avg_model_calls - 3.0).abs() < 1e-9);
        assert_eq!(agg.median_total_latency_ms, 100);
        assert_eq!(agg.p95_total_latency_ms, 300);
        let mut m = HashMap::new();
        m.insert("A".to_string(), agg);
        let table = render_report_table(&m);
        assert!(table.contains("| Task success | 50% | — | — | — |"));
    }
}
