# Fix plan: from model-emitted structure to Rust-owned structure

Diagnosis: `design/issues/001_even_the_trivial_rung_fails.md`.
Acceptance instrument throughout: `ladder_cli --rung N`
(default suite stays green at every step).

Through-line: each phase moves structure further from the model and
closer to Rust. Stop after any phase that proves sufficient — each has
its own acceptance gate.

---

## Phase 1 — Unblock the floor (make L1 clear)

Goal: `ladder_cli --rung 1` clears on all four backends with the 1B
models, changing only what the model sees and what Rust tolerates —
no new architecture.

### Why this phase first

Issue 001 shows L1 fails on envelope emission, not on reasoning: the
model never produces the nested `args` object. Everything here is aimed
at that single failure, and every item is verifiable within one rung-1
run. Nothing here invents data and nothing weakens semantic validation
(dangling refs, dep-type rules, terminal gates all stay hard).

### Changes

**Build order (ablate in this order, measure after each):** per-op
tools + C/D schemas (item 2) → few-shot (item 1) → kind-default
(item 3). After each step, run `ladder_cli --rung 1` per backend and
record which item cleared which backend. No item may be credited with
another item's win; the ablation log is the deliverable that tells us
what actually worked.

**1. Few-shot the exact envelope (prompts only).**
Add one complete valid `add_step` call to the B/Needle planning
surface (the literal L1 call:
`{"id":"s1","operation":"search_vocabulary","intent":…,
"args":{"kind":"search_vocab","terms":["contact"]},
"depends_on":[],"output":"VocabularyCandidates"}`), plus one
complete `finish_plan` call. Add one complete minimal plan JSON to
the C prompt and the D extraction prompt (same L1 content).
Rationale: the schema fragment `"args": {"type": "object"}` cannot
induce the envelope; a worked example can. If the model still emits
`{}`, we have learned envelope emission is beyond this model class
and phase 2 becomes mandatory rather than optional. Examples
demonstrate the per-op calls from item 2, not the legacy generic
call — one shape taught, not two.

**2. Per-operation tools with filtered schemas (lib + prompt surface).**
Replace the single `add_step` (whose schema says `"args":
{"type": "object"}` — the lie identified in issue 001 §1) with one
tool per operation — `add_search_vocabulary_step`,
`add_describe_vocabulary_step`, `add_search_instances_step`,
`add_describe_resource_step`, `add_query_step`, `add_create_step`,
`add_update_step`, `add_delete_step`, `add_validate_step`,
`add_verify_step` — each carrying **only its operation's exact args
schema**: required fields, value types, and the nested `TypedValue`
`kind` discriminators fixed per position. The `output` field is not
model-emitted at all: `expected_for(op)` is deterministic, so Rust
fills it. The model chooses the operation (by choosing the tool) and
supplies scalars; the envelope is then fully determined by the tool
it called.

  *Schema filtering, two dimensions.* (a) **Per-operation:** each
  tool definition embeds just its variant's schema — the model never
  sees the other nine. (b) **Per-turn relevance:** the tool set sent
  with each turn is filtered to operations legal in the current
  session state (e.g. before any `VocabularyCandidates` exist, only
  the search tools are offered; `update`/`validate`/`verify` appear
  only once their dep types are satisfiable). Early turns offer 2–3
  tools instead of 6-plus, so there are fewer ways to be wrong.
  Stretch: narrow nested ref options per position the same way
  (`uri_set_ref` offered only when a `UriSet` step exists).

  *Single source of truth.* Schemas are derived from the same
  definitions the validator enforces (generated from the
  `StepArguments` shapes, not hand-written prose), plus keyless
  golden tests asserting schema↔validator agreement: everything the
  per-op schemas accept must validate, and every   validator-accepted
  shape must be schema-expressible. Schema and validator can never
  drift apart silently again.

  *Same medicine for C/D.* The generator serves every consumer, not
  just the tool path: the B/A per-op tool definitions, the C
  `plan_schema_json` (exact `args` anyOf-by-operation in place of
  `{"type": "object"}`), and the D extraction schema (same
  anyOf). C/D keep emitting whole plans, but every step's args are
  then schema-exact at the boundary instead of schema-absent. If C/D
  still fail after this, their failure is content (over-planning,
  malformed JSON despite exact schemas), not envelope invisibility —
  and the per-backend gate below routes them to phase 2.

  *Filter contract.* Per-turn filtering is a new failure class (a
  wrong filter hides the tool the model needs), so it is
  conservative by rule: the offered set is always a *superset* of
  the operations legal in the current session state, keyless-pinned
  per state (tests enumerate session states → assert the exact
  offered sets). Every use of the generic   `add_step` fallback and every stall with needed tools unoffered is logged as a
  `filter-miss` metric — the tuning signal, reviewed before any
  filter tightening.

  Expectation, stated plainly: contributes ~nothing. Partial args
