//! Ladder CLI: run ONE rung of the planning ladder against live models.
//!
//! ```text
//! RUN_MODEL_TESTS=1 cargo run --features live-model --bin ladder_cli -- \
//!     --rung 1 --backend B --backend C
//! ```
//!
//! - `--rung N` (default 1): which ladder rung to run (1–10).
//! - `--backend X` (repeatable, default all): which planner type to run
//!   (`A` = Needle weights via `cactus-rs`, needs `--features live-model`;
//!   `B`/`C`/`D` = local OpenAI-compatible server;
//!   `S` = spine-first (Rust owns the shape, model fills scalar slots)).
//! - `--log-dir DIR` (default `/tmp`): where the per-backend logs go.
//!
//! One log file per backend, printed to the console: the **stream log**
//! (`ladder_stream_r{R}_{B}.log`) — model output tee'd token-by-token into
//! a text file as it arrives over SSE, so it can be watched live with
//! `tail -f` while a rung runs.
//!
//! On the agentstream logs path: agentstream only writes its own JSONL
//! event log with the `embedded-log` feature, which does not compile
//! against our `openai-backend`+`tool-registry` set (upstream `ext.rs`
//! type errors, verified). So this build records nothing inside
//! agentstream — the CLI prints that status line instead of a bogus path,
//! and the stream log (turn headers + token flow + footers) is the
//! tail-able record.
//!
//! Token budget is capped at 4092 per call everywhere (the 1B model's
//! reasoning fills whatever context it gets; bigger budgets only buy waits).

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};

use async_trait::async_trait;
use tokio::sync::mpsc;

const LIVE_MAX_TOKENS: u32 = 4092;
const LIVE_TIMEOUT_SECS: &str = "1800";

fn usage() -> ! {
    eprintln!(
        "usage: ladder_cli [--rung N] [--backend A|B|C|D]... [--log-dir DIR] [--base-url URL] [--model ID]"
    );
    eprintln!("  defaults: --rung 1, all backends, --log-dir /tmp");
    std::process::exit(1);
}

struct Args {
    rung: u8,
    backends: Vec<char>,
    log_dir: PathBuf,
    base_url: Option<String>,
    model: Option<String>,
}

fn parse_args() -> Args {
    let mut rung: u8 = 1;
    let mut backends: Vec<char> = vec![];
    let mut log_dir = PathBuf::from("/tmp");
    let mut base_url: Option<String> = None;
    let mut model: Option<String> = None;
    let mut it = std::env::args().skip(1).peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--rung" => {
                rung = it
                    .next()
                    .unwrap_or_else(|| usage())
                    .parse()
                    .unwrap_or_else(|_| usage());
            }
            "--backend" => {
                let b = it.next().unwrap_or_else(|| usage());
                for part in b.split(',') {
                    match part.trim().to_ascii_uppercase().as_str() {
                        "A" | "B" | "C" | "D" | "S" => {
                            backends.push(part.trim().to_ascii_uppercase().chars().next().unwrap())
                        }
                        _ => usage(),
                    }
                }
            }
            "--log-dir" => log_dir = PathBuf::from(it.next().unwrap_or_else(|| usage())),
            "--base-url" => base_url = Some(it.next().unwrap_or_else(|| usage())),
            "--model" => model = Some(it.next().unwrap_or_else(|| usage())),
            _ => usage(),
        }
    }
    if !(1..=10).contains(&rung) {
        eprintln!("rung must be 1..=10");
        std::process::exit(1);
    }
    if backends.is_empty() {
        backends = vec!['A', 'B', 'C', 'D', 'S'];
    }
    // De-dupe, keep A/B/C/D/S order.
    let set: HashSet<char> = backends.into_iter().collect();
    let backends: Vec<char> = ['A', 'B', 'C', 'D', 'S'].into_iter().filter(|b| set.contains(b)).collect();
    Args { rung, backends, log_dir, base_url, model }
}

// ---------------------------------------------------------------------------
// Streaming tee: forces the SSE path so tokens hit the file as they arrive,
// with a non-streaming fallback if the server refuses streaming.
// ---------------------------------------------------------------------------

