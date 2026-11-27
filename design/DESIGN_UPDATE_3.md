# **Four designs for planning RDF operations with small language models**

The four options use two distinct models:

- **A — Cactus Needle 3:** Sequential tool calls that incrementally construct a plan, with the current plan status fed back into each subsequent model invocation.
- **B — MiniCPM5–1B:** Tool-calling planner that constructs a plan through planning-tool invocations.
- **C — MiniCPM5–1B:** Direct structured output that generates the complete plan in one constrained generation.
- **D — MiniCPM5–1B:** Two-pass generation: first a natural-language operational plan, then a second structured-output call that extracts it into the formal plan.

The distinction is important: A evaluates a specialized, very small tool-calling model, while B, C, and D evaluate three different planning strategies using the same 1B model.

All four should produce the same canonical Rust plan representation and use the same RDF execution engine. That lets you compare planning quality independently of the model and execution implementation.

# **1. Common design foundation**

The objective is to convert an arbitrary natural-language request into a step-by-step operational plan for an OWL/RDF knowledge store. The plan can include vocabulary discovery, instance discovery, graph queries, resource creation, updates, deletions, validation, and verification. It must also represent dependencies between operations and allow the plan to evolve when the actual ontology or data differs from expectations.

For example:

> Find all contacts belonging to accounts in Ohio, classify them as prospective customers, and report how many were updated.

This requires discovering the relevant classes and properties, understanding how contacts relate to accounts, identifying how account location is represented, querying for the matching contacts, finding or creating a prospective-customer classification resource, updating the contacts, validating the graph, and verifying the final count.

The model's role is to determine *what operations should be performed and in what order*. Rust remains responsible for executing those operations, compiling queries, enforcing graph context, validating ontology constraints, and controlling mutations.

The four options differ in how the plan is constructed:

| Option | Model           | Planning mechanism                                      |
| ------ | --------------- | ------------------------------------------------------- |
| A      | Cactus Needle 3 | Sequential tool calls; status fed back after each call  |
| B      | MiniCPM5–1B     | Tool calls to construct and revise a plan               |
| C      | MiniCPM5–1B     | Complete plan generated as structured output            |
| D      | MiniCPM5–1B     | Natural-language plan followed by structured extraction |

Needle 3 is specifically optimized for tool calling and structured extraction, with a small 29–121M parameter model and schema-constrained tool arguments. MiniCPM5–1B is a general-purpose small language model that supports tool calling through its native XML-style format.&#x20;

