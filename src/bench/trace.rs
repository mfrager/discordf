//! Run traces (TESTING plan Phase T1, §10).
//!
//! Every benchmark run records the same event sequence regardless of
//! variant, so failures can be attributed to the planning approach rather
//! than to missing instrumentation.

use serde::{Deserialize, Serialize};
use std::time::Instant;

/// Trace events in lifecycle order (TESTING_1 §10).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum TraceEvent {
    RequestReceived { request: String },
    PlanningStarted { variant: String },
    ToolRequested { tool: String, args: String },
    ToolResponded { tool: String, ok: bool },
    PlanUpdated { steps: usize },
    PlanCompleted { steps: usize },
    SemanticValidation { errors: usize },
    QueryCompiled { patterns: usize },
    QueryExecuted { count: usize, truncated: bool },
    MutationPreviewed { targets: usize },
    ApprovalRequested { step: String },
    TransactionStarted { step: String },
    TransactionCommitted { added: usize, removed: usize },
    PostCommitVerification { answer: String },
    Finished { outcome: String },
}

/// Timestamped trace for one benchmark run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Trace {
    pub events: Vec<(u64, TraceEvent)>,
    #[serde(skip)]
    started: Option<Instant>,
}

impl Trace {
    pub fn new() -> Self {
        Self { events: vec![], started: Some(Instant::now()) }
    }

    pub fn record(&mut self, event: TraceEvent) {
        let ms = self.started.map(|s| s.elapsed().as_millis() as u64).unwrap_or(0);
        self.events.push((ms, event));
    }

    pub fn kinds(&self) -> Vec<&'static str> {
        self.events
            .iter()
            .map(|(_, e)| match e {
                TraceEvent::RequestReceived { .. } => "request_received",
                TraceEvent::PlanningStarted { .. } => "planning_started",
                TraceEvent::ToolRequested { .. } => "tool_requested",
                TraceEvent::ToolResponded { .. } => "tool_responded",
                TraceEvent::PlanUpdated { .. } => "plan_updated",
                TraceEvent::PlanCompleted { .. } => "plan_completed",
                TraceEvent::SemanticValidation { .. } => "semantic_validation",
                TraceEvent::QueryCompiled { .. } => "query_compiled",
                TraceEvent::QueryExecuted { .. } => "query_executed",
                TraceEvent::MutationPreviewed { .. } => "mutation_previewed",
                TraceEvent::ApprovalRequested { .. } => "approval_requested",
                TraceEvent::TransactionStarted { .. } => "transaction_started",
                TraceEvent::TransactionCommitted { .. } => "transaction_committed",
                TraceEvent::PostCommitVerification { .. } => "post_commit_verification",
                TraceEvent::Finished { .. } => "final_response",
            })
            .collect()
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(&self.events).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_round_trips() {
        let mut t = Trace::new();
        t.record(TraceEvent::RequestReceived { request: "q".to_string() });
        t.record(TraceEvent::Finished { outcome: "ok".to_string() });
        assert_eq!(t.kinds(), vec!["request_received", "final_response"]);
        let v: serde_json::Value = serde_json::from_str(&t.to_json()).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 2);
    }
}
