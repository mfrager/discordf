//! Agent execution loop (Phase 4) + progress/final assembly (Phase 5).
//!
//! Per request: `complete()` → validate args → grounding check → registry
//! dispatch with `ExecutionContext` → compact result → feed back as the
//! next turn. Stops on no-more-calls, `max_iterations` (8), or the
//! ops-per-request cap. Tool failure yields a structured
//! `{ tool, category, message }` and lets the model retry / search
//! elsewhere / explain.

pub mod conversation;
pub mod worker;

use crate::config::AppLimits;
use crate::model::{ModelAdapter, ModelCompletion};
use crate::policy::ExecutionContext;
use crate::tools::{ToolError, ToolRegistry, ToolState};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub tool: String,
    pub call_id: String,
    pub ok: bool,
    pub result: serde_json::Value,
}

/// One app-layer progress event. The model has no token streaming, so the
/// app emits these per tool call (plus start/finish) and the API surfaces
/// them via polling (`progress` array) or SSE.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProgressEvent {
    pub seq: u64,
    pub iteration: u64,
    pub tool: String,
    pub call_id: String,
    pub ok: bool,
    pub message: String,
}

/// User-facing assembled response (Phase 5): the app — not Needle —
/// summarizes completed ops + results + residual errors.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FinalResponse {
    pub summary: String,
    pub ops: usize,
    pub iterations: usize,
    pub succeeded: Vec<String>,
    pub failed: Vec<ToolError>,
    pub model_text: String,
}

impl FinalResponse {
    pub fn assemble(model_text: &str, history: &[(String, ToolResult)]) -> Self {
        let mut succeeded = Vec::new();
        let mut failed = Vec::new();
        for (name, r) in history {
            if r.ok {
                succeeded.push(format!("{}:{}", name, r.call_id));
            } else if let Ok(e) = serde_json::from_value::<ToolError>(r.result.clone()) {
                failed.push(e);
            } else {
                failed.push(ToolError::new(name, "error", r.result.to_string()));
            }
        }
        let summary = assemble_final_response(model_text, history);
        Self {
            summary,
            ops: history.len(),
            iterations: 0,
            succeeded,
            failed,
            model_text: model_text.to_string(),
        }
    }
}

/// App-owned final-response assembly: completed ops + results + residual
/// errors. Needle is the tool router, not the narrator.
pub fn assemble_final_response(model_text: &str, history: &[(String, ToolResult)]) -> String {
    if history.is_empty() {
        return format!("done. model said: {model_text} (no tool ops performed)");
    }
    let mut lines = vec![format!("completed {} op(s).", history.len())];
    for (name, r) in history {
        if r.ok {
            lines.push(format!("ok {} ({})", name, r.call_id));
        } else {
            let msg = r
                .result
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("failed");
            let cat = r
                .result
                .get("category")
                .and_then(|v| v.as_str())
                .unwrap_or("error");
            lines.push(format!(
                "failed {} ({}) [{}]: {}",
                name, r.call_id, cat, msg
            ));
        }
    }
    lines.push(format!("model said: {model_text}"));
    lines.join(" ")
}

#[derive(Debug, Clone)]
pub struct AgentResponse {
    pub final_text: String,
    pub iterations: usize,
    pub ops: usize,
    pub history: Vec<(String, ToolResult)>,
}

pub struct Agent {
    pub limits: AppLimits,
}

impl Agent {
    pub fn new(limits: AppLimits) -> Self {
        Self { limits }
    }

    /// Run one user request to completion.
    pub fn run_request(
        &self,
        model: &mut dyn ModelAdapter,
        registry: &ToolRegistry,
        state: &ToolState,
        ctx: &ExecutionContext,
        user_text: &str,
    ) -> AgentResponse {
        self.run_request_with_progress(model, registry, state, ctx, user_text, &mut |_| {})
    }

