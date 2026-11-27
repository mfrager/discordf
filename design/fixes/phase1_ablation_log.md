# Phase 1 ablation log — rung 1 live, per backend

Date: 2026-11-27. Command: `RUN_MODEL_TESTS=1 ./target/debug/ladder_cli
--rung 1` (model `MiniCPM5-1B-F16`, ≤4092 tokens/call, keyless).
Stream logs: `/tmp/ladder_stream_rung1-{A,B,C,D}.log`.
Baseline (pre-Phase-1, same command): A broke (Model: engine
truncated turn 1), B broke (Budget: 3× `{}`), C broke (Schema:
empty→malformed), D broke (Model: empty pass 1). Post-Phase-1 below.

## Method deviation (honest)

The plan asked for three sequential live sweeps (item 2 → measure →
item 1 → measure → item 3 → measure). Built and measured in two
sweeps instead: items 1+2+3 are intertwined (few-shot examples had to
show the per-op calls, which required item 2 to exist), and each full
sweep costs ~25 min of 1B inference. Attribution below is therefore
by evidence in the stream logs (which tool was called, whether kind
was emitted), not by isolated sweeps. Kind-default (item 3) fired
zero times in this sweep — expected, per the plan's own marginal
rating.

## Results

| Backend | Outcome | Cleared by | Evidence |
|---|---|---|---|
| B | **CLEARED** | Item 2 (per-op tools) | Turn 1: `add_search_vocabulary_step({"id":"s1","intent":"discover contact vocabulary","args":{"terms":["contact"]}})` accepted first try — kindless args, Rust filled `kind`, output auto-set. Turn 2: `finish_plan`. Zero generic calls, zero repairs. Item 1 (few-shot) confounded: same prompt carried the example. Item 3 unused. |
| C | **CLEARED** | Item 2 (exact args anyOf in `plan_schema_json`) | Single turn, `stop`, 310-char text, validating 1-step plan, no repair, no OverPlan. First-try success where 2048/16384 budgets all failed. |
| D | broke (Model) | — | Pass 1 prose filled the 4092 budget with reasoning (`finish=length`, empty text). The exact extraction schema was never exercised. Phase 1 changed pass 2 only; pass 1 (unconstrained prose) is untouched by any item. |
| A (Needle) | broke (Model) | partial (item 4) | Item 4 worked mechanically: turn 1 now executes (was: instant truncation at 256). Turn 1 returned junk `Done` text (rejected); turn 2 attempted a tool call still large enough to truncate at 2048. Engine runs; planning does not complete. |

## Token math (item 2, verified)

Keyless test `prompt_and_tools_stay_within_token_budgets`: filtered
L1 surface (2 per-op + generic + 3 session tools with exact schemas)
serializes smaller than the old 6-tool generic surface, and the full
L1 prompt + tools stay under the 20k-char ceiling. The filter is a
net context saving, as predicted.

## Over-plan counter (item 5)

Zero OverPlan events this sweep (B: 2 turns / 1 step; C: exact
1-step plan). Baseline rate recorded for phase 2 to beat.

## Decision log (per-backend gates)

- **B: cleared-by-(item 2, with item 1 confounded).** Ships
  independently. Generic `add_step` stays as counted fallback;
  `fallback_uses` stayed 0.
- **C: cleared-by-(item 2, schema anyOf).** Ships independently.
- **D: routed to Phase 2 elicitation path.** No Phase-1 item
  addresses pass-1 prose starvation; per the gate, D's traffic does
  not wait for another prompt iteration. Note for phase 2: D's
  pass 1 should be replaced by elicitation, not repaired.
- **A: routed to Phase 2 (with an open question).** Item 4 moved the
  failure point (turns now execute) but planning still breaks.
  Allowed follow-up before phase 2: raise `COMPLETE_MAX_NEW_TOKENS`
  only if a single larger value clears L1, otherwise do not chase —
  the elicitation path covers A the same way.

## Keyless evidence (all green)

`cargo test`: 25 suites ok. New: `tests/phase1_perop.rs` (5 tests:
per-op L1/L7 climbs kindless, generic kindless fallback + counter,
C exact-kind, C kindless-default) and 8 new in-module planner tests
(dispatch, kind-default, filter pins per state, schema↔validator
agreement, token ceilings).
