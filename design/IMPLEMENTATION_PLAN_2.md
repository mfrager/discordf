# Implementation Plan 2 — Contextual, Ontology-Aware RDF Agent with Needle + agentstream LLMs (5 Phases)

Source designs: `design/DESIGN_UPDATE_1.md`, `design/DESIGN_UPDATE_2.md`, `design/DESIGN_UPDATE_3.md`
Prior plan: `design/IMPLEMENTATION_PLAN.md` (7-phase CRUD baseline — already implemented: `RdfRepository` on Oxigraph, 5 tools, Tantivy hybrid search, Needle worker, Axum API)
Stack delta for this plan: `agentstream` (path `~/Scripts/agentstream`) for **all non-Needle LLMs** + `dotenvy` for `.env` + existing `cactus-rs` (Needle 3 only) + `oxigraph` + `tantivy` + `serde/serde_json` + `tokio` + `axum` + `tracing`

> Architectural invariants (carried from Plan 1, enforced in every phase below):
> 1. One RDF repository trait, one tool registry, Oxigraph authoritative, Tantivy disposable/rebuildable.
> 2. Needle 3 is a tool-calling controller only; the generic LLM (via `agentstream`) is a **planner** only. All deterministic graph semantics live in Rust.
> 3. **No raw SPARQL from any model.** Models emit typed graph patterns; Rust compiles to SPARQL.
> 4. Rust owns the `Plan` and the `OperationContext`; models propose, Rust validates/applies.
> 5. Mutations only via the executor (authz → SHACL → dry-run/confirm → commit → audit → verify).

## How non-Needle LLMs are wired (applies to all phases)

- `agentstream` is a **path dependency** (local checkout, not crates.io):
  ```toml
  [dependencies]
  agentstream = { path = "/home/mfrager/Scripts/agentstream", features = ["openai-backend", "tool-registry"] }
  dotenvy = "0.15"
  ```
  Pin the exact `agentstream` commit in `Cargo.lock` and record it in `OPERATIONS.md`. Feature `tool-registry` is required for planner Options B/C/D tool definitions; `openai-backend` for the OpenAI-compatible local endpoint. Do **not** enable `dspy`/`embedded-log` unless a phase explicitly needs it.
- All OpenAI-compatible config goes through `dotenvy` + `OPENAI_*` vars:
  - Call `let _ = dotenvy::dotenv();` once at startup (and in tests via a shared helper) **before** any `std::env::var("OPENAI_*")`.
  - `OPENAI_BASE_URL` — e.g. current `.env` value `http://localhost:8080`. Normalize: trim trailing `/`; if the value does not end in `/v1`, append `/v1` (agentstream posts to `{base}/chat/completions`). Document this normalization.
  - `OPENAI_MODEL` — the model id to send in every non-Needle request (e.g. local `minicpm5-1b` build). Resolved by a new `src/llm/mod.rs::resolve_model()`: `dotenvy` → `OPENAI_MODEL` → trim → if empty, fall back to `"default-local"` + `tracing::warn!` (never panic; evaluation harness asserts which model was actually used).
  - `OPENAI_API_KEY` — **optional for local endpoints.** New `src/llm/mod.rs::resolve_backend()` logic (do not call `BackendFactory::openai_from_env` directly because it errors when the key is missing):
    1. Load `.env` via `dotenvy`.
    2. Read `OPENAI_BASE_URL` (default `http://localhost:8080/v1`), `OPENAI_MODEL` (via `resolve_model`), `OPENAI_API_KEY` (may be missing/empty).
    3. If key is missing/empty **and** base URL host is `localhost`/`127.0.0.1`/`10.*`/`192.168.*`/`*.local`/unix socket → use placeholder key `"local-no-key"` and proceed (most local OpenAI-compatible servers ignore the bearer token). Log `key_mode="absent-local"` (never log the key itself).
    4. If key is missing/empty **and** base URL is `https://api.openai.com/*` (or any non-local host) → return a typed `LlmError::MissingKey` with remediation text.
    5. Build with `BackendFactory::openai_explicit(key, Some(base_url), Some(model), timeout)` or `OpenAiBackend::new(...)` directly.
  - `AGENTSTREAM_TIMEOUT` / `OPENAI_TIMEOUT` honored; default 120 s.