[image](https://www.google.com/s2/favicons?domain=https://cactuscompute.com\&sz=32)

Cactus+2



## **1.1 Canonical plan representation**

All four approaches should produce the same canonical Rust representation. This keeps the execution engine independent of the planner and allows the four approaches to be compared fairly.

The plan should be a directed acyclic graph of operations rather than merely a list of commands. Each step has an identifier, an operation type, an intent, typed arguments, dependencies, and an expected output type. Steps can refer to outputs from earlier steps, even when those outputs are not known when the plan is first constructed.

For example, a query step can refer to the vocabulary description produced by a previous step. An update step can refer to the contact URI set returned by the query and the classification resource returned by an instance search. These references should be represented as typed Rust values, not as arbitrary strings that the executor must interpret.

An illustrative plan might look like this:

```
{
  "goal": "Classify contacts at Ohio accounts as prospects",
  "context": {
    "dataset": "crm",
    "graphs": ["crm:production"]
  },
  "steps": [
    {
      "id": "s1",
      "operation": "search_vocabulary",
      "intent": "Discover contact, account, location and classification vocabulary",
      "depends_on": [],
      "output": "VocabularyCandidates"
    },
    {
      "id": "s2",
      "operation": "describe_vocabulary",
      "intent": "Resolve the relevant classes and relationships",
      "depends_on": ["s1"],
      "output": "ResolvedVocabulary"
    },
    {
      "id": "s3",
      "operation": "query",
      "intent": "Find contacts whose associated accounts are in Ohio",
      "depends_on": ["s2"],
      "output": "ContactUriSet"
    },
    {
      "id": "s4",
      "operation": "search_instances",
      "intent": "Find the prospective customer classification resource",
      "depends_on": ["s2"],
      "output": "ResourceCandidates"
    },
    {
      "id": "s5",
      "operation": "update",
      "intent": "Assign the prospect classification to matching contacts",
      "depends_on": ["s3", "s4"],
      "output": "MutationResult"
    },
    {
      "id": "s6",
      "operation": "validate",
      "intent": "Validate the updated graph",
      "depends_on": ["s5"],
      "output": "ValidationResult"
    },
    {
      "id": "s7",
      "operation": "verify",
      "intent": "Verify the final number of classified contacts",
      "depends_on": ["s5"],
      "output": "VerificationResult"
    }
  ]
}
```

The actual schema should use typed result references, such as `StepOutput<ContactUriSet>`, and typed RDF values distinguishing IRIs, blank nodes, literals, and unresolved vocabulary candidates. A step may be planned before all of its arguments are known; Rust resolves those arguments when the dependencies have completed.

## **1.2 Planning operations versus RDF operations**

The planner should have a limited vocabulary of operations. I would use `search_vocabulary`, `describe_vocabulary`, `search_instances`, `describe_resource`, `query`, `create`, `update`, `delete`, `validate`, and `verify`.

These are the operations that appear in the plan. They are not necessarily all available to the model during planning. In particular, the planner can be allowed to perform read-only discovery while constructing its plan, but actual mutations should be reserved for the execution phase.

This distinction prevents a model-generated planning call from accidentally modifying the knowledge graph. The planning tools manipulate a plan; the RDF tools operate on the repository. The Rust application controls the boundary between them.

## **1.3 Persistent planning and execution state**

Rust should maintain an `OperationContext` for each user request. It contains the original request, graph context, namespace mappings, discovered vocabulary, query results, plan, completed step outputs, pending steps, validation errors, and transaction state.

The model receives a compact representation of this state whenever it is invoked. It should not need to reconstruct the whole history. The plan and execution state remain authoritative in Rust.

The plan must support revision. If vocabulary discovery reveals that account location is represented through an address resource rather than a direct account property, the system should be able to revise the unexecuted query step or insert additional discovery steps. Completed steps should remain immutable in the execution history.

# **2. Option A — Cactus Needle 3 sequential tool calls**

Specialized tool-calling model

## **2.1 Concept**

Option A uses Cactus Needle 3 as the planning model. The model builds an operational plan incrementally by making one planning tool call at a time. After each call, Rust applies the requested change to the plan, updates the planning status, and feeds that current status back to Needle as the prompt for its next invocation.

The model continues this process until it calls a dedicated `finish_plan` terminator tool. That tool explicitly signals that Needle considers the plan complete. Rust then performs a final validation of the plan before handing it to the execution engine.

This design is particularly aligned with Needle 3's intended use. Needle takes text and tool schemas as input and returns structured tool calls. Its schemas are grammar-constrained, and the model is optimized for selecting tools and filling their arguments rather than producing general conversational responses.&#x20;

[image](https://www.google.com/s2/favicons?domain=https://cactuscompute.com\&sz=32)

Cactus+1



The critical idea is that the model is not asked to produce the whole plan at once. It makes a small planning decision, observes the updated state, and then makes another decision.

## **2.2 Planning tool definitions**

I would expose a very small set of planning tools.

`add_step` adds a new operation to the plan. It accepts a step ID, operation type, natural-language intent, arguments, dependencies, and expected output type. Arguments can contain known values or typed references to outputs of other steps. The tool only modifies the in-memory plan; it does not execute the RDF operation.

`revise_step` changes an unexecuted step. It can change the operation, intent, arguments, expected output, or dependencies. Rust should reject attempts to modify a completed or executing step. If a previous result must be superseded, Needle should add a replacement step and explicitly record the reason.

`remove_step` removes a step that is no longer necessary. Rust checks whether other steps depend on it and rejects removal if that would leave dangling references.

`inspect_plan` returns the current plan, its dependency graph, unresolved arguments, and any detected omissions. This gives Needle a way to reconsider the entire plan rather than relying only on the last tool result.

`finish_plan` terminates planning. It accepts a short completion rationale and any remaining assumptions. Rust then performs static validation and either accepts the plan or returns a structured validation failure.

A `request_clarification` tool can be added for cases where the user's intent is genuinely ambiguous. It should not be used merely because the ontology has not yet been searched.

The tool schemas should be compact. Needle 3 uses the tool descriptions and schemas provided to it; its documented current API does not describe a separate built-in tool retrieval head. Keeping the available planning toolset small will make its selection problem easier.&#x20;

[image](https://www.google.com/s2/favicons?domain=https://www.cactuscompute.com\&sz=32)

Cactus+1



## **2.3 The planning loop**

The first prompt contains the original user request, the planning instructions, the available planning tools, and the initial plan status. The initial status indicates that no steps have been added and no vocabulary or instance information has been discovered.

Needle returns a tool call. For example, it might call `add_step` to search for contact, account, location, and classification vocabulary. Rust validates the arguments and records the step as `s1`.

Rust then creates a new prompt. This prompt contains the original objective and a compact current-state summary:

```
User objective:
Classify contacts at Ohio accounts as prospects.

Current plan:
s1: Search vocabulary for contact, account, location,
    and classification terms.

Plan status:
1 step added.
No RDF operations executed.
No vocabulary terms resolved.

Remaining requirements:
- Resolve contact/account relationships.
- Identify the account location representation.
- Find matching contacts.
- Resolve the prospect classification.
- Update and validate the matching contacts.

Continue constructing the plan. Call finish_plan only when
all required operations and dependencies are represented.
```

Needle receives this new prompt through its next `complete()` call and chooses the next planning tool. It might add a vocabulary description step that depends on `s1`, then a query step that depends on the vocabulary description.

This process repeats until Needle calls `finish_plan`.

The important implementation detail is that Rust should reconstruct the prompt from the current state on every iteration. It should not blindly append the entire history of tool calls and results. The application owns the plan and produces a concise status prompt that gives Needle the information it needs for its next decision.

## **2.4 Using the actual Needle API**

The documented Needle 3 interface supports two relevant methods. `agent.complete(...)` performs a single model turn, leaving the application responsible for executing the tool and feeding the result back on the next call. `agent.run(...)` automates the tool execution loop. For this architecture, use `complete()` because Rust needs to inspect and validate every planning action before applying it.&#x20;

[image](https://www.google.com/s2/favicons?domain=https://github.com\&sz=32)

GitHub+1



Needle's documented package is Python-based. Since the main application is Rust, I would initially run a small local Python planning worker that hosts Needle and exposes a narrow IPC or HTTP interface to the Rust process. Rust remains the owner of the plan and invokes the worker for each model turn. The planning tools exposed to Needle can be thin wrappers that return proposed planning actions to Rust rather than directly mutating the plan.

A possible interaction looks like:

```
Rust PlannerSession
    |
    | prompt + planning tool schemas
    v
Needle 3 worker: complete()
    |
    | structured tool call
    v
Rust validates and applies planning action
    |
    | updated status + next prompt
    v
Needle 3 worker: complete()
    |
    | ...
    v
finish_plan
    |
    v
Rust final plan validation
```

This is different from calling `agent.run()` with executable tools that automatically perform their work. Here, the application deliberately owns the loop so it can inspect each decision.

## **2.5 Interspersing discovery and planning**

A useful extension is to allow read-only RDF discovery calls during the planning loop. Needle can call `search_vocabulary`, `describe_vocabulary`, or `search_instances` as planning-time tools. The Rust application executes these read-only operations and returns compact results.

Needle can then add or revise plan steps based on what it discovers. For example, it may initially expect an account to have a direct state property. A vocabulary description may show that account location is represented through an address resource. Needle can add a step to resolve the address relationship before constructing the query.

This is more powerful than having Needle build a static plan from the user's wording alone. It lets the model construct a plan grounded in the actual ontology.

However, the model should not perform mutations while planning. `create`, `update`, and `delete` should only be available to the later execution engine.

## **2.6 Limitations**

Needle 3 is optimized for tool selection and structured extraction, not deep multi-step reasoning. The planning actions therefore need to be small and explicit. It should not be expected to independently infer complex OWL semantics or devise intricate SPARQL queries.

The approach also requires multiple model invocations. The number of calls grows with the number of planning decisions. A maximum step count, duplicate-step detection, and no-progress detection are necessary to prevent endless loops.

A further concern is that the model can call `finish_plan` prematurely. Rust should not accept termination as proof of completeness. The final validator must check that the plan addresses the requested objective, has valid dependencies, and includes the necessary validation and verification steps.

**Assessment:** Option A is a strong fit for Needle 3's specialized tool-calling behavior. It is attractive when incremental, very fast planning decisions are more important than rich free-form reasoning. It also provides a useful baseline against which to compare MiniCPM5–1B.

# **3. Option B — MiniCPM5–1B tool-calling planner**

## **3.1 Concept**

Option B uses MiniCPM5–1B to construct the plan through tool calls. It follows the same broad incremental approach as Option A, but the model is a general-purpose 1B language model rather than Needle 3.

MiniCPM5–1B emits XML-style tool calls. The MiniCPM5 deployment documentation describes an SGLang parser that converts those calls into standard OpenAI-compatible tool calls. This makes it possible to use a conventional tool-calling interface from the Rust application.&#x20;

[image](https://www.google.com/s2/favicons?domain=https://huggingface.co\&sz=32)

openbmb/MiniCPM5-1B at main+1



The model is given planning tools and, optionally, read-only RDF discovery tools. It calls these tools as needed, receives results, and continues planning until it emits a final response or calls the explicit `finish_plan` tool.

The main difference from Option A is that MiniCPM5 is expected to contribute more general language understanding and reasoning to the planning process. It may be better at understanding complex user intent and decomposing it into a coherent sequence, but this needs to be measured rather than assumed.

## **3.2 Tool interface**

The planning tool set should be the same as Option A: `add_step`, `revise_step`, `remove_step`, `inspect_plan`, `request_clarification`, and `finish_plan`.

The model should also be allowed to call read-only discovery operations when appropriate. These include vocabulary search, vocabulary description, instance search, resource description, and read-only graph query.

I would not expose the full CRUD interface as planning tools. That would make it easy for the model to mix plan construction with execution. The planning tools should modify the plan, and read-only RDF tools should provide information. Mutations remain under the executor's control.

## **3.3 Model loop**

Rust sends MiniCPM5 a system prompt explaining its role as an operational planner. The prompt includes the original request, graph context, available operations, planning rules, and current plan state.

MiniCPM5 may respond with one or more tool calls. Rust parses the calls, validates them, and applies them to the planning state. If the model called `add_step`, the new step is inserted. If it called `search_vocabulary`, Rust executes the read-only search and returns the results.

The model then receives the tool results and continues planning. It can inspect the plan, add more steps, revise dependencies, or terminate with `finish_plan`.

The use of multiple tool calls in a single response should be handled carefully. Rust should not assume that calls are independent or that their ordering is safe. If the model adds both a query and an update in one response, the update must still explicitly depend on the query result and any classification-resource resolution. The Rust planner should reject a missing dependency rather than infer one silently.

## **3.4 Planning with actual ontology discovery**

The strongest version of Option B allows the model to perform read-only discovery while planning.

For the Ohio contacts example, MiniCPM5 might first call `search_vocabulary` for Contact, Account, state, address, and customer classification. It receives candidate vocabulary terms and then calls `describe_vocabulary` on the relevant candidates.

The returned ontology information can establish whether the account location is a literal, a geographic URI, or a relationship through an address resource. MiniCPM5 can then create a query step with the correct semantic intent and dependencies.

It can separately search for the prospect classification instance. If none exists, the plan can include a conditional creation step, subject to a policy that permits creating that resource.

The model can therefore alternate between vocabulary discovery, instance discovery, query construction, and plan modification. It is not forced to complete all vocabulary work before starting instance discovery.

## **3.5 Dependency management**

Rust should enforce dependencies as strongly as in Option A. Every step that consumes another step's output must reference it explicitly.

For example, the update step should require a `ContactUriSet`, a resolved classification predicate, and a `ResourceIri` for the prospect classification. Rust should reject an update if one of those values is missing or has the wrong type.

A plan compiler should check that the dependency graph is acyclic, that all references resolve to declared outputs, and that the expected input types match the producer's output types.

This makes the model's job easier. It only needs to identify the relationship between steps; Rust performs the actual type checking.

## **3.6 Model deployment**

The MiniCPM5–1B documentation describes SGLang deployment with an OpenAI-compatible API and the `minicpm5` tool-call parser. The documented parser is relatively recent, so the exact SGLang version matters: the current deployment guide notes that the parser landed in the SGLang main branch before the corresponding release was available. Pin and test a compatible version rather than assuming any installed SGLang release supports it.&#x20;

[image](https://www.google.com/s2/favicons?domain=https://github.com\&sz=32)

GitHub+1



Rust can call the local SGLang endpoint through an OpenAI-compatible client. This keeps the model interface independent of the RDF engine and avoids having to embed the Python model runtime in the main Rust process.

## **3.7 Limitations**

Tool calling does not guarantee that the plan is correct. MiniCPM5 may produce a valid tool call with an incorrect operation, omit an essential dependency, or prematurely declare the plan complete. Tool-call syntax and schema compliance only ensure that the output can be parsed.

The model may also over-plan. It might add redundant searches, repeatedly inspect the same vocabulary, or construct unnecessary intermediate steps. Rust should detect duplicate operations and provide feedback when a new step does not advance the plan.

**Assessment:** Option B is the most flexible MiniCPM5 approach. It combines incremental planning with the ability to inspect actual graph information during planning. It is the strongest candidate for complex operational requests if the model's tool-call accuracy is sufficient.

# **4. Option C — MiniCPM5–1B direct structured output**

## **4.1 Concept**

Option C asks MiniCPM5–1B to generate the complete operational plan in one structured-output generation. There are no planning tools, no incremental plan modifications, and no intermediate model calls. The model receives the user request, the graph context, the available operation definitions, and the plan schema. It returns a complete plan as JSON.

This is effectively a translation from natural language into a typed operational intermediate representation.

The model must identify the user's intended outcome, decompose it into operations, establish dependencies, identify the information each operation needs, and describe expected outputs. It must also identify unresolved vocabulary terms rather than inventing RDF identifiers.

This approach is the simplest of the three MiniCPM5 designs and provides a useful baseline for evaluating whether the 1B model can construct sufficiently good plans without an interactive loop.

## **4.2 Structured plan schema**

The schema should be designed around a few stable Rust structures.

`Plan` contains the overall goal, graph context, assumptions, clarification requirements, and steps. `PlanStep` contains the step ID, operation, intent, arguments, dependencies, and expected output type. `StepArguments` is a tagged union with different fields for vocabulary search, instance search, graph query, creation, update, deletion, validation, and verification.

The model should not be allowed to produce arbitrary operation names or arbitrary nested argument objects. Every operation should have a clearly defined argument schema. A vocabulary search takes search terms and namespace restrictions. A graph query takes a typed query intent or graph-pattern representation. An update takes a subject set, predicate, object, and mutation mode. These are different structures and should be represented differently.

The schema must support unresolved values. A plan can specify that it needs to find the predicate connecting contacts to accounts, without pretending that the predicate URI is already known. It should reference a prior vocabulary discovery step that will resolve that value.

The schema should also distinguish between an explicit value, a value that must be discovered, and a value derived from a previous operation. This distinction is important for preventing the model from hallucinating URIs or confusing a search term with an actual RDF resource.

## **4.3 Generation process**

Rust constructs a prompt containing the original request, graph context, available operations, planning rules, and the schema. MiniCPM5 then generates the plan.

For this approach, use constrained decoding if the selected inference backend supports the required JSON-schema or grammar constraints. MiniCPM5's published deployment guidance documents tool calling, while the exact structured-output mechanism should be selected and tested against the chosen backend rather than assumed to be identical to tool-call parsing.&#x20;

[image](https://www.google.com/s2/favicons?domain=https://github.com\&sz=32)

GitHub+1



The generated JSON is parsed into Rust types. Rust validates that the structure conforms to the schema and that the dependencies form a valid DAG. It then checks that every reference has a compatible producer and consumer.

The plan can be rejected if it contains invalid operation types, missing dependencies, unsupported fields, invalid graph scopes, or references to nonexistent step IDs.

## **4.4 Example**

For the Ohio contacts request, MiniCPM5 would generate the complete plan in one call. It might produce a vocabulary search, a vocabulary description, a query, an instance search for the prospect classification, an update, validation, and final verification.

The model does not know the actual results of the vocabulary search. Instead, it creates typed references that defer those values until execution.

For example, the query step references the resolved vocabulary output of the description step. The update step references the contact URI set produced by the query and the classification resource selected by the instance search.

The executor resolves those references as the plan runs. If the vocabulary search reveals that the assumed relationships do not exist, the executor can pause and request replanning.

## **4.5 Validation and repair**

The generated plan must pass several validation stages.

Structural validation checks the JSON schema and required fields. Dependency validation checks that all step references exist, that there are no cycles, and that output and input types match. Context validation checks that all named graphs, datasets, and namespace scopes are authorized. Operational validation checks that the plan has the necessary discovery steps before queries and appropriate validation after mutations.

A further check should assess whether the plan addresses all parts of the original request. For example, if the user requested both an update and a count of modified records, the plan needs a verification step that actually calculates that count.

When validation fails, Rust can send MiniCPM5 the invalid plan and a concise error report, asking it to regenerate the plan. This is a bounded repair loop, not the primary planning mechanism. The repaired plan must be validated again from the beginning.

I would cap this at one or two repair attempts. If the model cannot generate a valid plan, the application should report the issue or use a different planner rather than silently accepting an incomplete result.

## **4.6 Advantages and limitations**

The biggest advantage is simplicity. There is one model invocation, one structured output, and one validation stage. This reduces orchestration complexity and makes the planner easy to benchmark.

It is also deterministic from the application's perspective: a request produces a complete plan that can be inspected, stored, approved, and executed.

The limitation is that the model must plan without observing the results of discovery. It can anticipate that it needs to search for a relationship, but it cannot know which relationship the ontology actually contains. Deferred references help, but they do not give the model an opportunity to adapt its plan before execution.

This is likely to be sufficient for straightforward requests where the ontology is familiar or the discovery steps are predictable. It is less suitable for complex requests where each discovery result changes the next operation.

**Assessment:** Option C is the simplest baseline and should be implemented early. It will tell you how much planning ability MiniCPM5–1B has without an iterative tool loop.

# **5. Option D — MiniCPM5–1B two-step natural-language plan and structured extraction**

## **5.1 Concept**

Option D separates the planning task into two model invocations.

The first invocation generates a natural-language operational plan. It is not constrained to JSON. It focuses on understanding the request, determining the necessary operations, describing their order, and explaining the dependencies.

The second invocation receives that natural-language plan and converts it into the canonical structured plan representation. It is a structured extraction task rather than a fresh planning task.

Both invocations use MiniCPM5–1B.

The motivation is that generating a coherent plan in prose may be easier for a small model than simultaneously reasoning about the plan and generating a large nested JSON object. The second pass then focuses on encoding the already-decided sequence into the required structure.

## **5.2 First pass: natural-language planning**

The first prompt contains the user's request, the available RDF operations, the graph context, and instructions to generate an explicit operational plan.

The model should be asked to describe each step separately. It should identify the purpose of the step, the information it needs, the expected result, and any dependency on earlier steps. It should distinguish vocabulary discovery from instance discovery, and queries from mutations.

For example, the first pass might generate:

> First, search the CRM ontology for the Contact and Account classes, the relationship connecting contacts to accounts, and the vocabulary representing account location. Also search for the customer classification vocabulary. Describe the relevant terms and inspect their domains and ranges. Next, query for contacts whose associated accounts are located in Ohio, using the actual relationships and value representation discovered in the ontology. Separately search for an existing prospective-customer classification resource. If none exists, determine whether the ontology permits creating one. Once the contact set and classification resource have been resolved, update the matching contacts using the appropriate classification property. Validate the resulting graph and verify the number of contacts modified.

This first-stage output is a planning artifact, not an executable instruction. It should not contain raw SPARQL updates or direct graph mutations. It describes the intended operations and how they relate.

The prompt should explicitly require the model to state uncertainties. If the account location representation is unknown, the plan should say that it needs to be discovered rather than assuming a literal property.

## **5.3 Second pass: structured extraction**

The second prompt contains the complete natural-language plan and the formal plan schema. MiniCPM5 is instructed to extract the steps into the schema while preserving the original meaning, order, and dependencies.

The extractor should not add new operations, remove steps, or invent URIs. It should convert phrases such as "find the relationship connecting contacts to accounts" into a `search_vocabulary` or `describe_vocabulary` step, not guess a property IRI.

Similarly, "update the matching contacts" should become an update step whose target is a typed reference to the contact URI set produced by the earlier query.

The second pass should produce exactly the same `Plan` structure used in Option C. That means the executor and validator do not need to know whether a plan came from direct structured generation or two-stage extraction.

## **5.4 Structured extraction constraints**

The extraction prompt should treat the first-pass text as the source of truth. It should say explicitly that the model's job is to encode the plan, not improve it.

If the prose says that a step depends on two previous operations, the structured output must preserve both dependencies. If the prose leaves a value unresolved, the JSON must represent an unresolved value or a reference to a discovery result.

For example, if the first pass says:

> After identifying the matching contacts and locating the prospect classification resource, update the contacts.

The extractor must create an update step with dependencies on both the query step and the instance-search step. It must not omit one dependency simply because the sentence does not explicitly name step IDs.

Constrained output should be used for the second pass where possible. The model should only be able to produce valid operation names, typed argument structures, and supported reference types.

## **5.5 Consistency checking**

Rust should retain both the natural-language plan and the structured plan. This creates a useful audit trail: the prose shows what the model intended, and the JSON shows how that intent was encoded.

A consistency check can compare the two representations. The first pass should number its steps or use explicit phase labels so the extractor can preserve those identifiers. Rust can then check that every prose step has a corresponding structured step and that the ordering and dependencies are consistent.

For example, if the prose includes a separate instance search for the classification resource but the structured plan has no such step, the extractor has omitted part of the plan. Rust should reject it.

A further MiniCPM5 verification call could compare the two artifacts and identify omissions, but I would not make that mandatory initially. It adds another model invocation and does not replace deterministic validation.

## **5.6 Advantages and limitations**

The main advantage is the separation of semantic planning from structural encoding. The model can first focus on understanding and sequencing the operations, then focus on representing them accurately.

The natural-language plan is also useful for explaining the intended process to a user, debugging model behavior, and keeping an audit record of the planning decision.

The main disadvantages are the two inference calls and the possibility of translation errors. The extraction step may omit a dependency, misinterpret a step, or introduce an operation that was not present in the original plan.

The first-stage prose also needs to be sufficiently detailed. A vague plan such as "find the contacts and update their status" is not enough to extract a reliable operational sequence. The first prompt must require explicit discovery, selection, mutation, and verification steps.

**Assessment:** Option D is worth testing if MiniCPM5 produces more coherent operational plans in prose than in direct structured output. It provides a human-readable planning artifact, but its accuracy depends on preserving meaning across the two generations.

# **6. Comparison**

| Attribute                           | A: Needle 3                | B: MiniCPM tool calls | C: MiniCPM direct JSON      | D: MiniCPM two-pass             |
| ----------------------------------- | -------------------------- | --------------------- | --------------------------- | ------------------------------- |
| Model size                          | 29–121M                    | 1B                    | 1B                          | 1B                              |
| Calls per plan                      | Multiple                   | Multiple              | 1                           | 2                               |
| Incremental planning                | Yes                        | Yes                   | No                          | No                              |
| Read-only discovery during planning | Yes                        | Yes                   | No                          | No                              |
| Structured output                   | Tool schemas               | Tool schemas          | Plan schema                 | Extraction schema               |
| Natural-language plan artifact      | Optional                   | Optional              | No                          | Yes                             |
| Can revise plan interactively       | Yes                        | Yes                   | Only via repair             | Only via repair                 |
| Implementation complexity           | Medium                     | Medium                | Low                         | Medium                          |
| Primary strength                    | Fast tool selection        | Adaptive planning     | Simplicity                  | Separate reasoning and encoding |
| Primary weakness                    | Limited reasoning capacity | Tool-call reliability | No feedback during planning | Translation errors              |

One qualification: the two interactive approaches can be configured to perform read-only discovery during planning, but this is an architectural choice, not an automatic property of tool calling.

# **7. Shared Rust architecture**

I would implement a common planner interface with three MiniCPM5 implementations and one Needle implementation.

```
trait Planner {
    async fn create_plan(
        &self,
        request: &UserRequest,
        context: &OperationContext,
    ) -> Result<Plan, PlannerError>;
}
```

The implementations would be:

```
struct NeedleSequentialPlanner;
struct MiniCpmToolPlanner;
struct MiniCpmStructuredPlanner;
struct MiniCpmTwoPassPlanner;
```

The `create_plan` interface can internally use multiple model calls. For the interactive approaches, it manages the planning loop. For the structured approach, it makes one call. For the two-pass approach, it makes two calls.

I would keep the following modules independent of the selected planner:

| Module              | Responsibility                                       |
| ------------------- | ---------------------------------------------------- |
| `plan`              | Canonical plan and typed step representation         |
| `planner`           | Planner interface and implementations                |
| `plan_validator`    | Schema, dependency, and operational validation       |
| `plan_compiler`     | Resolve references and prepare executable operations |
| `operation_context` | Persistent request, graph context, and results       |
| `query`             | Query compilation and execution                      |
| `ontology`          | Vocabulary discovery, OWL, SKOS, SHACL               |
| `repository`        | Oxigraph and RDF transactions                        |
| `executor`          | Step scheduling, execution, and verification         |

The most important component is `plan_validator`. It should enforce the same correctness requirements regardless of which model produced the plan.

## **7.1 Dynamic replanning**

I would support dynamic replanning for all four approaches, even though it is most natural in A and B.

Consider the following sequence:

1. The planner proposes a vocabulary search.
2. Rust discovers that account location is represented by an address resource.
3. The original query plan is now incomplete.
4. The planner is invoked again with the updated execution state.
5. It adds a step to resolve addresses and revises the query dependencies.
6. Execution continues.

The completed steps are retained. Only pending steps are revised.

This is an important capability: the initial plan is not expected to anticipate every detail of the ontology. The planner should be able to respond to the actual graph.

## **7.2 Mutation safety**

The planning models should not have unrestricted access to mutations. The executor should perform all create, update, and delete operations through a transaction-aware interface.

Before committing, Rust should verify authorization, graph scope, ontology compatibility, and SHACL constraints. For operations affecting many resources, it should support previewing the selected URI set and requiring approval.

The final verification step should independently query the graph to confirm the requested outcome. The model's declaration that a task is complete is not evidence that the operation succeeded.

# **8. Recommended implementation and evaluation sequence**

I would implement the approaches in this order:

**First: Option C.** It is the simplest way to establish whether MiniCPM5–1B can generate useful operational plans at all. It provides a clean baseline and tests the canonical plan schema.

**Second: Option B.** This is likely to be the most useful production architecture if the model can reliably construct and revise plans. Its ability to inspect vocabulary and instance-search results during planning is a major advantage for an RDF system.

**Third: Option D.** Compare its natural-language planning quality against direct structured output. It may provide better plans if the model struggles with nested JSON, but that is an empirical question.

**Fourth: Option A.** Evaluate Needle 3 as a specialized, extremely small sequential planning controller. Its performance on tool selection and structured arguments may be excellent, but the experiment should establish whether it can reliably decompose complex RDF requests into a complete dependency graph.

I would test all four against the same 100-request evaluation set. Include simple searches, multi-pattern queries, vocabulary creation, multi-resource updates, and complex requests that alternate between vocabulary discovery and instance discovery.

Measure plan completeness, dependency correctness, vocabulary accuracy, query intent accuracy, schema validity, replanning success, inference latency, and total model calls. Importantly, evaluate the actual resulting RDF state, not just whether the generated plan looks reasonable.

# **9. Final recommendation**

Preferred production candidate: B

My preference is **Option B, MiniCPM5–1B tool calling**, with Option C as the initial benchmark.

The reason is that RDF planning is inherently discovery-dependent. The model often cannot know the correct sequence until it has inspected the ontology and some of the instance data. An incremental planner that can inspect results and adjust its remaining steps is a better conceptual fit than a one-shot plan generator.

Option A is particularly interesting because Needle 3 may be fast enough to make many small planning decisions cheaply. It gives you a way to test how much of the workflow can be handled by a specialized tool-selection model rather than a general-purpose LLM.

Option C is valuable because it provides a very simple baseline. If it can generate correct plans for most requests, you may not need a complicated agent loop for ordinary operations.

Option D is an experiment in separating semantic planning from structural encoding. I would only choose it if its measured improvement in plan quality justifies the extra inference call.

**One architectural decision should be common to all four:** the LLM produces a plan, Rust owns the plan, and the RDF execution engine owns the data. The planner can discover, reason, propose, and revise. It cannot bypass the validator or directly mutate the repository.

Useful implementation references:

- [Cactus Needle 3 Python API and manual tool-call loop](https://www.cactuscompute.com/blog/needle-python-docs?utm_source=chatgpt.com)
- [Needle 3 model and capabilities](https://cactuscompute.com/needle?trk=article-ssr-frontend-pulse_little-text-block\&utm_source=chatgpt.com)
- [MiniCPM5–1B model and tool-calling format](https://huggingface.co/openbmb/MiniCPM5-1B/blob/main/README.md?utm_source=chatgpt.com)
- [MiniCPM5 SGLang deployment guide](https://github.com/OpenBMB/MiniCPM/blob/main/docs/deployment/sglang.md?utm_source=chatgpt.com)
