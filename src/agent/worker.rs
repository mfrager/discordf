//! Serialized single-model worker (Phase 5).
//!
//! The `cactus-rs` engine is a process-wide singleton and is not safe for
//! concurrent conversations, so all inference goes through one dedicated
//! worker: a Tokio task owning the single `Box<dyn ModelAdapter>`.
//! Concurrent HTTP requests queue on an `mpsc` channel and execute
//! serially (`max_concurrency == 1` by construction). Between independent
//! sessions the worker calls `model.reset()` and the conversation log
//! supplies the relevant context again on the next turn.

use crate::agent::conversation::ConversationLog;
use crate::agent::{assemble_final_response, Agent, AgentResponse, ProgressEvent};
use crate::model::ModelAdapter;
use crate::policy::ExecutionContext;
use crate::tools::{ToolRegistry, ToolState};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};

/// Outcome returned to one chat caller.
#[derive(Debug, Clone)]
pub struct ChatOutcome {
    pub final_text: String,
    pub model_text: String,
    pub ops: usize,
    pub iterations: usize,
    pub progress: Vec<ProgressEvent>,
    pub history: Vec<(String, crate::agent::ToolResult)>,
}

/// Metrics proving serialization (observed by tests).
#[derive(Debug, Default)]
pub struct WorkerMetrics {
    pub current: AtomicUsize,
    pub max: AtomicUsize,
}

struct Job {
    session_id: String,
    user_text: String,
    ctx: ExecutionContext,
    progress_tx: Option<mpsc::UnboundedSender<ProgressEvent>>,
    reply: oneshot::Sender<Result<ChatOutcome, String>>,
}

struct WorkerState {
    model: Box<dyn ModelAdapter + Send>,
    agent: Agent,
    registry: Arc<ToolRegistry>,
    state: ToolState,
    log: Arc<ConversationLog>,
    metrics: Arc<WorkerMetrics>,
    last_session: Mutex<String>,
}

/// Cloneable handle used by the API layer.
#[derive(Clone)]
pub struct WorkerHandle {
    tx: mpsc::Sender<Job>,
    pub metrics: Arc<WorkerMetrics>,
}

impl WorkerHandle {
    /// Send one chat request and wait for the outcome.
    pub async fn chat(
        &self,
        session_id: &str,
        user_text: &str,
        ctx: ExecutionContext,
    ) -> Result<ChatOutcome, String> {
        self.chat_with_progress(session_id, user_text, ctx, None)
            .await
    }

    /// Same as [`chat`](Self::chat) but streams progress events to `progress_tx`.
    pub async fn chat_with_progress(
        &self,
        session_id: &str,
        user_text: &str,
        ctx: ExecutionContext,
        progress_tx: Option<mpsc::UnboundedSender<ProgressEvent>>,
    ) -> Result<ChatOutcome, String> {
        let (tx, rx) = oneshot::channel();
        let job = Job {
            session_id: session_id.to_string(),
            user_text: user_text.to_string(),
            ctx,
            progress_tx,
            reply: tx,
        };
        self.tx
            .send(job)
            .await
            .map_err(|e| format!("worker gone: {e}"))?;
        rx.await.map_err(|_| "worker dropped reply".to_string())?
    }
}

/// Spawn the worker task. Returns the handle plus the join handle (for
/// kill/restart chaos tests).
pub fn spawn_worker(
    model: Box<dyn ModelAdapter + Send>,
    agent: Agent,
    registry: Arc<ToolRegistry>,
    state: ToolState,
    log: Arc<ConversationLog>,
) -> (
    WorkerHandle,
    tokio::task::JoinHandle<()>,
    Arc<WorkerMetrics>,
) {
    spawn_worker_with_buffer(model, agent, registry, state, log, 64)
}

pub fn spawn_worker_with_buffer(
    model: Box<dyn ModelAdapter + Send>,
    agent: Agent,
    registry: Arc<ToolRegistry>,
    state: ToolState,
    log: Arc<ConversationLog>,
    buffer: usize,
) -> (
    WorkerHandle,
    tokio::task::JoinHandle<()>,
    Arc<WorkerMetrics>,
) {
    let metrics = Arc::new(WorkerMetrics::default());
    let (tx, rx) = mpsc::channel::<Job>(buffer);
    let worker = WorkerState {
        model,
        agent,
        registry,
        state,
        log,
        metrics: metrics.clone(),
        last_session: Mutex::new(String::new()),
    };
    let handle = tokio::task::spawn(worker_loop(worker, rx));
    (
        WorkerHandle {
            tx,
            metrics: metrics.clone(),
        },
        handle,
        metrics,
    )
}

