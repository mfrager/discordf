# **Recommended Solutions for the RDF Agent Design**

I would address the problems by making the Rust system considerably more responsible for planning correctness, semantic interpretation, and execution safety, while keeping the LLM responsible for interpreting user intent and proposing operations.

The key architectural principle should be:

**The LLM proposes what should happen. The Rust system determines whether it makes sense, whether it is permitted, and how to execute it safely.**

This is more than adding validation around the existing design. I would introduce several explicit subsystems and change how the agent interacts with them.

# **1. Proposed revised architecture**

**User request**

Natural-language intent

**1. Planning subsystem**

**A — Needle 3**

Sequential planning

**B — MiniCPM5**

Tool calling

**C — MiniCPM5**

Structured output

**D — MiniCPM5**

Two-pass planning

**2. Canonical operation plan**

Typed IR, dependencies, references, scopes, preconditions and expected effects

**3. Rust planning and verification engine**

Semantic resolver

Ontology and vocabulary

Query compiler

Typed query plans

Policy engine

Permissions and approvals

Plan validator

Dependencies and invariants

**4. Transactional execution engine**

Preview → Validate → Authorize → Commit → Verify

**5. RDF / OWL / SHACL store**

Authoritative graph state and inference

The important change is that the planning model no longer directly controls the execution of arbitrary graph operations. It works through a constrained, inspectable intermediate representation.

# **2. Solutions to the individual problems**

## **A. Planning and model limitations**

### **Problem: Small models produce incomplete or incorrectly ordered plans**

**Solution: Use a typed, dependency-aware Plan IR.**

Instead of asking the model to produce an informal sequence of tool calls, give it a constrained set of operations.

For example:

```
enum PlanOperation {
    DiscoverVocabulary {
        query: String,
        kinds: Vec<VocabularyKind>,
    },

    DiscoverInstances {
        class: ResourceRef,
        filters: Vec<Filter>,
    },

    Query {
        query: QueryPlan,
    },

    CreateResource {
        resource: ResourceDefinition,
    },

    UpdateResources {
        selection: SelectionRef,
        changes: Vec<GraphChange>,
    },

    Validate {
        target: ValidationTarget,
    },

    Finish,
}
```

Each operation receives an ID and may reference outputs of earlier operations.

```
struct PlanStep {
    id: StepId,
    depends_on: Vec<StepId>,
    operation: PlanOperation,
}
```

Rust validates that:

- Every referenced step exists.
- Dependencies are acyclic.
- Required inputs are available.
- Operations are compatible with their outputs.
- No operation executes before its dependencies complete.

This removes the need for the LLM to reliably manage all execution ordering itself.

### **Problem: The model loses track of the original objective**

**Solution: Maintain an immutable request specification.**

Keep the original request separate from the evolving execution context.

```
struct OperationContext {
    request: UserRequest,
    graph_scope: GraphScope,
    discoveries: DiscoveryRegistry,
    plan: Plan,
    results: ResultRegistry,
    transaction: TransactionState,
}
```

The model should receive a compact summary of:

- Original objective.
- Current plan.
- Completed steps.
- Outstanding steps.
- Unresolved questions.
- Relevant discovered resources.

Do not continuously append the entire conversation and all tool results to the prompt.

### **Problem: Ambiguous requests result in unjustified assumptions**

**Solution: Add explicit unresolved intent.**

The model should be able to represent uncertainty:

```
enum Resolution<T> {
    Resolved(T),
    Ambiguous(Vec<Candidate<T>>),
    Missing,
    NeedsClarification(String),
}
```

For example, "remove inactive customers" might mean:

- Remove a classification.
- Delete customer resources.
- Change an account status.
- Remove a relationship.

The system should not guess when these interpretations produce materially different effects.

## **B. Ontology and semantic limitations**

This is one of the most important areas to improve.

### **Problem: Vocabulary search returns plausible but incorrect terms**

**Solution: Introduce a Semantic Resolution Service.**

Instead of letting the model select an IRI directly from search results, have Rust return structured candidate descriptions.

For every candidate, include:

| Information          | Purpose                        |
| -------------------- | ------------------------------ |
| IRI                  | Exact resource identity        |
| Type                 | Class, property, concept, etc. |
| Labels               | Human-readable descriptions    |
| Comments             | Definitions and usage          |
| Domain               | Expected subject type          |
| Range                | Expected object or value type  |
| Superclasses         | Hierarchical meaning           |
| Equivalent resources | Alternative representations    |
| Inverse properties   | Relationship direction         |
| Source graph         | Ontology provenance            |

The resolver should rank candidates using lexical similarity, graph structure, ontology constraints, and contextual relevance.