- Needle 3 stays on `cactus-rs` behind `ModelAdapter::complete()` (Option A only). It **never** goes through `agentstream`. The generic LLM (whatever `OPENAI_MODEL` points at) **always** goes through `agentstream::backends::LLMBackend` + `agentstream::agent::AgentStream`. No other HTTP client for LLM calls.

---

## Phase 1 — Local LLM runtime: dotenvy + keyless agentstream backend

**Goal:** `agentstream` talks to the locally-hosted OpenAI-compatible endpoint with **no API key required**, model id sourced from `OPENAI_MODEL`. Needle path untouched.

### Tasks
1. `Cargo.toml`: add `agentstream` (path + features above) + `dotenvy = "0.15"`. Keep `cactus-rs` optional under `live-model`.
2. New `src/llm/mod.rs`:
   - `LlmConfig { base_url, model, key_mode: KeyPresent | AbsentLocal, timeout_secs }` + `LlmError`.
   - `load_llm_config() -> Result<LlmConfig, LlmError>`: `dotenvy::dotenv()` → read `OPENAI_BASE_URL` / `OPENAI_MODEL` / `OPENAI_API_KEY` per rules above; normalize base URL (`/v1` suffix, strip trailing `/`); `OPENAI_MODEL` empty → fallback + warn.
   - `build_backend(config) -> Arc<dyn LLMBackend>` via `BackendFactory::openai_explicit`.
   - `build_agent(backend, model, system, max_tokens) -> AgentStream` thin wrapper setting `AgentConfig::new(model, max_tokens)`.
   - `is_local_url(url)`: host allowlist check used by the key-absent path.
3. Wire into `src/main.rs` + `src/config.rs`: extend `AppConfig` with `llm: LlmConfig` (loaded after `dotenvy`); pass `Option<Arc<dyn LLMBackend>>` into planner construction (Phase 4). When the local server is down, startup still succeeds (planners report `LlmUnavailable`; mock backend used in tests).
4. Document in `README`/`OPERATIONS.md`: required `.env` shape (`OPENAI_BASE_URL=http://localhost:8080`, `OPENAI_MODEL=<local-id>`, `OPENAI_API_KEY` optional/unset for local), plus how to point at hosted OpenAI later (set key + `https://api.openai.com/v1`).

### Tests
- **Unit — dotenvy load:** write temp dir `.env` with `OPENAI_BASE_URL`, `OPENAI_MODEL`, no key → `load_llm_config()` returns `AbsentLocal`, model correct, base normalized to `…/v1`.
- **Unit — key gating:** local host + no key → ok (placeholder); `api.openai.com` + no key → `MissingKey` error; key present → `KeyPresent` regardless of host.
- **Unit — model fallback:** unset/blank `OPENAI_MODEL` → `"default-local"` + no panic.
- **Integration — keyless round-trip:** against `agentstream::dev-utils` mock OpenAI server (or `wiremock`-style stub) with no key configured → `AgentStream::send("hi")` succeeds and `model_from_env` equivalent returns the stub model.
- **Integration — startup without key:** boot binary with the repo's real `.env` (only `OPENAI_BASE_URL`) → server starts, `/health` 200, no `OPENAI_API_KEY is not set` crash.

### Acceptance criteria
- [ ] Binary starts with `.env` containing only `OPENAI_BASE_URL` (the checked-in state).
- [ ] No code path calls `openai_from_env` (which requires a key); all non-Needle paths go through `load_llm_config` + `openai_explicit`.
- [ ] `OPENAI_MODEL` is the single source of truth for the non-Needle model id.

---

## Phase 2 — Contextual operations foundation: GraphContext, split search, canonical Plan

**Goal:** Implement DESIGN_UPDATE_1 §§2–3 + DESIGN_UPDATE_3 §1: first-class context, vocabulary/instance split, and the shared canonical plan DAG that all four planners target.

