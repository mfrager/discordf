# NL Interface to OWL/RDF — 10-Phase Implementation Plan (Combined)

**Sources (both read in full, carefully combined):**
- `design/DESIGN_ENHANCEMENT_1.md` — the product architecture: Rust owns the workflow; System A (Needle 3, constrained extractor/selector) and System B (MiniCPM5-1B, iterative interpreter) evaluated independently; context construction; intent → vocab → paths → logical query → SPARQL → instances → mutation; 5-level difficulty ladder (25 tests × 2 systems = 50 runs); 6-dimension scoring + critical failures.
- `design/DESIGN_TESTING_1.md` — the process: one capability at a time with the live model; isolated / interactive / end-to-end harness modes; Phases 0–9 (baseline → intent → vocab → inspection → paths → logical query → instances → discovery loop → mutation proposal → validation/execution); failure-diagnosis order; experiment/regression control; phase gates; Rust-only vs model-only vs integrated variants.

**LLM library (mandatory):** `~/Scripts/agentstream` (`agentstream v2.0.0`) — `LLMBackend` trait + `OpenAiBackend`/`BackendFactory` + `AgentStream`/`AgentConfig` + `Tool`/`ToolRegistry` + `EventLog`/events. No direct `cactus-rs` FFI, no hand-rolled HTTP/SSE, no ad-hoc JSONL logging. Both System A and System B go through `agentstream`.
**Env loading (mandatory):** `dotenvy` — `dotenvy::dotenv()` loads `.env` before any `std::env::var`. All model, endpoint, graph, limit, and policy knobs come from `.env` (+ `OPENAI_*` / `AGENTSTREAM_*` conventions from `agentstream`).

**Current repo:** `needle-rdf-agent` — `oxigraph 0.4` (authoritative store) + `tantivy 0.22` (lexical search) + `tokio` + `axum` + `tracing` + `serde`. This plan migrates the `model/` layer from `cactus-rs` to `agentstream` while keeping `rdf/`, `ontology/`, `search/`, `tools/`, `policy/`, `agent/`, `api/` and their safety invariants.

---

## 0. Global architectural invariants (enforced in every phase)

From ENHANCEMENT §§1–2 + TESTING §§4,7,9 — non-negotiable:

1. **Rust owns the workflow.** The model never plans, never writes SPARQL, never picks graph scope/tenant/auth, never commits. Model output is a bounded semantic decision validated against supplied evidence; Rust retrieves evidence, resolves IRIs, compiles queries, enforces scope, executes, retains state.
2. **Two systems, one workflow code, separate evaluation.** System A (Needle 3) = constrained extractor + small candidate selector. System B (MiniCPM5-1B) = iterative interpreter + discovery operator. Same Rust workflow record, ontology services, query services, mutation pipeline, fixtures, expected outcomes. Never share model decisions or workflow state between A and B. Same request, independent runs.
3. **Workflow record is authoritative; prompt is a disposable view.** Record holds: original request, graph scope, stage, current interpretation, resolved refs, candidate relations, selected paths, query spec, result handles, pending clarification, mutation proposal. Each model call gets minimum sufficient state + evidence, rebuilt after every tool result (never append raw tool output to a growing transcript).
4. **Opaque candidate/handle IDs.** Rust maps `candidate_N` / `path_N` / `handle_N` → real IRI/graph internally. Model selects IDs only. Unknown ID / invented IRI / out-of-scope graph = reject + retry / clarify / controlled failure.
5. **Provenance labels.** Every context distinguishes `user_asserted` / `model_proposed` / `ontology_confirmed` / `user_confirmed` / `execution_observed`, plus asserted-vs-inferred. A `model_proposed` never becomes a premise without Rust or user confirmation.
6. **Bounded evidence.** Every package has caps: max candidates, max paths, max instances, max definition chars, max results page. Rank/filter in Rust; never truncate distinguishing text silently.
7. **Three harness modes always use the live model** (TESTING §1): `isolated` (one decision, fixed context), `interactive` (decide → Rust executes read-only tool → new context → decide), `end-to-end` (full workflow incl. mutation preview/validation). Isolated ≠ mock.
8. **Safety regression runs after every relevant change**, even prompt-only changes.

---

## 0.1 Global dependency + env changes (do once, use in all 10 phases)

### 0.1.1 `Cargo.toml` — add `agentstream` + `dotenvy`, demote `cactus-rs`