The LLM can then select among candidates rather than inventing vocabulary.

Importantly, ranking should not silently resolve ambiguity. When two candidates remain plausible, return both.

### **Problem: OWL inference is underspecified**

**Solution: Make reasoning configuration explicit.**

Define a `ReasoningContext`:

```
struct ReasoningContext {
    regime: EntailmentRegime,
    include_imports: bool,
    include_inferred: bool,
    inference_graph: Option<GraphId>,
}
```

Distinguish three types of information:

1. Asserted triples.
2. Inferred triples.
3. Validation results.

Do not silently mix them.

For example, a query for contacts belonging to a particular class might include subclass entailments, but a query for explicitly asserted membership might not.

The agent should be able to ask which interpretation is required.

### **Problem: Open-world semantics conflict with business expectations**

**Solution: Introduce explicit query semantics for absence.**

Distinguish:

- No matching statement found in the selected dataset.
- No matching statement found after inference.
- Explicitly asserted negative information.
- Unknown because data is incomplete.

For example, "accounts with no active contacts" should have a defined operational interpretation. It might mean no matching active-contact relationship exists in the selected graph, not that the account is known to have no active contacts in reality.

The query compiler should make this distinction visible.

## **C. Query construction limitations**

### **Problem: The model constructs invalid or semantically incorrect SPARQL**

**Solution: Use a typed query IR, not arbitrary SPARQL generation.**

The model should construct query patterns from a constrained vocabulary of operations.

For example:

```
struct QueryPlan {
    scope: GraphScope,
    patterns: Vec<TriplePattern>,
    filters: Vec<QueryFilter>,
    optional: Vec<OptionalPattern>,
    unions: Vec<QueryBranch>,
    projections: Vec<Projection>,
    grouping: Vec<GroupExpression>,
    ordering: Vec<Ordering>,
    limit: Option<usize>,
}
```

Rust then compiles this into SPARQL.

This gives you several advantages:

- Validated resource references.
- Typed predicates.
- Explicit graph scope.
- Controlled filter operators.
- Query-cost estimation.
- Reusable query fragments.
- Consistent pagination.

I would not start by exposing the entire SPARQL language to MiniCPM5–1B. Add capabilities incrementally.

### **Problem: Correctly constructed queries return incorrect or excessive results**

**Solution: Add query inspection and result verification.**

The execution workflow should be:

```
Construct QueryPlan
       ↓
Resolve vocabulary
       ↓
Compile SPARQL
       ↓
Validate query
       ↓
Estimate cost
       ↓
Execute query
       ↓
Inspect result metadata
       ↓
Return bounded result
```

Return metadata with every result:

```
{
  "complete": false,
  "returned": 100,
  "total": null,
  "next_cursor": "cursor-123",
  "truncated": true
}
```

The model must never mistake a partial result for a complete one.

### **Problem: Expensive queries**

**Solution: Introduce a query budget.**

Limit:

- Execution time.
- Number of intermediate results.
- Result size.
- Property-path depth.
- Join complexity.
- Maximum graph scope.

A query that exceeds its budget should return a structured error with suggested remediation, such as narrower graph scope or more selective filters.

## **D. Mutation and transaction limitations**

This is where I would apply the strongest safeguards.

### **Problem: A valid query can select the wrong resources**

**Solution: Require a mutation preview.**

Never immediately execute an LLM-generated bulk update.

Instead:

```
Identify affected resources
        ↓
Generate mutation preview
        ↓
Check count and scope
        ↓
Validate proposed graph changes
        ↓
Check authorization
        ↓
Require approval if necessary
        ↓
Commit
        ↓
Verify final graph
```

The preview should show:

- Number of affected resources.
- Exact resource URIs.
- Graphs affected.
- Predicates being changed.
- Old values.
- New values.
- Validation status.

For large result sets, show an aggregate and a sample, while preserving the complete selection as a server-side reference.

### **Problem: Data changes between query and mutation**

**Solution: Use snapshot-aware transactions.**

The selection and update must operate against a consistent dataset version.

For example:

```
struct MutationPreview {
    selection_id: SelectionId,
    snapshot_version: DatasetVersion,
    changes: Vec<ProposedChange>,
    affected_count: usize,
}
```

Before commit, verify that the underlying resources have not changed in ways that invalidate the selection or proposed changes.

If they have, abort and re-evaluate.

### **Problem: Duplicate operations after timeouts**

**Solution: Make operations idempotent.**

Assign every mutation a durable operation ID.

Store:

- Operation ID.
- Request hash.
- Transaction ID.
- Execution status.
- Commit version.
- Result.

