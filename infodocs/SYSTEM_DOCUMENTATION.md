# Needle RDF Agent — System Documentation

**Package:** `needle-rdf-agent` (directory `discordf/`)
**Version:** 0.1.0
**Edition:** Rust 2021

---

## 1. System Capabilities

The Needle RDF Agent is a **knowledge-grounded agent** that combines an authoritative RDF/OWL triple store with a tool-using LLM loop. It is designed for multi-tenant, policy-governed knowledge management with strict separation between schema (ontology) and data.

### 1.1 Core Capabilities

| Capability | Description |
|------------|-------------|
| **RDF Knowledge Store** | Oxigraph-backed repository with named-graph isolation (tenant graphs + shared ontology graph). Atomic multi-quad transactions with exact delta reporting. |
| **OWL Reasoning View** | On-demand subclass/subproperty closure, domain/range, inverse properties, equivalent classes. Inferred types computed at query time; never persisted. |
| **SHACL Validation** | Pre-commit shape validation (min/max count, datatype, class-range, domain checks) against staged state. Structured violations with five fixed fields. |
| **Hybrid Search** | Tantivy lexical index (BM25) + vector sidecar (cosine similarity). Configurable alpha fusion. Disposable index rebuildable from authoritative repo. |
| **Five CRUD Tools** | `search`, `read`, `create`, `update`, `delete` — all grounded, authorized, validated, and audited. |
| **Serialized Agent Loop** | Single-model worker (Needle 3 via `cactus-rs`) executing max 8 iterations / 32 ops per request. Progress events per tool call. |
| **Policy Gates** | Deny-by-default authorization (principal + tenant + graph allowlist + operation allowlist). Destructive ops require confirmation token. |
| **Audit Log** | One append-only record per mutation attempt (committed, dry-run, denied, validation-failed). |
| **Conversation Log** | Durable session transcripts (user messages, tool calls/results, final responses) with file-backed JSON persistence. |
| **HTTP API** | Axum server: `POST /chat`, `POST /tools/:name`, `GET /health`, `POST /admin/reindex`. 64 KiB body limit. |

### 1.2 Design Invariants (from DESIGN.md)

1. **Tools and agent code depend on `RdfRepository` trait, never on Oxigraph directly.**
2. **IRIs, blank nodes, language-tagged and typed literals are distinct variants** — a relationship between two resources is an IRI object, never a string.
3. **The store is a dataset of named graphs** — ontology graphs stay separate from application data.
4. **Every mutation is atomic and reports exactly which quads changed.**
5. **RDF store is authoritative; Tantivy index is disposable and rebuildable.**
6. **Inferred statements are computed on demand and never persisted as asserted.**
7. **SHACL validation runs before commit; violations are structured.**

---

## 2. Exact Internal Processing

### 2.1 Architecture Overview

```
┌─────────────────────────────────────────────────────────────────┐
│                        HTTP API (Axum)                          │
│  POST /chat  →  WorkerHandle  →  Serialized Worker Task         │
│  POST /tools/:name  →  ToolRegistry.dispatch()                  │
│  GET /health  /  POST /admin/reindex                            │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                      Agent Loop (Agent::run_request)            │
│  1. model.complete(input) → ModelCompletion                     │
│  2. If ToolCalls: validate args → registry.dispatch()           │
│  3. Feed results back → next iteration (max 8)                  │
│  4. If Done(text): assemble FinalResponse → return              │
└─────────────────────────────────────────────────────────────────┘
                              │
              ┌───────────────┼───────────────┐
              ▼               ▼               ▼
       ┌─────────────┐ ┌─────────────┐ ┌─────────────┐
       │   RDF Repo  │ │   Search    │ │  Ontology   │
       │ (Oxigraph)  │ │ (Tantivy)   │ │  Reasoner   │
       └─────────────┘ └─────────────┘ └─────────────┘
              │               │               │
              └───────────────┼───────────────┘
                              ▼
                    ┌─────────────────┐
                    │  Policy Gates   │
                    │ (authz → gate   │
                    │  → SHACL →     │
                    │  commit)       │
                    └─────────────────┘
                              │
                              ▼
                    ┌─────────────────┐
                    │  Audit + Index  │
                    │  Sync           │
                    └─────────────────┘
```

