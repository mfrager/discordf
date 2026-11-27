//! Phase 3 ablation log: what shipped, what deviated, what was dropped.
//!
//! The plan (`design/fixes/plan.md`) prescribes ablate-in-order with a
//! decision log. Phase 3's change is structural (compiler default, LLM
//! on demand), so this log records build deviations, scope rulings,
//! and the live gate instead of per-item ablations.

# Phase 3 ablation log (2026-11-27)

## Shipped (keyless, green)

- `src/planner/compiler.rs`: deterministic `compile_request`
  (classify → link → extract → ground → assemble → validate, no
  backend in signature: zero model calls by construction),
  `ground_mutation` (SHACL-required create props from
  `ToolState.shapes`, domain/range-unique update predicate+objects,
  `?x rdf:type` query template with `return_vars`), `compile_corpus`
  (per-row outcome + fate), `CompilerPlanner` (happy path →
  `compiler:<spine>`; ambiguity → one bounded per-field call →
  `compiler-slots:<spine>` or `Clarification`; no-spine → counted
  escalation to the B loop), `escalation_decision`.
- `tests/phase3_compiler.rs` (10 passed): ladder happy-path pin
  (L1–L4/L7/L10 compile, L5/L6 need terms, L8/L9 need the label —
  each asserted exactly), read-path E2E with scripted slot replay,
  create/classify/delete final states, contacts-classify loud
  boundary, planner-level zero-model proof (stub panics if touched),
  field replay, corpus gate (15/100 owned, fates published).
- `OPERATIONS.md`: compiler default, variant futures (B fallback
  only; A/C/D retired from planning duty), nightly compiler gate.

## Scope rulings (deviations from a naive reading, all documented)

1. **No NL→query compiler built** (plan non-goal honored). The query
   step carries a fixed single-triple template filled from linked
   entities — a slot, not a compiler. Qualifiers the template cannot
   encode (Ohio filter, stale-ness) are recorded as next-rule
   candidates in the ledger, which is the mechanism by which the
   fallback shrinks.
2. **L5/L6 compile via the slot path, not the happy path.** Their
   phrasing names no ontology term (`Acme-related`, `instances`),
   so terms come from one bounded model call (pinned keyless by the
   field-replay test). Guessing them would violate the no-guess rule.
3. **Classify E2E runs in a clean room (tenant_b).** Bench tenant_a
   accounts carry pre-existing SHACL dirt (names under the
   Contact-domain property — already recorded in OPERATIONS as
   fixture dirt), and status individuals are typed only in the
   ontology graph while the guard's instance check is
   instance-graph scoped. Both are pre-existing engine/fixture
   limits, worked around transparently in fixture setup (seeded
   accounts + tenant-visible status types), not in product code.
4. **PredicateChoice arm is defensive.** No current fixture yields
   two fitting predicates, so the disambiguation question for
   predicates is covered by unit shape (name/question), not by an
   executed path. It fires only on richer ontologies.

## Dropped: constrained-decoding spike (plan item 4)

Verdict: dropped, with the ladder evidence the plan asked for.
Phase-1 exact schemas already made invalid JSON unrepresentable on
the tool path (zero schema failures in every live sweep since);
every remaining fallback failure is semantic (representation traps,
missing values, starved prose), not syntactic — a grammar cannot
fix the C sink (budget burned on reasoning before any token) or
ground a value the request never names. No rung, live or scripted,
changed outcome class for want of grammar enforcement. If a future
backend shows a *syntactic* failure class, the spike can be
re-opened against that evidence.

## Live gate (CLOSED 2026-11-27)

- `live_compiler_ohio_e2e` (live 1B, budgets 4092, timeouts 1800s):
  **PASS in 3.26 s wall.** Deterministic create leg green (no model)
  + disambiguation leg green: the bare delete request named its one
  missing text field, the live model filled it first try, plan
  validated with `compiler-slots:delete` routing and exactly one
  model slot call. Logs: `/tmp/live_compiler_gate3.log` (pass),
  `/tmp/live_compiler_gate2.log` (fail, kept as evidence).
- First attempt failed honestly and diagnosably: the question text
  carried no JSON shape (the 1B ignores `response_format`
  scaffolding), the model answered in prose, elicitation burned its
  one repair, and the planner returned `Clarification` — the designed
  degradation, not a guessed plan. Fix was prompt-side (inline the
  exact shape + example, same lesson as phase-1 few-shot), no code
  path changed. The improved `Schema` error now quotes the last raw
  reply (truncated) so the next such failure is diagnosable from the
  test log alone.

## Decision log

- Corpus floor 15/100 held exactly (5 compiled + 10 needs-slots =
  the 15 phase-2 spine matches; same 85 unmatched IDs). The compiler
  owns everything the classifier covered, plus slot gaps — never
  less. Floor stands; growth comes only from next-rule candidates
  with live evidence.
- Steps-per-rung preserved by construction (compiler emits the same
  spine shapes; happy path uses 0 model turns vs ≥N loop turns).
- Escalation set reduced to B alone, recorded in OPERATIONS.md per
  the plan's phase-2-evidence rule. No planner code removed.
