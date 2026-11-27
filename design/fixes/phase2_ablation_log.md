//! Phase 2 ablation log: what shipped, what deviated, what stays open.
//!
//! The plan (`design/fixes/plan.md`) prescribes ablate-in-order with a
//! decision log. Phase 2's change is structural (Rust owns the DAG), so
//! there is no per-item ablation in the phase-1 sense; this log records
//! the build deviations and the live gate instead.

# Phase 2 ablation log (2026-11-27)

## Shipped (keyless, green)

- `src/planner/spine.rs`: 10 spine templates in table order (most
  specific first), whole-word classifier, `Slots` elicitation question,
  `assemble` (kind auto-filled, outputs/refs wired by template),
  `SpinePlanner` with counted fallback (`spine-miss` / `spine-failed`
  routing artifacts). No prompt changes to A/B/C/D loops (fallback
  untouched).
- `tests/ladder.rs` backend S: scripted S climb 10/10 + `ladder_s`
  live-climb helper. `--backend S` in the ladder CLI (spine-first,
  streams to `/tmp/ladder_stream_rung{L}_S.log`).
- `tests/phase2_spine.rs` coverage report (keyless): 10/10 ladder
  rungs pin exact spine levels; bench corpus **15/100 matched**
  (`create` 5, `delete` 3, `dual-discovery` 1, `query` 4, `resolve` 2).
  Misses inspected and accepted: bench requests are terse
  (`Find all contacts`, `What is the range of crm:hasStatus`) with no
  spine-covering shape — unmatched is the *correct* answer, handled by
  the counted fallback. No classifier broadening: the plan deliberately
  keeps it small; phase 3 grows it only on live evidence.
- `OPERATIONS.md`: spine-first default recorded with the 15/100
  baseline; free loops demoted to fallback path.

## Deviations: none

No plan item was bundled, reordered, or skipped. The escaping/byte
friction during test authoring (backslash-quote in code positions)
was tooling friction, not a design change — fixed, no plan impact.

## Live gate (CLOSED 2026-11-27: 10/10)

- `live_ladder_s_climb` (10 rungs, live 1B model, budgets 4092,
  timeouts 1800s): **L1–L10 all CLEARED on the spine path**
  (lookup, dual-discovery, resolve, query, grounded-search,
  describe-instances, classify, create, delete, reconcile).
  Zero `spine-failed` fallbacks in 513 s wall. Verdict: spine-first
  confirmed as the default path — including L9 (delete fan-in)
  and L10 (reconcile), the exact shapes where B starved on prose
  and A truncated at turn 2.
- Framing for phase 3: the constrained-decoding spike is NOT about
  the ladder anymore (S owns all 10 rungs). It is about the 85%
  unmatched bench corpus: whether grammar-constrained C/D loops
  can take over fallback traffic currently served by free-emission
  loops. The core Q stands (is the C sink the grammar?), relocated
  to the fallback path.

## Decision log

- Coverage 15/100 accepted as the shipping baseline (conservative
  classifier + loud fallback beats an over-fit classifier; phase 3
  grows it only where `spine-miss` volume justifies a new rule).
- No fallback-loop prompt edits in phase 2 (kept byte-identical so
  phase-1 gates stay valid; phase 3 compares against the same
  baseline).
