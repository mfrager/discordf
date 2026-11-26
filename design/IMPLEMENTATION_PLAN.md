# Needle 3 RDF/OWL Agent — Implementation Plan (7 Phases)

Source design: `design/DESIGN.md`
Stack: Rust + `cactus-rs = "0.2"` (Needle 3) + `oxigraph` (authoritative RDF store) + `tantivy` (search engine) + `serde` / `serde_json` + `tokio` + `axum` + `tracing`

> Architectural invariants (from DESIGN.md, enforced in every phase):
> 1. One agent execution loop, one tool registry, one RDF repository trait, one search subsystem.
> 2. Needle 3 is the decision-making tool router only: interpret request → choose operation → supply arguments. All deterministic graph semantics live in Rust.
> 3. Tools (`search`, `read`, `create`, `update`, `delete`) depend on the `RdfRepository` trait, never directly on Oxigraph.
> 4. RDF store is authoritative; Tantivy index is disposable and rebuildable.
> 5. No SPARQL from the model. `read` is resource-oriented with bounded traversal.
> 6. One process-wide `Needle` instance, serialized through a dedicated agent worker; `reset()` between independent sessions.

---

## Project module layout (target)

```
src/
  main.rs                 # binary entry, config, startup
  lib.rs                  # re-exports for tests
  config.rs               # env/file config, limits (8 iterations, result caps)
  agent/
    mod.rs                # execution loop orchestration
    worker.rs             # serialized Needle worker (mpsc + oneshot, single instance)
    conversation.rs       # app-owned durable history vs model conversation
  model/
    mod.rs                # ModelAdapter trait
    needle.rs             # cactus-rs adapter: complete(), embed(), grounding check
  tools/
    mod.rs                # ToolRegistry, ToolDef, dispatch, arg validation
    search.rs read.rs create.rs update.rs delete.rs
  rdf/
    mod.rs                # RdfRepository trait, Term, TripleDelta, error types
    oxigraph_impl.rs      # Oxigraph Store implementation, transactions
    named_graph.rs        # GraphName / tenant isolation helpers
  search/
    mod.rs                # SearchService trait: query → ranked URIs
    tantivy_index.rs      # Tantivy schema, writers, readers, rebuild
    embeddings.rs         # text normalization + Needle::embed() 3072-d + cache
    rank.rs               # lexical+vector hybrid scoring, filters
  ontology/
    mod.rs                # OntologyService trait
    loader.rs             # ontology graph loading at startup
    reasoner.rs           # subclass/subproperty, domain/range, inverses, inferred vs asserted
    shacl.rs              # SHACL validation before commit
  policy/
    mod.rs                # ExecutionContext (principal, tenant, graphs, ops)
    authz.rs              # authorization checks
    confirm.rs            # dry-run summaries, destructive-op confirmation
    audit.rs              # audit log record + store
  api/
    mod.rs routes.rs      # Axum HTTP API (if enabled)
  observability.rs        # tracing setup
tests/                    # integration + e2e (see phases)
benches/                  # search + repo micro-benchmarks
```

Recommended additions to `Cargo.toml`:

```toml
cactus-rs = "0.2"
oxigraph = "0.4"
tantivy = "0.22"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tokio = { version = "1", features = ["full"] }
axum = "0.7"
tracing = "0.1"
tracing-subscriber = "0.3"
thiserror = "1"
anyhow = "1"
```

---

## Phase 1 — Foundation: RDF Repository + Transactions + Named Graphs

**Goal:** Authoritative Oxigraph-backed knowledge store behind a stable trait, with strict RDF term typing. No model, no search, no tools yet.

### Tasks
1. Define core types in `rdf/mod.rs`:
   - `Term`: `Iri(String)` | `BlankNode(String)` | `Literal { value, datatype, lang }`. Never conflate IRI-objects with string literals.
   - `Triple`, `GraphName`, `TripleDelta { added, removed }`, `RdfError` (`thiserror`).
   - `RdfRepository` trait: `search_stub` (placeholder), `read`, `create`, `update`, `delete`, `list_graphs`, transaction-scoped `commit` semantics.