### 2.2 RDF Repository (`rdf/`)

**Trait:** `RdfRepository` (12 methods)
- `insert`, `remove`, `apply_atomic` — atomic batch mutations returning `TripleDelta`
- `match_quads` — pattern matching with S/P/O/G filters + limit
- `quads_for_subject`, `delete_subject`, `list_graphs`, `len`, `is_empty`, `contains`
- `search_stub` — placeholder for Phase 3 hook

**Implementation:** `OxigraphRepository`
- Wraps `oxigraph::store::Store`
- All batch mutations run inside `Store::transaction` (all-or-nothing)
- Reads bounded by `max_read` (from `AppLimits`)
- Test-only failure injection: `set_inject_failure_at(n)` forces n-th write to fail → full rollback
- **Term conversions** preserve strict typing:
  - `Term::Iri` ↔ `NamedNode`
  - `Term::BlankNode` ↔ `BlankNode`
  - `Term::Literal{value,datatype,lang}` ↔ `Literal` (lang-tagged, typed, or plain)
  - `GraphName::Default` / `GraphName::Named` ↔ `DefaultGraph` / `NamedNode` / `BlankNode`

**Key invariants enforced:**
- Predicate must be IRI (rejects literal predicates at `Triple::new`/`Quad::new`)
- Language tag and datatype are mutually exclusive by construction
- Literals never appear in subject position

### 2.3 Ontology Reasoning (`ontology/reasoner.rs`)

**Data structures built from ontology graph (`GraphName::ontology()`):**
```rust
subclass:     HashMap<Class, HashSet<Superclass>>
subproperty:  HashMap<Property, HashSet<Superproperty>>
domain:       HashMap<Property, Class>
range:        HashMap<Property, Class>
inverse:      HashMap<Property, Property>   // symmetric
equivalent:   HashMap<Class, HashSet<EquivalentClass>>  // symmetric
```

**Supported predicates (IRIs):**
- `rdfs:subClassOf`, `rdfs:subPropertyOf`
- `rdfs:domain`, `rdfs:range`
- `owl:inverseOf` (symmetric)
- `owl:equivalentClass` (symmetric)

**Public API (`OntologyService` trait):**
- `superclasses(class)` — transitive closure (excl. self)
- `is_subclass_of(sub, sup)` — includes equivalent-class bridging
- `subproperties(prop)` — transitive closure
- `is_subproperty_of(sub, sup)`
- `domain(prop)`, `range(prop)`, `inverse(prop)` — single-hop lookup
- `equivalents(class)` — symmetric, excl. self
- `asserted_types(repo, subject, graph)` — direct `rdf:type` values
- `inferred_types(repo, subject, graph)` — asserted + superclass closure + equivalent closure (fixpoint iteration)
- `is_instance_of(repo, subject, graph, class)` — inferred type membership
- `inverse_entailments(quad)` — derives `(o, inverse(p), s)` when both ends are IRIs

**Inference algorithm:**
1. Build maps by scanning ontology graph once at startup
2. Transitive closure via DFS with `HashSet` visited tracking
3. Equivalent-class bridging: one hop in both directions during closure
4. Fixpoint iteration for `inferred_types`: stack processes asserted types, adds superclasses & equivalents until stable

### 2.4 SHACL Validation (`ontology/shacl.rs`)

**Shapes (from `contact_shapes()`):**
```rust
Shape {
    id: "ex:ContactShape",
    target_class: "ex:Contact",
    constraints: [
        { path: "ex:name", min_count: 1, max_count: None, datatype: None, value_class: None },
        { path: "ex:email", min_count: 0, max_count: None, datatype: None, value_class: None },
        { path: "ex:account", min_count: 0, max_count: None, datatype: None, value_class: "ex:Account" },
    ]
}
```

**Validation entry points:**
- `validate_resource(repo, reasoner, graph, subject, shapes)` — reads current state
- `validate_staged_subject(repo, reasoner, graph, subject, staged_quads, shapes)` — pure, validates existing+adds−removes