have never been observed — with `args={}` the `terms` field still
rejects after the kind is defaulted. Kept because it is three lines
and the ablation log will confirm or refute that.

  *Token math.* Ten tools × exact schemas would inflate every prompt
  if all were sent; the per-turn filter is also a context saving:
  2–3 small exact tools per turn is a *smaller* prompt than today's
  6 generic tools with prose descriptions. Assert prompt-token
  ceilings per rung in the keyless tests so the schema work cannot
  silently bloat the context it was meant to clarify.

  The generic `add_step` stays as a fallback during transition (with
  usage counted); if per-op tools dominate its use, it retires. The
  same per-op surface serves phase 2's fallback path.

**3. Kind-default normalization at the Rust boundary (lib, tiny, marginal).**
When `args` parses as an object but lacks the `kind` discriminator,
derive it from the already-valid `operation`
(search_vocabulary→`search_vocab`, etc.) instead of rejecting.
Missing *data* fields (`terms`, `targets`, `question`) still reject
exactly as today — this invents no data, it only stops punishing the
model for a tag Rust already knows. Accept the bare payload shape
too (`{"terms":[...]}` without `kind`) via the same rule. Log every
normalization as a repair event so replay tests can pin how often
the model needed it (metric, not silent forgiveness).

**4. Needle engine envelope (config only).** Raise/configure
`max_new_tokens` for the tool-call sizes the planning surface emits,
so A gets past rung 1's truncation and the planning loop is actually
exercised. No prompt or loop changes.

**5. Over-planning counter (measurement only — not a fix).**
No prompt plea: a "prefer fewest steps" rule is the same failed
theory of change as validation feedback the model cannot act on.
Over-planning is fixed structurally in phase 2 (minimal spines), not
here. What phase 1 does instead: log steps-per-rung and over-plan
rate (validating plans with more steps than the rung needs — e.g. C's
invented step 2) as the baseline phase 2 must beat. Measurement only.

### Acceptance (per backend, not all-or-nothing)

- Each backend is gated independently after every ablation step:
  `ladder_cli --rung 1 --backend X` → CLEARED (live, ≤4092 tokens).
  The ablation log records which item cleared which backend.
- B gates on: per-operation tool call with schema-conformant `args`
  on turn ≤2; zero or one normalization event.
- C/D gate on: validating L1 plan with exactly the rung's steps (no
  invented step 2) — schema-exact args plus the over-plan counter at
  zero.
- Keyless: schema↔validator agreement tests green; per-turn offered
  sets asserted exactly per session state (conservative superset);
  prompt-token ceilings per rung hold; `filter-miss` rate reviewed.
- Routing branch: any backend still uncleared after all phase-1 items
  moves to the phase-2 elicitation path immediately — its traffic is
  not held hostage by other backends' gates, and B's win (if any)
  stands and ships independently. The phase-1 decision log records
  per backend: cleared-by-(item) or routed-to-phase-2.
- Default `cargo test` fully green; replay recordings extended with
  the normalization counters.
- Decision recorded: if few-shot + kind-default still leaves L1
  uncleared on any backend, that backend's free-emission path is
  declared unfixable-by-prompt and phase 2 covers it by construction.

### Non-goals

- No spine/template changes, no per-field elicitation, no constrained
  decoding, no validator strictness changes beyond the kind default.
- Higher rungs may still fail; that is phase 2's acceptance, not this
  phase's.

---

## Phase 2 — Invert the division of labor (Rust owns structure)

Goal: the model never emits a nested envelope again. Rust owns the
DAG and every `args` object; the model supplies scalars, selections,
and prose — the things small models can actually do. Full 10-rung
ladder clears live.

### Why this phase

Phase 1 proves (or fails to prove) that prompting can coax one valid
envelope out of the model. Either way it leaves the fragility in
place: every future operation shape, every new rung, every new model
re-runs the same lottery. Structure must flow *around* the model,
validated by construction, with the model filling only what cannot be
derived.

### Changes

**1. Spine templates in Rust (the ladder shapes, generalized).**
Encode the known discovery spine as Rust data, not prose: ordered
operation sequences with dep edges, ref kinds, and output types
(L1–L10 already enumerate the vocabulary of shapes; generalize to
per-request-class spines: lookup, grounded-search, create/update/
delete with terminal validate→verify). Request → spine mapping
starts as a small classifier (keyword/data driven, deterministic,
tested keyless). The model is not consulted on structure at all.