### Tasks
1. `src/plan/mod.rs` (new, planner-independent):
   - `GraphContext { dataset, graphs: Vec<GraphName>, namespaces: HashMap<Prefix,Iri>, ontology_refs: Vec<Iri>, inference: InferenceMode, tenant, tx: Option<TxId> }` — replaces free-text graph scoping in tool args. All RDF tools accept it with per-call overrides.
   - `Plan { goal, context: GraphContext, steps: Vec<PlanStep>, assumptions: Vec<String> }`, `PlanStep { id: StepId (s1…), operation: Operation, intent: String, args: StepArguments (tagged union), depends_on: Vec<StepId>, output: OutputType }`.
   - `StepArguments` variants: `SearchVocab{terms, namespaces}`, `DescribeVocab{iris}`, `SearchInstances{types?, text?, namespace?}`, `DescribeResource{uris}`, `Query{pattern: QueryPattern}`, `Create/Update/Delete{…typed refs…}`, `Validate`, `Verify{question}`.
   - `TypedValue`: `Iri | Blank | Literal{value,datatype,lang} | Unresolved | StepRef{step, field} | UriSetRef`. No bare strings where an IRI is meant — this is what stops hallucinated URIs (UPDATE_2 §4, UPDATE_3 §4.2).
   - `OperationContext`: `{ request, graph_context, namespaces, discovered: VocabCache, results: HashMap<StepId, StepOutput>, pending: Vec<StepId>, validation_errors, tx_state }`. Owned and mutated only by Rust.
2. Tool registry evolution (keep old 5 tools working; add read-only discovery):
   - Split `search` → `search_vocabulary` (OWL classes/properties/restrictions, `OntologyService`) + `search_instances` (real resources, `SearchService` + repo). Keep legacy `search` as a deprecated fan-out for Plan-1 E2E compat.
   - Add `describe_vocabulary { iris }` (domain/range/label/comment/superclass) and `describe_resource { uris }` (thin read wrapper) for the planning loop.
   - Add `validate { target }` (SHACL) and `verify { question / check }` stubs backed by existing `ontology/shacl.rs` + repo counts; full executor wiring in Phase 5.
   - Every handler takes `&GraphContext`; enforce namespace isolation + graph allowlist at dispatch (reuse `policy/authz.rs`).
3. `src/plan/validator.rs`: structural (schema/required fields) + dependency (DAG, no cycles, refs resolve, producer-output → consumer-input type match) + context (graphs/dataset authorized) + operational (mutations have discovery deps; every mutation plan ends in `validate` + `verify`) checks. Same validator for all planners.
4. `OperationContext` prompt renderer: compact status summary (`goal, steps done/pending, resolved vocab, result counts, remaining requirements`) used by planners in Phase 4 — never dump full history.

### Tests
- **Unit — plan DAG:** valid Ohio-contacts plan (s1 vocab → s2 describe → s3 query → s4 instance search → s5 update → s6 validate → s7 verify) passes; cycle / dangling ref / type-mismatch (update consuming `VocabularyCandidates` as `UriSet`) rejected with field-level errors.
- **Unit — split search:** `search_vocabulary("contact")` returns `crm:Contact` class def, never instances; `search_instances("Alice")` returns instance URIs, never `owl:Class` defs; regression test that conflation is impossible via output-type mismatch.
- **Unit — context enforcement:** query/create outside `allowed_graphs` denied; vocabulary search scoped to unlisted namespace returns empty + `scope-limited` flag.
- **Snapshot:** golden JSON for the canonical Ohio plan + validator error payloads.

### Acceptance criteria
- [ ] All four future planners compile against `plan::{Plan, PlanStep, StepArguments}` with no planner-specific schema forks.
- [ ] No mutation tool accepts a raw string where a `StepRef`/`UriSet` is required.
- [ ] Legacy 5-tool E2E (Plan 1) still green with the deprecated `search` shim.

---

## Phase 3 — Safe typed query subsystem (no model-written SPARQL)