If the agent retries after an ambiguous timeout, Rust checks the operation ID and returns the previous result rather than executing the mutation again.

### **Problem: Deletion is ambiguous**

**Solution: Create distinct deletion operations.**

Never use one generic `delete` operation for all deletion semantics.

Provide separate operations:

```
enum DeleteOperation {
    RemovePredicateValues,
    RemoveOutgoingStatements,
    RemoveResourceDescription,
    RemoveIncomingReferences,
    RemoveSubgraph,
}
```

The model must specify the intended operation. Destructive variants should have stronger approval requirements.

## **E. Context and state limitations**

### **Problem: The operation context grows indefinitely**

**Solution: Store large results outside the model context.**

The model should receive handles to results rather than complete result sets.

For example:

```
{
  "result_id": "result-482",
  "count": 1250,
  "complete": true,
  "sample": [
    "crm:account-001",
    "crm:account-002"
  ]
}
```

Rust can subsequently perform operations against the selection.

The model does not need to see all 1,250 URIs.

This is particularly valuable for MiniCPM5–1B, where context management is important.

### **Problem: The model confuses namespaces, graphs, and tenants**

**Solution: Represent these as separate types.**

Do not use one generic string called `context`.

```
struct GraphScope {
    dataset: DatasetId,
    tenant: TenantId,
    graphs: Vec<GraphId>,
    namespaces: Vec<NamespaceId>,
    reasoning: ReasoningContext,
}
```

The model may propose a scope, but Rust must enforce the actual authorized scope.

### **Problem: Workflows cannot recover after a restart**

**Solution: Persist workflow state.**

Store each operation as a durable state machine:

```
Created
  → Planning
  → Ready
  → Executing
  → AwaitingApproval
  → Committed
  → Verified
```

Also support:

```
Failed
Cancelled
NeedsReplanning
```

A restarted agent should resume from authoritative Rust state, not reconstruct its progress from conversational history.

# **3. Improve the tool interface**

The previous design risks exposing too many tools to a small model.

I would consolidate them into a smaller number of conceptual tools.

| Tool          | Responsibility                                         |
| ------------- | ------------------------------------------------------ |
| `discover`    | Search vocabulary and instances                        |
| `inspect`     | Read resources, ontology definitions and relationships |
| `query`       | Construct, validate and execute typed queries          |
| `propose`     | Create a proposed graph mutation                       |
| `validate`    | Run semantic, SHACL and policy validation              |
| `commit`      | Commit an authorized mutation                          |
| `status`      | Inspect plan, workflow and transaction status          |
| `finish_plan` | End planning and submit the plan                       |

The tools should have explicit modes and typed arguments rather than dozens of similar operations.

For example:

```
{
  "mode": "vocabulary",
  "search": "property connecting contacts to accounts",
  "scope": {
    "ontology_graphs": ["crm:ontology"]
  }
}
```

And:

```
{
  "mode": "instances",
  "class": "crm:Contact",
  "scope": {
    "tenant": "tenant-a",
    "graphs": ["crm:instances"]
  }
}
```

The model can use one discovery tool while Rust routes the operation to the appropriate search implementation.

I would retain separate modes internally because vocabulary search and instance search have different semantics.

# **4. Add a semantic evidence system**

This is a particularly valuable addition that was not fully developed in the previous design.

The agent should not simply store a selected IRI. It should store why that IRI was selected.

For example:

```
struct SemanticEvidence {
    selected: ResourceId,
    candidates: Vec<ResourceId>,
    matching_labels: Vec<String>,
    definition: Option<String>,
    domain_match: bool,
    range_match: bool,
    hierarchy_match: bool,
    source_graphs: Vec<GraphId>,
    unresolved_conflicts: Vec<String>,
}
```

This provides an auditable record of semantic decisions.

It also gives the planner a better opportunity to reconsider a choice when new information becomes available.

A critical distinction: evidence is not proof. The resolver can establish that a property is structurally compatible, but the user may still intend a different business concept.

# **5. Add a formal plan validation stage**

Before any plan is executable, it should pass several independent checks.

| Validation            | What it checks                                 |
| --------------------- | ---------------------------------------------- |
| Schema validation     | Plan IR is structurally valid                  |
| Dependency validation | No missing dependencies or cycles              |
| Reference validation  | All resource and step references resolve       |
| Semantic validation   | Classes and properties are compatible          |
| Scope validation      | Operations remain inside permitted graphs      |
| Query validation      | Query is well-formed and within limits         |
| Mutation validation   | Changes satisfy ontology and SHACL rules       |
| Policy validation     | User is permitted to perform the operation     |
| Safety validation     | Destructive actions meet approval requirements |