**2. Per-field elicitation (the model's only generation job).** For
each unfilled scalar slot in the chosen spine (`terms`, `intent`
strings, `question`, disambiguation choices), ask the model one tiny
question with a tiny schema (e.g. `{"terms": ["contact"]}`) — never
the whole plan, never a nested union. Model output is validated per
field and retried per field; a field failure burns one cheap turn,
not a whole plan. Unknown/graph-grounded values keep the existing
`unresolved` / `step_ref` / `uri_set_ref` machinery — but those
nodes are now *placed* by Rust, only *named* with model-supplied
scalars.

**3. Assembly + validation in Rust (unchanged validator, new
caller).** Rust assembles the elicited scalars into `Plan`/`PlanStep`
values directly (no JSON round-trip through the model), then runs the
existing `validate_plan` unchanged. Today's strictness becomes free:
it now checks Rust's construction, not the model's prose. B/C/D
free-emission loops become the fallback path, not the default: used
only when no spine matches (logged as `unmatched-request`, which is
itself the signal for adding the next spine).

**4. Constrained decoding spike (bounded, may fail).** Time-boxed
investigation: llama-server grammar/GBNF-enforced tool args for the
fallback path, so invalid JSON is unrepresentable. Adopt only with a
ladder-L1-before/after proof and a streaming + `response_format`
compatibility check; otherwise drop it — the other items must stand
without it.

**5. Minimality by construction.** Spines are minimal by definition
(no invented steps possible); over-planning disappears structurally
rather than by prompt plea. Keep phase 1's measurement counter to
prove it (steps-per-rung before/after).

### Acceptance

- `ladder_cli --rung 1..10` → CLEARED on B, C, D live (A follows the
  same elicitation path through its adapter); full `tests/ladder.rs`
  live climbs report 10/10 highest-cleared.
- T2 live planning subset: every case either spine-matched (with
  field-level trace) or explicitly `unmatched-request` — no silent
  free-emission fallback.
- Field-level replay tests pin the new behavior keyless (scripted
  scalar answers → assembled plan validates).
- Default `cargo test` green; OPERATIONS.md documents the new
  default path and the demoted fallback.

### Non-goals

- No NL→query compiler, no removal of the planner variants, no
  latency/throughput work. The executor, validator, ontology, and
  storage layers are untouched.
- The classifier stays deliberately small; uncovered request classes
  fall back loudly (phase 3 decides their fate).

---

## Phase 3 — Rethink the pipeline (compiler default, LLM on demand)

Goal: natural-language → RDF queries stops depending on LLM planning
as its default path. A deterministic compiler handles covered request
classes end to end; the model answers disambiguation questions and
fills slots; the planner loop survives only as a loud, measured
escalation. Live NL→final-state works on the 1B models.

### Why this phase last

Phases 1–2 make the model *able* to participate (envelope, then
structure). This phase asks whether it *needs* to. Most production
requests ("Ohio contacts as prospects", "delete stale X", "create a
contact") are a known spine plus entity linking plus slot values —
compilable without any planning loop. Every turn spent planning a
known shape is latency, cost, and failure surface for zero expressive
gain. The LLM's comparative advantage is ambiguity resolution, not
DAG emission; the pipeline should reflect that.

### Changes

**1. Request compiler (deterministic default).** Request class →
spine + entity linking (vocabulary/ontology terms resolved against
the reasoner and search index, same grounding the executor trusts) +
slot filling, all in Rust. Output is a `Plan` assembled exactly like
phase 2's assembly step and validated by the unchanged validator. No
model call on the happy path at all.

**2. LLM disambiguation + slots (bounded, on demand).** The model is
invoked only for what is genuinely underdetermined: which candidate
entity ("Ohio" vs "OH", account vs contact sense), which slot value,
whether to proceed on ambiguity (the existing
`request_clarification` semantics, kept). Same per-field tiny
schemas as phase 2; same per-field retry budget. A disambiguation
failure degrades to a clarifying question to the user, never to a
guessed plan.

**3. Planner loop as escalation (measured, not default).**
Free-emission B/C/D and the Needle loop remain for requests no spine
or compiler rule covers — but escalation is explicit, counted, and
reported (`unmatched-request` rate becomes the headline metric).
Each escalation is a candidate for the next compiler rule, so the
fallback shrinks over time by construction. Decide the variants'
future on phase-2 live evidence: keep the best-shaped fallback,
retire the rest, and say so in OPERATIONS.md (today's B-default /
C-fallback decision gets revisited here, not before).

**4. Regression gates (lock the win).** Nightly: full 10-rung live
ladder (all backends) + T2 live subset + the Ohio end-to-end, with
`unmatched-request` rate and steps-per-rung tracked, not just
pass/fail. Any change that reintroduces model-emitted structure must
clear the same gates; the ladder stays the acceptance instrument
from phases 1–2.

### Acceptance

- End-to-end NL → validated plan → executed final state on live 1B
  models for the covered request classes (Ohio classify/create/
  delete + ladder L7–L10 equivalents), with zero planning-loop turns
  on the happy path.
- `unmatched-request` rate measured and published; fallback
  escalations each carry a decision (new compiler rule vs accepted
  LLM planning).
- TESTING plan + OPERATIONS.md updated to the new default path;
  default `cargo test` green; live nightly green.

### Non-goals

- No new model, no weights work, no larger context, no agent
  framework adoption. If a bigger model later arrives, it slots into
  the disambiguation/slot role — the compiler default stands
  regardless of model quality, because determinism, not capability,
  is what makes the happy path trustworthy.
- No removal of safety invariants: confirmation gates, authz, SHACL,
  audit, and the zero-write planning guarantee are untouched
  throughout all three phases.
