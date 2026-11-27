# **Incremental live-model testing and development loop**

The testing process should be organized around one principle: **test one capability at a time using the actual running model, identify the precise source of failure, improve that part of the system, and rerun the same tests before introducing the next capability.**

System A (Needle 3) and System B (MiniCPM5-1B) should have separate testing loops. They share the same RDF fixtures, ontology services, test definitions, and deterministic Rust components, but they must not share model-generated decisions or workflow state.

The process should not begin by testing complete natural-language-to-SPARQL workflows. That would make failures difficult to diagnose: an incorrect result might originate in intent extraction, vocabulary retrieval, path selection, query construction, or model prompting. Instead, each capability is introduced as an independently observable stage, with explicit input, expected output, and pass/fail criteria.

Each stage has a development cycle:

1. Establish a deterministic baseline.
2. Run the live model against a controlled test set.
3. Capture the exact model context, output, and execution trace.
4. Compare the result against ground truth.
5. Categorize failures by their source.
6. Modify the relevant code, context builder, retrieval logic, or model adapter.
7. Rerun the same tests, including previously passing tests.
8. Freeze the stage when it meets its acceptance criteria.

The next stage is introduced only after the current stage is sufficiently reliable. This makes it possible to determine exactly which capabilities each model contributes and which capabilities are better implemented deterministically in Rust.

## **1. Test harness architecture**

The test harness should be independent of the production conversation interface. It should invoke the actual local model endpoint and the actual Rust services, but it should provide controlled requests and deterministic graph fixtures.

