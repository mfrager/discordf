//! Observability (Phase 1 skeleton + Phase 5 spans).
//!
//! - [`init_tracing`] installs the global subscriber (idempotent).
//! - [`request_span`] opens a per-request span carrying request/principal/
//!   tenant; [`tool_span`] and [`repo_span`] nest per-tool / per-repo-call
//!   spans so iteration counts, scores and validation outcomes are traceable.
//! - [`redact`] strips literals marked sensitive before logging or
//!   returning tool payloads in traces.

use tracing::{info_span, Span};
use tracing_subscriber::{fmt, EnvFilter};

/// Initialise the global `tracing` subscriber. Safe to call multiple times
/// (subsequent calls are no-ops) so tests can invoke it freely.
pub fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = fmt().with_env_filter(filter).with_target(true).try_init();
}

/// Per-request span: `request_id`, `principal`, `tenant`.
pub fn request_span(request_id: &str, principal: &str, tenant: &str) -> Span {
    info_span!(
        "request",
        request_id = %request_id,
        principal = %principal,
        tenant = %tenant,
    )
}

/// Per-tool-call span nested inside the request span.
pub fn tool_span(tool: &str, call_id: &str, iteration: u64) -> Span {
    info_span!(
        "tool",
        tool = %tool,
        call_id = %call_id,
        iteration = iteration,
    )
}

/// Per-repository-call span (CRUD / search sync path).
pub fn repo_span(op: &str, graph: &str) -> Span {
    info_span!("repo", op = %op, graph = %graph)
}

/// Log iteration/validation outcome fields on the current span.
pub fn record_outcome(iterations: usize, ops: usize, ok: bool) {
    let s = Span::current();
    s.record("iterations", iterations);
    s.record("ops", ops);
    s.record("ok", ok);
}

/// Redact literals marked sensitive: any JSON object with
/// `"sensitive": true` has its `value`/`result` fields replaced, and keys
/// containing `password`/`secret`/`token` (case-insensitive) are masked.
/// Returns a redacted clone; the input is never mutated.
pub fn redact(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let sensitive_flag = map
                .get("sensitive")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                let kl = k.to_lowercase();
                let sensitive_value =
                    sensitive_flag && (k == "value" || k == "result" || k == "text");
                let secret_key =
                    kl.contains("password") || kl.contains("secret") || kl.contains("token");
                if sensitive_value || secret_key {
                    out.insert(
                        k.clone(),
                        serde_json::Value::String("[redacted]".to_string()),
                    );
                } else {
                    out.insert(k.clone(), redact(v));
                }
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(xs) => serde_json::Value::Array(xs.iter().map(redact).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_sensitive_literals() {
        let v = serde_json::json!({
            "predicate": "http://example.com/name",
            "sensitive": true,
            "value": "hunter2",
            "nested": {"api_token": "abc", "label": "ok"}
        });
        let r = redact(&v);
        assert_eq!(r["value"], "[redacted]");
        assert_eq!(r["nested"]["api_token"], "[redacted]");
        assert_eq!(r["nested"]["label"], "ok");
        // Input untouched.
        assert_eq!(v["value"], "hunter2");
    }

    #[test]
    fn spans_construct_without_subscriber_panic() {
        let _ = request_span("r1", "alice", "acme");
        let _ = tool_span("search", "c1", 1);
        let _ = repo_span("read", "urn:needle:tenant:acme");
    }
}