I would implement this as a pipeline that returns structured errors rather than simply accepting or rejecting the whole plan.

For example:

```
struct PlanValidation {
    valid: bool,
    errors: Vec<ValidationError>,
    warnings: Vec<ValidationWarning>,
    required_approvals: Vec<ApprovalRequirement>,
}
```

This enables the model to correct a plan without having to start from scratch.

# **6. Improve the four planning variants**

The four approaches should share the same plan representation, but they should not necessarily use identical interaction patterns.

## **A — Cactus Needle 3: Sequential planning**

**Recommended change:** Make each planning call produce one meaningful plan modification.

Do not ask Needle to regenerate the entire plan every time.

For example:

```
add_discovery_step
add_query_step
add_dependency
revise_step
finish_plan
```

Rust maintains the authoritative plan. Needle receives a compact summary after each operation.

Advantages:

- Easier to inspect intermediate decisions.
- Easier to recover from individual mistakes.
- No need to regenerate the whole plan.
- Natural opportunity for iterative ontology discovery.

Primary risk: too many sequential calls and accumulated planning drift.

Mitigation: limit planning iterations and detect repeated operations.

## **B — MiniCPM5–1B: Tool-calling planner**

**Recommended change:** Use a hybrid of planning and read-only discovery.

Allow the model to call discovery, inspection and query-preview tools during planning, but do not give it unrestricted mutation capabilities.

A typical loop:

```
User request
    ↓
Tool call
    ↓
Rust executes read-only discovery
    ↓
Tool result
    ↓
MiniCPM5 revises plan
    ↓
More discovery if needed
    ↓
finish_plan
```

This is probably the most flexible approach.

Its principal risk is tool selection and argument accuracy.

Mitigation: keep tool definitions concise, use typed arguments, and validate every call.

## **C — MiniCPM5–1B: Direct structured output**

**Recommended change:** Use a constrained planning schema with explicit unresolved references.

For example, the model should be able to produce:

```
{
  "steps": [],
  "unresolved": [
    {
      "question": "Which customer classification is intended?",
      "candidates": [
        "sales:Prospect",
        "legacy:customer"
      ]
    }
  ],
  "requires_clarification": true
}
```

Do not force the model to produce a fully executable plan when it lacks sufficient information.

The primary risk is incomplete plans that nevertheless pass JSON validation.

Mitigation: independent plan completeness checks and semantic verification.

## **D — MiniCPM5–1B: Two-pass planning**

**Recommended change:** Give the second pass both the original request and the first-pass plan.

Do not extract structured output from prose alone.

The second pass should receive:

1. Original user request.
2. Natural-language plan.
3. Available ontology evidence.
4. Discovered resources.
5. Expected plan schema.

This allows it to detect omissions and contradictions.

The main risk is that the extraction step changes the meaning of the original plan.

Mitigation: compare the structured plan against the first-pass reasoning and require Rust validation.

# **7. Add a dedicated planner repair loop**

One capability I would explicitly add is automatic plan repair.

Currently, the architecture assumes that a planner produces a plan, Rust validates it, and the system either executes it or reports an error.

That is too rigid.

Instead:

**Generate plan**

**Rust validation**

**Validation result**

Valid

Execute or request approval

Invalid

Return structured errors to planner

**Revise plan**

Repeat with a strict retry budget; never bypass validation.

For example, Rust might return:

```
{
  "error": "PROPERTY_RANGE_MISMATCH",
  "step": "step-4",
  "property": "crm:worksFor",
  "expected_range": "crm:Account",
  "actual_type": "geo:Region",
  "suggestion": "Inspect account geographic relationships"
}
```

The model can then correct the plan.

I would allow two or three repair attempts initially. After that, the system should ask for clarification or report that it cannot safely complete the operation.

# **8. Make the execution engine independent of the LLM**

I would make this a hard architectural boundary.

The execution engine should accept only validated plans:

```
trait PlanExecutor {
    async fn preview(
        &self,
        plan: &ValidatedPlan,
    ) -> Result<MutationPreview, ExecutionError>;

    async fn execute(
        &self,
        plan: &AuthorizedPlan,
    ) -> Result<ExecutionResult, ExecutionError>;
}
```

Notice that `ValidatedPlan` and `AuthorizedPlan` are different types.

This prevents ordinary model output from being passed directly into the mutation executor.

A useful Rust design is to make these types constructible only through the corresponding validation and authorization services.

The model cannot manufacture an `AuthorizedPlan` by returning JSON.

# **9. Improve the benchmark to test the solutions**