\#chatgpt-mermaid-\_r_5b6\_{font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,Segoe UI,Helvetica,Apple Color Emoji,Arial,sans-serif,Segoe UI Emoji,Segoe UI Symbol;font-size:16px;fill:rgb(13, 13, 13);}@keyframes edge-animation-frame{from{stroke-dashoffset:0;}}@keyframes dash{to{stroke-dashoffset:0;}}#chatgpt-mermaid-\_r_5b6\_ .edge-animation-slow{stroke-dasharray:9,5!important;stroke-dashoffset:900;animation:dash 50s linear infinite;stroke-linecap:round;}#chatgpt-mermaid-\_r_5b6\_ .edge-animation-fast{stroke-dasharray:9,5!important;stroke-dashoffset:900;animation:dash 20s linear infinite;stroke-linecap:round;}#chatgpt-mermaid-\_r_5b6\_ .error-icon{fill:rgb(243, 243, 243);}#chatgpt-mermaid-\_r_5b6\_ .error-text{fill:rgb(13, 13, 13);stroke:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_5b6\_ .edge-thickness-normal{stroke-width:1px;}#chatgpt-mermaid-\_r_5b6\_ .edge-thickness-thick{stroke-width:3.5px;}#chatgpt-mermaid-\_r_5b6\_ .edge-pattern-solid{stroke-dasharray:0;}#chatgpt-mermaid-\_r_5b6\_ .edge-thickness-invisible{stroke-width:0;fill:none;}#chatgpt-mermaid-\_r_5b6\_ .edge-pattern-dashed{stroke-dasharray:3;}#chatgpt-mermaid-\_r_5b6\_ .edge-pattern-dotted{stroke-dasharray:2;}#chatgpt-mermaid-\_r_5b6\_ .marker{fill:rgb(143, 143, 143);stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_5b6\_ .marker.cross{stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_5b6\_ svg{font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,Segoe UI,Helvetica,Apple Color Emoji,Arial,sans-serif,Segoe UI Emoji,Segoe UI Symbol;font-size:16px;}#chatgpt-mermaid-\_r_5b6\_ p{margin:0;}#chatgpt-mermaid-\_r_5b6\_ .label{font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,Segoe UI,Helvetica,Apple Color Emoji,Arial,sans-serif,Segoe UI Emoji,Segoe UI Symbol;color:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_5b6\_ .cluster-label text{fill:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_5b6\_ .cluster-label span{color:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_5b6\_ .cluster-label span p{background-color:transparent;}#chatgpt-mermaid-\_r_5b6\_ .label text,#chatgpt-mermaid-\_r_5b6\_ span{fill:rgb(13, 13, 13);color:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_5b6\_ .node rect,#chatgpt-mermaid-\_r_5b6\_ .node circle,#chatgpt-mermaid-\_r_5b6\_ .node ellipse,#chatgpt-mermaid-\_r_5b6\_ .node polygon,#chatgpt-mermaid-\_r_5b6\_ .node path{fill:rgb(222, 234, 251);stroke:rgb(83, 154, 248);stroke-width:1px;}#chatgpt-mermaid-\_r_5b6\_ .rough-node .label text,#chatgpt-mermaid-\_r_5b6\_ .node .label text,#chatgpt-mermaid-\_r_5b6\_ .image-shape .label,#chatgpt-mermaid-\_r_5b6\_ .icon-shape .label{text-anchor:middle;}#chatgpt-mermaid-\_r_5b6\_ .node .katex path{fill:#000;stroke:#000;stroke-width:1px;}#chatgpt-mermaid-\_r_5b6\_ .rough-node .label,#chatgpt-mermaid-\_r_5b6\_ .node .label,#chatgpt-mermaid-\_r_5b6\_ .image-shape .label,#chatgpt-mermaid-\_r_5b6\_ .icon-shape .label{text-align:center;}#chatgpt-mermaid-\_r_5b6\_ .node.clickable{cursor:pointer;}#chatgpt-mermaid-\_r_5b6\_ .root .anchor path{fill:rgb(143, 143, 143)!important;stroke-width:0;stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_5b6\_ .arrowheadPath{fill:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_5b6\_ .edgePath .path{stroke:rgb(143, 143, 143);stroke-width:1px;}#chatgpt-mermaid-\_r_5b6\_ .flowchart-link{stroke:rgb(143, 143, 143);fill:none;}#chatgpt-mermaid-\_r_5b6\_ .edgeLabel{background-color:rgb(252, 252, 252);text-align:center;}#chatgpt-mermaid-\_r_5b6\_ .edgeLabel p{background-color:rgb(252, 252, 252);}#chatgpt-mermaid-\_r_5b6\_ .edgeLabel rect{opacity:0.5;background-color:rgb(252, 252, 252);fill:rgb(252, 252, 252);}#chatgpt-mermaid-\_r_5b6\_ .labelBkg{background-color:rgba(252, 252, 252, 0.5);}#chatgpt-mermaid-\_r_5b6\_ .cluster rect{fill:rgb(243, 243, 243);stroke:rgba(0, 0, 0, 0.1);stroke-width:1px;}#chatgpt-mermaid-\_r_5b6\_ .cluster text{fill:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_5b6\_ .cluster span{color:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_5b6\_ div.mermaidTooltip{position:absolute;text-align:center;max-width:200px;padding:2px;font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,Segoe UI,Helvetica,Apple Color Emoji,Arial,sans-serif,Segoe UI Emoji,Segoe UI Symbol;font-size:12px;background:rgb(243, 243, 243);border:1px solid rgba(0, 0, 0, 0.1);border-radius:2px;pointer-events:none;z-index:100;}#chatgpt-mermaid-\_r_5b6\_ .flowchartTitleText{text-anchor:middle;font-size:18px;fill:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_5b6\_ rect.text{fill:none;stroke-width:0;}#chatgpt-mermaid-\_r_5b6\_ .icon-shape,#chatgpt-mermaid-\_r_5b6\_ .image-shape{background-color:rgb(252, 252, 252);text-align:center;}#chatgpt-mermaid-\_r_5b6\_ .icon-shape p,#chatgpt-mermaid-\_r_5b6\_ .image-shape p{background-color:rgb(252, 252, 252);padding:2px;}#chatgpt-mermaid-\_r_5b6\_ .icon-shape .label rect,#chatgpt-mermaid-\_r_5b6\_ .image-shape .label rect{opacity:0.5;background-color:rgb(252, 252, 252);fill:rgb(252, 252, 252);}#chatgpt-mermaid-\_r_5b6\_ .label-icon{display:inline-block;height:1em;overflow:visible;vertical-align:-0.125em;}#chatgpt-mermaid-\_r_5b6\_ .node .label-icon path{fill:currentColor;stroke:revert;stroke-width:revert;}#chatgpt-mermaid-\_r_5b6\_ .node .neo-node{stroke:rgb(83, 154, 248);}#chatgpt-mermaid-\_r_5b6\_ [data-look="neo"].node rect,#chatgpt-mermaid-\_r_5b6\_ [data-look="neo"].cluster rect,#chatgpt-mermaid-\_r_5b6\_ [data-look="neo"].node polygon{stroke:url(#chatgpt-mermaid-\_r_5b6\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_5b6\_ [data-look="neo"].swimlane.cluster rect{filter:none;}#chatgpt-mermaid-\_r_5b6\_ [data-look="neo"].node path{stroke:url(#chatgpt-mermaid-\_r_5b6\_-gradient);stroke-width:1px;}#chatgpt-mermaid-\_r_5b6\_ [data-look="neo"].node .outer-path{filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_5b6\_ [data-look="neo"].node .neo-line path{stroke:rgb(83, 154, 248);filter:none;}#chatgpt-mermaid-\_r_5b6\_ [data-look="neo"].node circle{stroke:url(#chatgpt-mermaid-\_r_5b6\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_5b6\_ [data-look="neo"].node circle .state-start{fill:#000000;}#chatgpt-mermaid-\_r_5b6\_ [data-look="neo"].icon-shape .icon{fill:url(#chatgpt-mermaid-\_r_5b6\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_5b6\_ [data-look="neo"].icon-shape .icon-neo path{stroke:url(#chatgpt-mermaid-\_r_5b6\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_5b6\_ .node text{font-size:14px;font-weight:600;letter-spacing:normal;fill:rgb(0, 79, 153);}#chatgpt-mermaid-\_r_5b6\_ .edgeLabels text{font-size:13px;font-weight:600;letter-spacing:-0.08px;fill:rgb(0, 79, 153);}#chatgpt-mermaid-\_r_5b6\_ .node tspan[font-weight="normal"],#chatgpt-mermaid-\_r_5b6\_ .edgeLabels tspan[font-weight="normal"]{font-weight:600;}#chatgpt-mermaid-\_r_5b6\_ .edgeLabel .label rect{opacity:1;rx:13px;ry:13px;fill:rgb(245, 250, 255);stroke:rgb(206, 219, 229);stroke-width:1px;}#chatgpt-mermaid-\_r_5b6\_ .node rect,#chatgpt-mermaid-\_r_5b6\_ .node circle,#chatgpt-mermaid-\_r_5b6\_ .node ellipse,#chatgpt-mermaid-\_r_5b6\_ .node polygon,#chatgpt-mermaid-\_r_5b6\_ .node path{fill:rgb(229, 243, 255);stroke:rgba(0, 0, 0, 0.1);stroke-width:1px;}#chatgpt-mermaid-\_r_5b6\_ .node rect{rx:16px;ry:16px;}#chatgpt-mermaid-\_r_5b6\_ .mermaid-decision .label-container{fill:rgb(245, 250, 255);stroke:rgb(206, 219, 229);stroke-dasharray:2px,2px;}#chatgpt-mermaid-\_r_5b6\_ .edgePaths .flowchart-link{stroke:rgb(143, 143, 143);stroke-width:1px;stroke-linecap:round;stroke-linejoin:round;}#chatgpt-mermaid-\_r_5b6\_ .marker{fill:rgb(143, 143, 143);stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_5b6\_ :root{--mermaid-font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,Segoe UI,Helvetica,Apple Color Emoji,Arial,sans-serif,Segoe UI Emoji,Segoe UI Symbol;}Test CaseHarnessLive Model AdapterPrompt and Context BuilderNeedle 3 or MiniCPM5Structured Model OutputOutput ValidatorWorkflow ControllerOntology / Query / MutationServicesExecution EvidenceGround Truth EvaluatorFailure DiagnosisCode or Context Changes