2. Implement `oxigraph_impl.rs` on `oxigraph::Store`:
   - Dataset with named graphs for projects/tenants/ontologies vs. app data.
   - Atomic mutations: each `create`/`update`/`delete` in a transaction boundary; return exact `TripleDelta`.
   - Enforce result caps (max triples / resources / traversal paths) at the repo layer.
3. Add `config.rs` limits (iteration cap 8, ops-per-request cap, read depth default, pagination defaults).
4. Wire `tokio` runtime + `tracing` subscriber skeleton.

### Tests (Phase 1)
- **Unit — term typing:** IRI relationship preserved as IRI, not coerced to literal; lang-tagged vs typed literals round-trip.
- **Unit — CRUD:** create/read/update/delete single resource; verify `TripleDelta` exactness.
- **Unit — named-graph isolation:** same URI in graph A vs B do not leak; `list_graphs` correct; ontology graph separated from data graph.
- **Unit — transaction atomicity:** injected failure mid-batch rolls back fully (no partial triples); double-commit is idempotent-safe.
- **Unit — blank nodes:** blank-node subjects/objects preserved, not skolemized accidentally.
- **Integration — persistence:** reopen Store from disk, data survives; concurrent readers don't block writer incorrectly.
- **Property test:** random triple batches → create then read returns identical term set.

### Acceptance criteria
- [ ] All repo operations go through trait; no Oxigraph imports outside `rdf/`.
- [ ] Transaction rollback test passes; deltas exactly match committed triples.
- [ ] Named-graph isolation test passes.

---

## Phase 2 — Ontology, OWL Reasoning View, SHACL Validation, Policy & Audit

**Goal:** Ontology management + mutation safety + authorization before any model can mutate anything. Rust owns correctness.

### Tasks
1. `ontology/loader.rs`: load ontology graphs at startup into dedicated named graph(s); keep ontology definitions separate from app data.
2. `ontology/reasoner.rs` behind `OntologyService` trait:
   - Class hierarchy, subclass/subproperty closure, domain/range, inverse, equivalentClass.
   - Inferred statements computed on demand, stored/marked separately from asserted statements (never persist inferred as asserted).
3. `ontology/shacl.rs`: SHACL validation service compatible with Oxigraph stack; validate before commit. Example: `Contact` requires `name`, allows N `email`, `account` range must be `Account`.
4. `policy/`:
   - `ExecutionContext { request_id, principal, tenant, allowed_graphs, permitted_ops }`.
   - `authz.rs`: deny-by-default; graph allowlist + op allowlist checked in every mutation handler.
   - `confirm.rs`: dry-run mode → compute affected triples + validation result + human-readable summary without committing. Destructive ops (delete resource, predicate-wipe, graph-clear) require explicit confirmation token.
   - `audit.rs`: append-only log `{ request_id, principal, target_uri, graph, op, delta, validation_result, commit_status }`. Model success messages never count as commit proof — only audit `committed=true`.
5. Integrate validation+authz into repo mutation path (validate → authorize → dry-run-or-commit → audit).

### Tests (Phase 2 — wide variety)
- **Unit — hierarchy:** subclass transitive closure (A⊂B⊂C implies A⊂C); equivalent-class symmetry.
- **Unit — domain/range:** property with domain `Contact` rejects subject typed `Account` (or flags violation per shape severity).
- **Unit — inverses:** `ex:employs` ↔ `ex:employedBy` entailment visible in reasoned view, absent in asserted view.
- **Unit — SHACL pass/fail:** valid `Contact` commits; missing `name` fails with structured violation; `account → non-Account IRI` fails.
- **Unit — violation shape:** error payload contains shape ID, focus node, path, severity, message (assert exact fields).
- **Unit — authz matrix:** table test over (principal × graph × op) — allowed vs denied; cross-tenant access denied.
- **Unit — dry-run:** dry-run returns identical delta to real commit but changes nothing; second dry-run is identical (determinism).
- **Unit — destructive gate:** delete without confirmation token rejected; with token proceeds (when authorized+valid).
- **Integration — audit:** every mutation (commit, dry-run, denied, validation-failed) appends exactly one audit record with all fields; audit store survives restart.
- **Fuzz/property:** random resources against shapes — validator never panics, always returns typed result.
- **Snapshot:** serialize representative violations + audit records to JSON snapshots; fail on unexpected schema drift.

