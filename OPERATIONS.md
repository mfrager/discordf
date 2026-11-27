# Operations — Plan 2 Phase 1 LLM runtime

## Required `.env` shape (keyless local default)

```env
OPENAI_BASE_URL=http://localhost:8080
OPENAI_MODEL=minicpm5-1b
# OPENAI_API_KEY intentionally unset for local endpoints.
```

- `dotenvy` loads `.env` once at startup (`load_llm_config()` calls
  `dotenvy::dotenv()` before any `std::env::var("OPENAI_*")`).
- `OPENAI_BASE_URL` is normalized: trailing `/` stripped, `/v1` appended when
  missing (`http://localhost:8080` → `http://localhost:8080/v1`), because
  `agentstream` posts to `{base}/chat/completions`.
- `OPENAI_MODEL` is the single source of truth for every non-Needle request.
  Unset/blank → `default-local` + `tracing::warn!` (never panics).
- `OPENAI_API_KEY` is **optional for local endpoints**. Hosts treated as local:
  `localhost`, `127.*`, `::1`, `10.*`, `172.16–31.*`, `192.168.*`, `*.local`,
  `unix:` sockets. Missing key + local host → placeholder bearer
  `local-no-key` (`KeyMode::AbsentLocal`). Missing key + remote host
  (e.g. `https://api.openai.com/v1`) → typed `LlmError::MissingKey`.
- Timeouts: `AGENTSTREAM_TIMEOUT` (preferred) or `OPENAI_TIMEOUT`, seconds.
  Default `120`.

## Pointing at hosted OpenAI later

```env
OPENAI_BASE_URL=https://api.openai.com/v1
OPENAI_MODEL=gpt-4o-mini
OPENAI_API_KEY=sk-...
```

No code change needed; `KeyMode::KeyPresent` is used automatically.

## agentstream pin

- Path dependency: `agentstream = { path = "/home/mfrager/Scripts/agentstream",
  features = ["openai-backend", "tool-registry"] }`.
- Pinned commit: `7c9d53cd0df98cc21ba94fcc59f6d52de43b7cc4`
  (record here on each bump; `Cargo.lock` is authoritative).
- Never call `BackendFactory::openai_from_env` (it requires a key). All
  non-Needle paths go through `crate::llm::load_llm_config` +
  `BackendFactory::openai_explicit`.
- Needle 3 stays on `cactus-rs` (`ModelAdapter::complete()`); it never goes
  through `agentstream`.

## Startup behavior

- Binary starts with only `OPENAI_BASE_URL` set; `/health` returns 200 with no
  `OPENAI_API_KEY is not set` crash.
- Backend construction performs no I/O: a down local server does not prevent
  startup. Planner calls (Phase 4) surface transport errors per request.