There should be two model adapters, one for each system, but a shared test-harness interface. The harness should not abstract away model-specific behavior. In particular, it must preserve the actual request format, sampling settings, reasoning configuration, tool-call format, and output parsing used by the respective live model.

The harness should record every model invocation as an immutable test event. Each event should include the test ID, system ID, phase, invocation number, model name and version, endpoint configuration, exact input messages, available actions, model output, parsed output, validation result, execution result, and elapsed time. For MiniCPM, it should additionally record whether reasoning was enabled and retain the reasoning-related response fields available from the runtime. For Needle, it should record the selected model configuration and any routing or tool-selection metadata exposed by its runtime.

Every test run should have a fixed test fixture and a unique run identifier. The fixture includes the RDF dataset, ontology, graph scope, reference date, inference settings, authorization context, and expected answers. The evaluator must be independent of the model. It should compare structured semantic outcomes rather than relying on a second LLM to judge the first LLM.

A test should be replayable. Given the same model version, settings, fixture, and context-builder version, it should be possible to reconstruct exactly what the model was asked and what it returned. This is particularly important when modifying prompts: otherwise, a model improvement may actually be caused by a change in the input evidence.

### **The test harness must support three execution modes**

| Mode        | Behavior                                                                                 | Purpose                                 |
| ----------- | ---------------------------------------------------------------------------------------- | --------------------------------------- |
| Isolated    | Invoke one model decision with fixed context.                                            | Diagnose an individual capability.      |
| Interactive | Invoke the model, execute its requested read-only action, return evidence, and continue. | Test discovery and iterative reasoning. |
| End-to-end  | Execute the entire workflow, including mutation preview and validation.                  | Test integration and safety.            |

All three modes should use the same live model. The isolated mode is not a substitute for live testing: it is a way to isolate one decision while preserving the real model behavior.

# **2. The incremental development loop**

The following phases apply independently to System A and System B. Each phase has its own test set, context requirements, acceptance criteria, and opportunities to rebuild the corresponding code.

## **Phase 0 — Establish the live-model baseline**

Foundation

Before testing any RDF functionality, establish that each model can be called reliably through its actual serving configuration.

For System A, invoke Needle 3 using its intended inference interface. For System B, invoke MiniCPM5-1B through the actual llama.cpp endpoint and model template. The test harness must use the same message format, tool-call conventions, and decoding settings that the eventual application will use.

Use simple inputs that require no ontology knowledge. Test ordinary text responses, constrained structured output, incomplete output, invalid structured output, and requests with deliberately irrelevant information.

For MiniCPM5, test both reasoning-enabled and reasoning-disabled configurations. Keep the selected configuration fixed during each comparison. For Needle, test its structured extraction and tool-selection capabilities using the actual production inference mode.

**Context supplied:** Only the system instruction, a small task description, and the requested output schema. No RDF context or workflow history.

**Tests:**

- Produce a valid structured response.
- Extract two or three fields from a short request.
- Return an explicit unresolved value when information is absent.
- Handle a malformed or unsupported request.
- Repeat the same request several times to assess output consistency.
- Return output within the application's configured limits.

**Record:** Output validity, schema compliance, latency, token counts where available, response length, malformed responses, and repeatability.

**Rebuild opportunities:** Model adapter, chat-template handling, reasoning configuration, decoding parameters, structured-output constraints, output parser, retry behavior, and timeout handling.

**Exit condition:** Both systems can be invoked repeatedly and return outputs that the Rust application can reliably parse and validate. No RDF-related work should be used to conceal problems in the basic model adapter.

## **Phase 1 — Intent extraction**

Capability 1

Test whether the model can understand the user's request and extract its semantic structure without having to resolve ontology concepts.

The model receives a natural-language request and must identify the action, target entities, relationship phrases, conditions, requested output, and mutation intent. All extracted concepts remain ordinary-language phrases. It must not supply RDF IRIs, SPARQL, or invented ontology properties.

For example:

> Find contacts working for accounts in Ohio that attended a technology conference last year.

Expected output:

```
{
  "action": "find",
  "targets": ["contacts"],
  "conditions": [
    {
      "subject": "contacts",
      "relationship": "working for",
      "object": "accounts"
    },
    {
      "subject": "accounts",
      "relationship": "in",
      "object": "Ohio"
    },
    {
      "subject": "contacts",
      "relationship": "attended",
      "object": "technology conference",
      "time": "last year"
    }
  ],
  "requested_output": ["contacts"],
  "mutation": false
}
```

The actual expected schema should also support condition IDs, source spans, temporal expressions, and explicit unresolved fields. The example above illustrates the semantic content, not a complete production schema.

**Context supplied:** The original request, extraction instructions, the permitted action vocabulary, and a strict schema. No ontology candidates, graph information, or discovery tools.

**Test progression:**

| Test group | Input characteristics                               |
| ---------- | --------------------------------------------------- |
| 1A         | Simple find, describe, create, update, delete.      |
| 1B         | Multiple entities and relationship phrases.         |
| 1C         | Multiple conditions and requested output fields.    |
| 1D         | Negation, temporal phrases, and Boolean conditions. |
| 1E         | Ambiguous wording and compound requests.            |

For each group, use multiple paraphrases. For example, “contacts employed by Acme,” “people who work at Acme,” and “Acme's employees” should preserve the same intended relationship structure.

**System A:** This should be a primary capability. Needle should be tested on direct extraction, consistency, and its ability to avoid adding information that the user did not provide.

**System B:** Test whether MiniCPM extracts the same structure and preserves more complicated conditions. It should not start ontology discovery yet.

**Rebuild opportunities:** Extraction schema, task prompt, field definitions, phrase preservation, temporal normalization, condition representation, and output validation.

**Exit condition:** The model reliably extracts all material conditions without inventing ontology mappings, changing relationship direction, or dropping constraints. Ambiguous phrases remain ambiguous instead of being silently normalized.

## **Phase 2 — Ontology vocabulary matching**

Capability 2

Now introduce the actual ontology, but only for vocabulary matching. The system must resolve extracted phrases to existing ontology classes, properties, or SKOS concepts.

The retrieval component should produce a candidate set. The model's job is to choose among the candidates or indicate that none is suitable.

For example, the phrase `technology conference` might produce:

| Candidate       | Type         | Definition                             |
| --------------- | ------------ | -------------------------------------- |
| Conference      | OWL class    | A formal organized gathering.          |
| TechnologyEvent | OWL class    | An event concerning technology.        |
| TradeShow       | OWL class    | An exhibition for commercial products. |
| Technology      | SKOS concept | A subject-area concept.                |

The model must distinguish the event class from the subject-area concept.

**Context supplied:** The original phrase, its semantic role, already resolved neighboring concepts, and the candidate list. Every candidate has an opaque ID, preferred label, alternative labels, definition, type, and relevant hierarchy information. The model must return a candidate ID or `unresolved`.

The retrieval results should be generated before the model call. At this phase, the retrieval system should not use model-selected concepts to alter the candidate list during the same test.

**Test progression:**

- Exact label matches.
- Alternative labels and synonyms.
- Different labels with equivalent meaning.
- Similar labels with different meanings.
- Class versus property confusion.
- Class versus SKOS concept confusion.
- No suitable candidate.
- Multiple genuinely plausible candidates.

**System A:** Focus on reliable candidate selection with small lists. Test how candidate-list size affects accuracy. Compare three candidates, five candidates, and larger lists without changing the underlying semantic problem.

**System B:** Test candidate interpretation and whether the model can identify missing information that would distinguish two concepts. It may request an inspection, but it cannot invent a candidate.

**Rebuild opportunities:** Vocabulary indexing, candidate ranking, label normalization, definition extraction, candidate descriptions, context formatting, and model selection instructions.

**Exit condition:** The correct candidate is selected when the evidence is sufficient; the model returns unresolved when it is not. No unsupported IRI or candidate ID is accepted.

## **Phase 3 — Ontology inspection and evidence interpretation**

Capability 3

This phase tests whether the models can understand actual ontology evidence rather than merely matching labels.

Introduce inspection results containing:

- RDF type.
- Class hierarchy.
- Property domain and range.
- Inverse properties.
- Equivalent classes and properties.
- Subclass and subproperty relationships.
- Definitions and comments.
- Relevant restrictions and characteristics.
- Whether a relationship is directly asserted or inferred.

The goal is not to ask the model to reason over OWL in general. The goal is to determine whether it can interpret the limited, structured evidence retrieved by Rust.

For example, the model may receive:

```
{
  "property": "candidate_17",
  "label": "works for",
  "domain": "Contact",
  "range": "Account",
  "inverse": "candidate_18",
  "definition": "Identifies the account for which a contact works."
}
```

It should understand that this relationship connects Contact to Account, and that the inverse direction is Account to Contact.

**Context supplied:** The user's original relationship phrase, the already selected endpoint concepts, and the ontology inspection results. Include explicit descriptions of which statements are asserted and which are inferred. Do not include unrelated ontology properties.

**Test progression:**

1. Identify the domain and range of a property.
2. Determine whether a property connects two specified classes.
3. Interpret inverse relationships.
4. Distinguish subclasses from equivalent classes.
5. Distinguish subproperties from equivalent properties.
6. Recognize when a property is incompatible with the requested endpoints.
7. Identify missing evidence.