```toml
[dependencies]
# LLM lib — local checkout, NOT crates.io:
agentstream = { path = "/home/mfrager/Scripts/agentstream", version = "2.0", features = ["openai-backend", "tool-registry", "embedded-log", "tracing-log"] }
dotenvy = "0.15"
# keep:
oxigraph = "0.4"
tantivy = "0.22"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tokio = { version = "1", features = ["full"] }
axum = { version = "0.7", features = ["json"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "fmt"] }
thiserror = "1"
anyhow = "1"

[dev-dependencies]
tempfile = "3"
agentstream = { path = "/home/mfrager/Scripts/agentstream", version = "2.0", features = ["openai-backend", "tool-registry", "embedded-log", "dev-utils"] }

[features]
live-model = []          # now means "allow live backend via agentstream"; no cactus-rs
# REMOVE or keep-behind-flag: cactus-rs = {...}  → delete; LiveEngine/NeedleAdapter become adapters over agentstream (Phase 1)
```

Why these features: `openai-backend` → `OpenAiBackend` + `BackendFactory::openai_from_env`; `tool-registry` → `ToolRegistry` + `AgentStream` tool accessors; `embedded-log` → `EventLog` on the agent, causal `parent_id`/`prompt_run_id`; `tracing-log` → diagnostics; `dev-utils` (tests only) → `CountingBackend` + mock HTTP server.

### 0.1.2 `.env` loading with `dotenvy` (all binaries, tests, harnesses)

```rust
// src/main.rs (first lines of main) + tests/harness entry points:
fn load_env() { let _ = dotenvy::dotenv(); }
// then BackendFactory::openai_from_env(120) reads OPENAI_API_KEY etc.
```

`.env` keys (document in `.env.example`; `.env` stays gitignored):

| Key | Purpose | Default |
|---|---|---|
| `OPENAI_API_KEY` | agentstream backend key (required live) | — |
| `OPENAI_BASE_URL` | OpenAI-compatible endpoint; System A → Needle-3 server; System B → llama.cpp MiniCPM server | `https://api.openai.com/v1` |
| `OPENAI_MODEL` | `BackendFactory::model_from_env()` default model id | per-system override below |
| `SYSTEM_A_MODEL` / `SYSTEM_A_BASE_URL` | Needle 3 endpoint + model id | e.g. MiniCPM-style local URL |
| `SYSTEM_B_MODEL` / `SYSTEM_B_BASE_URL` | MiniCPM5-1B endpoint + model id | e.g. `http://127.0.0.1:8080/v1` |
| `AGENTSTREAM_TIMEOUT` | `BackendFactory` timeout secs | `120` |
| `NEEDLE_MAX_ITERATIONS` etc. | existing `AppLimits::from_env` caps (8 iters, 32 ops, 500 triples/read…) | see `src/config.rs` |
| `NEEDLE_ADDR` | Axum bind | `127.0.0.1:3000` |
| `APP_DEFAULT_GRAPH`, `APP_TENANT`, `APP_REFERENCE_DATE` | scope + temporal ground truth for fixtures | per-fixture override |
| `LOG_DIR` | `ExplicitLogDir::new` for agentstream JSONL | `./logs` |

`AppLimits::from_env` + `BackendFactory::{openai_from_env, model_from_env}` must be called **after** `dotenvy::dotenv()`. Tests that need env use `dotenvy::from_filename_iter` or set vars explicitly; never commit `.env`.

### 0.1.3 `agentstream` usage pattern (canonical, all phases)

```rust
use std::sync::Arc;
use agentstream::agent::{AgentConfig, AgentStream};
use agentstream::backends::BackendFactory;
use agentstream::event_log::{EventLog, ExplicitLogDir, SessionMemo};
use agentstream::events::EventDepth;

let backend = BackendFactory::openai_from_env(120)?; // reads OPENAI_* after dotenvy
let model = BackendFactory::model_from_env().unwrap_or("gpt-4o-mini".into());
let log = EventLog::new(ExplicitLogDir::new(&log_path), 50);
let config = AgentConfig::new(model, 1024).with_depth(EventDepth::OUTERMOST)
    .with_log(log, SessionMemo::new());
let mut agent = AgentStream::new(config, backend);
agent.set_system("...stable instruction: RDF authoritative; never invent IRIs; return only schema...");
// Bounded decision with schema + tools:
let resp = agent.send(&prompt).with_schema(schema_json, Some("intent".into()))
    .with_tools(tool_defs).await?;
```

Discovery/ontology tools are exposed as `agentstream::tools::Tool` (`Tool::new(name, desc).with_parameters(schema)`) and executed via `ToolRegistry::execute`; `AgentResponse::tool_calls: Vec<ToolCallData>` carries requests. Every invocation is recorded as immutable test events (`test_id, system, phase, invocation_no, model, endpoint, input messages, available actions, output, parsed, validation, execution, elapsed`; + `reasoning` fields for MiniCPM, config/routing metadata for Needle).

---

## Target module layout (end of Phase 10)