**Checks performed per constraint:**
1. **Cardinality:** count values for `path` → fail if `< min_count` or `> max_count`
2. **Datatype:** if `datatype` set, object must be `Literal` with matching datatype (plain literals default to `xsd:string`)
3. **Class range:** if `value_class` set, object must be IRI and `reasoner.is_instance_of(object, value_class)` must be true
4. **Domain:** for each staged predicate with declared domain, focus node must be instance of that domain (uses `reasoner.inferred_types_from_type_iris`)

**Output:** `ValidationResult { conforms: bool, violations: Vec<Violation> }`
```rust
Violation { shape_id, focus_node, path, severity: Violation|Warning, message }
```

### 2.5 Search Subsystem (`search/`)

**Architecture:** Tantivy (lexical) + Vector sidecar (`HashMap<embedding_id, Vec<f32>>`)
- `embedding_id = "graph||uri"` — stable key across rebuilds
- Vector encoder pluggable via `Embedder` trait:
  - Production: `LiveEmbedder` → `Needle::embed()` (3072-d, via `cactus-rs`)
  - Tests: `HashEmbedder` — deterministic FNV-1a token histogram + L2 norm (128-d default)

**Document schema (`SearchDocument`):**
| Field | Source |
|-------|--------|
| `uri` | Resource IRI |
| `graph` | Graph name |
| `type_uris` | All `rdf:type` object IRIs |
| `label` | First `rdfs:label`/`name`/`title` literal |
| `comment` | First `rdfs:comment`/`description`/`abstract` literal |
| `local_name` | Short name after last `/`, `#`, `:` — camelCase split (`JohnSmith` → `john smith`) |
| `annotations` | Concatenated other literal values + short names of IRI objects |
| `embedding_id` | `graph||uri` |

**Search modes (`SearchMode`):**
- `Lexical` — BM25 via Tantivy `QueryParser` on `label, comment, local_name, annotations`
- `Vector` — cosine similarity against query embedding
- `Hybrid` (default) — min-max normalize both score sets, fuse: `alpha * lexical + (1-alpha) * vector` (alpha=0.5 default). Items in both → `Provenance::Hybrid`.

**Index maintenance:**
- `upsert(doc)` — adds document + embedding, deletes old by `embedding_id`
- `delete(uri, graph)` — removes doc + vector
- `commit()` — commits writer, reloads reader, persists `vectors.json` sidecar
- `rebuild_from_repo(repo)` — full scan, groups by `(graph, subject)`, builds docs, bulk upsert + commit
- `sync_resource(repo, uri, graph)` — called by tools after every commit; re-reads resource, upserts or deletes

### 2.6 Tool System (`tools/`)

**Registry:** `ToolRegistry` — name → `ToolDef { name, description, schema }`
- 5 tools: `search`, `read`, `create`, `update`, `delete`
- Strict JSON schema validation via `serde` (deny unknown fields)
- Descriptions follow contract: when-to-use + args + returns (≤ 600 chars)

**Shared state:** `ToolState { repo, search, reasoner, shapes, audit, limits }`

**Pre-dispatch gates (in `tools/mod.rs`):**
1. **Argument parsing** — strict `serde::from_value` → `ToolError::args`
2. **Read authorization** (`check_read_auth`) — for `search`/`read`:
   - Legacy empty context → allowed
   - Otherwise: `Operation::Read` in `permitted_ops` AND graph in `allowed_graphs`
3. **Grounding gate** (`check_grounding`) — for `create`/`update`/`delete`:
   - All URIs must be valid IRIs (scheme + no whitespace)
   - `update`/`delete` targets must already exist in store (prevents invented URIs)

**Tool implementations:**

| Tool | Args | Flow |
|------|------|------|
| `search` | `query`, `context?`, `namespace?`, `limit?` | `check_read_auth` → `search.search()` → returns `Candidate[]` |
| `read` | `uris[]`, `context?`, `predicates?`, `depth?` | `check_read_auth` → bounded BFS traversal (depth 0-3) → returns `ResourceView[]` with `TermView` (preserves IRI vs literal) |
| `create` | `uri?`, `graph`, `types[]`, `properties[]` | `check_grounding` → build quads → `execute_guarded` → `search.sync_resource` |
| `update` | `uri`, `graph`, `add[]`, `remove[]` | `check_grounding` → build add/remove quads → `execute_guarded` → `search.sync_resource` |
| `delete` | `uri?`, `graph`, `triples?`, `confirmation_token?` | `check_grounding` → resolve quads to remove → `execute_guarded` (confirmation-gated) → `search.sync_resource` |