async fn worker_loop(mut worker: WorkerState, mut rx: mpsc::Receiver<Job>) {
    while let Some(job) = rx.recv().await {
        // Serialization + concurrency accounting (max must stay 1).
        let cur = worker.metrics.current.fetch_add(1, Ordering::SeqCst) + 1;
        let mut max = worker.metrics.max.load(Ordering::SeqCst);
        while cur > max {
            match worker
                .metrics
                .max
                .compare_exchange(max, cur, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => break,
                Err(v) => max = v,
            }
        }

        let outcome = run_job(&mut worker, &job);
        worker.metrics.current.fetch_sub(1, Ordering::SeqCst);
        let _ = job.reply.send(outcome);
    }
}

fn run_job(worker: &mut WorkerState, job: &Job) -> Result<ChatOutcome, String> {
    // Reset between independent sessions so context never leaks across users.
    {
        let mut last = worker.last_session.lock().unwrap();
        if *last != job.session_id {
            worker.model.reset();
            *last = job.session_id.clone();
        }
    }

    let _ = worker
        .log
        .record_user_message(&job.session_id, &job.user_text);

    let mut progress: Vec<ProgressEvent> = Vec::new();
    let push = |progress: &mut Vec<ProgressEvent>, ev: ProgressEvent| {
        if let Some(tx) = &job.progress_tx {
            let _ = tx.send(ev.clone());
        }
        progress.push(ev);
    };

    push(
        &mut progress,
        ProgressEvent {
            seq: 0,
            iteration: 0,
            tool: "start".to_string(),
            call_id: String::new(),
            ok: true,
            message: format!("request {} started", job.ctx.request_id),
        },
    );

    // Collect agent progress events separately to avoid double-borrowing
    // `progress` inside the callback.
    let mut agent_events: Vec<ProgressEvent> = Vec::new();
    let resp: AgentResponse = worker.agent.run_request_with_progress(
        &mut *worker.model,
        &worker.registry,
        &worker.state,
        &job.ctx,
        &job.user_text,
        &mut |ev| {
            agent_events.push(ev);
        },
    );
    for ev in agent_events {
        let seq = progress.len() as u64;
        let mut ev = ev;
        ev.seq = seq;
        push(&mut progress, ev);
    }

    for (name, r) in &resp.history {
        let _ = worker.log.record_tool_call(
            &job.session_id,
            &r.call_id,
            name,
            serde_json::json!({"tool": name}),
        );
        let _ = worker.log.record_tool_result(
            &job.session_id,
            &r.call_id,
            name,
            r.ok,
            r.result.clone(),
        );
    }

    let assembled = assemble_final_response(&resp.final_text, &resp.history);
    let _ = worker
        .log
        .record_final(&job.session_id, &assembled, resp.ops, resp.iterations);

    let finish_seq = progress.len() as u64;
    push(
        &mut progress,
        ProgressEvent {
            seq: finish_seq,
            iteration: resp.iterations as u64,
            tool: "finish".to_string(),
            call_id: String::new(),
            ok: true,
            message: format!("done in {} iterations, {} ops", resp.iterations, resp.ops),
        },
    );

    Ok(ChatOutcome {
        final_text: assembled,
        model_text: resp.final_text,
        ops: resp.ops,
        iterations: resp.iterations,
        progress,
        history: resp.history,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppLimits;
    use crate::model::MockModel;

    fn test_state() -> (Arc<ToolRegistry>, ToolState) {
        let repo: Arc<dyn crate::rdf::RdfRepository> =
            Arc::new(crate::rdf::OxigraphRepository::new_in_memory(1000).unwrap());
        let search: Arc<dyn crate::search::SearchService> =
            Arc::new(crate::search::TantivyIndex::in_ram().unwrap());
        let reasoner = Arc::new(crate::ontology::OntologyReasoner::empty());
        let audit = Arc::new(crate::policy::AuditLog::in_memory());
        let state = ToolState::new(repo, search, reasoner, vec![], audit, AppLimits::default());
        (Arc::new(ToolRegistry::new()), state)
    }

    #[tokio::test]
    async fn sessions_reset_between_ids() {
        let (reg, state) = test_state();
        let log = Arc::new(ConversationLog::in_memory());
        let model: Box<dyn ModelAdapter + Send> = Box::new(MockModel::done("hi"));
        let (h, _j, _m) = spawn_worker(
            model,
            Agent::new(AppLimits::default()),
            reg,
            state,
            log.clone(),
        );
        let ctx = ExecutionContext::new("r1", "p", "t", vec![], vec![]);
        h.chat("s1", "hello", ctx.clone()).await.unwrap();
        h.chat("s2", "hello", ctx).await.unwrap();
        assert_eq!(log.session_ids(), vec!["s1", "s2"]);
    }
}