### Acceptance criteria
- [ ] No mutation path bypasses authz + SHACL + audit.
- [ ] Inferred triples never persisted as asserted (tested by reopen-and-diff).
- [ ] Authz matrix 100% passing.

---

## Phase 3 — Search Subsystem on Tantivy (Lexical + Vector Hybrid)

**Goal:** Separate search subsystem turning natural-language queries into ranked resource URIs, backed by **`tantivy`**. RDF store stays authoritative; index is disposable.

> Must use `tantivy` crate for the search engine (lexical core). Vector similarity is computed in Rust over embeddings stored alongside / derived from the Tantivy docs.

### Tasks
1. `search/tantivy_index.rs`:
   - Tantivy schema: `uri (STORED|FAST)`, `graph (STRING|FAST)`, `type_uris (STRING)`, `label (TEXT|STORED)`, `comment/description (TEXT|STORED)`, `local_name (TEXT)`, `annotations (TEXT)`, `embedding_id (STRING|STORED)` + `updated_at (FAST)`.
   - Tokenizers: default + lowercase + ascii-folding; optional CJK/edge-ngram field for local names. Record tokenizer choice in code.
   - Single `IndexWriter` (50MB heap initial) + `ReloadPolicy::OnCommitWithDelay`; `IndexReader` reopened per search; commit on every CRUD batch; full `rebuild()` from repo scan.
2. `search/embeddings.rs`:
   - Normalized text: `label + " " + comment + " " + types + " " + selected relation labels`.
   - `Needle::embed()` → 3072-d `Vec<f32>` (via `cactus-rs`); L2-normalize; in-memory + on-disk cache keyed by text hash to avoid re-embedding unchanged resources. Pluggable stub embedder behind trait for tests without model weights.
   - Store vectors in a sidecar (e.g., `Vec<f32>` per doc in a sled/json file or in-memory map persisted alongside Tantivy) keyed by `embedding_id`; keep Tantivy authoritative for lexical, sidecar for vectors (document rationale).
3. `search/rank.rs` + `search/mod.rs` (`SearchService` trait):
   - Signature: `search(query, context_graph?, namespace?, limit?) → Vec<Candidate { uri, label, type, description, score, provenance }>` where `provenance ∈ {lexical, vector, hybrid}`.
   - Pipeline: apply namespace + named-graph filters first → Tantivy BM25 top-N → vector cosine top-N → weighted hybrid (default 0.5/0.5, configurable) → return compact candidates only (never full graphs).
4. Index maintenance: hook into create/update/delete → upsert/delete Tantivy doc + embedding; background `rebuild()` command for recovery.

### Tests (Phase 3)
- **Unit — schema/tokenizer:** ascii-folding (`café` matches `cafe`), case-insensitivity, local-name match (`ex:JohnSmith` found by `john smith`).
- **Unit — CRUD sync:** create → searchable within commit; update → stale terms gone; delete → doc gone; rebuild-from-repo produces identical doc count.
- **Unit — filters:** namespace filter excludes other namespaces; graph-context filter excludes other tenants.
- **Unit — ranking:** exact-label match outranks partial; BM25 + cosine fusion deterministic given fixed vectors (seeded stub embedder).
- **Unit — compactness:** candidate contains only allowed fields; assert no triples leak into search results.
- **Integration — hybrid quality:** small fixture (30–50 resources, labels/descriptions/types) × ~20 queries; assert recall@5 lexical vs vector vs hybrid; hybrid ≥ each alone (record table in test output).
- **Integration — ambiguous query:** `apple` returns both `Fruit` and `Company` types with scores; namespace filter disambiguates.
- **Performance:** 10k docs index build time + p95 search latency assertions (e.g., p95 < 100ms on CI hardware, tune threshold once measured); writer commit under load doesn't lose docs.
- **Property:** random labels/descriptions → index/search never panics; every created URI eventually searchable after commit.
- **Snapshot:** golden `Candidate` JSON for 5 canonical queries.

### Acceptance criteria
- [ ] `tantivy` is the lexical engine (no hand-rolled inverted index).
- [ ] Deleting index dir + `rebuild()` restores identical search results.
- [ ] Hybrid recall@5 ≥ lexical-only and ≥ vector-only on fixture set.

---

## Phase 4 — Five Tools + Needle 3 Execution Loop (cactus-rs)