**System A:** Test small evidence packages with one or two competing properties. It should make straightforward compatibility decisions, not perform unrestricted OWL reasoning.

**System B:** Test interpretation across several related properties and class hierarchies. It should be able to request additional inspection when the supplied evidence does not answer the question.

**Rebuild opportunities:** Ontology inspection serializers, evidence normalization, domain/range representation, hierarchy presentation, definition retrieval, and evidence-status labels.

**Exit condition:** Correct interpretation of all supplied ontology evidence, including direction, hierarchy, and assertion status. A model that invents an unstated inverse or treats a subclass as equivalent fails.

## **Phase 4 — Relationship path resolution**

Capability 4

This is the first phase where the model must choose among multi-step relationship paths.

Rust's path resolver searches for valid paths between two resolved concepts. The model receives those paths and decides which one corresponds to the user's wording. The model must not construct arbitrary paths or invent properties.

For example, the ontology may contain:

```
Contact
  └── worksFor → Account

Account
  ├── headquarteredIn → Region
  └── operatesIn → Region

Region
  └── withinState → State
```

The user asks:

> Find contacts working for accounts in Ohio.

The system should distinguish headquarters from operating location. It should not automatically choose the shortest path or assume that either interpretation is equivalent.

**Context supplied:** The original request, the relevant relationship phrase, resolved start and end concepts, the candidate paths, intermediate classes, property definitions, path directions, and the semantic differences between the paths.

Each candidate path should have a stable ID. The model returns a candidate path ID, a request for further inspection, or an ambiguity response.

**Test progression:**

- Single direct path.
- Single inverse path.
- Single multi-hop path.
- Two multi-hop paths with different semantics.
- Several paths with overlapping definitions.
- No valid path.
- Path exists only through inference.
- Path is valid structurally but semantically incompatible with the request.

**System A:** Rust should perform path discovery and reduce the choices to a small candidate set. Needle should be tested on selection accuracy and its ability to reject unsupported paths.

**System B:** MiniCPM should be tested on selecting paths, comparing definitions, and identifying what additional ontology evidence it needs. It should not be allowed to replace the resolver's candidates with a path of its own invention.

**Rebuild opportunities:** Path-search algorithm, candidate ranking, maximum path depth, cycle handling, path descriptions, intermediate-class presentation, and model-selection prompts.

**Exit condition:** The selected path must be valid and semantically appropriate. The model must reject or escalate unresolved ambiguity rather than selecting a plausible but unsupported route.

## **Phase 5 — Logical query construction**

Capability 5

Now test whether the model can convert an accepted intent and resolved ontology paths into a logical query specification.

This phase should not yet test arbitrary SPARQL generation. The model returns a structured logical representation; Rust validates and compiles it.

Example:

```
{
  "return": "Contact",
  "where": [
    {
      "operator": "relationship",
      "path": "path_12"
    },
    {
      "operator": "equals",
      "subject": "Account",
      "property": "property_21",
      "value": "Ohio"
    }
  ]
}
```

The production schema should distinguish resource-valued terms from literals and include explicit variable bindings and condition scopes. The example is intentionally abbreviated.

**Context supplied:** Accepted intent, resolved classes and properties, selected paths, definitions of permitted operators, requested output, and the exact logical query schema. All identifiers must refer to previously supplied candidates or accepted workflow objects.

**Test progression:**

| Subphase | Query complexity                               |
| -------- | ---------------------------------------------- |
| 5A       | Single class selection.                        |
| 5B       | One relationship and one filter.               |
| 5C       | Multiple relationships and filters.            |
| 5D       | AND / OR combinations.                         |
| 5E       | NOT EXISTS and correlated negative conditions. |
| 5F       | Temporal conditions and nested expressions.    |

The query compiler should be tested independently using hand-authored logical specifications. This is important: if the compiler is wrong, it must not be mistaken for a model failure.

**System A:** Prefer deterministic query construction from extracted intent and accepted path IDs. Test Needle on filling missing fields and selecting among alternative query interpretations, not on generating complicated query structures.

**System B:** Test its ability to produce complete logical query specifications, preserve all conditions, and correctly represent Boolean and negative semantics.

**Rebuild opportunities:** Logical query schema, query-specification context, operator definitions, condition normalization, validation, and query compiler.

**Exit condition:** The logical specification contains every intended condition, references only accepted ontology objects, and compiles into the expected graph pattern.

## **Phase 6 — Instance search and result interpretation**

Capability 6

At this point, introduce actual graph instances. The system must execute queries and interpret results without losing the original user intent.

The model should receive compact result evidence rather than raw unrestricted RDF dumps. Results should contain stable resource handles, labels, relevant types, selected properties, and the reason each resource matched. The application should retain the full resource identifiers and graph scope internally.

**Context supplied:** Original request, accepted query specification, result count, paginated result handles, relevant properties, and any execution warnings. Each result should include only the information needed for the model's next decision.

**Test progression:**

- One matching resource.
- Several matching resources.
- No results.
- Duplicate labels and similarly named resources.
- Pagination.
- Results containing missing optional properties.
- Results with inferred relationships.
- Results where the evidence conflicts with the user's presumed interpretation.

**System A:** Test selecting the correct result handles from small sets and extracting straightforward result summaries. Do not expect it to interpret large result sets.

**System B:** Test result interpretation, selection of resources for follow-up inspection, and recognition of incomplete evidence.

