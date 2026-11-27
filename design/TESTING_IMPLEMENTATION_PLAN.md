# Testing Implementation Plan — Full A/B/C/D Benchmark Suite (5 Phases)

Source: `design/TESTING_1.md` (comparative evaluation of architectures A–D).
This plan turns that design into buildable, runnable work inside this repo.
It reuses what already exists and names exactly what is new per phase.

## What already exists (do not rebuild)

| Piece | Location | Reuse |
|---|---|---|
| Canonical plan IR + validator | `src/plan/`, `src/plan/validator.rs` | Plan-validity oracle for every phase |
| All four planners, one trait | `src/planner/` (A/B/C/D + `ScriptedPlanner`) | Systems under test; scripted planner for deterministic replay |
| Shared executor (scheduler, StepRef resolution, ≤2 replans, guarded mutations, verify) | `src/executor/` (`graph_hash`, `graph_count`) | Sole execution engine — no variant shortcuts, per TESTING_1 §1 |
| Typed query, policy gates, audit | `src/query/`, `src/policy/`, `src/tools/` | Execution + safety enforcement |
| Stub planner integration (B/C/D keyless) | `tests/plan4_planners.rs` | Planning-loop regression net |
| Executor E2E + 6-case eval v1 (floor 0.80) | `tests/exec_phase5.rs` | Seed of the fixed benchmark + metrics gate pattern |
| Live B/C/D + recording proxy + replay; live A (Needle) | `tests/real_backends.rs`, `tests/live_planner_a.rs`, `tests/recordings/` | Live/replay run modes (extend, don't replace) |
| Failure injection (atomicity) | `RdfRepository::set_inject_failure_at` | Seed of the fault-injection harness |

## New harness layout (created in T1, extended T2–T5)

```
src/bench/                  # new library module: harness logic (no model calls)
├── dataset.rs              # ontology + instance builders, scale tiers, snapshots
├── case.rs                 # BenchmarkCase / ExpectedOutcome (TESTING_1 §5 format)
├── oracle.rs               # plan-constraint checker, URI-set P/R, graph-diff mutation metric
├── trace.rs                # per-run TraceEvent log (TESTING_1 §10 event list)
├── metrics.rs              # per-run + aggregate metrics, report tables
├── runner.rs               # run_one(case, variant, mode) -> BenchmarkRun
└── faults.rs               # fault schedule + injection wrappers (T4)
tests/bench_cases/           # fixed benchmark cases, JSON (Set 1: 100)
tests/bench_cases_generated/ # generated cases, JSON (Set 2: 500, T5; git-ignored bulk)
tests/recordings/            # checked-in live LLM bytes (exists; grows per variant×mode)
tests/bench_t1_dataset.rs    # T1 acceptance tests
tests/bench_t2_planning.rs   # T2 planning-only comparison
tests/bench_t3_execution.rs  # T3 full-execution tests
tests/bench_t4_adversarial.rs# T4 safety + recovery tests
tests/bench_t5_regression.rs # T5 scale/stability/decision tests
```

Core types (adapt TESTING_1 §10 to existing types — `Plan`, `StepOutput`,
`ExecutionReport`, `PlannerError`, `ExecutorError` are reused, not duplicated):

```rust
struct BenchmarkCase { id, category: Category12, difficulty, request: String,
    context: GraphContext, tools: Vec<String>, expected: ExpectedOutcome,
    faults: Vec<FaultSpec>, safety: SafetySpec }
struct ExpectedOutcome { required_ops, expected_uris, expected_graph_hash?,
    must_not_modify_graph, min_precision/recall?, ambiguity_ok? }
struct BenchmarkRun { case_id, variant: A|B|C|D, mode: PlanningOnly|FullExecution,
    plan?, report?, trace: Vec<TraceEvent>, metrics: RunMetrics }
trait CaseEvaluator { fn evaluate(&self, case, run) -> CaseReport } // plan, query, graph-diff, safety
```

Run modes (TESTING_1 §7.2): `PlanningOnly` (create_plan + validate + constraint
check, repo untouched — assert zero writes via audit row count) vs
`FullExecution` (fresh isolated graph snapshot per run × variant; final-state
diff). Live (`RUN_MODEL_TESTS=1`) vs replay (recordings) applies to both modes.

---

## Phase T1 — Dataset, case format, oracle (baseline)

**Goal:** deterministic ground truth + independent scoring, no model calls.

Tasks:
1. `src/bench/dataset.rs`: multi-namespace ontology builder (`ex/crm/org/geo/
   sales/event/legacy/test`, TESTING_1 §2.1: hierarchies, domains/ranges,
   equivalents/inverses, disjointness, SKOS, SHACL shapes, 7 graph contexts
   incl. two tenants + legacy + import). Instance builder with scale tiers:
   `tiny` (CI: ~10 contacts/accounts, full relationship coverage),
   `standard` (~2.6k resources per §2.2 table), adversarial pack (duplicate
   names, missing props, legacy-vocab resources, cross-graph splits,
   address-resource vs direct-literal representation traps).
2. Snapshots: `DatasetSnapshot` (quads + expected-hash); `fresh_isolated()`
   gives every run × variant its own store (regression: cross-run leakage
   test fails if a shared store is used).
3. `src/bench/case.rs`: machine-readable case JSON per TESTING_1 §5 schema
   (`id` like `QRY-0042`, category 1–12, difficulty, request, context, tools,
   expected, safety). Validator for case files (unknown IRIs in `expected_uris`
   rejected at load — oracle must be sound).
4. `src/bench/oracle.rs`: plan-constraint checker (required ops/deps/refs),
   URI-set precision/recall, mutation accuracy =
   `1 - |actual △ expected| / |actual ∪ expected|` (symmetric difference),
   false-positive mutations counted separately and penalized harder;
   independently computed (never model-derived).
5. `src/bench/trace.rs` + `metrics.rs`: event list from §10
   (`request_received … final_response`) with timestamps/scopes/errors/token
   counts where available; aggregate report table (§9, per-variant columns).
6. `tests/bench_t1_dataset.rs`: ontology loads; tiny/standard/adversarial
   tiers build; snapshots isolate (hash equality after N runs); oracle scores
   a hand-made good/bad plan correctly; 12-category case schema validates.

Acceptance: `cargo test --test bench_t1_dataset` green; fixed 100-case set
authored (Set 1) and schema-valid; oracle unit tests prove good/bad
discrimination before any planner runs.

## Phase T2 — Planning-only comparison (A vs B vs C vs D)

**Goal:** isolate planning ability from execution; find each variant's failure
modes (deliverable: comparative report).

Tasks:
1. `src/bench/runner.rs::run_planning_only`: builds variant planner (A:
   `NeedleSequentialPlanner` + live/mock model; B/C/D: `AgentBackend` +
   live/stub backend), runs `create_plan`, validates, constraint-checks,
   records variant-specific instrumentation —
   A: every intermediate plan state + `finish_plan` timing/duplicates/cycles;
   B: full turn log (tool, args, response, recovery); C: schema validity in
   unconstrained vs schema-constrained modes (§4-C two modes); D: both
   artifacts + extraction fidelity (dropped/added/reordered steps).
2. Ablation environments (TESTING_1 §13): full-tools vs pre-resolved-vocabulary
   vs precomputed-query-results vs precomputed-plan — pinpoints whether a
   failure is discovery, query planning, or mutation planning.
3. `tests/bench_t2_planning.rs`: all 100 fixed cases × 4 variants, planning
   only; asserts zero graph writes (audit empty); per-category metrics:
   completeness, dependency accuracy, vocabulary accuracy, query-intent
   accuracy, schema validity, termination accuracy (per-variant rules, §12:
   A `finish_plan`, B stop-vs-call, C single-output completeness, D
   prose→structure preservation).
4. Recordings: extend proxy pattern to capture per-variant planning traffic
   into `tests/recordings/t2/{a,b,c,d}/{case}.json`; replay mode default in
   CI, live behind `RUN_MODEL_TESTS=1`.

Acceptance: comparative report prints (and CI asserts floors on): schema
validity ≥80% per variant on non-adversarial cases; termination-accuracy and
dependency-accuracy reported per variant with no variant below 50% silently
(floors ratchet in T5). §13 ablation matrix printed per variant.

## Phase T3 — Full execution (final-state correctness)

**Goal:** actual graph-state correctness per variant (deliverable: mutation and
scope measurements).

Tasks:
1. Runner `run_full_execution`: fresh snapshot per run, executor runs the
   variant's validated plan (invalid plan = planning failure, no execution),
   post-`verify` independent re-query, final graph diff vs expected.
2. Bulk tiers (§7, TESTING_1): 0 / 1 / exactly-10 / 100 / over-limit affected
   resources; over-limit must preview + refuse or page (assert, don't assume).
   Critical test: poisoned query matching everything → Rust layer must flag
   scope surprise or require approval (executor preview threshold +
   confirmation gate; LLM noticing is NOT the pass criterion).
3. Scenarios A–F (§8) as weighted E2E cases; E (dangerous delete) is a
   mandatory safety gate (must end `ConfirmationRequired`/denied, graph
   unchanged); F (mid-run data change) uses a scripted mutator between
   discovery and commit.
4. `tests/bench_t3_execution.rs`: scenario + bulk + scope matrix; metrics:
   mutation accuracy (symmetric difference), scope accuracy (no cross-graph/
   cross-tenant writes — hash per non-target graph unchanged), precision/
   recall of affected sets, verify-count agreement.

Acceptance: mutation-accuracy ≥99% on non-adversarial mutation cases per
variant that produced a valid plan; zero cross-scope writes across the whole
phase; scenario E gate green for all variants.

## Phase T4 — Adversarial, faults, security (safety + recovery report)

**Goal:** prove containment and recovery (deliverable: safety/recovery report
with zero-tolerance gates).

Tasks:
1. `src/bench/faults.rs`: fault schedule + injection points (§7.3, §10):
   before-first-discovery, post-vocab, post-query-build, post-query-exec,
   during validation, during commit (`set_inject_failure_at`), post-commit
   pre-ack (the committed-but-unacked timeout — must re-check status, never
   blindly repeat non-idempotent ops). Tool-level wrappers: malformed outputs,
   empty/truncated pages, stale reads, transaction conflicts.
2. Adversarial cases (Set 3, 100): misleading labels, conflicting ontology
   definitions, prompt-injection literals in labels/comments/results
   (`Ignore previous instructions…`), cross-tenant baits, unauthorized-URI
   results, indirect broad-deletion wording. Pass criterion is Rust-layer
   containment (authz/graph gates hold regardless of model behavior) +
   model-doesn't-treat-data-as-instructions.
3. Recovery metrics (§6.3/§10): success, retries vs unnecessary retries,
   duplicate mutations (must be 0 on non-idempotent ops), infinite-loop
   detection (existing Budget guards count as the mechanism — assert they
   trip), final-report correctness.
4. `tests/bench_t4_adversarial.rs`: fault matrix × variants; injection-literal
   battery; dangerous-request battery; each safety gate asserts: graph hash
   unchanged (target + non-target), exactly-one audit row with the precise
   `CommitStatus` (`Denied`/`ValidationFailed`/`confirmation`), zero
   `Committed` phantoms.

Acceptance: **zero** unauthorized mutations, unapproved destructive ops, and
cross-tenant leaks across all variants (categorical gates, §6.4 — any single
failure fails the phase, never averaged away). Recovery success reported per
variant; transient-fault recovery floor 95% target tracked for T5.

## Phase T5 — Scale, stability, decision (regression-tested recommendation)

**Goal:** generated scale, run-to-run stability, costed winner + fallback
(deliverable: regression-tested recommended architecture).

Tasks:
1. Set 2 generator (500 cases): programmatic variation over labels,
   namespaces, sizes, counts, relationship patterns, ambiguity, missing data;
   generator seeded (reproducible); bulk JSON git-ignored, manifest checked
   in. Nightly job runs full 500 × 4; CI runs fixed-100 replay.
2. Stability: fixed-100 × 3 runs per variant + 5-seed subset; report variance;
   nondeterminism budget documented (live-model runs are expected to vary;
   replay runs must be bit-identical).
3. Operational metrics (§6.3): model calls, tool calls, tokens in/out,
   planning/execution/total latency (median + p95), replans, invalid calls,
   schema failures, peak context, cost per successful task.
4. Decision record (`OPERATIONS.md` + report): per-§9 table (no single blended
   score until failure categories are inspected), per-§12 acceptance targets
   (98/95/90/90/99/95/95 + three zeros), named production default + fallback
   with the numbers that chose them, and the conditions that would overturn.
5. `tests/bench_t5_regression.rs`: stability + spot-check of generated cases
   in CI (fast subset); floors ratcheted from T2–T4 observations; any accuracy
   drop vs recorded baseline fails CI.

Acceptance: §12 table filled per variant; winner + fallback named with
numbers; full `cargo test` green keyless; nightly live + `live-model` jobs
defined (ignored locally, required in nightly).

## Cross-phase rules

- Executor is identical for all variants in all phases (already true in
  `src/executor/`); any phase needing executor changes proves them first in
  `tests/exec_phase5.rs`.
- Expected answers live outside the LLM (oracles, hashes, snapshots); the
  model's explanation is never evidence.
- Keyless CI default: every phase's tests pass with no `OPENAI_API_KEY` and
  no weights (replay/stub/scripted). Live runs are opt-in (`RUN_MODEL_TESTS=1`,
  `--features live-model` for A/Needle) and write the recordings CI replays.
- Floors only ratchet upward; T5 locks them.

## Addendum — fixes/plan Phase 3: compiler default (2026-11-27)

The planning default after fixes/plan Phase 3 is the deterministic
request compiler (`src/planner/compiler.rs`), not a planner loop:
classify → link → fill slots → ground → assemble → validate, with zero
model calls on the happy path. The model answers only bounded
per-field slot questions; the B tool loop survives solely as the loud,
counted escalation for `Unmatched` requests.

New gate metrics (tracked alongside the T5 operational set):
`unmatched-request` rate over the fixed-100 corpus (published per row
with its fate: `compiler-default` / `rule-candidate:<rule>` /
`accepted-llm`), per-call `model_slot_calls` (0 on the happy path),
and steps-per-rung (compiler emits the same spine shapes, so the
before/after is loop turns eliminated, not steps changed). Corpus
floor: 15/100 owned (the phase-2 spine floor, exactly); growth comes
only from `rule-candidate` rows with live evidence.

Nightly additions (ignored locally, required in nightly):
`RUN_MODEL_TESTS=1 cargo test --test phase3_compiler
live_compiler_ohio_e2e -- --ignored --test-threads=1` (deterministic
create leg + live disambiguation leg). Variant futures are decided:
escalation fallback = B loop only; A/C/D retired from planning duty
(code and tests stay), recorded in `OPERATIONS.md`.