struct TeeBackend {
    inner: Arc<dyn agentstream::backends::LLMBackend>,
    file: Arc<Mutex<File>>,
    turn: AtomicU64,
    label: String,
}

impl TeeBackend {
    fn header(&self) -> String {
        format!("ladder_cli {} turn", self.label)
    }

    fn append(&self, text: &str) {
        if let Ok(mut f) = self.file.lock() {
            let _ = f.write_all(text.as_bytes());
            let _ = f.flush();
        }
    }

    async fn drain_to_file(
        file: Arc<Mutex<File>>,
        mut rx: mpsc::Receiver<agentstream::backends::BackendChunk>,
    ) {
        while let Some(chunk) = rx.recv().await {
            let mut line = String::new();
            if let Some(t) = chunk.text {
                line.push_str(&t);
            }
            if let Some(th) = chunk.thinking {
                if !line.is_empty() && !th.is_empty() {
                    line.push('\n');
                }
                if !th.is_empty() {
                    line.push_str("[thinking] ");
                    line.push_str(&th);
                }
            }
            if let Some(calls) = chunk.tool_calls {
                line.push_str("\n[tool_calls] ");
                for c in &calls {
                    line.push_str(&format!("{}({}) ", c.name, c.input));
                }
                line.push('\n');
            }
            if chunk.is_final {
                line.push_str("\n[final]\n");
            }
            if !line.is_empty() {
                if let Ok(mut f) = file.lock() {
                    let _ = f.write_all(line.as_bytes());
                    let _ = f.flush();
                }
            }
        }
    }
}

#[async_trait]
impl agentstream::backends::LLMBackend for TeeBackend {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn default_model(&self) -> Option<&str> {
        self.inner.default_model()
    }

    fn supports_output_schema(&self) -> bool {
        self.inner.supports_output_schema()
    }