**Rebuild opportunities:** Result serialization, handle management, pagination, resource inspection, evidence filtering, and result explanations.

**Exit condition:** Correct result identification, no invented resources, and no confusion between resource labels and resource identity.

## **Phase 7 — Iterative discovery loop**

Capability 7

This phase tests the most important difference between the two systems: whether they can make a decision, receive evidence, and make a better subsequent decision.

The system now uses the interactive execution mode. It receives a request, chooses a permitted discovery action, receives the result, and decides what to do next.

The workflow controller retains state. The model receives a newly constructed context on each iteration. It does not receive a growing unfiltered transcript.

For each iteration, the context contains:

- Original request.
- Current accepted intent.
- Resolved ontology concepts.
- Relevant discovered paths.
- Outstanding semantic questions.
- Results of the most recent action.
- Available next actions.
- A termination option.
- A strict response schema.

**Test progression:**

1. One discovery action followed by completion.
2. Two sequential vocabulary searches.
3. Vocabulary search followed by property inspection.
4. Path discovery followed by path selection.
5. Failed search followed by revised search.
6. Discovery that exposes an ambiguity.
7. Discovery that establishes that no suitable ontology path exists.
8. A request requiring several successive discovery decisions.

**System A:** Rust should control the sequence and use Needle for the individual decisions. The model should not have to maintain the entire discovery process. Measure whether the overall system can complete the tasks with this constrained pattern.

**System B:** MiniCPM should decide which discovery action is useful next, evaluate its result, and terminate or ask for clarification when appropriate.

**Rebuild opportunities:** Workflow state, context reconstruction, action schemas, discovery tool descriptions, evidence compaction, iteration limits, repeated-call detection, and termination conditions.

**Exit condition:** The system completes the required discovery or stops for a legitimate reason. It must not loop indefinitely, repeat ineffective searches, forget accepted conditions, or make decisions from stale evidence.

## **Phase 8 — Mutation interpretation and proposal**

Safety-sensitive

Only introduce mutation testing after the read-only workflow is reliable.

This phase tests whether the model can understand the desired change without deciding how the graph should be modified at the RDF statement level.

The model identifies the target, requested semantic change, and intended value. Rust determines the actual triples and generates a preview.

**Context supplied:** Original request, accepted intent, resolved resource handles, relevant current assertions, available classification or property candidates, and mutation-specific instructions. Explicitly identify the difference between adding, replacing, and deleting assertions.

**Test progression:**

- Create a new concept.
- Add a classification.
- Replace a classification.
- Remove one classification without deleting the resource.
- Add a relationship.
- Remove a relationship.
- Update a literal property.
- Handle an ambiguous mutation request.
- Identify an already-satisfied request as a no-op.

**System A:** Test extraction of the desired operation and selection of the correct target and classification. Keep the mutation semantics constrained.

**System B:** Test more complex change interpretation, especially when the request contains multiple conditions or several resources.

**Rebuild opportunities:** Mutation intent schema, target resolution, operation classification, change preview, and clarification logic.

**Exit condition:** Every proposal represents exactly the user's requested semantic change. No model response directly commits an operation.

## **Phase 9 — Validation, confirmation, and execution**

Critical safety phase

This phase is primarily a test of Rust's deterministic mutation system, with the model participating only where interpretation or explanation is required.

The model must not be able to bypass validation, change authorization, change graph scope, or execute a mutation directly.

The application must validate the proposed changes against:

- Resource existence and identity.
- Graph and tenant scope.
- Authorization.
- Property type and domain/range compatibility.
- SHACL constraints.
- Application-specific business rules.
- Transaction requirements.
- User confirmation requirements.

The context supplied to the model should contain the mutation preview and validation report, but only if the model is being asked to explain or interpret the outcome. The model must not be allowed to convert a failed validation into an approved operation.

**Test progression:**

1. Valid mutation proposal.
2. Invalid property or value.
3. Missing target resource.
4. Unauthorized target.
5. Cross-graph target.
6. SHACL violation.
7. Conflicting existing assertion.
8. User rejects confirmation.
9. User approves confirmation.
10. Execution failure followed by safe recovery.

**System A and System B:** Both should behave identically from the perspective of safety. A more capable model must not receive more authority.

**Rebuild opportunities:** Validation rules, authorization boundaries, transaction handling, preview generation, confirmation handling, rollback, and post-commit verification.

**Exit condition:** No unauthorized or unconfirmed mutation can be executed, and successful changes are verified against the resulting graph.

# **3. Separate incremental loops for A and B**

The phases above are shared in purpose, but the actual development process should differ between the two models.

## **System A — Needle 3**

Constrained decision model

System A should be developed with a progressively expanding set of bounded decisions. It should not be forced into the same interactive agent pattern as MiniCPM.

| Development stage | What Needle is asked to do            | What Rust supplies and owns                   |
| ----------------- | ------------------------------------- | --------------------------------------------- |
| A1                | Extract intent                        | Request and extraction schema                 |
| A2                | Select vocabulary candidate           | Retrieved candidate list                      |
| A3                | Interpret ontology evidence           | Normalized inspection package                 |
| A4                | Select relationship path              | Resolver-generated path candidates            |
| A5                | Fill logical query fields             | Accepted intent and paths                     |
| A6                | Select or summarize results           | Bounded result handles                        |
| A7                | Make successive small decisions       | Entire discovery state and orchestration      |
| A8                | Interpret mutation request            | Current resources and change candidates       |
| A9                | Explain proposal or validation result | Validation and execution remain deterministic |