**Property input kinds:** `iri`, `literal`, `typed` (needs `datatype`), `lang` (needs `lang`), `bnode`

### 2.7 Policy Gates (`policy/`)

**`ExecutionContext`** — travels with every request:
```rust
request_id, principal, tenant, allowed_graphs: Vec<GraphName>, permitted_ops: Vec<Operation>
```
- `Operation::Read | Create | Update | Delete`
- Helper: `ExecutionContext::for_tenant(request_id, principal, tenant, permitted_ops)` → allows tenant graph + ontology graph

**Authorization (`authz::authorize`): deny-by-default**
- Graph must be in `allowed_graphs`
- Operation must be in `permitted_ops`
- Cross-tenant denied unless explicitly listed

**Destructive gate (`confirm::is_destructive`):**
- `Update` (has removes) or `Delete` → requires confirmation token
- Token format: `"confirm:<sha256(dry_run_summary)>"`
- Dry-run (`MutationOptions::dry_run=true`) never needs token

**Guarded mutation (`gate::execute_guarded`): single write path**
```
1. Classify op: Create / Update / Delete (from to_add/to_remove)
2. Cross-graph check: all quads must target target_graph
3. authorize(ctx, target_graph, op) → PolicyError::Denied
4. if destructive && !dry_run && !confirmation_satisfied(token) → PolicyError::ConfirmationRequired
5. Staged validation per affected subject:
     existing = repo.match_quads(subject)
     staged = existing + to_add - to_remove
     validate_staged_subject(staged) → PolicyError::ValidationFailed
6. if dry_run: preview_delta() → return MutationOutcome{dry_run:true, summary}
7. repo.apply_atomic(to_add, to_remove) → delta
8. AuditRecord appended at every step (denied, validation_failed, dry_run, committed)
```

**Audit log (`audit.rs`):**
```rust
AuditRecord {
    request_id, principal, target_uri, graph, op,
    delta_added, delta_removed, delta: TripleDelta,
    validation_conforms, validation_violations,
    commit_status: Committed | DryRun | Denied | ValidationFailed,
    note
}
```

### 2.8 Agent Loop (`agent/`)

**`Agent::run_request_with_progress`** — single request to completion:
```rust
input = user_text
while iterations < max_iterations (8):
    completion = model.complete(input)
    match completion:
        Done(text) → final_text = text; break
        ToolCalls(calls):
            for call in calls:
                if ops >= max_ops_per_request (32): emit limit error
                result = registry.dispatch(state, ctx, call.name, call.args)
                emit ProgressEvent
                history.push((name, result))
            model.submit_tool_results(results)
            input = "continue. tool_results: {json}"
            if ops >= max_ops_per_request: break
```

**Progress events:** one per tool call + start/finish (no token streaming from Needle)

**Final assembly (`assemble_final_response`):**
- Counts ops, lists succeeded/failed with call IDs and error messages
- Appends model's final text

**Worker (`agent/worker.rs`): serialized single-model executor**
- Tokio task owning `Box<dyn ModelAdapter>` (Needle or Mock)
- `mpsc` channel queues `Job { session_id, user_text, ctx, progress_tx, reply }`
- Concurrency = 1 by construction (metrics track current/max)
- Between sessions: `model.reset()` + conversation log supplies context
- Records: user message → tool calls/results → final response

### 2.9 Model Integration (`model/`)

**`ModelAdapter` trait:**
```rust
complete(&mut self, user_text: &str) → ModelCompletion (ToolCalls | Done)
submit_tool_results(&mut self, &[ToolResult]) → ()
embed(&mut self, text: &str) → Vec<f32>  // for search vectors
reset(&mut self)  // between sessions
```

