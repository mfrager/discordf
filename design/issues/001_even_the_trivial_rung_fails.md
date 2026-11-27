# Why even the trivial rung fails — design analysis, not a model complaint

Date: 2026-11-27. Status: diagnosis only; no code changed.

## Verdict

The system asks its weakest component (a 1B local model, or the Needle
engine at its default token cap) to perform its hardest generation task:
emitting a machine-grade nested plan envelope — discriminated `args`
unions, typed `step_ref` / `uri_set_ref` values, exact output enums,
explicit DAG edges — as free text, with the true schema hidden from it.
Ladder rung L1 ("List the vocabulary terms for contacts", a single
`search_vocabulary` step) fails on **all four** planner types (A/B/C/D),
so complexity is not the variable: the floor is above the ceiling. This
is a design gap, not a budget gap — raising the budget 8x changed
nothing, capping at 4092 changed nothing, and the failure reproduces
identically at 1 step and at 6 steps.

## Evidence (all keyless, EXIT=0, no transport failures)

Run: `RUN_MODEL_TESTS=1 ./target/debug/ladder_cli --rung 1`
(model `MiniCPM5-1B-F16`, ≤4092 tokens/call). Stream logs:
`/tmp/ladder_stream_rung1-{A,B,C,D}.log`.

- **B rung1: broke (Budget).** Turn 1 emits
  `add_step({"id":"s1","operation":"search_vocabulary","intent":"list",
  "args":{},"output":"VocabularyCandidates","depends_on":[]})` → rejected
  (`missing field 'kind'`). Turn 2's *reasoning explicitly acknowledges*
  the error ("a previous step failed because it didn't have the required
  'kind' field") and then emits `"args":{}` **again**. Turn 3's reasoning
  verbalizes `kind="contact"` at length — and still emits `"args":{}`.
  Three stagnant turns → Budget stop.
- **C rung1: broke (Schema).** 164KB of reasoning for a 1-step plan; the
  model over-plans ("Step 2: describe each term" — invented work for a
  list-terms request), then emits JSON that fails the plan schema.
- **D rung1: broke (Schema).** 238KB across two passes; extraction output
  malformed.
- **A rung1 (Needle, real weights): broke (Model).** The failure is
  engine-side, not even model-quality: `tool call truncated: token budget
  exhausted (truncated) — reset the conversation, shorten the input, or
  raise max_new_tokens`. Needle never got to emit the call.

## Root causes

### 1. The `args` envelope is invisible to the model (the core gap)

`operation_schema()` (src/planner/mod.rs) declares
`"args": {"type": "object"}` — unconstrained. The real constraint, the
per-operation `StepArguments` tagged union (`{"kind":"search_vocab",
"terms":[...]}` plus nested `step_ref` / `uri_set_ref` / `unresolved`
typed values), exists **only** in Rust (`validate_plan` + serde) and in
one prose line of the system prompt (rule 3). The C/D JSON schema
(`plan_schema_json`) has the same hole: full step envelope, but
`"args": {"type": "object"}`. The model must therefore hallucinate the
exact nested envelope from prose — and a 1B model collapses to `{}`.
Top-level scalars (`id`, `operation`, `output`) come out right every
time; the nested object never does. Nothing in the design ever *shows*
the model a single complete valid `add_step` call: the prompts contain
zero worked examples (no few-shot), so the envelope must be induced
from a JSON-schema fragment that does not contain it.

### 2. Structure flows through the model instead of around it

The plan DAG — step ids, `depends_on` edges, typed cross-references,
exact `OutputType` enums (all `additionalProperties: false`) — is
generated as free text by the weakest component and only checked
afterwards by the strongest (Rust). Validation feedback names the
violation but carries zero gradient for a model this small: turn 2 of
the B log is the proof — comprehension in reasoning, identical output
in action. The repair/retry loops assume a model that can *use*
corrections; this one cannot, so every loop is a turn-burner that ends
in Budget/Schema stops. Strictness that only Rust can see is strictness
the model cannot satisfy.

### 3. Reasoning-tax on a fixed budget

The 1B model is a reasoning model: it spends the context narrating its
intent (500 → 800 → 1350 thinking tokens across B's three turns; 164KB
for C's one call) and starves the answer. Bigger caps only lengthened
the narration (16k run: C went empty→malformed, B/D unchanged).
Budget is a cost control, never a fix.

### 4. No minimality pressure

C invents a second step for a one-step task. The prompts describe when
to *finish* but nothing penalizes extra steps, and the validator
accepts any validating shape — so the model gilds, and each invented
step is another chance to break the envelope.

### 5. Needle path: engine config, not model quality

A's rung-1 failure is `max_new_tokens` truncation inside `cactus-rs`
before any planning logic runs. The Needle loop, caps, and validators
were never exercised. Separate gap, same lesson: the token envelope
around tool calls is unconfigured for the call sizes this design
emits.

## What this means for the end-to-end design

NL → plan → RDF queries currently routes **all** structure through the
model: the model chooses operations, invents ids and edges, and
hand-serializes nested typed values. The deterministic parts of the
system (validator, executor, ontology, registry) know the legal shapes
exactly but contribute them only as post-hoc rejection. The discovery
spine itself (`search → describe → query → mutate → validate →
verify`) is fixed and known in advance — the ladder proves every rung
is a composition of a handful of shapes — yet the design re-derives it
from prose on every run through the least reliable component. That
inversion is the failed design: small models can *select*, *rank*,
*fill small scalar slots*, and *answer disambiguation questions*;
they cannot reliably *emit nested machine schemas from prose*.

## Rethink directions (for discussion, not yet a plan)

1. **Deterministic spine, model-filled slots.** Rust owns the DAG
   (rung shapes are enumerable); the model supplies only scalars
   (`terms`, `intent` strings) or picks among enumerated candidates.
   The envelope is then constructed, never generated.
2. **Per-field elicitation.** One tiny schema per question
   (`{"terms": [...]}`) instead of one giant plan schema; Rust
   assembles and validates.
3. **Constrained decoding.** Grammar/GBNF-enforced tool args at the
   llama-server layer, so invalid JSON is unrepresentable rather than
   rejected-after-the-fact. (Note: needs care with `response_format:
   json_schema` + streaming; verify server support first.)
4. **Relax-then-normalize at the boundary.** Accept near-miss enums /
   missing discriminators at the model boundary and normalize in Rust
   (e.g. default `kind` from `operation`), reserving hard rejection for
   semantic violations (dangling refs, type mismatches across edges).
5. **Few-shot the envelope.** If the model must emit structure at all,
   the prompt must contain complete valid calls, not schema fragments —
   starting with the exact L1 call, which is currently absent.
6. **Fix the Needle envelope.** Raise/configure `max_new_tokens` for
   the tool-call sizes the planning surface emits before judging A's
   planning ability.

Acceptance test for any direction: `ladder_cli --rung 1` clears on all
four backends. Until L1 clears, higher rungs, bigger budgets, and more
prompt prose are not the lever.