- Startup log line: `LLM runtime: base_url=… model=… key_mode=… timeout_secs=…`
  (key never logged; `LlmConfig`'s `Debug` redacts it).

## Phase 4 planners

- Unified trait `planner::Planner::create_plan(&UserRequest, &mut
  OperationContext) -> Result<Plan, PlannerError>` — identical for all four
  structs; the executor cannot tell which planner produced a `Plan`.
- A `NeedleSequentialPlanner`: Needle 3 via `cactus-rs`, sequential
  `complete()` loop (`MockModel` in tests, live Needle behind `live-model`).
- B `AgentToolPlanner`, C `AgentStructuredPlanner`, D `AgentTwoPassPlanner`:
  generic LLM via the Phase-1 `agentstream` backend, model id always
  `OPENAI_MODEL` (single source of truth; `AgentBackend::from_config`).
- Planning-tool surface (A + B): `add_step | revise_step | remove_step |
  inspect_plan | finish_plan | request_clarification` plus read-only
  `search_vocabulary | describe_vocabulary | search_instances |
  describe_resource | inspect_query`. `create/update/delete/execute_query/
  build_query` (and legacy `search`/`read`) are rejected at the planning
  boundary — planning loops never mutate the repo (spy-asserted in tests).
- Caps: `max_plan_steps = 16`, `max_turns = 32`, no-progress abort after 3
  stagnant turns; C/D repair loops bounded at 2 retries with validator
  feedback.
- No Python worker (plan §5): Rust owns every loop — `complete()` (never
  `run()`) for A, `AgentStream` turn loop with `clear_history()` +
  re-rendered compact `OperationContext` status per turn for B. Needle stays
  in-process; the generic LLM stays out-of-process over HTTP.
- Keyless CI: `tests/plan4_planners.rs` runs B/C/D against a stub
  OpenAI-compatible endpoint with no `OPENAI_API_KEY` set and asserts the
  configured model id propagates on every request.

## Phase 5 executor + evaluation

- `executor::Executor::execute(&plan, &mut OperationContext, planner)` runs
  any planner's DAG: topological scheduler over `depends_on`, `StepRef` /
  `UriSetRef` resolution from `OperationContext.results`, per-step dispatch
  to `ToolRegistry` (discovery + guarded mutations) and the typed query
  tools (`build_query` → `execute_query`). Completed outputs are immutable.
- Discovery surprise (unresolved value, empty query/instance result,
  ungrounded ref) pauses execution and re-invokes the same
  `Planner::create_plan` on pending steps only — at most `max_replans = 2`
  rounds, then `ReplanExhausted`. No autonomous planning loops: the happy
  path makes zero planner callbacks (asserted in tests).
- Mutation safety: mutations flow through the Phase-2 guarded path
  (authz → destructive gate → SHACL → commit → audit) via the tool
  handlers; `verify` re-queries the graph independently. Target sets above
  25 URIs are recorded as preview artifacts before commit. `delete` (any
  removal) requires a `confirm-`-prefixed token, else `ConfirmationRequired`.
  The ontology graph is never writable through the executor (denied).
- No model SPARQL: the only query input is the typed `QueryPattern` object;
  raw `sparql`/`insert`/`delete` fields are rejected at the executor boundary
  (the query module enforces the rest).
- E2E (`tests/exec_phase5.rs`, keyless, scripted planners — no weights):
  Ohio complex commits + validates + verifies with identical final triples
  across planner identities; replan revises pending steps while completed
  outputs stay untouched; SHACL/unauth/unconfirmed plans are denied with the
  graph hash unchanged and exactly one audit row each
  (`ValidationFailed`/`Denied`); ontology hash unchanged; 10 parallel chats
  serialize; injected mid-commit failure leaves no phantom `Committed` audit.
- Metrics gate: versioned eval fixtures v1 (6 cases covering the UPDATE_3 §8
  categories: simple/ambiguous search, multi-pattern query, vocab creation,
  multi-resource update, discovery-alternating) print an aggregate table and
  assert schema/deps/final-state rates ≥ 0.80. Production default per
  UPDATE_3 §9: B (tool loop) with C as fallback — all planners share this
  executor path, so the default rests on planner-loop reliability, overturn
  only with eval numbers.

## Phase 5 real-backend results (2026-11-27)

- Endpoint: `http://localhost:8080` (`MiniCPM5-1B-F16`, keyless,
  `AbsentLocal`). Server verified to support chat + `tool_calls` +
  `response_format: json_schema`; the model always emits a long
  `reasoning_content` trace, so single-shot planners need large `max_tokens`
  (1024/turn for B, 2048 for C/D) and still starve on a 1B budget.
- `tests/real_backends.rs`: B/C/D run live (`RUN_MODEL_TESTS=1 ... --
  --ignored`) through the real `agentstream` HTTP backend, keyless. A
  recording proxy (`RECORD=1`) saves raw response bodies to
  `tests/recordings/{b,c,d}.json`; default `cargo test` replays them through
  a stub (deterministic CI, no server). Regenerate recordings when the
  model/endpoint changes; replay tests pin the recorded outcome class.
- `tests/live_planner_a.rs` (`--features live-model`): Needle 3 with real
  cached weights, engine built on the **planning surface**
  (`planner_system_prompt` + `planning_tool_defs`) — the production wiring
  for A. The five RDF `tool_defs` belong to the executor path only.
- Observed (honest model-quality findings, all transport keyless and clean,
  model id asserted on every request):
  - B: real `add_step` tool calls parsed/applied; model repeats the same
    invalid call (empty `args`) despite validation feedback → typed `Budget`
    stop after 3 stagnant turns. Loop mechanics proven; 1B model cannot
    self-correct.
  - C: 3/3 calls burn the whole budget on reasoning, empty `content` →
    typed `Schema` after repairs.
  - D: pass-1 empty (reasoning fills budget) → typed `Model` error at once.
  - A: real Needle engine, 10-turn cap, typed `Budget` stop in ~18 s.
- Live tests assert no-transport-failure + model-id propagation, and join
  planner→executor→final-state automatically whenever a model returns a
  valid plan (none did on this 1B build). Replay tests pin the three typed
  outcomes deterministically. Overturn by re-running live against a larger
  local model — no code changes needed (model id stays `OPENAI_MODEL`).