**Goal:** Register `search`, `read`, `create`, `update`, `delete` with Needle 3 and run the single 8-iteration orchestration loop. Model proposes; Rust disposes.

### Tasks
1. `tools/`: strict JSON schemas (schemars or hand-written `serde_json::json!` schemas) + concise descriptions (when-to-use, args, returns; no ontology dumps). One Rust handler per tool, all taking `ExecutionContext`:
   - `search { query, context?, namespace?, limit? }` → delegates to Phase 3.
   - `read { uris[], context?, predicates?, depth? }` → depth 0/1 direct props; deeper follows selected rels with triple/resource/path caps; preserves literal-vs-IRI distinction; returns actual URIs for follow-ups.
   - `create { uri?, graph, types[], properties[] }`, `update { uri, graph, add[], remove[] }`, `delete { uri | triples, graph, confirmation_token? }` → all via repo + Phase 2 policy gates.
   - Common `ToolRegistry: name → (schema, handler)`; unknown-tool and arg-validation errors are structured.
2. `model/needle.rs` (`cactus-rs`):
   - Build one process-wide `Needle` at startup via builder; load Needle 3 weights; set system instructions; register five tool JSON schemas.
   - Async adapter: `complete(user_text) → Completion::{ ToolCalls(Vec<ToolCall{name, args, id, assistant_content}>), Done(final_text) }`; `embed(text) → [f32; 3072]`; expose grounding-validation signal; `reset()` for new sessions.
   - Normalize calls: parse args with `serde`; reject malformed args before dispatch.
3. `agent/mod.rs` loop (manual — `cactus-rs` exposes `complete()`, not `agent.run()`):
   - Per request: `complete()` → for each tool call: validate args → grounding check (reject invented/unsupported values before sensitive ops) → registry dispatch with `ExecutionContext` → serialize compact structured result → feed back as next turn. Preserve original request + accumulated results. Stop on no-more-calls or 8 iterations or ops-per-request cap. Tool failure → structured `{ tool, category, message }`, let Needle retry/search-elsewhere/explain.

### Tests (Phase 4)
- **Unit — schemas:** each tool rejects missing/extra/wrong-typed args; error shape `{ tool, category, message }`.
- **Unit — registry:** unknown tool name → typed error, no panic; arg JSON → `serde` struct round-trip for all five tools.
- **Unit — read bounds:** depth 0 returns only direct props; depth 2 follows allowlisted predicates; oversized fan-out truncated at caps with `truncated: true` flag.
- **Unit — read fidelity:** literal vs IRI-reference preserved; references return real URIs usable in next `read`.
- **Unit — loop:** mocked `ModelAdapter` script (search→read→update→done) verifies ordering, result accumulation, iteration-cap cutoff at 8, ops-cap enforcement.
- **Unit — grounding gate:** invented URI/unsupported enum rejected pre-execution for create/update/delete; search/read still allowed.
- **Unit — error path:** failing tool (e.g., SHACL violation) fed back; mock model retries with corrected args and succeeds.
- **Integration — no-model tools:** all five tools exercised via plain Rust calls (no weights) against fixture repo; representative args × expected results.
- **Contract — prompt:** tool descriptions contain when-to-use + args + returns; assert no description exceeds size budget (keeps prompts lean).

### Acceptance criteria
- [ ] Five tools callable without model weights (pure Rust tests green).
- [ ] Mock-model multi-step run completes in correct order within 8 iterations.
- [ ] Only dependency for inference is `cactus-rs`; no custom FFI.

---

## Phase 5 — Conversation Management, Lifecycle, HTTP API & Observability

**Goal:** Durable app-owned history, serialized single-instance lifecycle, Axum API, progress reporting (no token streaming in Needle).