**Goal:** Implement DESIGN_UPDATE_1 §4 + DESIGN_UPDATE_2 §§2–4: models propose **typed patterns**, Rust compiles/validates/executes SPARQL against Oxigraph.

### Tasks
1. `src/query/mod.rs` (new):
   - `QueryPattern { operation: Select|Ask|Construct, graphs: Vec<GraphName>, patterns: Vec<TriplePattern{subject: TermPattern, predicate: IriOrVar, object: ValuePattern}>, filters: Vec<FilterOp{Eq, Contains, Regex, Gt/Lt, LangMatch}>, return_vars: Vec<Var>, limit }`. Variables `?contact`-style; IRIs validated against `OntologyService` + `describe_vocabulary` cache.
   - `build_query(pattern) -> QueryPlanId` (validate + store in `OperationContext`), `inspect_query(id) -> { sparql, inferred_constraints, explanation, match_estimate }`, `execute_query(id) -> { uris, rows, count, truncated }`. Expose exactly these three tools to planners (never a raw `query { sparql }` tool).
   - Compiler: `QueryPattern` → parameterized SPARQL 1.1 `SELECT/ASK/CONSTRUCT` with `GRAPH` scoping; allowlist: no `UPDATE`/`DELETE`/`INSERT`/`LOAD`/`CLEAR` strings accepted; reject unbound predicates, unknown IRIs, cross-graph escapes.
2. Rust enforcement table (UPDATE_2 — implement every row or fail the phase):
   IRI validity (every class/predicate resolves) · property compatibility (subject/object types vs domain/range; warn not coerce on `"Ohio"` vs `"OH"` vs geo-URI vs SKOS concept) · literal correctness (datatype/lang match) · namespace isolation · variable/pattern/join validity · safety (timeout, `LIMIT` clamp to `max_resources_per_read`, traversal-depth cap).
3. Literal-representation discovery helper: `inspect_query` must surface the actual stored representation (sample values) so the planner can refine the pattern instead of assuming `"Ohio"`.
4. Wire `execute_query` results back into `OperationContext.results[QueryPlanId]` as `ContactUriSet`-style typed outputs consumable via `StepRef`.

### Tests
- **Unit — compile:** Ohio-contacts JSON pattern → expected SPARQL with `GRAPH <crm:production>`, `LIMIT 50`, no string-concat injection.
- **Unit — rejection matrix:** unknown predicate IRI; domain/range mismatch (`crm:account` on non-Contact); literal `"Ohio"` vs stored `"OH"` (returns 0 + representation hint, not silent success); cross-graph `GRAPH` escape; `SPARQL UPDATE` smuggled in filter → rejected as `query-safety`.
- **Unit — three-tool flow:** `build → inspect → execute` round-trip on seeded fixture; `inspect` explanation mentions matched constraints; `execute` twice is deterministic; oversized result sets set `truncated:true`.
- **Integration — representation trap:** seed accounts with state as URI + others with literal; planner-visible `inspect` output forces explicit representation choice; assert no test assumes `"Ohio"` literal.

### Acceptance criteria
- [ ] No tool accepts free-text SPARQL; `grep -r "sparql" src/tools` shows only the compiler module.
- [ ] Every `execute_query` enforces IRI/domain/range/namespace/timeout/result-cap gates.
- [ ] Ohio example from UPDATE_2 runs end-to-end via typed patterns (no hand-written SPARQL in test code except the expected-output assertion).

---

## Phase 4 — Unified Planner trait: Needle sequential + three agentstream planners

**Goal:** Implement DESIGN_UPDATE_3 §§2–5 behind one trait. Option A stays on Needle/`complete()`; Options B/C/D run on the Phase-1 `agentstream` backend using `OPENAI_MODEL`.

```rust
#[async_trait]
trait Planner: Send + Sync {
    async fn create_plan(&self, request: &UserRequest, ctx: &mut OperationContext) -> Result<Plan, PlannerError>;
}
struct NeedleSequentialPlanner;   // A: cactus-rs complete() loop, planning tools only
struct AgentToolPlanner;          // B: agentstream tool-loop (generic LLM, was MiniCPM5-1B)
struct AgentStructuredPlanner;    // C: agentstream single JSON structured output
struct AgentTwoPassPlanner;       // D: agentstream NL plan → structured extraction
```

