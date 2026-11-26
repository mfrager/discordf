//! HTTP API (Phase 5): Axum routes over the serialized worker.
//!
//! - `POST /chat` — main path: auth headers → `ExecutionContext` → worker
//!   (serialized single Needle) → assembled final response + `progress`
//!   polling array (the app's substitute for token streaming, which Needle
//!   does not provide).
//! - `POST /tools/:name` — admin/debug direct tool dispatch (no model).
//! - `GET /health` — liveness.
//! - `POST /admin/reindex` — rebuild the disposable Tantivy index from the
//!   authoritative repo.
//!
//! Auth is a stub mapping headers to [`ExecutionContext`]
//! (`x-principal` / `x-tenant` → real auth later). Missing principal yields
//! 401; cross-graph access is denied by the Phase 2 allowlist.

use crate::agent::conversation::ConversationLog;
use crate::agent::worker::WorkerHandle;
use crate::agent::ProgressEvent;
use crate::observability::{redact, request_span};
use crate::policy::{ExecutionContext, Operation};
use crate::rdf::GraphName;
use crate::tools::{ToolRegistry, ToolState};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json};
use axum::routing::{get, post};
use axum::Router;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tracing::Instrument;

static REQUEST_SEQ: AtomicU64 = AtomicU64::new(1);

/// Shared API state (cheap to clone via `Arc`).
#[derive(Clone)]
pub struct ApiState {
    pub worker: WorkerHandle,
    pub registry: Arc<ToolRegistry>,
    pub tools: ToolState,
    pub log: Arc<ConversationLog>,
}

impl ApiState {
    pub fn new(
        worker: WorkerHandle,
        registry: Arc<ToolRegistry>,
        tools: ToolState,
        log: Arc<ConversationLog>,
    ) -> Self {
        Self {
            worker,
            registry,
            tools,
            log,
        }
    }
}

pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/chat", post(chat))
        .route("/tools/:name", post(tool_call))
        .route("/admin/reindex", post(reindex))
        // Reject oversized bodies (test asserts 413 for >64KiB).
        .layer(tower_http::limit::RequestBodyLimitLayer::new(64 * 1024))
        .with_state(Arc::new(state))
}

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({"status": "ok"}))
}

#[derive(Debug, Deserialize)]
pub struct ChatBody {
    pub message: String,
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ChatResponse {
    pub final_text: String,
    pub ops: usize,
    pub iterations: usize,
    pub progress: Vec<ProgressEvent>,
    pub request_id: String,
}

/// Build an `ExecutionContext` from auth headers. Missing principal (or
/// tenant) → 401; the caller maps cross-graph misuse to 403 via authz.
fn ctx_from_headers(
    headers: &HeaderMap,
    default_session: &str,
) -> Result<ExecutionContext, (StatusCode, Json<serde_json::Value>)> {
    let principal = headers
        .get("x-principal")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .trim()
        .to_string();
    if principal.is_empty() {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "missing x-principal"})),
        ));
    }
    let tenant = headers
        .get("x-tenant")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .trim()
        .to_string();
    if tenant.is_empty() {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "missing x-tenant"})),
        ));
    }
    let request_id = headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            format!(
                "req-{}-{default_session}",
                REQUEST_SEQ.fetch_add(1, Ordering::SeqCst)
            )
        });
    // Optional explicit graph allowlist; defaults to the tenant's own graph
    // plus the shared ontology graph.
    let allowed_graphs = match headers.get("x-graphs").and_then(|v| v.to_str().ok()) {
        Some(list) if !list.trim().is_empty() => list
            .split(',')
            .filter_map(|g| GraphName::named(g.trim()).ok())
            .collect::<Vec<_>>(),
        _ => vec![
            GraphName::tenant(&tenant).unwrap_or(GraphName::Default),
            GraphName::ontology(),
        ],
    };
    Ok(ExecutionContext::new(
        request_id,
        principal,
        tenant,
        allowed_graphs,
        vec![
            Operation::Read,
            Operation::Create,
            Operation::Update,
            Operation::Delete,
        ],
    ))
}

async fn chat(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Json(body): Json<ChatBody>,
) -> impl IntoResponse {
    let session_id = body
        .session_id
        .clone()
        .unwrap_or_else(|| "default".to_string());
    let ctx = match ctx_from_headers(&headers, &session_id) {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };
    let request_id = ctx.request_id.clone();
    let span = request_span(&ctx.request_id, &ctx.principal, &ctx.tenant);
    async move {
        if body.message.len() > 32 * 1024 {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(serde_json::json!({"error": "message too large"})),
            )
                .into_response();
        }
        match state.worker.chat(&session_id, &body.message, ctx).await {
            Ok(out) => {
                // Redact sensitive literals before logging; response keeps
                // real values (redaction is for traces).
                let _redacted = redact(&serde_json::to_value(&out.history).unwrap_or_default());
                tracing::info!(
                    iterations = out.iterations,
                    ops = out.ops,
                    progress_events = out.progress.len(),
                    "chat complete"
                );
                (
                    StatusCode::OK,
                    Json(serde_json::json!(ChatResponse {
                        final_text: out.final_text,
                        ops: out.ops,
                        iterations: out.iterations,
                        progress: out.progress,
                        request_id,
                    })),
                )
                    .into_response()
            }
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e})),
            )
                .into_response(),
        }
    }
    .instrument(span)
    .await
}

async fn tool_call(
    State(state): State<Arc<ApiState>>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let ctx = match ctx_from_headers(&headers, "tool") {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };
    if state.registry.get(&name).is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"tool": name, "category": "unknown_tool", "message": format!("unknown tool: {name}")})),
        )
            .into_response();
    }
    let args: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"tool": name, "category": "args", "message": format!("invalid args: {e}")})),
            )
                .into_response()
        }
    };
    match state.registry.dispatch(&state.tools, &ctx, &name, args) {
        Ok(value) => (
            StatusCode::OK,
            Json(serde_json::json!({"ok": true, "result": value})),
        )
            .into_response(),
        Err(e) => {
            let status = match e.category.as_str() {
                "args" => StatusCode::BAD_REQUEST,
                "unknown_tool" => StatusCode::NOT_FOUND,
                "auth" | "confirmation" | "grounding" => StatusCode::FORBIDDEN,
                "validation" => StatusCode::UNPROCESSABLE_ENTITY,
                _ => StatusCode::BAD_REQUEST,
            };
            (status, Json(serde_json::to_value(&e).unwrap_or_default())).into_response()
        }
    }
}

async fn reindex(State(state): State<Arc<ApiState>>, headers: HeaderMap) -> impl IntoResponse {
    let ctx = match ctx_from_headers(&headers, "reindex") {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };
    let span = request_span(&ctx.request_id, &ctx.principal, &ctx.tenant);
    async move {
        match state.tools.search.rebuild_from_repo(&*state.tools.repo) {
            Ok(n) => (StatusCode::OK, Json(serde_json::json!({"docs": n}))).into_response(),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response(),
        }
    }
    .instrument(span)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_principal_is_401() {
        let h = HeaderMap::new();
        let r = ctx_from_headers(&h, "s");
        assert!(r.is_err());
        let (status, _) = r.unwrap_err();
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
}