```
src/
  main.rs                 # dotenvy::dotenv() first; build agentstream backends A/B; serve Axum
  lib.rs
  config.rs               # AppLimits + AppConfig + from_env (after dotenvy)
  workflow.rs             # NEW: WorkflowRecord state machine (stages/transitions), provenance enum
  context.rs              # NEW: context builder (7-section prompt composer + caps)
  model/
    mod.rs                # ModelAdapter trait → thin wrapper over agentstream AgentStream
    agentstream_a.rs      # NEW: System-A backend (Needle 3 via OpenAI-compat) + constrained decode
    agentstream_b.rs      # NEW: System-B backend (MiniCPM5-1B via llama.cpp) + reasoning toggle
    mock.rs               # existing MockModel for offline tests
  ontology/               # loader.rs, reasoner.rs, shacl.rs, vocab.rs (candidate search/rank)
  search/                 # tantivy_index.rs, rank.rs, SearchService trait
  query/                  # NEW: logical.rs (LogicalQuery spec) + sparql.rs (compiler+validator)
  tools/
    mod.rs                # ToolRegistry bridging crate tools ↔ agentstream Tool defs
    search.rs read.rs create.rs update.rs delete.rs
    discover.rs           # NEW: vocab_search/inspect_class/inspect_property/discover_paths/inspect_resource (read-only)
  policy/                 # authz.rs, confirm.rs, audit.rs, ExecutionContext
  agent/                  # worker.rs (serialize per-backend), conversation.rs
  api/                    # routes.rs (POST /chat, POST /tools/{name}, GET /health, POST /admin/reindex)
  observability.rs
tests/
  fixtures/               # ontology_fixture_01..05 + data + ground_truth.json (scope, ref date, expected spec/results/deltas)
  harness.rs              # isolated/interactive/end-to-end runner, run records, replay
  phase0..phase9_*.rs     # per-phase live tests (RUN_MODEL_TESTS=1 for live; recorded playback otherwise)
design/IMPLEMENTATION_PLAN_10PHASE.md  # this file
.env.example
```

---

## Phase 1 — Live-model baseline + `agentstream` + `dotenvy` foundation