### Tasks
1. Planning-tool surface (shared by A + B; thin wrappers that mutate only the in-memory `Plan`, never the repo):
   `add_step | revise_step | remove_step | inspect_plan | finish_plan | request_clarification` (+ read-only `search_vocabulary/describe_vocabulary/search_instances/describe_resource/inspect_query` during planning). No `create/update/delete/execute_query` in the planning loop — mutations execute only in Phase 5.
   - A: Needle schemas = grammar-constrained JSON (existing `ToolDef` style); loop = `complete()` → validate → apply → re-render compact status prompt → repeat until `finish_plan` or caps (`max_plan_steps=16`, duplicate/no-progress detection, `finish_plan` still passes through `plan_validator`).
   - B: same tools as `agentstream::tools::Tool` definitions sent via `AgentStream::send(...).with_tools(tools)`; parse `AgentResponse.tool_calls`; enforce explicit `depends_on` (never infer); handle multi-call responses order-sensitively.
2. C (direct structured output): single `AgentStream::send` with `with_schema(plan_schema_json, Some("operational_plan"))`; parse `parsed_data`/text JSON into `Plan`; bounded repair loop (≤2 retries with validator error feedback).
3. D (two-pass): pass 1 = unconstrained NL plan (`"number each step, state deps, flag unknown representations"`); pass 2 = `with_schema` extraction with `"encode, do not improve"` instruction; consistency check (every prose step has a structured step; deps preserved) + retain both artifacts for audit.
4. Prompt builders shared across B/C/D: `system = role + ops + rules + GraphContext summary`; per-turn context = compact `OperationContext` rendering (Phase 2.4), not full history. All three use `resolve_model()` id in `BackendRequest.model` so evaluation logs prove which local model ran.
5. Python-worker pattern from UPDATE_3 §2.4 is **not** needed: Needle stays in-process via `cactus-rs`; generic LLM stays out-of-process behind its local HTTP endpoint via `agentstream`. Document why (Rust owns loop; `complete()` not `run()` for A; `AgentStream` turn loop for B).

### Tests (mock `LLMBackend`, no weights required)
- **Unit — A loop:** scripted Needle `add_step×3 → finish_plan` builds the Ohio DAG; premature `finish_plan` rejected by validator; duplicate-step + infinite-loop guards trip.
- **Unit — B loop:** stub agentstream backend returning `search_vocabulary` then `add_step`s → plan gains discovery-grounded deps; missing-dep update rejected even when both calls arrive in one response.
- **Unit — C/D parsing:** malformed JSON → typed `Schema` error + repair retry; D extractor dropping the instance-search step → `Consistency` error naming the omitted prose step.
- **Integration — backend parity:** same prompt through mock backend asserts `OPENAI_MODEL` id propagated and `agentstream` tool schemas validate.

### Acceptance criteria
- [ ] `create_plan` signature identical for all four structs; executor cannot tell which planner produced a `Plan`.
- [ ] Planning loops never mutate the repo (assert via read-only backend spy: zero `create/update/delete/execute` calls pre-`finish_plan`).
- [ ] B/C/D work with the keyless local backend from Phase 1 (CI runs them against the mock server with no `OPENAI_API_KEY` set).

---

## Phase 5 — Executor, dynamic replanning, mutation safety & comparative evaluation

**Goal:** Execute any planner's DAG safely (UPDATE_1 §§5–8, UPDATE_3 §§7–9) and crown a production default by measuring, not assuming.

