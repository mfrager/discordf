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
  capped at 4092/call everywhere (B, C/D, bench live subsets, ladder)
  since 2026-11-27: the 1B model's reasoning fills whatever context it
  gets, so bigger budgets only bought longer waits (C moved empty→malformed
  at 16k, B/D unchanged). HTTP timeouts stay 1800 s so a full 4092-token
  trickle still lands.
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

## Testing plan Phase T1 — benchmark foundation (2026-11-27)

- New `src/bench/` module (no model calls): `dataset` (8-namespace
  ontology builder, tiny/standard instance tiers per §2.2 counts,
  adversarial pack with prompt-injection literals as data, 7 graph contexts,
  `bench_shapes`, snapshots + per-graph hashes for run isolation),
  `case` (100 fixed cases across the 12 categories — 10/10/10/10/8/12/8/8/8/
  6/6/4 — machine-readable JSON in `tests/bench_cases/`, regenerated with
  `UPDATE_BENCH_CASES=1`), `oracle` (plan-constraint checker with §5 alias
  mapping, URI-set precision/recall, graph-diff mutation accuracy =
  1−|△|/|∪| with separately counted false positives, `CaseScore` with
  read-only-envelope violation flag), `trace` (§10 event lifecycle +
  JSON), `metrics` (per-run + aggregate, §9 comparison table renderer).
- `tests/bench_t1_dataset.rs` (7 tests): tier shape/counts (incl. 1002
  contacts / 200 accounts / 50 events / 500 registrations / 200 geo at
  standard), snapshot isolation (equal builds hash equal, mutation
  diverges, non-target graph hash stable), case set loads/validates (unique
  ids, known ops, sound safety envelopes), oracle discriminates good/bad
  plans and graphs (incl. must-not-modify violation flag), trace/report
  smoke. Full `cargo test` green; zero clippy hits on bench code.

## Testing plan Phase T2 — planning-only comparison (2026-11-27)

- New `src/bench/runner.rs`: `Variant` (A/B/C/D) + `Ablation` (§13 four
  environments as structural tool-list variations), isolated `BenchFixture`
  (bench ontology + tiny instances + adversarial pack, read-only
  `ExecutionContext`), `synth_plan` (validator-clean plan synthesis from
  each case's `required_ops` with dependency-type-correct wiring;
  read-only `validate`/`verify` deliberately skipped to preserve schema
  validity and reported as completeness gaps), `ScriptedVariants` (same
  plan content through each real planner loop — A via `MockModel`
  add_step/finish_plan, B via scripted tool-turn backend, C via plan JSON,
  D via prose + plan JSON with retained artifacts), `run_planning_only`
  (plan → validate → constraint-check + zero-write gate via graph hash +
  audit count), `PlanningAggregate` + §9-style planning table + ablation
  matrix renderers, `RECORD=1` recording writer
  (`tests/recordings/t2/{a,b,c,d}/{case}.json`), keyless live-backend
  helper for the opt-in live subset.