**Production:** `LiveModelHandle` + `LiveEngine` (Needle 3 via `cactus-rs`, 3072-d embeddings)
- Feature-gated: `--features live-model` + weights at `NEEDLE_WEIGHTS_PATH`
- Single shared engine drives both tool routing and search embedder

**Tests:** `MockModel` — scripted completions, records seen inputs + submitted results

### 2.10 Conversation Log (`agent/conversation.rs`)

**Durable transcript per session:**
```rust
SessionTranscript {
    session_id,
    messages: Vec<ChatMessage{seq, role, text}>,
    calls: Vec<CallRecord{seq, call_id, tool, args}>,
    results: Vec<ResultRecord{seq, call_id, tool, ok, result}>,
    finals: Vec<FinalRecord{seq, text, ops, iterations}>,
    next_seq
}
```

**Storage:** file-backed JSON (`ConversationLog::open(path)`) or in-memory
- Serializes entire `HashMap<session_id, SessionTranscript>` on every mutation
- Reloads on open → identical transcript semantics

### 2.11 HTTP API (`api/`)

| Route | Auth | Description |
|-------|------|-------------|
| `GET /health` | none | Liveness |
| `POST /chat` | `x-principal`, `x-tenant`, `x-request-id?`, `x-graphs?` | Main chat → worker → `ChatResponse{final_text, ops, iterations, progress[]}` |
| `POST /tools/:name` | same | Direct tool dispatch (no model) |
| `POST /admin/reindex` | same | Full Tantivy rebuild from repo |

**Auth mapping:** headers → `ExecutionContext`
- Missing `x-principal`/`x-tenant` → 401
- `allowed_graphs`: from `x-graphs` comma-list OR default `[tenant_graph, ontology_graph]`
- `permitted_ops`: all four by default

**Body limit:** 64 KiB (tower-http `RequestBodyLimitLayer`)

---

## 3. Application to Example Types

### 3.1 Multi-Tenant Contact Management

**Scenario:** Tenants "acme" and "globex" each manage their own contacts/accounts in isolated graphs.

**Setup:**
```rust
// At startup
let repo = OxigraphRepository::new_in_memory(500)?;
load_default_ontology(&repo)?;  // writes to GraphName::ontology()

// Per-request context
let ctx = ExecutionContext::for_tenant("req-1", "alice", "acme", vec![
    Operation::Read, Operation::Create, Operation::Update, Operation::Delete
]);
```

**Create a contact (acme tenant):**
```json
POST /chat  { "message": "Create a contact Alice with email alice@acme.com" }
// Model → tool call:
create { uri: "ex:Alice", graph: "acme", types: ["ex:Contact"], properties: [
    { predicate: "ex:name", kind: "literal", value: "Alice" },
    { predicate: "ex:email", kind: "literal", value: "alice@acme.com" }
] }
```

**Internal flow:**
1. `check_grounding("create")` — URI valid, target doesn't need to exist for create
2. `execute_guarded`:
   - `authorize`: acme graph in allowlist, Create permitted ✓
   - Not destructive (no removes) → no confirmation needed
   - Staged validation: subject `ex:Alice` gets `ex:Contact` type → `ContactShape` checks:
     - `ex:name` count ≥ 1 ✓
     - `ex:email` values are literals ✓
   - `repo.apply_atomic` commits 3 quads (type + name + email)
   - `audit.append(Committed)`
   - `search.sync_resource` → Tantivy upserts doc for `ex:Alice`
3. Returns: `{ "uri": "ex:Alice", "added": 3, "summary": "committed: added 3 quad(s)..." }`

**Cross-tenant isolation:**
- Globex principal with `tenant=globex` gets `allowed_graphs=[globex, ontology]`
- Attempt to read acme graph → `authorize` fails: "graph acme not in allowlist for principal bob"
- Attempt to write to acme graph → same denial

### 3.2 Ontology-Driven Inference

**Ontology (loaded at startup):**
```
ex:Employee rdfs:subClassOf ex:Contact .
ex:Contact  rdfs:subClassOf ex:Person .
ex:Person   owl:equivalentClass ex:Human .
ex:workEmail rdfs:subPropertyOf ex:email .
ex:account  rdfs:domain ex:Contact ; rdfs:range ex:Account .
ex:employs  owl:inverseOf ex:employedBy .
```