    fn handles_native_tool_messages(&self) -> bool {
        self.inner.handles_native_tool_messages()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn complete(
        &self,
        request: agentstream::backends::BackendRequest,
        abort: Option<agentstream::backends::AbortSignal>,
    ) -> Result<agentstream::backends::BackendResponse, String> {
        use agentstream::backends::BackendRequest;
        let n = self.turn.fetch_add(1, Ordering::SeqCst) + 1;
        let last_user = request.messages.last().map(|m| {
            m.get("content")
                .and_then(|c| c.as_str())
                .unwrap_or("?")
                .chars()
                .take(160)
                .collect::<String>()
        });
        self.append(&format!(
            "\n===== {} #{n} model={} max_tokens={} tools={} schema={} =====\nuser: {}\n----- stream -----\n",
            self.header(),
            request.model,
            request.max_tokens,
            request.tools.len(),
            request.output_schema.is_some(),
            last_user.as_deref().unwrap_or("?"),
        ));
        // Streaming-first: tokens land in the file as SSE deltas arrive.
        let mut streamed = request.clone();
        streamed.stream = true;
        let (tx, rx) = mpsc::channel(1024);
        let writer = tokio::spawn(Self::drain_to_file(self.file.clone(), rx));
        match self.inner.stream_complete(streamed, tx, abort).await {
            Ok(resp) => {
                let _ = writer.await;
                self.append(&format!(
                    "\n----- end turn #{n}: finish={:?} usage={:?} tool_calls={} text_len={} thinking_len={} -----\n",
                    resp.finish_reason,
                    resp.usage,
                    resp.tool_calls.len(),
                    resp.text.len(),
                    resp.thinking.as_ref().map(|t| t.len()).unwrap_or(0),
                ));
                Ok(resp)
            }
            Err(e) => {
                writer.abort();
                self.append(&format!("\n[stream path failed: {e}; falling back to non-streaming]\n"));
                let mut plain: BackendRequest = request;
                plain.stream = false;
                let resp = self.inner.complete(plain, None).await?;
                self.append(&format!(
                    "[fallback full text ({} chars)]\n{}\n[fallback thinking ({} chars)]\n{}\n----- end turn #{n} (fallback) -----\n",
                    resp.text.len(),
                    resp.text,
                    resp.thinking.as_ref().map(|t| t.len()).unwrap_or(0),
                    resp.thinking.as_deref().unwrap_or(""),
                ));
                Ok(resp)
            }
        }
    }

    async fn stream_complete(
        &self,
        request: agentstream::backends::BackendRequest,
        chunk_tx: mpsc::Sender<agentstream::backends::BackendChunk>,
        abort: Option<agentstream::backends::AbortSignal>,
    ) -> Result<agentstream::backends::BackendResponse, String> {
        let (inner_tx, mut inner_rx) = mpsc::channel(1024);
        let file = self.file.clone();
        let label = self.label.clone();
        let fwd = tokio::spawn(async move {
            while let Some(chunk) = inner_rx.recv().await {
                Self::drain_to_file_one(&file, &label, &chunk);
                if chunk_tx.send(chunk).await.is_err() {
                    break;
                }
            }
        });
        let resp = self.inner.stream_complete(request, inner_tx, abort).await;
        let _ = fwd.await;
        resp
    }
}

impl TeeBackend {
    fn drain_to_file_one(
        file: &Arc<Mutex<File>>,
        label: &str,
        chunk: &agentstream::backends::BackendChunk,
    ) {
        let mut line = String::new();
        if let Some(t) = &chunk.text {
            line.push_str(t);
        }
        if let Some(th) = &chunk.thinking {
            if !th.is_empty() {
                line.push_str("[thinking] ");
                line.push_str(th);
            }
        }
        if let Some(calls) = &chunk.tool_calls {
            line.push_str("\n[tool_calls] ");
            for c in calls {
                line.push_str(&format!("{}({}) ", c.name, c.input));
            }
            line.push('\n');
        }
        if chunk.is_final {
            line.push_str(&format!("\n[{label} final]\n"));
        }
        if !line.is_empty() {
            if let Ok(mut f) = file.lock() {
                let _ = f.write_all(line.as_bytes());
                let _ = f.flush();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Fixtures + runners
// ---------------------------------------------------------------------------

use needle_rdf_agent::config::AppLimits;
use needle_rdf_agent::ladder_spec::{ladder, Rung};
use needle_rdf_agent::llm::{build_backend, config_from_vars, KeyMode};
use needle_rdf_agent::ontology::{contact_shapes, load_default_ontology, OntologyReasoner};
use needle_rdf_agent::plan::{validate_plan, GraphContext, OperationContext, Plan};
use needle_rdf_agent::planner::{
    AgentBackend, AgentStructuredPlanner, AgentToolPlanner, AgentTwoPassPlanner, Planner,
    PlannerError, PlanningServices, UserRequest,
};
use needle_rdf_agent::policy::{AuditLog, ExecutionContext, Operation as PolicyOp};
use needle_rdf_agent::rdf::{GraphName, OxigraphRepository};
use needle_rdf_agent::tools::{ToolRegistry, ToolState};

fn services() -> (PlanningServices, GraphContext) {
    let repo = Arc::new(OxigraphRepository::new_in_memory(50_000).unwrap());
    load_default_ontology(&*repo).unwrap();
    let reasoner = Arc::new(OntologyReasoner::from_repository(&*repo).unwrap());
    let search = Arc::new(needle_rdf_agent::search::TantivyIndex::in_ram().unwrap());
    let audit = Arc::new(AuditLog::in_memory());
    let state = ToolState::new(
        repo,
        search,
        reasoner,
        contact_shapes(),
        audit,
        AppLimits::default(),
    );
    let graph = GraphName::tenant("acme").unwrap();
    let exec_ctx = ExecutionContext::new(
        "req-ladder-cli",
        "alice",
        "acme",
        vec![graph.clone(), GraphName::ontology()],
        vec![
            PolicyOp::Read,
            PolicyOp::Create,
            PolicyOp::Update,
            PolicyOp::Delete,
        ],
    );
    let graph_ctx = GraphContext {
        graphs: vec![graph],
        namespaces: [("ex".to_string(), "http://example.com/".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    (
        PlanningServices::new(Arc::new(ToolRegistry::new()), state, exec_ctx),
        graph_ctx,
    )
}

fn is_transport(msg: &str) -> bool {
    ["timeout", "timed out", "connection", "hyper", "reqwest", "transport", "dns", "TcpStream", "pool"]
        .iter()
        .any(|m| msg.contains(m))
}

fn outcome_class(e: &PlannerError) -> String {
    let s = format!("{e:?}");
    for class in ["Budget", "Schema", "Model", "Consistency", "Clarification", "Plan"] {
        if s.contains(class) {
            return class.to_string();
        }
    }
    "Other".to_string()
}

fn check_result(rung: &Rung, result: Result<Plan, PlannerError>) -> Result<(), String> {
    match result {
        Ok(plan) => match validate_plan(&plan) {
            Ok(()) if plan.steps.len() == rung.steps.len() => Ok(()),
            // Over-plan counter (Phase 1, item 5): validating but longer
            // than the rung — measurement, not success.
            Ok(()) if plan.steps.len() > rung.steps.len() => Err(format!(
                "OverPlan({} steps, want {})",
                plan.steps.len(),
                rung.steps.len()
            )),
            Ok(()) => Err(format!(
                "UnderPlan({} steps, want {})",
                plan.steps.len(),
                rung.steps.len()
            )),
            Err(errs) => Err(format!("InvalidPlan({})", errs[0])),
        },
        Err(e) => {
            let msg = format!("{e:?}");
            if is_transport(&msg) {
                eprintln!("TRANSPORT FAILURE (channel must be clean): {e:?}");
                std::process::exit(2);
            }
            Err(outcome_class(&e))
        }
    }
}

fn open_log(path: &PathBuf) -> File {
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .unwrap_or_else(|e| {
            eprintln!("cannot open log file {}: {e}", path.display());
            std::process::exit(1);
        })
}

fn live_backend(base_url: &str, model: &str, tee: TeeBackend) -> AgentBackend {
    std::env::remove_var("OPENAI_API_KEY");
    let cfg = config_from_vars(Some(base_url), Some(model), None, Some(LIVE_TIMEOUT_SECS))
        .expect("keyless live CLI config");
    assert_eq!(cfg.key_mode, KeyMode::AbsentLocal, "live runs are keyless");
    AgentBackend::new(Arc::new(tee), cfg.model.clone()).with_max_tokens(LIVE_MAX_TOKENS)
}

async fn run_bcd(rung: &Rung, which: char, args: &Args, base: &str, model: &str) {
    let tag = format!("rung{}-{which}", rung.level);
    let stream_path = args.log_dir.join(format!("ladder_stream_{tag}.log"));
    let tee = TeeBackend {
        inner: build_backend_for(base, model),
        file: Arc::new(Mutex::new(open_log(&stream_path))),
        turn: AtomicU64::new(0),
        label: tag.clone(),
    };
    // Seed the stream file so `tail -f` has something immediately.
    tee.append(&format!("ladder_cli {tag}: backend {which}, model {model}, max_tokens {LIVE_MAX_TOKENS}\n"));
    println!("agentstream event log: disabled (embedded-log does not compile upstream; see header)");
    println!("stream log (tail -f):  {}", stream_path.display());

    let request_text = rung.request.to_string();
    let outcome = match which {
        'B' => {
            let (planning, graph_ctx) = services();
            let backend = live_backend(base, model, tee);
            let planner = AgentToolPlanner::new(backend, planning);
            let request = UserRequest::new(request_text);
            let mut ctx = OperationContext::new(request.text.clone(), graph_ctx);
            check_result(rung, planner.create_plan(&request, &mut ctx).await)
        }
        'C' => {
            let (_planning, graph_ctx) = services();
            let backend = live_backend(base, model, tee);
            let planner = AgentStructuredPlanner::new(backend);
            let request = UserRequest::new(request_text);
            let mut ctx = OperationContext::new(request.text.clone(), graph_ctx);
            check_result(rung, planner.create_plan(&request, &mut ctx).await)
        }
        'D' => {
            let (_planning, graph_ctx) = services();
            let backend = live_backend(base, model, tee);
            let planner = AgentTwoPassPlanner::new(backend);
            let request = UserRequest::new(request_text);
            let mut ctx = OperationContext::new(request.text.clone(), graph_ctx);
            check_result(rung, planner.create_plan(&request, &mut ctx).await)
        }
        _ => unreachable!(),
    };
    match outcome {
        Ok(()) => println!("LADDER CLI rung{} {which}: CLEARED", rung.level),
        Err(class) => println!("LADDER CLI rung{} {which}: broke ({class})", rung.level),
    }
}

async fn run_s(rung: &Rung, args: &Args, base: &str, model: &str) {
    use needle_rdf_agent::planner::spine::SpinePlanner;
    let tag = format!("rung{}-S", rung.level);
    let stream_path = args.log_dir.join(format!("ladder_stream_{tag}.log"));
    let tee = TeeBackend {
        inner: build_backend_for(base, model),
        file: Arc::new(Mutex::new(open_log(&stream_path))),
        turn: AtomicU64::new(0),
        label: tag.clone(),
    };
    tee.append(&format!("ladder_cli {tag}: backend S (spine-first), model {model}, max_tokens {LIVE_MAX_TOKENS}\n"));
    println!("agentstream event log: disabled (embedded-log does not compile upstream; see header)");
    println!("stream log (tail -f):  {}", stream_path.display());
    let (planning, graph_ctx) = services();
    let backend = live_backend(base, model, tee);
    let planner = SpinePlanner::new(backend, planning);
    let request = UserRequest::new(rung.request);
    let mut ctx = OperationContext::new(request.text.clone(), graph_ctx);
    let outcome = check_result(rung, planner.create_plan(&request, &mut ctx).await);
    let routing = ctx.artifacts.get("routing").cloned().unwrap_or_default();
    match outcome {
        Ok(()) => println!("LADDER CLI rung{} S: CLEARED ({routing})", rung.level),
        Err(class) => println!("LADDER CLI rung{} S: broke ({class}, {routing})", rung.level),
    }
}

fn build_backend_for(base: &str, model: &str) -> Arc<dyn agentstream::backends::LLMBackend> {
    std::env::remove_var("OPENAI_API_KEY");
    let cfg = config_from_vars(Some(base), Some(model), None, Some(LIVE_TIMEOUT_SECS))
        .expect("keyless live CLI config");
    build_backend(&cfg)
}

// ---------------------------------------------------------------------------
// A: Needle live path (append-only per-turn log; no agentstream involved)
// ---------------------------------------------------------------------------

#[cfg(feature = "live-model")]
mod needle_path {
    use super::*;
    use needle_rdf_agent::model::{LiveEngine, LiveModelHandle, ModelAdapter, ModelCompletion, ModelError};
    use needle_rdf_agent::planner::{planner_system_prompt, planning_tool_defs, NeedleSequentialPlanner, PlanCaps};

    struct LogAdapter {
        inner: LiveModelHandle,
        file: Arc<Mutex<File>>,
    }

    impl LogAdapter {
        fn append(&self, text: &str) {
            if let Ok(mut f) = self.file.lock() {
                let _ = f.write_all(text.as_bytes());
                let _ = f.flush();
            }
        }
    }

    impl ModelAdapter for LogAdapter {
        fn complete(&mut self, user_text: &str) -> Result<ModelCompletion, ModelError> {
            let preview: String = user_text.chars().take(240).collect();
            self.append(&format!("\n===== needle turn =====\nuser: {preview}\n----- output -----\n"));
            let out = self.inner.complete(user_text);
            match &out {
                Ok(c) => self.append(&format!("{c:?}\n----- end turn -----\n")),
                Err(e) => self.append(&format!("[engine error] {e:?}\n----- end turn -----\n")),
            }
            out
        }

        fn submit_tool_results(
            &mut self,
            results: &[needle_rdf_agent::agent::ToolResult],
        ) -> Result<(), ModelError> {
            self.inner.submit_tool_results(results)
        }

        fn embed(&mut self, text: &str) -> Result<Vec<f32>, ModelError> {
            self.inner.embed(text)
        }

        fn reset(&mut self) {
            self.inner.reset();
        }
    }

    pub async fn run_a(rung: &Rung, args: &Args) {
        let tag = format!("rung{}-A", rung.level);
        let stream_path = args.log_dir.join(format!("ladder_stream_{tag}.log"));
        let file = Arc::new(Mutex::new(open_log(&stream_path)));
        println!("agentstream event log: n/a (Needle path uses no agentstream backend)");
        println!("stream log (tail -f):  {}", stream_path.display());
        {
            let mut f = file.lock().unwrap();
            let _ = writeln!(f, "ladder_cli {tag}: backend A (Needle live weights)");
        }

        let weights = match LiveEngine::resolve_weights() {
            Ok(w) => w,
            Err(e) => {
                println!("LADDER CLI rung{} A: SKIPPED (no weights: {e:?})", rung.level);
                return;
            }
        };
        let (planning, graph_ctx) = services();
        let engine = match LiveEngine::build(
            weights,
            &planner_system_prompt(&graph_ctx),
            planning_tool_defs(),
        ) {
            Ok(e) => e,
            Err(e) => {
                println!("LADDER CLI rung{} A: SKIPPED (engine build: {e:?})", rung.level);
                return;
            }
        };
        let adapter = LogAdapter { inner: LiveModelHandle::new(engine), file };
        let planner = NeedleSequentialPlanner::new(Box::new(adapter), planning)
            .with_caps(PlanCaps { max_plan_steps: 16, max_turns: 10, max_no_progress: 3 });
        let request = UserRequest::new(rung.request);
        let mut ctx = OperationContext::new(request.text.clone(), graph_ctx);
        // Needle runs on a blocking engine: keep it off the async runtime.
        let result = tokio::task::block_in_place(|| planner.create_plan_sync(&request, &mut ctx));
        match check_result(rung, result) {
            Ok(()) => println!("LADDER CLI rung{} A: CLEARED", rung.level),
            Err(class) => println!("LADDER CLI rung{} A: broke ({class})", rung.level),
        }
    }
}

#[tokio::main]
async fn main() {
    if !std::env::var("RUN_MODEL_TESTS").map(|v| v == "1").unwrap_or(false) {
        eprintln!("ladder_cli runs live models: set RUN_MODEL_TESTS=1");
        std::process::exit(1);
    }
    let args = parse_args();
    let rung: Rung = ladder()
        .into_iter()
        .find(|r| r.level == args.rung)
        .expect("rung 1..=10");
    println!(
        "ladder_cli: rung L{} ({} steps): {}",
        rung.level,
        rung.steps.len(),
        rung.request
    );

    let base = args
        .base_url
        .clone()
        .or_else(|| std::env::var("OPENAI_BASE_URL").ok())
        .unwrap_or_else(|| "http://localhost:8080".to_string());
    let model = args
        .model
        .clone()
        .or_else(|| std::env::var("OPENAI_MODEL").ok())
        .unwrap_or_else(|| "MiniCPM5-1B-F16".to_string());

    // B/C/D/S first (cheap HTTP), Needle last (process-global engine slot).
    for which in args.backends.clone() {
        match which {
            'B' | 'C' | 'D' => run_bcd(&rung, which, &args, &base, &model).await,
            'S' => run_s(&rung, &args, &base, &model).await,
            'A' => {
                #[cfg(feature = "live-model")]
                {
                    needle_path::run_a(&rung, &args).await;
                }
                #[cfg(not(feature = "live-model"))]
                {
                    println!(
                        "LADDER CLI rung{} A: SKIPPED (rebuild with --features live-model)",
                        rung.level
                    );
                }
            }
            _ => unreachable!(),
        }
    }
}