    /// Same as [`run_request`](Self::run_request) but calls `on_progress`
    /// after every tool dispatch. The API layer collects these into
    /// polling/SSE progress events since Needle has no token streaming.
    pub fn run_request_with_progress(
        &self,
        model: &mut dyn ModelAdapter,
        registry: &ToolRegistry,
        state: &ToolState,
        ctx: &ExecutionContext,
        user_text: &str,
        on_progress: &mut dyn FnMut(ProgressEvent),
    ) -> AgentResponse {
        let mut history = Vec::new();
        let mut ops = 0usize;
        let mut input = user_text.to_string();
        let mut iterations = 0usize;
        let mut final_text = String::new();
        let mut seq = 0u64;

        while iterations < self.limits.max_iterations {
            iterations += 1;
            let completion = match model.complete(&input) {
                Ok(c) => c,
                Err(e) => {
                    final_text = format!("model error: {e}");
                    break;
                }
            };
            let calls = match completion {
                ModelCompletion::Done(text) => {
                    final_text = text;
                    break;
                }
                ModelCompletion::ToolCalls(calls) => calls,
            };
            if calls.is_empty() {
                final_text = "done".to_string();
                break;
            }
            let mut results = Vec::new();
            for call in calls {
                if ops >= self.limits.max_ops_per_request {
                    let r = ToolResult {
                        tool: call.name.clone(),
                        call_id: call.id.clone(),
                        ok: false,
                        result: serde_json::json!(ToolError::new(
                            &call.name,
                            "limit",
                            format!(
                                "ops-per-request cap ({}) reached",
                                self.limits.max_ops_per_request
                            )
                        )),
                    };
                    on_progress(ProgressEvent {
                        seq,
                        iteration: iterations as u64,
                        tool: call.name.clone(),
                        call_id: call.id.clone(),
                        ok: false,
                        message: "ops-per-request cap reached".to_string(),
                    });
                    seq += 1;
                    history.push((call.name.clone(), r.clone()));
                    results.push(r);
                    continue;
                }
                ops += 1;
                let out = registry.dispatch(state, ctx, &call.name, call.args.clone());
                let r = match out {
                    Ok(value) => ToolResult {
                        tool: call.name.clone(),
                        call_id: call.id.clone(),
                        ok: true,
                        result: value,
                    },
                    Err(e) => {
                        let v = serde_json::to_value(&e).unwrap_or_default();
                        ToolResult {
                            tool: call.name.clone(),
                            call_id: call.id.clone(),
                            ok: false,
                            result: v,
                        }
                    }
                };
                on_progress(ProgressEvent {
                    seq,
                    iteration: iterations as u64,
                    tool: call.name.clone(),
                    call_id: call.id.clone(),
                    ok: r.ok,
                    message: if r.ok {
                        format!("{} ok", call.name)
                    } else {
                        format!("{} failed", call.name)
                    },
                });
                seq += 1;
                history.push((call.name.clone(), r.clone()));
                results.push(r);
            }
            let _ = model.submit_tool_results(&results);
            input = format!(
                "continue. tool_results: {}",
                serde_json::to_value(&results).unwrap_or_default()
            );
            if ops >= self.limits.max_ops_per_request {
                final_text = format!(
                    "stopped: ops-per-request cap ({}) reached after {iterations} iterations",
                    self.limits.max_ops_per_request
                );
                break;
            }
        }
        if final_text.is_empty() {
            final_text = format!("stopped after {iterations} iterations");
        }
        AgentResponse {
            final_text,
            iterations,
            ops,
            history,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn done_immediately_counts_one_iteration() {
        let agent = Agent::new(AppLimits::default());
        let mut m = crate::model::MockModel::done("hello");
        let reg = ToolRegistry::new();
        let repo: std::sync::Arc<dyn crate::rdf::RdfRepository> =
            std::sync::Arc::new(crate::rdf::OxigraphRepository::new_in_memory(100).unwrap());
        let search: std::sync::Arc<dyn crate::search::SearchService> =
            std::sync::Arc::new(crate::search::TantivyIndex::in_ram().unwrap());
        let reasoner = std::sync::Arc::new(crate::ontology::OntologyReasoner::empty());
        let audit = std::sync::Arc::new(crate::policy::AuditLog::in_memory());
        let state = ToolState::new(repo, search, reasoner, vec![], audit, AppLimits::default());
        let ctx = ExecutionContext::new("r", "p", "t", vec![], vec![]);
        let resp = agent.run_request(&mut m, &reg, &state, &ctx, "hi");
        assert_eq!(resp.iterations, 1);
        assert_eq!(resp.ops, 0);
        assert_eq!(resp.final_text, "hello");
    }

    #[test]
    fn progress_events_fire_per_tool_call() {
        let agent = Agent::new(AppLimits::default());
        let mut m = crate::model::MockModel::new(vec![
            ModelCompletion::ToolCalls(vec![crate::model::ToolCall::new(
                "search",
                serde_json::json!({"query": "x"}),
                "c1",
            )]),
            ModelCompletion::Done("done".to_string()),
        ]);
        let reg = ToolRegistry::new();
        let repo: std::sync::Arc<dyn crate::rdf::RdfRepository> =
            std::sync::Arc::new(crate::rdf::OxigraphRepository::new_in_memory(100).unwrap());
        let search: std::sync::Arc<dyn crate::search::SearchService> =
            std::sync::Arc::new(crate::search::TantivyIndex::in_ram().unwrap());
        let reasoner = std::sync::Arc::new(crate::ontology::OntologyReasoner::empty());
        let audit = std::sync::Arc::new(crate::policy::AuditLog::in_memory());
        let state = ToolState::new(repo, search, reasoner, vec![], audit, AppLimits::default());
        let ctx = ExecutionContext::new("r", "p", "t", vec![], vec![]);
        let mut events = Vec::new();
        let resp = agent
            .run_request_with_progress(&mut m, &reg, &state, &ctx, "hi", &mut |e| events.push(e));
        assert_eq!(resp.ops, 1);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].tool, "search");
    }

    #[test]
    fn final_assembly_summarizes_ops_and_errors() {
        let ok = ToolResult {
            tool: "search".into(),
            call_id: "c1".into(),
            ok: true,
            result: serde_json::json!({}),
        };
        let err = ToolResult {
            tool: "create".into(),
            call_id: "c2".into(),
            ok: false,
            result: serde_json::json!({"tool":"create","category":"validation","message":"bad"}),
        };
        let s = assemble_final_response(
            "model done",
            &[("search".into(), ok), ("create".into(), err)],
        );
        assert!(s.contains("completed 2 op(s)"));
        assert!(s.contains("ok search"));
        assert!(s.contains("failed create"));
        assert!(s.contains("model done"));
    }
}