For each stage, Needle should be evaluated with increasingly difficult inputs, but the size and complexity of its individual decision should remain controlled.

The key experiment is to discover how far a single constrained call can be pushed before accuracy deteriorates. For example, compare Needle's selection accuracy with three, five, and ten candidates, or compare its ability to extract three conditions versus eight conditions. This identifies practical limits instead of relying on model-size assumptions.

For System A, an important metric is the **percentage of the workflow completed without requiring a more capable model or user clarification**, not the percentage of all requests completed in one call.

## **System B — MiniCPM5-1B**

Iterative reasoning model

System B should be developed with progressively more complex observe-decide-act cycles.

| Development stage | What MiniCPM is asked to do       | What Rust supplies and owns                  |
| ----------------- | --------------------------------- | -------------------------------------------- |
| B1                | Extract intent                    | Request and extraction schema                |
| B2                | Match vocabulary                  | Candidate search results                     |
| B3                | Interpret ontology evidence       | Class/property inspection                    |
| B4                | Select or investigate paths       | Candidate paths and discovery tools          |
| B5                | Construct logical query           | Accepted concepts and paths                  |
| B6                | Interpret query results           | Structured result evidence                   |
| B7                | Decide next discovery action      | Workflow state and tool execution            |
| B8                | Interpret complex mutation intent | Resource and assertion evidence              |
| B9                | Interpret validation results      | Mutation policy and deterministic validation |

The most important B-specific experiment is to determine whether MiniCPM makes better decisions when it is allowed to request additional evidence.

For each discovery task, compare three conditions:

1. **Fixed context:** All relevant evidence is supplied in one call.
2. **One-step discovery:** The model can request one additional inspection.
3. **Iterative discovery:** The model can make multiple successive discovery decisions.

This will reveal whether iterative discovery actually improves accuracy, or merely increases token consumption and latency.

MiniCPM should also be tested with reasoning enabled and disabled during the isolated phases. Keep all other settings fixed. This helps determine whether reasoning is useful for vocabulary selection, path resolution, and query construction, rather than assuming that it helps every task.

# **4. Failure diagnosis and rebuild procedure**

A failed test should never be classified simply as a model failure. Each failure must be assigned to a specific component.

| Failure category      | Example                                              | Component to investigate        |
| --------------------- | ---------------------------------------------------- | ------------------------------- |
| Extraction            | Drops “excluding active opportunities.”              | Intent prompt/schema            |
| Retrieval             | Correct concept absent from candidates.              | Vocabulary search               |
| Selection             | Chooses the wrong supplied candidate.                | Model context/decision          |
| Inspection            | Misinterprets property direction.                    | Evidence serializer or model    |
| Path resolution       | Correct property exists but resolver misses it.      | Path resolver                   |
| Path selection        | Chooses headquarters rather than operating location. | Path descriptions or model      |
| Query specification   | Omits a negative condition.                          | Logical query representation    |
| Compilation           | Valid specification produces incorrect SPARQL.       | Query compiler                  |
| Execution             | Query is correct but result set is wrong.            | Dataset/query engine            |
| Result interpretation | Selects the wrong resource handle.                   | Result context                  |
| Workflow              | Repeats a failed search indefinitely.                | Controller/termination logic    |
| Mutation              | Removes a resource instead of one assertion.         | Mutation interpretation         |
| Safety                | Commits without confirmation.                        | Authorization/transaction layer |

The diagnostic process should proceed in a fixed order.

**First, replay the failing decision in isolation.** Use the exact recorded context and live model. If the model still produces the same incorrect decision, the problem is likely in the prompt, evidence presentation, or model capability. If it now succeeds, investigate nondeterminism or earlier workflow context construction.

**Second, replace the model output with the expected answer.** Run the rest of the system with that answer injected. If the workflow still fails, the model was not the cause. The problem lies in validation, path resolution, query construction, compilation, or execution.

**Third, replace retrieved evidence with known-good evidence.** If the model now succeeds, investigate retrieval and evidence serialization. If it still fails, investigate the decision prompt or the model's capability.

**Fourth, compare the actual context with the intended context.** Check whether the original phrase, previously accepted decisions, candidate descriptions, and ambiguity distinctions were preserved. Many apparent reasoning failures are actually context-construction failures.

**Fifth, modify only the component implicated by the experiment.** Do not simultaneously change the prompt, retrieval algorithm, output schema, and decoding settings. If several changes are made together, it becomes impossible to establish which one improved the outcome.

After a modification, rerun the failing test, all tests in the current phase, and a regression set from earlier phases.

# **5. Experiment management and regression control**

Each test should be stored as a versioned experiment, not just a prompt in a text file. A test record should contain:

```
{
  "test_id": "A4-003",
  "system": "A",
  "phase": 4,
  "difficulty": 3,
  "model": "Needle-3",
  "fixture": "ontology_fixture_03",
  "input": "Find contacts working for accounts in Ohio.",
  "expected": {
    "decision": "select_path",
    "path_id": "expected_path_02"
  },
  "context_version": "4",
  "workflow_version": "7"
}
```

A separate run record should store the actual model output, retrieved evidence, selected path, validation results, and final evaluation. The test definition must not be modified to make a failing result pass. If the expected behavior changes because the ontology or application semantics change, create a new test version and preserve the previous result.

Use three regression sets:

- **Phase regression:** All tests belonging to the current phase.
- **Foundation regression:** Representative tests from all completed earlier phases.
- **Safety regression:** Every test involving scope, negative conditions, destructive operations, authorization, and confirmation.

The safety regression should run after every relevant code change, even if the change appears to affect only prompts or retrieval.

For live-model testing, repeat important tests several times. Small models can be sensitive to prompt ordering, decoding settings, and candidate-list composition. Report both average accuracy and worst-case behavior. For destructive operations, even one unsafe result is a critical failure.

## **Metrics to retain**

| Metric                        | Why it matters                                                  |
| ----------------------------- | --------------------------------------------------------------- |
| Exact semantic accuracy       | Whether the actual decision was correct.                        |
| Evidence-grounded accuracy    | Whether decisions were supported by supplied ontology evidence. |
| Clarification precision       | Whether the system asks questions only when necessary.          |
| Unsupported-decision rate     | How often the model invents or assumes information.             |
| Calls per successful workflow | Measures iterative overhead.                                    |
| Repeated-action rate          | Detects discovery loops that make no progress.                  |
| Context size per call         | Reveals unnecessary prompt growth.                              |
| Latency per phase             | Identifies expensive stages.                                    |
| Regression pass rate          | Detects improvements that break earlier capabilities.           |
| Critical safety failures      | Measures unacceptable mutation or scope behavior.               |

# **6. The development gates**

Each phase should have a formal gate before the next capability is introduced. These gates should be based on semantic correctness, not on a subjective impression that the model seems to work.

**Recommended phase acceptance gates**

**0**

**Model interface**

Reliable structured responses and parsing.

**1**

**Intent extraction**

No material dropped or invented conditions.

**2**

**Vocabulary matching**

Correct candidate selection or explicit ambiguity.

**3**

**Ontology interpretation**

Correct use of domain, range, hierarchy, and evidence.

**4**

**Path resolution**

Valid and semantically correct relationship paths.

**5**

**Query construction**

All conditions preserved and correct query compilation.

**6**

**Instance results**

Exact expected result set and resource identity.

**7**

**Discovery loop**

No state loss, redundant loops, or unsupported decisions.

**8**

**Mutation proposal**

Exact intended change, no unintended deletion.

**9**

**Execution safety**

No unauthorized or unconfirmed commits.

A phase should not advance just because the average score is acceptable. It should advance when the relevant behavior is repeatable and the remaining failures are understood. If a phase fails, the development loop returns to the relevant component rather than proceeding and accumulating more interacting failure modes.

# **7. How to determine what actually belongs in the model**

The most valuable result of this process will be a capability boundary for each model.

For every task, run three versions:

| Variant             | Execution                                                         |
| ------------------- | ----------------------------------------------------------------- |
| Rust-only           | Deterministic code handles the task without a model.              |
| Model-only decision | Model receives fixed evidence and makes the decision.             |
| Integrated          | Rust retrieves evidence and the model makes the bounded decision. |

This distinction matters because a model can appear to perform well when Rust has already done most of the semantic work. Conversely, a model can appear weak when the context builder fails to present information in a form it can use.

For example, relationship path selection should be tested as follows:

- Rust-only: Does the resolver find the correct paths and distinguish them using deterministic metadata?
- Model-only: Given a fixed set of candidate paths and their definitions, can the model select the correct one?
- Integrated: Can the system retrieve the paths, present them correctly, and obtain the right decision?

The integrated result is what matters for the product, but the other two experiments explain why it works or fails.

I would also add a fourth variant for important capabilities: **model-assisted retrieval**, where the model proposes a narrower search phrase or inspection target, but Rust still executes and validates it. This is especially useful for MiniCPM's iterative discovery behavior.

# **8. Final operating model**

The complete development process is two independent, incremental experiments:

## **System A — Needle 3**

Progressively expand constrained decision complexity.

1. Establish reliable live inference and structured extraction.
2. Measure intent extraction limits.
3. Measure candidate-selection limits.
4. Test ontology evidence interpretation.
5. Test bounded path selection.
6. Test deterministic query construction with Needle-supplied fields.
7. Test small result-selection decisions.
8. Test Rust-orchestrated multi-step discovery.
9. Test mutation interpretation and safety boundaries.

**Primary question:** How much semantic work can Needle reliably perform when each decision is small and well specified?

## **System B — MiniCPM5-1B**

Progressively expand evidence-driven reasoning and interaction.

1. Establish reliable live inference in both reasoning modes.
2. Measure intent extraction limits.
3. Measure vocabulary interpretation.
4. Test ontology evidence interpretation.
5. Test multi-hop path selection.
6. Test logical query specification.
7. Test result interpretation.
8. Test iterative discovery with successive evidence.
9. Test complex mutation interpretation and safety boundaries.

**Primary question:** How much additional correctness does iterative model-driven discovery provide over fixed-context decisions?

**The final objective is not to make either model perform every task.** It is to discover the smallest, most reliable model-assisted architecture that completes the required RDF operations. If Rust can resolve a relationship deterministically, it should. If Needle can reliably choose between three candidates, there is no reason to involve MiniCPM. If MiniCPM can resolve an ambiguity only after inspecting additional ontology evidence, the workflow should support that capability without granting it control over the graph.

The phase-by-phase experiments will reveal exactly where each model is useful, where deterministic Rust code is better, and where the system must ask the user instead of guessing.