**Assert:** `ex:Alice a ex:Employee` (in acme graph)

**Inferred types for Alice:**
1. Asserted: `{ex:Employee}`
2. Superclass closure: `ex:Employee → ex:Contact → ex:Person`
3. Equivalent bridge: `ex:Person ≡ ex:Human` → add `ex:Human`
4. Result: `{ex:Employee, ex:Contact, ex:Person, ex:Human}`

**SHACL domain check on `ex:account`:**
- Predicate `ex:account` has domain `ex:Contact`
- Alice's inferred types include `ex:Contact` → passes
- If Alice were typed only `ex:Account` → domain violation

**Inverse entailment:**
- Assert: `ex:Acme ex:employs ex:Alice`
- Reasoner derives: `ex:Alice ex:employedBy ex:Acme` (both ends IRIs)
- Returned by `inverse_entailments()` for read-time expansion (not persisted)

### 3.3 Hybrid Search: Finding Resources

**Index state (after indexing Alice, Bob, Acme corp):**
| URI | label | type_uris | comment | local_name |
|-----|-------|-----------|---------|------------|
| ex:Alice | "Alice" | [ex:Contact] | "Senior engineer" | "alice" |
| ex:Bob | "Bob" | [ex:Contact] | "Manager" | "bob" |
| ex:Acme | "Acme Corp" | [ex:Account] | "Tech company" | "acme corp" |

**Query: "engineer at tech company"**
- **Lexical (BM25):** matches "engineer" in Alice's comment, "tech company" in Acme's comment
- **Vector (cosine):** `HashEmbedder` canonicalizes "engineer"+"tech" → similar bucket pattern for Alice+Acme
- **Hybrid (α=0.5):** min-max normalize both, fuse
  - Alice: high lexical (comment) + high vector → Hybrid, ranked #1
  - Acme: high lexical (comment) + medium vector → Hybrid, ranked #2
  - Bob: low both → Lexical only, ranked #3

**Search tool call:**
```json
search { query: "engineer at tech company", context: "acme", namespace: "http://example.com/", limit: 10 }
```
→ Returns `Candidate[]` with `provenance: Hybrid`, `score`, `label`, `type`, `description`

**Namespace filter:** only URIs starting with `http://example.com/`

### 3.4 Bounded Graph Traversal (read tool)

**Scenario:** Read Alice with depth=2, following `ex:account` and `ex:employs`

**Data:**
```
ex:Alice  ex:name       "Alice" .
ex:Alice  ex:account    ex:Acme .
ex:Acme   ex:name       "Acme Corp" .
ex:Acme   ex:employs    ex:Alice .
```

**Call:** `read { uris: ["ex:Alice"], depth: 2, predicates: ["ex:account", "ex:employs"] }`

**Traversal:**
1. Depth 0: Alice direct properties → name, account (IRI)
2. Depth 1: follow `ex:account` → Acme (IRI object) → enqueue Acme at depth=1
3. Depth 1: Acme direct properties (filtered to allowlist) → name, employs
4. Depth 2: follow `ex:employs` from Acme → Alice (already visited) → skip

**Result:**
```json
{
  "resources": [
    { "uri": "ex:Alice", "graph": "acme", "properties": [
      { "predicate": "ex:name", "object": { "kind": "literal", "value": "Alice", "target_uri": null } },
      { "predicate": "ex:account", "object": { "kind": "iri", "value": "ex:Acme", "target_uri": "ex:Acme" } }
    ]},
    { "uri": "ex:Acme", "graph": "acme", "properties": [
      { "predicate": "ex:name", "object": { "kind": "literal", "value": "Acme Corp" } },
      { "predicate": "ex:employs", "object": { "kind": "iri", "value": "ex:Alice", "target_uri": "ex:Alice" } }
    ]}
  ],
  "truncated": false
}
```

**Caps enforced (from `AppLimits`):**
- `max_triples_per_read: 500` — total quads scanned
- `max_resources_per_read: 50` — resources returned
- `max_traversal_paths: 200` — IRI hops followed

### 3.5 Destructive Operation with Confirmation