### Tasks
1. `agent/conversation.rs`: persist `{ user msgs, tool calls, tool results, final responses }` in app DB (SQLite via `sqlx`/`rusqlite` or Oxigraph graph — pick one, document); reconstructable sessions; model conversation kept separate.
2. `agent/worker.rs`: dedicated agent worker owning the single `Needle` instance (tokio `mpsc` queue + `oneshot` replies); concurrent HTTP requests serialized; `reset()` + re-supply relevant context between independent sessions. Document: separate worker processes if true multi-instance ever needed.
3. `api/`: `axum` routes `POST /chat`, `POST /tools/{name}` (admin/debug), `GET /health`, `POST /admin/reindex`; inject `ExecutionContext` from auth headers (principal/tenant stub → real auth later); per-request progress events (SSE or polling) from app layer since Needle has no streaming.
4. `observability.rs`: `tracing` spans per request/tool/repo-call; log iteration counts, scores, validation outcomes; redact literals marked sensitive.
5. Final-response assembly: app summarizes completed ops + results + residual errors (Needle is a tool router, not the narrator); optional hook for a larger synthesis model later without changing tool architecture.

### Tests (Phase 5 — wide variety)
- **Unit — history:** store → reload → identical transcript; tool calls/results paired by call ID.
- **Unit — worker serialization:** 20 concurrent requests through worker execute serially (assert via atomic counter max-concurrency == 1); sessions don't leak context (reset verified by differing follow-up behavior).
- **Integration — API:** `POST /chat` happy path (mock model) returns summarized ops; `/health` 200; unknown route 404; oversized body rejected.
- **Integration — auth plumbing:** missing principal → 401/denied; wrong tenant → cross-graph read denied (ties to Phase 2 authz).
- **Integration — progress:** long multi-tool run emits ≥2 progress events before final response.
- **Load/chaos:** kill worker mid-request → request fails cleanly, worker restarts, next request succeeds; audit log has no `committed` phantom for the killed op.
- **Snapshot:** golden final-response JSON for 3 canonical multi-step runs.

### Acceptance criteria
- [ ] Concurrent-request test proves max model concurrency is 1.
- [ ] History round-trip + API integration tests green.
- [ ] No token-streaming assumptions in code (single completed inference per turn).

---

## Phase 6 — Integration Benchmark: Natural-Language Accuracy + Search Comparison

**Goal:** Prove the dependable cycle — Needle discovers → reads → authorized-op → result → decides — with measured accuracy, before hardening.

### Tasks
1. Build NL benchmark (model weights required; gate behind `RUN_MODEL_TESTS=1`):
   - Categories: simple search, ambiguous search, relationship discovery, create, property update, multi-step (search→read→update), reject/deny (SHACL-violating, unauthorized, destructive-without-confirm).
   - Record per case: tool-selection accuracy, argument accuracy, URI accuracy, operation ordering, final graph correctness.
2. Search comparison: lexical vs vector vs hybrid on same fixture; publish recall@k table.
3. Failure taxonomy: retryable (transient) vs correctable (bad args → model can fix) vs terminal (auth/validation) — assert loop behavior per class.
4. Fix iteration: tune tool descriptions, result compactness, hybrid weights from measurements.

### Tests (Phase 6 — benchmark as tests)
- **Benchmark — simple/ambiguous:** `find Contact john` → correct URI; `apple` + namespace disambiguation.
- **Benchmark — relationships:** `who works for Account X` traverses correct predicate, returns real URIs.
- **Benchmark — mutations:** `create Contact …`, `change email of …` produce exact expected deltas; unauthorized principal's mutation denied and benchmark asserts denial (not success).
- **Benchmark — multi-step:** `find Alice then set her account to Acme` asserts search→read→update order and final triple state.
- **Benchmark — rejections:** SHACL-invalid create rejected with violations surfaced; delete without confirmation rejected; model explains rather than hallucinating success.
- **Metrics test:** aggregate accuracy printed + asserted above floor (e.g., tool-selection ≥ 80% initial; raise floor as tuned — record actuals).
- **Regression:** benchmark fixtures versioned; any accuracy drop fails CI (when model tests enabled).

### Acceptance criteria
- [ ] Benchmark runs with one command; accuracy table printed.
- [ ] Hybrid search comparison numbers recorded.
- [ ] All rejection cases correctly denied with structured errors.

---

## Phase 7 — End-to-End Tests, Release Hardening & First Usable Release

**Goal:** Full user-to-graph E2E coverage plus the non-goals guardrail: no autonomous planning, no autonomous ontology modification in v1.

**E2E harness:** spin up real binary (Axum + worker + Oxigraph + Tantivy tempdir + real or recorded `cactus-rs` model) and drive `POST /chat`; assert HTTP response + final RDF graph + audit log + search visibility. Provide `tests/e2e/` with a shared harness (`E2eApp::spawn()`) and per-file scenarios. Support `RECORDED=1` playback mode so CI without weights still runs E2E via recorded model transcripts.