- `tests/bench_t2_planning.rs` (5 tests + 1 ignored live): 100 cases × 4
  variants planning-only through the real planner loops; per-run zero-write
  assertion (hash + audit unchanged, audit stays empty); floors — schema
  validity ≥80% non-adversarial, dependency + termination ≥50% per variant
  (ratchet in T5). Observed keyless: schema 100% all variants (floors
  pass), completeness 86% (14 systematic gaps: OWL 0/8 + Recovery 0/6 —
  read-only `validate`/`verify` required-ops unsatisfiable per the
  validator's MutationResult-dependency rule, skipped by design),
  dependency/vocab/query-intent/termination 100%, zero writes 100%, avg
  3.9 steps. Ablation matrix 4×4 all `pass` (structural in scripted mode;
  live runs are where ablations diverge). Instrumentation: A no-duplicates
  + ≤16 steps, B model-id propagation + turn counts asserted per run, C
  JSON round-trip, D `nl_plan`/`structured_plan` artifacts +
  `check_prose_consistency`.
- Live subset (`t2_live_subset`, ignored): B/C/D against `OPENAI_BASE_URL`
  keyless for INT-001 + QRY-001; asserts scoring reached (no transport
  failure), model id non-empty, zero writes. Full `cargo test` green
  (lib 103 incl. 5 runner unit tests); no new clippy hits in bench code.

## Testing plan Phase T3 — full-execution trials (2026-11-27)

- Extended `src/bench/runner.rs` with the T3 execution section:
  `ExecFixture` (fresh isolated fixture per run × variant; `tenant-a/b`,
  `legacy`, ontology, adversarial pack; read caps raised to 2000 resources /
  20k triples so the 100-tier fits one query — safety invariants
  `LARGE_UPDATE_PREVIEW=25`, confirmation gate, authz, SHACL untouched),
  real typed query shapes `ExecQueryKind` (AllContacts/AllAccounts,
  OhioContacts/OhioAccounts, TrapKind, LegacyCustomers, MatchAllTyped,
  EmptyZZ, SingleContactName, ExactLegacySubject Eq-pinned), independent
  registry re-query (`independent_query_uris`) + `verify` count as oracles,
  `ExpectedMutation` (UpdateProbe add-only / DeleteSubjects full-resource /
  CreateProbe auto-URI / ReadOnly) + `exec_plan_for` (discovery spine →
  real query → mutation → validate → verify; read-only stops after query),
  `run_full_execution` (oracle change-set scoring: mutation accuracy,
  false-positive count, precision, recall; scope gate over all graphs;
  audit Denied/Committed; preview flag incl. on the error path; verify
  agreement; final-state proof hooks) + `ExecAggregate` §9-style execution
  table renderer, `uri_set_diff`, `ScriptedVariants::for_plan` (A/B/C/D
  plan → real planner loops → executor, no model bytes).
- `tests/bench_t3_execution.rs` (6 tests + 1 ignored live):
  17-case × 4-variant mutation battery (accuracy 1.0 + scope_ok every
  run), bulk tiers 0/1/10/100/over-limit (preview fires >25; exact-100 and
  empty-tier accuracy 1.0), poisoned scope (MatchAllTyped: update arm trips
  preview then the SHACL gate holds with ValidationFailed — detection +
  refusal with containment; delete arm refused without token), scenarios
  A–F × 4 variants (A multi-hop join → probe, B create-then-operate,
  C legacy migration, D read-only inconsistency diagnosis, E refused
  full-delete with ledger row, F frozen-IRI stale plan fails then live
  re-query recovers 1.0), scope-confinement matrix (cross-tenant read
  rejected; over-limit delete refused pre-write; trap deletes remove only
  trap subjects; committed/denied ledger assertions), oracle-shape checks
  (Eq-pinned/single-name shapes compile; verify counts agree post-commit),
  ignored live subset (bulk-1 + A→D one-variant E2E per variant vs real
  models, zero-write proof for the D read-only probe).
- Fixture findings (documented in-test, not harness bugs): T1 seed
  accounts/legacy subjects carry out-of-domain properties and
  ontology-graph-local status typings, so account/seed-subject-touching
  mutations trip staged SHACL — T3 mutations stay on SHACL-clean paths
  (contact name probes, Trap deletes, prop-less creates, Eq-pinned live
  subsets); T5 decision: whether to clean the fixture for account
  assignment paths. Pre-existing write-path policy confirmed: update
  dispatch is fail-fast with per-target commits (pinned by the poisoned
  arm); creates mint `urn:needle:gen` URIs (namespace check passes them).
- Full `cargo test` green (lib 103 + all integration suites incl. T3
  6 passed/1 ignored); zero clippy hits in the new T3 code.

## Testing plan Phase T4 — adversarial, faults, security (2026-11-27)

- New `src/bench/faults.rs`: §7.3 seven-point fault model (`FaultPoint`:
  before-discovery → post-commit-pre-ack), `FaultSchedule`/`FaultInjector`
  (transient N-failures-then-pass with attempt/failure/retry accounting),
  `ToolFault` + `apply_tool_fault` (deterministic JSON corruption:
  malformed/empty/truncated/stale), `arm_commit_failure`/
  `disarm_commit_failure` (real `set_inject_failure_at` storage faults),
  `AckLoss` (committed-but-unacked: re-check, never re-execute),
  `RecoveryReport` + `count_committed_from` (§6.3 metrics). 4 lib unit
  tests (injector N-fail/pass, 7-point matrix coverage, mutators, ack).
- `tests/bench_t4_adversarial.rs` (5 tests + 1 ignored live):
  fault matrix 7 points × 4 variants (first attempt fails through the
  REAL owning layer — unknown tool, empty describe, unknown predicate,
  unknown plan id, duplicated-id plan, injected commit failure with
  atomic rollback + ledger silence, lost ack with verify/re-query
  re-check; retry converges: exactly 1 retry, 0 duplicates, accuracy
  1.0, scope intact — 28/28 cells), injection battery (literal stored
  as data on all contacts × 4 with zero deletes; search returns the
  decoy as data; `DELETE WHERE` filter refused by query-safety while
  bare \"delete\" compiles as data matching 0 rows — both deterministic),
  dangerous battery (broad delete refused × 4 with exactly-one-Denied/
  zero-Committed ledger; wrong token refused; cross-tenant, ontology,
  evil-URI, out-of-namespace predicate all denied with zero writes),
  ledger precision (Denied/ValidationFailed/Storage-no-row/Committed
  mapped exactly), recovery metrics (per-variant transient-commit →
  retry: 1 retry, 0 unnecessary, 0 duplicates, 100% success tracking
  the 95% T5 target; duplicate-step loop trips Budget with zero writes;
  report counts agree with oracle), ignored live adversarial subset
  (planning-only containment vs real backends).
- Layer findings (documented in-test): `describe_vocabulary` degrades
  unknown IRIs to `known:false` (no error — empty-iris is the real
  post-vocab failure); query-safety blocks only SPARQL statement
  keywords, so prose \"delete\" is data by design; `ConfirmationRequired`
  ledgers as `Denied`; storage failures append no audit row.
- Full `cargo test` green (lib 107 + all suites incl. T4 5 passed/1
  ignored); zero clippy hits in new T4 code.

## Testing plan Phase T5 — scale, stability, decision (2026-11-27)

- New `src/bench/generator.rs`: Set 2 (500 cases, seed `0xC10C5EED…0001`,
  version 1) — programmatic variation over labels (synonym pools),
  namespaces, counts, relationship patterns, ambiguity markers, missing-data
  markers; round-robin over all 12 categories (41–42 each); determinism
  unit-tested (same seed reproduces, other seeds differ). Bulk JSON is
  git-ignored (`tests/bench_cases_generated/*` except `manifest.json`);
  the manifest (seed/count/version/FNV-1a content hash `bfe49421cfed45ce`)
  is checked in and round-trips through `load_cases_dir`. Regenerate with
  `GENERATE_SET2=1 cargo test --test bench_t5_regression t5_materialize_set2
  -- --ignored` (nightly runs the full 500 × 4; CI runs fixed-100 replay).
- New `src/bench/scale.rs`: stability (`StabilityStats`/`StabilityReport`,
  bit-identity over repeated plan bytes), operational metrics (§6.3:
  model/tool calls, tokens, planning/execution/total latency median + p95,
  replans, invalid calls, schema failures, peak context, cost-per-success at
  placeholder local rates), and the §9/§12 decision record (`decide`:
  winner = highest mean among safety-clean variants; violators never win).
  Ratcheted CI floors locked (`RegressionFloors`: schema 0.95 ≥ T2 0.80,
  dependency/termination 0.90 ≥ T2 0.50, mutation 0.99 = T3, scope 1.00 = T3,
  recovery 0.95 = T4 target).
- `tests/bench_t5_regression.rs` (5 tests + 2 ignored): Set-2 shape +
  manifest determinism (500 in-memory, 24-file writer round-trip), stability
  (stratified 24-case × 3 runs × 4 variants planning-only — bit-identical
  every cell, pooled distinct == 24/no drift), generated spot-check (12 gen
  cases × 4 variants: schema + zero-write green), locked floors (planning
  schema/dependency/termination + exec update/create/delete accuracy 1.00 +
  transient-commit → 1-retry convergence, all ≥ floors), §12 decision table
  (replay: 4-way tie 100% pass every row incl. three zeros — deterministic
  scripts, expected). Operational slice on the subset: task success 83%
  (same OWL/Recovery read-only gaps as T2), model calls C 1.0 < D 2.0 <
  A/B 4.7, tool calls C/D 0 vs A/B 3.7, replans/invalid/schema-failures 0.
- Decision (regression-tested, numbers above + live 1B-model evidence of no
  valid plan from any variant): replay correctness cannot discriminate (tie),
  operational cost favors C, live evidence disqualifies none but crowns none
  — production default RETAINED: B (tool loop; only loop that interleaves
  discovery with execution state: T3 multi-hop/discovery-alternating paths)
  with C as fallback (cheapest: 1 model call, 0 tool calls). Overturn when:
  (1) any safety violation (categorical disqualification), (2) a larger local
  model changes the live outcome class (re-run live subsets + re-score §12),
  (3) fixture SHACL dirt is cleaned so account-assignment paths validate
  (T3 battery means may shift). floors only ratchet upward; T5 locks them.
- Phase 2 spine-first (fixes/plan.md, 2026-11-27): classification-covered
  requests now plan via `SpinePlanner` (Rust owns the DAG, model fills
  scalar slots only; ladder backend `S`). Free-emission A/B/C/D loops are
  the fallback path for unmatched requests and spine failures (counted via
  `spine-miss` / `spine-failed` routing artifacts). Coverage baseline
  `cargo test --test phase2_spine` (keyless): 10/10 ladder rungs pinned to
  exact spine levels, bench   corpus 15/100 matched (query/lookup/resolve
  phrasings; terse bench requests like `Find all contacts` correctly
  unmatched, handled by fallback). Live S sweep CLOSED 10/10
  (all rungs spine path, zero fallbacks, 513 s wall); D/A routing to
  phase 2 stands (D: prose starvation, A: turn-2 truncation at 2048).
- Phase 3 compiler-default (fixes/plan.md, 2026-11-27): covered request
  classes compile with zero model calls via `CompilerPlanner`
  (`src/planner/compiler.rs`): classify → link every content word
  against the live ontology (same substring rule the vocab tool trusts)
  → fill slots → ground single-candidate mutation values (SHACL-required
  create props, domain/range-unique update predicate+objects, `?x rdf:type`
  query template) → phase-2 `assemble` → unchanged validator. Ambiguity
  routes to one bounded per-field model call (NeedsSlots → clarification,
  never a guessed plan); no-spine requests escalate loudly to the B loop
  (`compiler-escalated`, counted). Keyless E2E (`tests/phase3_compiler.rs`,
  10 passed): read paths execute to discovery outputs; create commits a
  new Contact; classify commits hasStatus on clean-room accounts; delete
  removes all contacts; contacts-classify stays loud at the unresolved
  predicate (no status property exists — correct). Corpus: 5 compiled +
  10 needs-slots = 15/100 owned (exactly the phase-2 spine floor; 85
  unmatched = accepted-llm with per-row fates). Steps-per-rung preserved
  (same spine shapes, 0 model turns on the happy path).
- Variant futures (phase-2 live evidence: S 10/10, B L1 clear, C L1 clear,
  D broken, A broken; compiler now owns the deterministic traffic):
  escalation fallback = B tool loop only (interleaves discovery with
  execution state). A/C/D retired from planning duty — A: Needle turn-2
  truncation on 1B (engine stays for weights research); C: superseded by
  the compiler happy path on its best cases, cannot interleave discovery;
  D: live prose starvation, superseded by template assembly. Code and
  tests stay; the supported escalation set is B alone.
- Nightly jobs (ignored locally, required in nightly):
  `GENERATE_SET2=1 cargo test --test bench_t5_regression t5_materialize_set2
  -- --ignored` (full 500 × 4 battery) and `RUN_MODEL_TESTS=1 cargo test
  --test bench_t5_regression t5_live_nightly_subset -- --ignored
  --test-threads=1` (generated cases vs real keyless backends) plus the T2–T4
  live subsets, `--features live-model` A run, and `RUN_MODEL_TESTS=1 cargo
  test --test phase3_compiler live_compiler_ohio_e2e -- --ignored
  --test-threads=1` (deterministic create leg + live disambiguation leg).
- Full `cargo test` green (lib 113 + all suites incl. T5 5 passed/2 ignored);
  zero clippy hits in new T5 code (generator/scale/T5 test).