The previous testing plan should be expanded to test whether these protections actually work.

| New test                         | What it verifies                    |
| -------------------------------- | ----------------------------------- |
| Ambiguous vocabulary resolution  | Candidate ranking and clarification |
| Invalid dependency injection     | Plan validator                      |
| Incorrect domain/range           | Semantic resolver                   |
| Wrong tenant in plan             | Scope enforcement                   |
| Query returns partial results    | Completeness tracking               |
| Query exceeds cost budget        | Query safeguards                    |
| Mutation selection becomes stale | Snapshot validation                 |
| Duplicate commit request         | Idempotency                         |
| SHACL failure                    | Mutation repair                     |
| Malicious ontology comment       | Prompt-injection resistance         |
| Model invents an IRI             | Resource resolution                 |
| Planner exceeds retry budget     | Termination                         |
| Restart during execution         | Durable workflow recovery           |
| Timeout after successful commit  | Transaction reconciliation          |

I would also add a test that deliberately gives the model an incorrect but syntactically valid plan. This is essential: the system should not assume that a valid plan is a correct plan.

# **10. Implementation priorities**

I would not build all of this at once. Some improvements provide much greater value than others.

**Phase 1 — Correctness foundation**

Highest priority

1. Canonical typed Plan IR.
2. Semantic vocabulary resolver.
3. Typed query IR and SPARQL compiler.
4. Plan validator.
5. Graph-scope enforcement.

These prevent the most consequential planning and semantic errors.

**Phase 2 — Safe execution**

High priority

1. Mutation preview.
2. SHACL validation.
3. Transaction snapshots.
4. Idempotency keys.
5. Approval policies.
6. Post-commit verification.

These prevent unintended graph modifications.

**Phase 3 — Agent reliability**

1. Plan repair loop.
2. Durable workflow state.
3. Context compression.
4. Query result handles.
5. Error classification.
6. Retry and termination budgets.

These make longer workflows practical and recoverable.

**Phase 4 — Advanced capabilities**

1. Explicit OWL entailment configuration.
2. Semantic evidence and provenance.
3. Query-cost estimation.
4. Cross-graph reasoning.
5. More advanced query constructs.
6. Workflow optimization.

These improve expressiveness and scalability.

# **11. My recommendation for A–D**

I would retain all four variants, but make one important adjustment: separate the planner from the discovery process.

Instead of each planner having a completely different tool environment, give them the same discovery and inspection capabilities and require all four to produce the same IR.

Then evaluate them on:

- How many discovery calls they need.
- Whether they find the right vocabulary.
- Whether their plans are complete.
- How many repair iterations they require.
- How often they need user clarification.
- How accurately they construct query plans.
- How reliably they stop.

My initial expectations would be:

| Variant                 | Likely strength                        | Main weakness to investigate        |
| ----------------------- | -------------------------------------- | ----------------------------------- |
| A — Needle 3            | Incremental tool-oriented planning     | Excessive sequential calls          |
| B — MiniCPM5 tools      | Adaptive discovery and replanning      | Tool selection and loop reliability |
| C — MiniCPM5 structured | Low-latency plan generation            | Omitted steps and hidden ambiguity  |
| D — MiniCPM5 two-pass   | Explicit planning before serialization | Information loss between passes     |

I would not assume B is the winner. In particular, C could be surprisingly competitive if the task is sufficiently constrained and the Rust semantic resolver does most of the difficult work.

## **12. The most important design decision**

There is one architectural question I would resolve before implementing the rest:

**Should the LLM construct the complete operational plan, or should it primarily specify intent while Rust incrementally constructs the plan?**

I recommend the second approach.

For example, rather than asking MiniCPM5 to construct an entire multi-step SPARQL workflow, it could specify:

```
{
  "goal": "Find contacts belonging to accounts in Ohio",
  "entities": ["Contact", "Account", "Region"],
  "constraints": [
    {
      "entity": "Account",
      "property": "location",
      "value": "Ohio"
    }
  ],
  "requested_result": "Contact"
}
```

Rust then:

1. Resolves the entity and property candidates.
2. Inspects domain/range and ontology relationships.
3. Constructs a query plan.
4. Checks whether additional information is needed.
5. Executes the query.
6. Returns a compact result reference.

The LLM can continue planning from the actual results.

This approach reduces the amount of exact graph mechanics the small model must understand. It also means that the Rust implementation, rather than the LLM, becomes increasingly capable as you improve its ontology and query services.

**My preferred direction is therefore a hybrid: model-driven intent interpretation, Rust-driven semantic and query planning, and model-assisted iterative orchestration.** Keep A–D as competing front ends to that same architecture.
