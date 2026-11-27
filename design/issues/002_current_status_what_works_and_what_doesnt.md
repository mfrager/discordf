#{002} Current status: what works and what doesn't (2026-11-27)

(This is the status summary requested after fixes/plan Phases 1-3.
For the original diagnosis see 001_even_the_trivial_rung_fails.md;
for the fix plan see design/fixes/plan.md; for per-phase build logs
see design/fixes/phase1_ablation_log.md, phase2_ablation_log.md,
phase3_ablation_log.md.)

## TL;DR

The pipeline is now trustworthy on what it covers but no broader
than before. Covered requests compile deterministically with zero
model calls, execute to real final states, and fail loudly otherwise.
Corpus coverage is flat at 15/100 owned; the same 85 unmatched
requests still escalate to the B planning loop. Every number below
is from a green run, not an aspiration.

## What works

- **Ladder L1-L10: deterministic end to end.** Phase 2 spine-first
  sweep cleared all 10 rungs on the live 1B model (lookup,
  dual-discovery, resolve, query, grounded-search,
  describe-instances, classify, create, delete, reconcile) with zero
  `spine-failed` fallbacks (513 s wall, log
  `/tmp/live_s_sweep.log`). Phase 3 keeps the same 10/10 shapes via
  the compiler happy path with 0 model turns.
- **Compiler happy path: zero model calls by construction.**
  `compile_request` (src/planner/compiler.rs) takes no backend:
  classify -> link against the live ontology -> fill slots -> ground
  single-candidate mutation values -> phase-2 assemble -> unchanged
  validator. Proven at the planner level by a stub that fails the
  test if touched (compiler_planner_happy_path_needs_no_model).
- **Final states, not just valid plans.** Keyless E2E
  (tests/phase3_compiler.rs, 9 passed / 1 live-ignored): create
  commits a new Contact (+1 instance, SHACL-conformant label);
  classify commits hasStatus on clean-room accounts; delete removes
  all 10 contacts; read paths execute to real discovery outputs;
  read-only E2E leaves the audit log untouched.
- **Live gate closed.** `live_compiler_ohio_e2e` passed on the 1B
  model in 3.26 s wall (log `/tmp/live_compiler_gate3.log`):
  deterministic create leg + live disambiguation leg (bare delete
  request -> model filled the one missing text field first try ->
  `compiler-slots:delete`, exactly one model slot call).
- **Failures are loud and counted.** Contacts-classify (no status
  property exists in the ontology) stops at an `unresolved`
  predicate escalation instead of a guessed predicate. The first
  live-gate attempt failed honestly too: prose answer ->
  `Clarification` to the user, never a hallucinated plan. Routing
  artifacts (`compiler:<spine>`, `compiler-slots:<spine>`,
  `compiler-escalated:<reason>`) plus per-call `compiler_stats`
  make every outcome attributable.
- **Full suite green.** Default `cargo test`: lib 131 + all suites
  pass, phase3_compiler 9 passed / 1 ignored (live gate).

## What doesn't

- **Coverage is flat: 15/100 bench corpus owned** (5 compiled + 10
  needs-slots; same 85 `accepted-llm` IDs as the phase-2 spine
  floor). Terse bench requests (`Find all contacts`, vocabulary
  probes) name no spine-covering shape; that is the correct
  unmatched verdict, but it means 85% of the corpus still rides the
  B fallback loop. Growth comes only from `rule-candidate` rows
  with live evidence, one at a time.
- **Query qualifiers unhandled.** The query step is a fixed
  single-triple template (`?x rdf:type <linked class>`), a slot,
  not a query compiler (plan non-goal, honored). Ohio-filter,
  stale-ness, and multi-hop qualifiers are recorded as next-rule
  candidates, not executed as filters.
- **Pre-existing engine/fixture limits worked around, not fixed.**
  Bench tenant_a accounts carry SHACL dirt (names under the
  Contact-domain property); status individuals are typed only in
  the ontology graph while the guard's instance check is
  instance-graph scoped. Classify E2E therefore runs in a clean
  room (tenant_b with seeded accounts + tenant-visible status
  types). No product code changed for this; the dirt is still
  recorded in OPERATIONS.md.
- **Constrained-decoding spike dropped.** Phase-1 exact schemas
  already made invalid JSON unrepresentable; every remaining
  failure is semantic (representation traps, missing values,
  starved prose). A grammar cannot fix the C sink (budget burned on
  reasoning before any token) or ground a value the request never
  names. Re-open only on future *syntactic* failure evidence.
- **Variant futures decided, not all resolved.** Escalation fallback
  = B loop only; A/C/D retired from planning duty (A: turn-2
  truncation; C: superseded, cannot interleave discovery; D: live
  prose starvation). Code and tests stay, per plan.

## Open threads (next-rule backlog, verbatim from the ledger)

- `rule-candidate` rows from the corpus report (terms/text gaps on
  L5/L6/L8/L9 phrasings) are the prioritized compiler-rule work.
- `accepted-llm` rows (the 85) each carry the decision to keep them
  on the B loop until a rule earns them.
- PredicateChoice disambiguation is defensive-only: no current
  fixture yields two fitting predicates, so it is covered by unit
  shape, not an executed path; it fires only on richer ontologies.
- Nightly live jobs (ignored locally, required in nightly):
  `live_compiler_ohio_e2e` plus the T2-T5 live subsets and the
  `--features live-model` A run.

## How to verify any of this

- `cargo test --test phase3_compiler -- --nocapture` (keyless E2E +
  corpus report with per-row fates).
- `cargo test --test phase2_spine -- --nocapture` (spine coverage
  baseline 15/100).
- `RUN_MODEL_TESTS=1 OPENAI_BASE_URL=http://localhost:8080
  OPENAI_MODEL=MiniCPM5-1B-F16 cargo test --test phase3_compiler
  live_compiler_ohio_e2e -- --ignored --nocapture --test-threads=1`
  (live gate; needs the local server).
- `cargo test` (full default suite, green at time of writing).