**Scenario:** Delete contact Alice (requires confirmation)

**Step 1 — Dry run (no token):**
```json
delete { uri: "ex:Alice", graph: "acme", confirmation_token: null }
```

**Internal:**
1. `check_grounding("delete")` — Alice exists ✓
2. `execute_guarded`:
   - `authorize`: Delete permitted ✓
   - `is_destructive`: to_remove non-empty, to_add empty → true
   - `dry_run` default false, but no token → `PolicyError::ConfirmationRequired`
   - Audit: `CommitStatus::Denied`, note: "destructive Delete requires confirmation token (dry-run first)"

**Step 2 — Dry run explicit:**
```json
delete { uri: "ex:Alice", graph: "acme", confirmation_token: null, dry_run: true }
```
→ Returns preview delta: `{ "added": 0, "removed": 3, "summary": "would remove 3 quad(s): ..." }`
→ Audit: `CommitStatus::DryRun`

**Step 3 — Confirm:**
- Client computes token: `"confirm:<sha256(dry_run_summary)>"`
```json
delete { uri: "ex:Alice", graph: "acme", confirmation_token: "confirm:abc123..." }
```
→ `confirmation_satisfied(token)` verifies hash matches current preview
→ Commits removal → Audit: `CommitStatus::Committed`
→ `search.sync_resource` deletes Alice from Tantivy

### 3.6 SHACL Validation Failure

**Scenario:** Create Contact without required `ex:name`

```json
create { graph: "acme", types: ["ex:Contact"], properties: [
    { predicate: "ex:email", kind: "literal", value: "bob@acme.com" }
] }
```

**Validation:**
1. Staged subject has type `ex:Contact` → `ContactShape` applies
2. Constraint `ex:name` min_count=1 → staged has 0 values for `ex:name`
3. Violation:
```json
{
  "shape_id": "ex:ContactShape",
  "focus_node": "ex:Bob",
  "path": "ex:name",
  "severity": "Violation",
  "message": "Contact requires at least one ex:name"
}
```
4. `execute_guarded` returns `PolicyError::ValidationFailed(violations)`
5. Audit: `CommitStatus::ValidationFailed`, `validation_violations: 1`
6. Tool returns structured error:
```json
{ "tool": "create", "category": "validation", "message": "SHACL failed: [{\"shape_id\":\"ex:ContactShape\",...}]" }
```

### 3.7 Agent Loop: Multi-Step Task

**User:** "Find Alice's account and add a work email for her"

**Iteration 1:**
- Model → `search { query: "Alice account" }`
- Returns: `ex:Alice` (Contact), `ex:Acme` (Account)
- Model → `read { uris: ["ex:Alice"], depth: 1 }`
- Returns: Alice has `ex:account ex:Acme`

**Iteration 2:**
- Model → `update { uri: "ex:Alice", graph: "acme", add: [
    { predicate: "ex:workEmail", kind: "literal", value: "alice.work@acme.com" }
  ] }`
- `check_grounding` → Alice exists ✓
- `execute_guarded` → `Operation::Update`
- Staged validation: Alice types include Contact → `ex:workEmail` subproperty of `ex:email` (domain Contact) ✓
- Commits → Audit + index sync

**Iteration 3:**
- Model → `Done("Added work email alice.work@acme.com to Alice")`

**Final response:**
```
completed 3 op(s).
ok search (c1) ok read (c2) ok update (c3)
model said: Added work email alice.work@acme.com to Alice
```

**Progress events (for polling/SSE):**
```
seq=0  iteration=0  tool="start"     ok  "request req-1 started"
seq=1  iteration=1  tool="search"    ok  "search ok"
seq=2  iteration=1  tool="read"      ok  "read ok"
seq=3  iteration=2  tool="update"    ok  "update ok"
seq=4  iteration=2  tool="finish"    ok  "done in 2 iterations, 3 ops"
```

### 3.8 Conversation Persistence & Session Isolation

**Session "user-123":**
```
POST /chat { "message": "Create contact Alice", "session_id": "user-123" }
→ worker.chat("user-123", ...) → records in ConversationLog
POST /chat { "message": "Add email for Alice", "session_id": "user-123" }
→ same session_id → model continues conversation (no reset)
```