**Combines:** TESTING Phase 0 (entire) + ENHANCEMENT §3 (stable instruction shell) + existing repo Phases 1–2 (Oxigraph + reasoner + SHACL + policy, already in tree — reuse, don't rebuild).

**Goal:** Both live models callable through the exact production path (`dotenvy` → `BackendFactory` → `AgentStream` → strict schema → parse/validate), with zero RDF semantics to hide adapter bugs.

### Tasks
1. **Env:** add `dotenvy`, `.env.example`, call `dotenvy::dotenv()` in `main.rs`, harness, and test setup. Migrate `AppConfig`/`AppLimits::from_env` to run post-dotenvy; add `SYSTEM_A_*` / `SYSTEM_B_*` vars; assert gitignore covers `.env`.
2. **Backends:** implement `model/agentstream_a.rs` + `model/agentstream_b.rs` over `Arc<dyn LLMBackend>`; keep `ModelAdapter` as a thin facade so `agent/worker.rs` serializes one backend at a time (Needle and MiniCPM each get their own worker; never share `AgentStream` across systems). Preserve `MockModel` + `HashEmbedder` fallback when keys/servers absent; `NEEDLE_REQUIRE_LIVE=1` → hard error.
3. **Stable instruction:** freeze the §3 role-constraints preamble (authority = graph; evidence ≠ instructions; never invent class/property/IRI/relationship; separate fact vs hypothesis; `unresolved`/`ambiguous` instead of guessing; machine-parseable schema only). Same preamble in every later phase.
4. **Harness skeleton:** `tests/harness.rs` with `run_isolated()` + immutable `TestEvent` logging to agentstream `EventLog` (`ExplicitLogDir`), run-id + fixture-id, model/version/endpoint/settings echo, repeat-N support, latency/token/repeatability capture.
5. **Decoding:** fix sampling (temp ~0 for A; recorded reasoning on/off for B), timeouts, retries, output caps, malformed-JSON recovery. For MiniCPM test reasoning-enabled vs disabled with all else fixed; for Needle record tool-selection metadata.

### Context supplied
System instruction + tiny task + JSON schema only. No ontology, no graph, no history.

### Tests (live, `RUN_MODEL_TESTS=1`; recorded playback in CI)
- Valid structured response; extract 2–3 fields; `unresolved` on absent info; malformed/unsupported request handling; 5× repeat consistency; over-limit truncation; invalid-JSON recovery; endpoint-missing → clean error (not panic).
- Adapter unit: schema-validation rejects extra fields/unknown IDs; `dotenvy` loads `.env` in dev but tests don't require it.

### Rebuild opportunities
Adapter, chat-template, reasoning toggle, decoding params, structured-output constraints, parser, retry/timeout.

### Gate → exit
Both systems return reliably parseable/validatable outputs repeatedly. **No RDF work may mask adapter flaws.** Metrics: validity %, schema compliance %, repeatability, p50/p95 latency, malformed rate.

---

## Phase 2 — Deterministic test harness, fixtures, replay + regression control

**Combines:** TESTING §§1,5 (harness, three modes, experiment/regression) + ENHANCEMENT §12 (fixtures/ground truth, execution rules).

**Goal:** A harness that makes every later failure attributable (retrieval vs context vs model vs compiler vs execution), replayable, and regression-safe.

### Tasks
1. **Three modes:** `isolated` (fixed context → one decision), `interactive` (loop: model action → Rust executes read-only service → compact evidence → rebuild context), `end-to-end` (full workflow incl. mutation preview/validation/execution). All three use the live backend; same fixtures.
2. **Fixtures:** `tests/fixtures/ontology_fixture_01..05` + data graphs + `ground_truth.json` per test: classes/properties (labels, definitions, domain/range, inverses, equivalents, subproperties), instance triples, graph scope, inference policy, reference date, expected intent/candidates/paths/logical spec/result set/mutation set, ambiguity verdicts (choose vs ask). Paraphrases per test that avoid reusing ontology labels.
3. **Records:** versioned test def (`test_id, system, phase, difficulty, model, fixture, input, expected, context_version, workflow_version`) + separate run record (exact messages, actions, output, parsed, validation, execution, elapsed). Never edit expected to make a failure pass — new test version instead.
4. **Regression sets:** phase regression (current phase), foundation regression (sampled earlier phases), safety regression (scope, negation, destructive ops, authz, confirmation) — safety runs after every relevant change.
5. **Evaluator:** deterministic, no LLM-judge. Compares semantic outcomes (IDs, paths, spec, result sets, deltas) + records model/call counts, context sizes, repeated-action rate.

### Tests
- Harness self-tests: replay exact context → identical request bytes; log hydration (`EventLog::load`) preserves causal chain; `verify_prompt_runs` ok; fixture loader validates scope/ref-date/active-definitions present (fail fast if ambiguous term lacks a rule).
- Repeatability: important tests ×N; report mean + worst case; one unsafe mutation = critical failure regardless of mean.

### Gate → exit
Any failing decision is replayable byte-for-byte; regression sets defined and green on main.

---

## Phase 3 — Intent interpretation (phrases first, IRIs never)

**Combines:** TESTING Phase 1 + ENHANCEMENT §§4,11-Level-1 + §12 scoring (intent 15%).

**Goal:** Extract action/entities/relationship-phrases/conditions/output/mutation-flag as **phrases with direction**, preserving `in Ohio`, `excluding X`, temporal phrases, Boolean structure. No IRIs, no SPARQL, no invented properties.

### Tasks
1. **Schema:** `Intent { action, targets[], conditions[{id, subject_phrase, rel_phrase, object_phrase, time?, negated?, bool_op?}], requested_output[], mutation: bool, spans? }`. Allowed actions enum (`find/describe/create/update/delete`). Field for condition IDs + source spans + explicit `unresolved`.
2. **Rust:** `workflow.rs` stores accepted intent separately from resolution; `context.rs` builds intent-extraction prompt (original request + action vocab + schema + phrase-preservation + direction rules). Validate: no IRI-looking values, no dropped conditions, no direction flips.
3. **System A:** single constrained call for simple intents; repeated bounded calls as conditions grow. Measure extraction vs condition count (3 vs 8) and candidate-list independence.
4. **System B:** same structure, must not start discovery in this phase; must preserve complex conditions/negation/temporals.

### Context supplied
Original request + extraction instructions + permitted actions + strict schema. No candidates, no graph, no tools.

### Tests
- 1A simple verbs; 1B multi-entity/rel-phrases; 1C multi-condition/output fields; 1D negation/temporal/Boolean; 1E ambiguous/compound. Each × multiple paraphrases (`employed by` / `work at` / `X's employees` → same structure).
- ENHANCEMENT L1 (1.1–1.5): `find contacts`, `accounts named Acme`, `contacts working for Acme`, prospect classification, `describe` ≠ mutation. Pass = all five exact, zero invented IDs, no read/mutation confusion.

### Rebuild opportunities
Schema, task prompt, field defs, phrase preservation, temporal normalization, validation.

### Gate → exit
No material dropped/invented conditions; ambiguous stays ambiguous. Record A-vs-B extraction limits (where deterministic Rust should take over).

---

## Phase 4 — Vocabulary matching + ontology inspection/evidence

**Combines:** TESTING Phases 2–3 + ENHANCEMENT §§5,11-Level-2 (parts) + scoring (grounding 20%).

**Goal:** Resolve phrases → classes/properties/SKOS concepts using **Rust retrieval + bounded model selection**, with correct type/domain/range/inverse/hierarchy interpretation.

### Tasks
1. **Retrieval (Rust-only, testable without model):** `ontology/vocab.rs` over labels/altLabels/SKOS prefs/comments/synonyms + hierarchy + domain/range compatibility; lexical+semantic rank; caps (default ≤5 for A, ≤10 for B). Every candidate: `candidate_id (opaque), label, altLabels, definition, type (OWL class / obj-prop / datatype-prop / SKOS), super/sub info, domain/range, inverse/equivalent/subproperty, asserted-vs-inferred, graph scope`. Missing = missing (never synthesized).
2. **Inspection serializer:** `reasoner.rs` evidence normalizer (type, hierarchy, domain/range, inverses, equivalents, restrictions/characteristics). Explicit asserted-vs-inferred tags.
3. **Model decisions:** return `candidate_id` or `unresolved` (+ optional `ambiguous[candidates]`); B may request one more targeted inspection but never invent. Validate: ID was supplied; type matches requested role (class vs property vs SKOS); domain/range compatible or escalation.
4. **Provenance:** accepted mappings store `{phrase, concept_id, basis, alternatives, source}` with `ontology_confirmed` only after Rust check.

### Context supplied
Exact phrase + its role + resolved neighbors + candidate list (opaque IDs) + inspection package. No full ontology dump, no unrelated properties.

### Tests
- Exact/alt-label/same-meaning/different-meaning; class-vs-property; class-vs-SKOS (`TechnologyEvent` vs `Technology` subject); no-suitable-candidate; genuinely-ambiguous (must ask, not guess).
- Evidence interpretation: domain/range fit, inverse direction, subclass vs equivalent, subproperty vs equivalent, incompatibility detection, missing-evidence flag.
- A: accuracy vs list size (3/5/10); B: fixed-context vs one-inspection vs iterative (does extra evidence actually help?); reasoning on/off.

### Gate → exit
Correct pick when evidence suffices; explicit `unresolved`/clarify otherwise; zero accepted invented IRIs. Resolver recall measured independently (Rust-only variant).

---

## Phase 5 — Relationship path resolution (the semantic core)

**Combines:** TESTING Phase 4 + ENHANCEMENT §§6,11-Levels-2/3 + scoring (relationship 20%).

**Goal:** Map relationship phrases to **resolver-generated, ontology-supported paths** (incl. multi-hop + intermediates + inverses), distinguishing e.g. `headquarteredIn` vs `operatesIn`, `worksFor` direction, attendance-via-registration. Never shortest-path-by-default, never invented direct property.

### Tasks
1. **Resolver (Rust):** `ontology/paths.rs`: BFS/constrained search over asserted + policy-gated inferred axioms; uses domain/range, inverses, equivalents, subproperties, hierarchy; respects scope + inference policy; cycle-safe; max depth + max paths caps. Output per path: `path_id, ordered hops[{property_id, from, to, direction}], intermediate classes, human meaning, definitions, asserted/inferred flags`.
2. **Selection:** A = choose among ≤3–4 clearly distinguished paths or `unresolved`; B = compare definitions/intermediates, request evidence, or return ambiguity. Both: path must be from resolver output; validate ID + semantic fit; inferred-only paths gated by policy.
3. **Absence handling:** no valid path → report no-supported-path (or ask about alternate concept), never synthesize.
4. **Experiments:** Rust-only (does resolver find + distinguish?), model-only (fixed candidates → correct pick?), integrated (retrieve+present+decide), + model-assisted retrieval (model proposes narrower search, Rust executes/validates) for B.

### Context supplied
Original phrase + resolved endpoints + surrounding conditions + candidate paths with semantic distinctions. No full-graph dump.

### Tests
- Direct / inverse / single-multi-hop / two-semantically-different multi-hops / overlapping defs / no-path / inference-only / structurally-valid-but-semantically-wrong.
- ENHANCEMENT L2 (2.1–2.5: employed-by, HQ vs operating, broad `associated with`, owns-subsidiary direction, employees-via-inverse) + L3 (3.1–3.5: attendance-via-resource, operating-in-Ohio, contact→account→region, order→order-line→product, parent-org→HQ).

### Gate → exit
Selected path exactly matches valid intended semantics with all intermediates; direction correct; ambiguity escalated. Resolver + presentation + model contributions disentangled by the 3-variant experiment.

---

## Phase 6 — Logical query construction, compilation, execution

**Combines:** TESTING Phase 5 + ENHANCEMENT §§7,11-Level-4 + scoring (query 20%).

**Goal:** Lossless intent+paths → **logical spec** (model proposes structure; Rust validates/compiles) → scoped, parameterized SPARQL → correct result set with open-world honesty.

### Tasks
1. **Logical schema** (`query/logical.rs`): small, 1B-usable: `return, where[{operator: relationship|equals|textMatch|numeric|exists|notExists, path|property, value/var, scope, correlation}]`, explicit variable bindings + condition scopes + nested AND/OR/NOT(+XOR where supported). Resource-vs-literal typing enforced.
2. **Compiler** (`query/sparql.rs`, hand-tested without model): injects authorized graph scope/tenant filters (never from model); parameterized terms (no string-concat SPARQL); parse+validate pre-execution; pagination + `truncated` flags. Temporal ops use fixture reference date + stated date property; `active`/`current` require explicit rule.
3. **Model role:** A = deterministic construction from intent+accepted paths; Needle fills only missing bounded fields / picks among interpretations. B = proposes full spec; Rust returns compact validation report for repair. Both: every ref must be an accepted ID; no raw SPARQL in prompt or output.
4. **Negation/open-world:** `NOT EXISTS` scoped to dataset+inference regime; user-facing text states "no matching assertion in scope" not "does not exist in reality"; correlation to parent resource enforced (e.g. workshop-non-registration tied to same event).

### Context supplied
Accepted intent + resolved concepts/paths + operator defs + output request + logical schema + 1–2 minimal examples. No SPARQL syntax.

### Tests
- 5A single class → 5F temporal/nested; compiler unit tests with hand-authored specs (compiler bugs ≠ model bugs).
- ENHANCEMENT L4 (4.1 multi-condition+time; 4.2 XOR vs OR; 4.3 correlated NOT EXISTS; 4.4 scoped absence; 4.5 `associated with` ambiguity → clarify).
- Execution: paginated, empty, duplicate-label, missing-optional, inferred-flagged, conflicting-evidence cases.

### Gate → exit
Every condition preserved with correct bindings/paths/Boolean/temporal/negative scope; executed result set == expected; plausible-but-wrong queries fail.

---

## Phase 7 — Instance search/results + iterative discovery loop (A vs B divergence)

**Combines:** TESTING Phases 6–7 + ENHANCEMENT §§3-rebuild,8 + System A7/B7 expectations.

**Goal:** Compact, handle-based instance evidence + the **bounded observe-decide-act loop** where B earns its keep and A stays Rust-orchestrated. No state loss, no loops, no stale evidence.

### Tasks
1. **Instance services (Rust):** `tools/discover.rs` + `search/` + `read`: `vocab_search, inspect_class, inspect_property, discover_paths, search_instances, inspect_resource`. Returns: `handle_N → (IRI, graph)` + labels/types/selected props/match reason + warnings; targeted field filtering (incl. sensitive-data redaction pre-prompt); pagination; inferred flags.
2. **Loop controller** (`workflow.rs` + `agent/worker.rs` over agentstream): per iteration rebuild context from record (original request, accepted intent, resolved concepts, paths, outstanding questions, last action result, allowed next actions, termination/clarify options, strict schema). A: Rust sequences steps, Needle makes each small decision. B: MiniCPM picks next read-only action / declares sufficiency / asks clarification / declares unsatisfiable. Shared caps: max iterations (8), max ops (32), no-repeat detection, termination guarantees. Tool results fed as next-turn evidence (agentstream `AgentStream::send` continuation), never raw transcript growth.
3. **Result interpretation:** select handle from bounded set or summarize; validate handle ∈ current result + scope; label ≠ identity; empty/inaccessible distinguished (`no match` vs `insufficient evidence` vs `exists but out of scope`).

### Context supplied (per iteration)
7-section package: stable instruction; task+stage; accepted state (provenance-tagged); new evidence (bounded); candidates (opaque IDs); allowed actions; response schema. Old superseded candidates dropped.

### Tests
- 1-resource / N-resources / empty / dup-labels / pagination / missing-optionals / inferred / conflicting.
- Discovery sequences: 1-action; 2 vocab searches; vocab→inspect; path-discovery→selection; failed-search→revised; ambiguity-exposing; proven-no-path; multi-step chains. Assert: completes or legitimately stops; no infinite/ineffective repeats; no forgotten conditions; no stale-evidence decisions.
- B-specific: fixed-context vs 1-step vs iterative on same tasks (accuracy vs tokens/latency); reasoning on/off.

### Gate → exit
Required discovery completes or stops for legitimate reason; worst-case loop behavior bounded and logged. Primary B question answered with numbers: does iterative discovery beat fixed context enough to justify cost?

---

## Phase 8 — Mutation interpretation, preview, validation, confirmation, execution

**Combines:** TESTING Phases 8–9 (safety-sensitive + critical) + ENHANCEMENT §§9–10 (second half) + scoring (workflow/safety 10% + critical failures) + existing `policy/` + `ontology/shacl.rs`.

**Goal:** Exact semantic change proposed by model, **exact triples computed + validated + confirmed + transacted by Rust**. No model-committed, unconfirmed, cross-scope, or destructive-by-accident mutation — ever.

### Tasks
1. **Mutation intent:** `{target_handle(s), semantic_op (add/replace/remove classification|relationship|literal, create concept, delete resource), property/classification candidates, new values, original wording, unconfirmed assumptions}`. Model proposes; Rust resolves: create → SKOS/OWL shape + duplicate check + IRI minting per policy (model never mints); update → add vs replace disambiguation (ask if unclear); delete → exact assertions vs resource vs classification scoping (`remove prospect tag` ≠ `delete accounts`).
2. **Preview + gates (Rust, model has zero authority):** existence/identity; graph+tenant scope; authz (deny-by-default); datatype/domain/range; SHACL; business rules; confirmation-token requirement by risk; transactional commit; post-commit re-read verification. Failure → structured report; model may only explain/interpret, never flip to approved.
3. **UX:** dry-run summary (affected triples + validation + human text) without committing; approve/reject flow; no-op detection (already-satisfied → no write + audit `noop`).

### Context supplied
Original request + accepted intent + resolved handles + current assertions + candidate targets + requested values + add-vs-replace-vs-delete distinction + "proposing, not committing" instruction. For A: extract op + pick among resolved targets. For B: complex multi-resource proposals, same validation.

### Tests
- Proposal: create / add-tag / replace-tag / remove-tag-not-resource / add-rel / remove-rel / literal-update / ambiguous (must clarify) / noop.
- Validation matrix (10): valid; bad prop/value; missing target; unauthorized; cross-graph; SHACL violation; conflicting assertion; reject-confirm; approve-confirm; crash-mid-commit → safe recovery, no `committed` phantom.
- ENHANCEMENT L5 spot-checks (5.1–5.5) at proposal level; full E2E in Phase 9.

### Gate → exit
Proposal == requested change, nothing more; **zero** unauthorized/unconfirmed commits across all runs; post-commit graph verified; audit has exactly one record per attempt (`committed/denied/validation-failed/noop`). One unsafe commit = phase fail.

---

## Phase 9 — Full read-and-mutate workflows + dual-system L1–L5 evaluation

**Combines:** ENHANCEMENT §§10–13 (end-to-end behavior, execution rules, 6-dim scoring, A/B comparison, 25×2 matrix) + TESTING §§3–4,6,8 (loops, gates, capability boundary).

**Goal:** Prove the complete `discover → read → query → propose → validate → confirm → verify` chain on the canonical Level-5 request, then score **all 25 tests on both systems independently** and publish the capability boundary.

### Tasks
1. **Workflow state machine:** harden `workflow.rs` stages (`extract → vocab → inspect → paths → query-spec → execute → instances → mutation-proposal → preview → confirm → commit → verify`) with allowed transitions, required evidence per stage, and guard rails (can't skip validation, can't change scope, can't reinterpret result as authorization). Persist record + per-call contexts for post-mortem (which stage failed: retrieval/context/model/validation/execution).
2. **Canonical E2E:** `Find all contacts working for accounts in Ohio that attended a technology conference during the past year, excluding accounts with active opportunities, and mark the remaining accounts as prospects.` Preserve: read+mutation duality; `in Ohio` unresolved until HQ-vs-operating evidence; attendance representation; past-year → reference-date rule; active-opportunity → status/date rule; `mark` ≠ auto-commit (follow mutation policy + confirmation).
3. **Evaluation runs:** L1 (1.1–1.5) → L5 (5.1–5.5) per ENHANCEMENT §11, identical fixtures/scope/outcomes for A and B, independent conversations, no answer leakage across levels (L4 gets no free L3 path). Paraphrase repeats without ontology labels.
4. **Scoring:** 6 dims (intent 15 / grounding 20 / relationship 20 / query 20 / uncertainty 15 / workflow-safety 10; 0–5 each; weighted/5). Report per-level, not one global accuracy. Track critical failures separately (invented IRI, contradicted relation, dropped condition, absence-as-proof, scope crossing, resource-vs-tag deletion, unconfirmed commit). Also track: exact-semantic accuracy, evidence-grounded accuracy, clarification precision/recall, unsupported-decision rate, calls/success, repeated-action rate, context size/call, latency/phase, regression pass rate.
5. **Capability boundary:** for key tasks run Rust-only / model-only / integrated (+ model-assisted retrieval for B) to attribute credit correctly.

### Tests (the 25 + paraphrases + safety)
As specified in ENHANCEMENT §§11–13; evaluator asserts semantic outcome + selected IDs + paths + logical spec + result set + deltas + clarification behavior + call counts. Expected: A strong L1–L2, measured L3 with Rust-driven retrieval, limited L4–L5 without guessing; B strong L2–L4, measured L5 with clarification/confirmation discipline. Both judged on **knowing when evidence is insufficient**.

### Gate → exit
Per-level scorecards published for A and B; no critical failures; every failure traced to retrieval/context/model/compile/execution; contexts retained. Answers: how far each model can safely go before Rust/user must intervene.

---

## Phase 10 — Hardening, regression, release

**Goal:** First usable release: `discover → read → authorized-op → result → decide` with transactions, SHACL, authz, audit, hybrid search, dual-model support, and guarded non-goals — all green without weights (recorded playback) and with live models.

### Tasks
1. **E2E suite** (`tests/e2e/`, shared `E2eApp::spawn()` over real Axum + worker + Oxigraph + Tantivy tempdirs + live-or-recorded agentstream backend): E2E-01 search→read; -02 ambiguous→refine; -03 IRI traversal; -04 create→search-visible; -05 update round-trip + audit delta; -06 SHACL reject (graph unchanged); -07 cross-tenant deny; -08 delete dry-run→confirm→gone; -09 multi-step ordering in persisted history; -10 malformed-tool recovery; -11 restart persistence + rebuild; -12 10-parallel serialization (concurrency==1 per worker, unique request_ids); -13 literal/IR I fidelity over HTTP; -14 non-goals (`run SPARQL` / `edit ontology` refused, ontology hash unchanged). `#[serial]` where live singleton; `RECORDED=1` playback for CI.
2. **Performance/chaos:** 10k-doc index build + p95 search (<100 ms tune-once); writer-under-load; worker kill-restart (no phantom `committed`); input caps + timeout/iteration backstops; rebuild CLI + backup notes.
3. **Regression forever:** phase + foundation + safety sets in CI; benchmark-accuracy floor (tool-selection ≥80% initial, raise as tuned); hybrid recall@5 ≥ lexical and ≥ vector alone; golden snapshots (violations, candidates, final responses) fail on drift.
4. **Docs:** `README` quickstart (`cp .env.example .env`, set keys, `cargo run`), `OPERATIONS.md` (endpoints, rebuild, backup), capability-boundary note (when to use A vs B vs Rust-only vs ask-user).

### Acceptance (release gate)
- [ ] `cargo test` + `cargo test --test e2e` + `RECORDED=1` E2E green without weights; live (`RUN_MODEL_TESTS=1`) green with both backends.
- [ ] No `cactus-rs`/custom-FFI code paths; `agentstream` is the only LLM lib; `dotenvy` loads all env.
- [ ] v1 non-goals asserted by absence: no planner/DAG, no model SPARQL, no model ontology edits.
- [ ] Scorecards + recall tables + audit samples published from Phase 9 runs.

---

## Appendix A — Test execution rules (from both docs, binding)

- Same request + equivalent initial evidence for A and B; independent state; no cross-system leakage; no free prior-level answers.
- Fixtures pin: ontology, data, scope, inference policy, reference date, authz context, expected spec/results/deltas, ambiguity verdicts. Negative/temporal/business terms (`active`, `prospect`, `former customer`) must have explicit rules or the test is invalid.
- Score semantic correctness, not JSON validity or tool-call fluency. Necessary clarification ≠ failure; lucky guess on insufficient evidence = failure.
- Record per test: outcome, IDs, paths, logical spec, results, deltas, clarification, invalid decisions, call counts, confusion of proposed-vs-confirmed, full per-call contexts.

## Appendix B — Failure diagnosis order (TESTING §4, mandatory)

1. Replay failing decision in isolation with recorded context (prompt/evidence/capability vs nondeterminism).
2. Inject expected answer → run rest of pipeline (model vs downstream fault).
3. Swap in known-good evidence (retrieval/serializer vs decision fault).
4. Diff actual vs intended context (phrase/state/candidate/ambiguity preservation).
5. Change **one** component only; rerun failing + phase + foundation + safety regressions.

Categories → component: extraction→prompt/schema; retrieval→vocab search; selection→context/decision; inspection→serializer/model; path-miss→resolver; path-pick→descriptions/model; spec→representation; compile→compiler; execution→dataset/engine; result-pick→result context; loop→controller; mutation-scope→interpretation; commit-without-confirm→authz/transaction layer.

## Appendix C — Phase-gate summary

| Phase | Gate |
|---|---|
| 1 | Reliable structured I/O on both live backends via agentstream+dotenvy |
| 2 | Replayable harness + pinned fixtures + regression sets green |
| 3 | Intent complete, directed, IRI-free; ambiguity preserved |
| 4 | Correct candidate or explicit unresolved; zero invented IRIs |
| 5 | Valid, semantically-correct paths with intermediates; no guessing |
| 6 | All conditions preserved; result set == expected; open-world honest |
| 7 | Discovery completes or legitimately stops; no loops/state-loss/stale use |
| 8 | Exact proposals; zero unauthorized/unconfirmed commits; verified graphs |
| 9 | L1–L5 scorecards for A+B published; boundaries known; zero critical failures |
| 10 | Release E2E + perf + chaos + docs green (live and recorded) |