### Tasks
1. `src/executor/mod.rs`: topological scheduler over `Plan.steps`; resolve `StepRef`s from `OperationContext.results`; per-step dispatch to `ToolRegistry`/`query`/`ontology`; retain completed outputs immutable; on discovery surprise (e.g. address-resource instead of direct `crm:state`) pause, re-invoke the same `Planner::create_plan` with updated `OperationContext` to revise **pending** steps only (replanning); abort + report when repair exceeds 2 rounds.
2. Mutation safety (reuse Phase-2 policy from Plan 1): `validate → authorize → dry-run/preview → confirm-if-destructive → commit (Oxigraph tx) → audit → verify`. `verify` re-queries the graph independently (model's "done" is never proof). Preview URI sets for large updates; require explicit token for `delete`/predicate-wipe/graph-clear.
3. Evaluation harness `tests/eval_100/` (the UPDATE_3 §8 set, versioned fixtures): simple search, ambiguous search, multi-pattern queries, vocab creation, multi-resource updates, discovery-alternating complex requests. Metrics per planner (A/B/C/D, same fixtures, same engine): plan completeness, dep correctness, vocab accuracy, query-intent accuracy, schema validity, replanning success, latency, model calls, **final RDF state correctness** (not plan aesthetics).
4. Order per UPDATE_3 §8: implement/measure C first (baseline), then B (likely production if tool-call reliability holds), then D (only if prose→structure beats C), then A (Needle cost/latency baseline). Record recall@5 lexical/vector/hybrid + accuracy floors in test output; regressions fail CI. Live-model runs gated behind `RUN_MODEL_TESTS=1` (real `OPENAI_MODEL` endpoint) with `RECORDED=1` playback for keyless CI.
5. Release gate: default planner selected by eval numbers; `README` documents the winner + when to use the others; explicit v1 non-goals asserted by tests (no autonomous planning loops in executor, no model SPARQL, no model-edited ontology graphs — ontology-hash-unchanged test).

### Tests / benchmarks
- **E2E — Ohio complex:** `classify Ohio-account contacts as prospects` via each planner → same final triples + `validate` + `verify count`; B/C/D run keyless against local endpoint or recorded playback.
- **E2E — replan:** fixture where location is an address resource → executor pauses, replans, adds address-resolution step, completes; completed steps untouched.
- **E2E — safety:** SHACL-violating / unauthorized / unconfirmed-destructive plans → denied with structured errors, graph hash unchanged, one audit row each (`validation-failed|denied|confirmation-required`).
- **Load:** 10 parallel chats serialize correctly (single Needle instance; agentstream planners concurrency-safe); kill-mid-commit → clean failure, no phantom `committed` audit.
- **Metrics gate:** aggregate accuracy printed + asserted above floor (initial floor: tool/plan-schema validity ≥80%; raise after tuning); any accuracy drop fails CI.

### Acceptance criteria (release gate)
- [ ] `cargo test` + `RUN_MODEL_TESTS=1` eval + `RECORDED=1` eval all green without `OPENAI_API_KEY` set.
- [ ] Eval table names the production planner (expected: B with C as fallback, per UPDATE_3 §9 — overturn only with numbers).
- [ ] Final release demonstrates: grounded vocab discovery → typed query → scoped mutation → validate → verify, with transactions, authz, audit, and keyless local LLM operation.

---

## Test-strategy summary

| Type | Phases | Examples |
|---|---|---|
| Unit | 1–5 | dotenvy/key-gating/model-fallback; DAG validation; split-search; SPARQL compile + rejection matrix; planner loops; executor topological order |
| Integration | 1,3,4,5 | keyless mock-server round-trip; startup with real `.env`; inspect→execute determinism; backend parity; replan on address-resource |
| Snapshot/golden | 2,3 | canonical Ohio `Plan` JSON; validator errors; compiled SPARQL + explanations |
| Safety/property | 2,3,5 | fuzz patterns/shapes (validator never panics); cross-graph escape attempts; destructive-gate matrix |
| Benchmark/accuracy | 5 | 100-request set × 4 planners; recall@5; latency/calls; final-RDF-state correctness |
| E2E | 5 | Ohio complex × 4 planners; replan; SHACL/unauth/unconfirmed denials; 10-way concurrency; ontology-hash non-goals |

Run: `cargo test` → `cargo test --test e2e` → `RECORDED=1 cargo test` (keyless CI) → `RUN_MODEL_TESTS=1 cargo test -- --include-ignored` (live local `OPENAI_MODEL`) + eval accuracy report.