### Tasks
1. Harness + fixtures: seed ontology (Contact/Account + SHACL shapes) + sample data; temp dirs for Oxigraph + Tantivy; isolated tenant graphs per test.
2. Write the E2E suite below; enforce `#[serial]` (single Needle instance) where model is live.
3. Hardening: input size caps, timeout + iteration backstop, index rebuild CLI, backup/restore notes, load smoke (N concurrent chats serialize correctly), docs (`README` quickstart, `OPERATIONS.md`).
4. Explicit v1 non-goals (assert by absence): no planner/workflow engine, no model-written SPARQL, no model-edited ontology graphs.

### E2E tests (Phase 7 — full variety)
- **E2E-01 search→read happy path:** `find Alice` → 200, correct URI in response, no mutation in audit log.
- **E2E-02 ambiguous + refine:** `apple` returns both candidates → follow-up `the company one` reads correct URI.
- **E2E-03 relationship traversal:** `who is Alice's account` follows IRI (not literal) to `Account/Acme`; assert returned URI dereferenceable via `read`.
- **E2E-04 create + search visibility:** create `Contact/Bob` via chat → subsequent `find Bob` discovers it (Tantivy sync proven end-to-end).
- **E2E-05 update round-trip:** change Bob's email → `read` shows new literal, old gone; audit has one `committed` record with exact delta.
- **E2E-06 SHACL rejection:** create `Contact` without `name` → 200 with explained failure, graph unchanged, audit records `validation-failed`.
- **E2E-07 unauthorized:** tenant B principal mutating tenant A graph → denied; cross-tenant `read` denied; audit records `denied`.
- **E2E-08 destructive gate:** `delete Alice` without confirmation → dry-run summary returned, triples intact; resend with confirmation token → deleted + subsequent search omits Alice.
- **E2E-09 multi-step:** `move Alice to Acme account` → asserts search→read→update sequence in persisted history and final graph state.
- **E2E-10 error recovery:** first tool call malformed (stub) → model corrects and completes; response notes transient failure, final state correct.
- **E2E-11 persistence:** restart app mid-suite → prior commits + audit + history survive; search rebuild still finds data.
- **E2E-12 concurrency:** 10 parallel chats serialize (no 500s, no mixed transcripts, audit request_ids unique).
- **E2E-13 type fidelity:** relationship stored as IRI-object verified via raw repo dump (not string literal); lang/typed literals round-trip over HTTP JSON.
- **E2E-14 non-goals:** attempt `run SPARQL …` / `edit the ontology …` → refused or routed to safe alternative; assert ontology graph hash unchanged.

### Acceptance criteria (release gate)
- [ ] All E2E pass with live model; `RECORDED=1` subset passes without weights.
- [ ] `cargo test` (unit+integration) + `cargo test --test e2e` + benchmark smoke all green.
- [ ] First usable release demonstrates: discover → read → authorized-op → result → decide, with transactions, validation, authz, audit, and Tantivy hybrid search.

---

## Test-strategy summary (variety checklist)

| Type | Where | Examples |
|---|---|---|
| Unit | Phases 1–5 | term typing, CRUD, SHACL shapes, authz matrix, tokenizer, rank fusion, schemas, worker |
| Integration | Phases 1–5 | persistence reopen, audit restore, hybrid recall@5, tool-without-model, API routes |
| Property / fuzz | Phases 1–3 | random triples, random shapes, random labels |
| Snapshot / golden | Phases 2,3,5 | violations, candidates, final responses |
| Performance | Phase 3, 5 | 10k-doc index, p95 search, concurrent chats |
| Concurrency / chaos | Phases 1,5,7 | Store contention, worker kill-restart, parallel E2E |
| Benchmark / accuracy | Phase 6 | tool/arg/URI accuracy, ordering, graph correctness |
| End-to-end | Phase 7 | E2E-01…E2E-14 via live HTTP + graph + audit assertions |

Run: `cargo test` → `cargo test --test e2e` → `RUN_MODEL_TESTS=1 cargo test -- --include-ignored` (live Needle) → search recall report.