**Session "user-456":**
```
POST /chat { "message": "Create contact Bob", "session_id": "user-456" }
→ different session_id → worker calls model.reset() first
→ fresh conversation, no context from user-123
```

**Transcript structure (file-backed):**
```json
{
  "user-123": {
    "session_id": "user-123",
    "messages": [{ "seq": 1, "role": "user", "text": "Create contact Alice" }, ...],
    "calls": [{ "seq": 2, "call_id": "c1", "tool": "search", "args": {...} }, ...],
    "results": [{ "seq": 3, "call_id": "c1", "tool": "search", "ok": true, "result": {...} }, ...],
    "finals": [{ "seq": 5, "text": "completed 2 op(s)...", "ops": 2, "iterations": 2 }],
    "next_seq": 6
  }
}
```

---

## 4. Configuration Reference

### 4.1 AppLimits (with env overrides)

| Parameter | Default | Env Var | Description |
|-----------|---------|---------|-------------|
| `max_iterations` | 8 | `NEEDLE_MAX_ITERATIONS` | Agent loop iterations per request |
| `max_ops_per_request` | 32 | `NEEDLE_MAX_OPS` | Tool calls per request |
| `max_triples_per_read` | 500 | `NEEDLE_MAX_TRIPLES_PER_READ` | Repository read cap |
| `max_resources_per_read` | 50 | `NEEDLE_MAX_RESOURCES_PER_READ` | Resources per `read` call |
| `max_traversal_paths` | 200 | `NEEDLE_MAX_TRAVERSAL_PATHS` | IRI hops in traversal |
| `default_read_depth` | 1 | — | Default `read` depth (0-3) |
| `default_page_limit` | 50 | — | Default search/read page size |

### 4.2 Feature Flags

| Feature | Effect |
|---------|--------|
| `test-hooks` | Exposes `OxigraphRepository::set_inject_failure_at` for atomicity tests |
| `live-model` | Enables `cactus-rs` integration (Needle 3 weights → `LiveEngine`) |

### 4.3 Environment Variables (Runtime)

| Variable | Purpose |
|----------|---------|
| `NEEDLE_ADDR` | Bind address (default `127.0.0.1:3000`) |
| `NEEDLE_WEIGHTS_PATH` | Path to Needle weights (for `live-model`) |
| `NEEDLE_REQUIRE_LIVE=1` | Fail startup if live engine unavailable |
| `NEEDLE_MAX_*` | Override `AppLimits` (see above) |

---

## 5. Testing Strategy

**Unit tests** (in-module `#[cfg(test)]`):
- Term/Quad construction invariants
- Ontology reasoner closure logic
- SHACL validation cases (pass/fail/violation structure)
- Search ranking determinism + hybrid fusion
- Policy authz deny/allow
- Agent loop iteration/ops caps
- Conversation log round-trip

**Integration tests** (tests/ directory):
- Full CRUD via HTTP API
- Cross-tenant isolation
- Dry-run → confirm flow
- Index rebuild + sync
- Serialized worker session reset

**Property tests** (where applicable):
- `TripleDelta` round-trip
- `HashEmbedder` determinism + normalization
- `rank_hybrid` fusion ordering

---

## 6. Extension Points

| Area | How to Extend |
|------|---------------|
| **Ontology** | Add quads to `GraphName::ontology()` → reasoner auto-loads on rebuild |
| **SHACL Shapes** | Extend `contact_shapes()` or add new `Shape` entries to `ToolState.shapes` |
| **Search Embedder** | Implement `Embedder` trait → pass to `TantivyIndex::in_ram_with_embedder()` |
| **Tools** | Add entry to `tool_defs()` + handler module + dispatch arm in `ToolRegistry` |
| **Auth** | Replace `ctx_from_headers` with real auth (JWT, OIDC) → populate `ExecutionContext` |
| **Persistence** | Swap `ConversationLog` backend (SQLite, Postgres) — same trait surface |
| **Model** | Implement `ModelAdapter` for other LLM backends |

---

*Generated from source code review of `needle-rdf-agent` v0.1.0*