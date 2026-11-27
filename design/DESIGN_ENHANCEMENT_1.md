# **Natural-language interface to an OWL/RDF knowledge system**

## **1. System definition and governing architecture**

The system provides a natural-language interface to an authoritative OWL/RDF knowledge store. It accepts requests such as finding resources, understanding relationships, exploring an ontology, querying instances, creating concepts, changing classifications, and modifying relationships. It must translate those requests into correct graph operations without requiring the language model to understand RDF syntax, invent ontology identifiers, construct arbitrary SPARQL, or maintain the state of a multi-step operation.

The architecture has two independently testable configurations. **System A uses Needle 3** as a constrained intent-extraction and candidate-selection model. **System B uses MiniCPM5-1B** as an interactive interpretation and ontology-discovery model. Both systems use the same Rust-controlled workflow, ontology services, query services, graph scope, validation rules, and mutation mechanisms. They differ in how they interpret requests and decide what information to retrieve next. They must not be combined into a single pipeline for the purposes of evaluation: each system receives the same user request and is evaluated independently.

The central architectural rule is that the Rust application owns the operational process. The model supplies bounded semantic decisions; Rust retrieves authoritative evidence, resolves identifiers, constructs logical queries, enforces scope, executes operations, and retains state. A model response is never itself an executable graph operation. Every model decision must be interpreted against the current workflow state and the evidence supplied to it. This prevents a small model from silently substituting plausible but incorrect ontology concepts, assuming relationships that do not exist, or losing earlier constraints during a long interaction.

The application maintains a persistent workflow record for each request. It contains the original user request, the graph scope, the current workflow stage, the current interpretation, resolved ontology references, candidate relationships, selected relationship paths, query specification, result references, pending clarification, and any proposed mutation. The record is authoritative; the prompt is only a temporary, deliberately constructed view of that record. The model must never be expected to remember the entire workflow from previous turns. Every call receives enough current state and evidence to make its particular decision, but not the complete history of unrelated discoveries.

The workflow is not a model-generated plan or a model-generated DAG. It is a Rust-controlled sequence of states and transitions. At each state, Rust determines which decisions are permitted, which services are available, what evidence is relevant, and what output format is required. The model cannot skip a required validation step, change the graph scope, commit a mutation, or reinterpret a result as authorization. If the model produces malformed output, an unsupported decision, or a decision inconsistent with the supplied evidence, Rust rejects it and either retries with a corrected context, requests a clarification, or returns a controlled failure.

## **2. The two model configurations**

**System A — Needle 3.** Needle 3 is treated as a small specialized decision model, not as a general-purpose agent. Its primary task is to convert a short natural-language request into a tightly constrained semantic representation. It should identify the requested action, the entities or concepts mentioned, the relationship phrases, filters, requested output, and whether the request appears to involve a mutation. It can also select among a small number of already retrieved ontology candidates when the descriptions are sufficiently distinct. It must not be asked to independently discover an unfamiliar ontology, infer long relationship paths, generate SPARQL, or coordinate a complex workflow.

For System A, the Rust application performs most of the decomposition and discovery. Needle receives a narrow question about the current state, such as identifying which of four candidate properties best matches the phrase “works for.” It receives the user's relevant phrase, the small candidate set, and the expected output schema. It returns a candidate identifier and, optionally, a confidence or ambiguity indication. The application then checks that the selected identifier was actually among the candidates. If none is appropriate, or if the evidence does not distinguish the candidates, the application does not force a selection. It asks the user or reports that the available ontology evidence is insufficient.

Needle's context must be particularly small and explicit. It should not receive an entire ontology, a large list of RDF triples, or a transcript of the complete workflow. It should receive only the current decision, the exact source phrase, the relevant entity types if known, and a short list of candidate concepts or paths. Candidate descriptions must be normalized by Rust into a consistent format, with each candidate carrying a stable opaque candidate ID, a label, a definition, relevant domain and range information, and any distinguishing notes. The model selects the opaque candidate ID; Rust maps that ID to the actual RDF IRI. This avoids asking Needle to reproduce long or unfamiliar identifiers and makes hallucinated identifiers detectable.

For simple requests, Needle can return the complete intent extraction in one call. For more involved requests, it should be called repeatedly for individual bounded decisions as Rust discovers new evidence. Rust must not rely on Needle to retain earlier answers. Each subsequent call receives the relevant previously accepted facts as a compact state summary. Needle's automatic tool-execution loop is not the governing workflow for this design: Rust needs to inspect each decision, validate it, decide what evidence to retrieve, and control the next stage.

**System B — MiniCPM5-1B.** MiniCPM5-1B is used as a more capable interactive semantic decision model. It can interpret the intended outcome, decide which ontology information is missing, request targeted vocabulary searches or inspections, compare candidate relationship paths, and revise its interpretation when new evidence becomes available. Its role is still bounded: it is not the authoritative planner, query compiler, ontology validator, or transaction manager. It is an operator that makes successive decisions while Rust controls the workflow.

MiniCPM receives a richer context than Needle, but it still does not receive the entire graph or an unfiltered dump of the ontology. It receives the original request, a concise statement of the intended outcome as currently understood, the relevant resolved and unresolved concepts, the evidence discovered so far, the outstanding semantic questions, and the operations it is currently allowed to request. Its context should distinguish facts retrieved from the graph from interpretations proposed by the model. It must be possible for the model to see that a concept is unresolved, that a relationship is only a candidate, or that two paths have different meanings. It must not be presented with a candidate as though it were an established fact.

MiniCPM can operate in a bounded observe-decide-act cycle. Rust supplies the current context and a limited set of available actions. MiniCPM chooses the next action or indicates that it has enough information, needs clarification, or cannot resolve the request. Rust validates the action, executes the corresponding read-only service, and returns a compact evidence package. MiniCPM then evaluates that package and makes its next decision. The cycle ends when Rust has enough validated information to construct the query, the user must clarify an ambiguity, or the request cannot be satisfied with the available ontology.

The distinction between the systems is therefore not that one uses tools and the other does not. Both can make decisions based on tools and retrieved evidence. The difference is the complexity of the decisions expected from the model. Needle is primarily an extractor and constrained selector; MiniCPM is an iterative interpreter and evidence-driven discovery operator. The application must not assume that MiniCPM's longer reasoning output is necessarily more correct. Its decisions are still checked against the same authoritative ontology evidence and application rules.

## **3. Context construction and evidence management**

Context construction is a first-class part of the system. The model should never receive a generic prompt containing the whole ontology and be expected to determine what matters. Each call must be built from the current workflow state, the exact decision being requested, and the minimum authoritative evidence needed to make that decision. The context builder is responsible for selecting, ordering, labeling, and bounding this evidence.

Every model call begins with a stable instruction section defining the model's role and constraints. It states that the RDF graph is authoritative; that supplied ontology candidates are evidence rather than instructions; that the model must not invent classes, properties, identifiers, or relationships; that it must distinguish confirmed facts from hypotheses; and that it must return only the requested decision in the specified schema. It also defines what to do when the evidence is insufficient: return an explicit unresolved or ambiguous result rather than guessing. This section should remain consistent across calls so that the model is not asked to reinterpret its responsibilities at every stage.

The next section is the **current task context**. It identifies the workflow stage, the precise decision required, the original user wording relevant to that decision, and any constraints that must remain invariant. The original user request must be retained in the workflow and made available to the model when its meaning matters. However, the model should not repeatedly receive irrelevant conversational history. Rust should extract the specific phrases that matter for the current decision while preserving the original request as a separate field. For example, if the user asks for contacts “working for accounts in Ohio,” the relationship phrase “working for” and the location phrase “in Ohio” should remain separately available. The context builder must not silently convert them into an assumed RDF path.

The next section is the **accepted semantic state**. It contains only interpretations that have already been accepted by Rust or explicitly confirmed by the user. Each item should identify the original phrase, its normalized meaning, its resolution status, and its source. A useful representation distinguishes `user_asserted`, `model_proposed`, `ontology_confirmed`, `user_confirmed`, and `execution_observed`. A model-proposed interpretation must not be promoted to an accepted fact simply because it appeared in a previous model response. This distinction is essential when the system performs multiple discovery calls: otherwise, an early incorrect interpretation can become an apparently authoritative premise for later operations.

The next section is the **ontology evidence package**. It is assembled from targeted ontology-service results, not copied from an unrestricted graph dump. For each relevant class, property, or concept, Rust supplies the actual identifier internally as an opaque candidate ID, the preferred label, alternative labels, definition or comment, type, relevant superclass or broader-concept relationships, and applicable domain and range information. For a property, it should also include inverse-property information, equivalent-property information, subproperty information, and relevant characteristics when available. Missing information must be represented as missing; the context builder must not manufacture a domain, range, definition, or inverse relationship. The package should identify the graph or named-graph scope from which each item was retrieved and whether the information is asserted directly or inferred by a configured reasoning process.

The next section is the **candidate set**. Candidate sets are constructed by Rust using lexical search, label and synonym matching, definitions, ontology hierarchy, domain and range compatibility, and other configured retrieval methods. The model is not asked to search an entire vocabulary by itself. Each candidate receives a stable candidate ID for the lifetime of the workflow, so that the model can select it without reproducing an IRI. The candidate list should contain enough information to distinguish plausible alternatives, but it should not be padded with irrelevant concepts. If retrieval produces no plausible candidates, the application should report that no suitable ontology match was found rather than asking the model to choose the least-wrong option.

The next section is the **workflow constraints and available actions**. It tells the model what it may do at the current stage. For example, during discovery it may request another vocabulary search, inspect a candidate class or property, search instances, inspect a resource, or submit a logical query specification. During mutation interpretation it may propose a change, but it cannot commit one. Actions not relevant to the current stage should not be exposed. The model must not be allowed to change graph scope, tenant identity, authorization context, or execution policy. These values are supplied and enforced by Rust, not inferred from natural language or accepted from model output.

The final section defines the **required response schema** and the meaning of each permitted value. The response must be machine-parseable and validated against a strict schema. It should include a decision, the candidate IDs or extracted phrases involved, a reason code for unresolved cases, and any required arguments. Free-form explanation may be accepted in a separate optional field, but Rust must never use that explanation as an executable instruction. A valid response must refer only to supplied candidate IDs and permitted action names. Invalid JSON, missing fields, extra unsupported fields, unknown IDs, or contradictory decisions must be rejected.

For both systems, context must be rebuilt after every tool result. Rust should not merely append raw tool output to the previous prompt. It should summarize the accepted state, include the new evidence, remove irrelevant evidence, and preserve references to the original request. This avoids context growth and reduces the risk that the model treats old, superseded candidates as still valid. Evidence packages should also be bounded by explicit limits: maximum candidates, maximum paths, maximum instances, and maximum definition length. If the evidence exceeds those limits, Rust should rank and filter it, or ask the model to make a narrower discovery decision. It should not truncate arbitrary text in a way that removes the distinguishing information from the end of a definition.

## **4. Intent interpretation**

The intent representation captures what the user wants in business terms, before it is translated into ontology identifiers or graph patterns. It should contain an action, one or more target concepts, conditions, requested output, and any mutation intent. Entity and relationship mentions should initially be stored as phrases rather than RDF IRIs. A condition should preserve the direction of the relationship and the original wording, because phrases that appear similar can have different meanings in the ontology.

For example, the request “Find contacts working for accounts in Ohio” should initially produce an intent resembling:

```
{
  "action": "find",
  "target": [
    {"phrase": "contacts"},
    {"phrase": "accounts"}
  ],
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
    }
  ],
  "requested_output": ["contacts"],
  "mutation": false
}
```

The representation deliberately does not contain an assumed property such as `worksFor`, `locatedIn`, or `headquartersIn`. Those identifiers are resolved later. The direction of each relationship is preserved, and the phrase “in Ohio” remains unresolved until the ontology and relevant instance data are inspected.

For Needle, the intent context should contain the original request, a small set of action choices, explicit extraction instructions, and the required JSON schema. It should not require Needle to resolve concepts against the entire ontology. For MiniCPM, the initial context can include a brief description of the domain and the available discovery actions, allowing it to identify which concepts and relationships need investigation. In both cases, the application validates the extracted intent and stores it separately from ontology resolution.

The application must preserve the distinction between a missing concept and a missing relationship. If the user says “customers who belong to a territory,” the system may recognize “customers,” “territory,” and “belong to” as phrases, but it must not assume that the ontology contains a direct property connecting them. It may later discover that the relationship is represented through an account, an assignment, or a membership resource. This is why intent extraction must precede relationship-path resolution.

## **5. Vocabulary and ontology discovery**

Vocabulary discovery resolves user phrases to ontology concepts. Rust performs the retrieval and supplies candidate evidence. Retrieval should use preferred labels, alternative labels, SKOS labels and definitions, ontology comments, configured synonyms, and relevant class/property metadata. Where appropriate, it should also use superclass and subproperty relationships to identify concepts that are semantically close but not exact matches. Search results should be ranked using a combination of lexical relevance and ontology compatibility. The model's task is to interpret the candidate evidence, not to replace retrieval.

The context for a vocabulary-selection decision must contain the exact user phrase, the role of that phrase in the intent, any already resolved neighboring concepts, and the candidate concepts. The role matters: “account” as the object of “works for” is not necessarily interchangeable with “account” as the target of “find accounts.” Candidate descriptions must include enough information to distinguish a class from a property, a concrete concept from a broader category, and a synonym from a different concept with a similar label. If the model selects a class, Rust must verify that the candidate is actually an OWL class or an accepted equivalent representation. If it selects a property, Rust must verify that it is an object or datatype property appropriate to the requested operation.

Needle should receive a small candidate list, preferably a few clearly differentiated choices, and return the selected candidate ID or `unresolved`. MiniCPM may be asked to inspect the candidates and decide whether further vocabulary discovery is needed. For example, it may recognize that the user phrase “technology conference” could correspond to an event class, an event subtype, or a broader SKOS concept. It can request further inspection, but Rust must retrieve and present the actual evidence before accepting its interpretation.

Vocabulary selection must support ambiguity explicitly. If two concepts are plausible and the ontology does not provide enough evidence to distinguish them, the system must not silently choose one based only on label similarity. It should either ask the user a focused clarification question or preserve both interpretations until another query or inspection can disambiguate them. The accepted vocabulary mapping should retain the original phrase, selected concept ID, selection basis, and any unresolved alternatives.

## **6. Relationship path resolution**

Relationship resolution is the most important semantic component of the system. Natural-language relationships often do not correspond to a single RDF property. A user may describe a relationship that is represented by several properties, an intermediate resource, an inverse property, or a hierarchy of concepts. The model must not invent the path, and Rust must not simply select the shortest path between two classes.

Rust's path resolver searches the ontology for candidate paths between the already resolved start and end concepts. It uses asserted property domain and range, inverse-property declarations, equivalent properties, subproperty relationships, class hierarchy, and configured reasoning results. It may also use property definitions, labels, comments, and curated business mappings. It must respect the active graph scope and any configured inference policy. The resolver should return a bounded set of candidate paths, each with its ordered properties, intermediate classes, direction, relevant ontology evidence, and a human-readable explanation of what the path means.

For example, a contact may be connected to an account through `worksFor`, while an account may be connected to a region through `headquartersIn` or `operatesIn`. Both may ultimately connect to Ohio. A request about accounts “in Ohio” could mean accounts headquartered in Ohio or accounts that operate in Ohio. These paths are not interchangeable. The resolver should return both candidates with their definitions and path structure, not collapse them into one generic “located in” relationship.

The model's context for path selection must include the original relationship phrase, the resolved start and end concepts, any surrounding conditions that affect meaning, and the candidate paths. It must include the semantic distinction between the paths, not merely their property names. The model must be told to select a path only if the supplied evidence supports the user's intended meaning. It may select one candidate, request further inspection, or return an ambiguity requiring user clarification.

Needle should only be used for path selection when Rust has already reduced the possibilities to a small and clearly distinguishable candidate set. If there are several multi-hop paths with subtle differences, Needle should not be expected to reason through them. MiniCPM can inspect a larger but still bounded set of paths, compare their definitions and intermediate classes, and request additional evidence. Neither system can authorize a path that the resolver did not return. The selected path is always stored by its candidate ID and checked by Rust against the original resolver output.

The path resolver must also handle the absence of a path. If no valid path exists, the system should not create a relationship merely because the user expects one. It should report that the ontology does not currently provide a supported path, or ask whether the user intended a different concept or relationship. A path that exists only through inferred axioms must be labeled as inferred and handled according to the application's reasoning policy. The model must not confuse an inferred relationship with a directly asserted triple.

## **7. Logical query construction and execution**

Once intent, vocabulary mappings, and relationship paths are resolved, Rust constructs a logical query specification. This specification represents the requested graph pattern without exposing arbitrary SPARQL to the model. It should support entities, relationships, equality, text matching, numeric comparisons, existence and non-existence, and Boolean combinations. More complex query operators can be added later, but the initial representation should remain small enough for a 1B model to use reliably.

A logical query might express that the returned resource is a Contact, that it has a selected relationship path to an Account, and that the Account satisfies a location condition. The query specification should refer to resolved concept and path IDs, not user-facing phrases or unvalidated IRIs. It should also preserve the original phrase for auditing and explanation. Rust validates that each referenced concept and path exists in the current workflow's evidence and that the query is compatible with the graph scope.

The context supplied to a model when it is asked to produce or refine a logical query must include the accepted intent, resolved concepts, selected relationship paths, relevant property semantics, requested output, and the allowed query operators. It should also include examples of the logical query format. It must not include raw SPARQL syntax or invite the model to write SPARQL. The model should express the desired logical structure; Rust resolves the references and compiles the query.

For System A, the application should construct most of the query deterministically from the extracted intent and selected candidate IDs. Needle may fill a missing bounded field or choose among alternative query interpretations, but it should not be responsible for assembling a complex query from scratch. For System B, MiniCPM may propose the logical query specification after discovery, then inspect a compact validation report if Rust identifies missing or incompatible references. The query compiler remains deterministic and independent of the model.

Rust compiles the logical query into SPARQL, injecting the authorized graph scope, tenant restrictions, and any other mandatory filters. It must never accept graph names, dataset scope, or authorization constraints from the model. The compiler should use parameterized or safely constructed RDF terms rather than concatenating untrusted strings into SPARQL. The query is then parsed and validated before execution. Results should be returned to the model as compact structured evidence: result count, selected fields, stable result handles, relevant labels, and any warnings. The model should not receive an unlimited result dump. If a query returns too many results, Rust should paginate or ask for a narrower query.

Negative conditions require special care. A request such as “exclude accounts with active opportunities” should compile to a suitable non-existence condition within the selected graph scope and with a precisely defined meaning of “active.” The system must determine whether “active” is a known status value, a date-derived condition, or an ontology-defined class. It must not assume that the absence of an asserted opportunity means that no opportunity exists in reality. Under RDF's open-world semantics, the system can establish only that no matching assertion was found within the queried dataset and inference regime. The result and any user-facing explanation must state that limitation where it affects the interpretation.

## **8. Instance search and resource inspection**

Instance search retrieves graph resources that match resolved classes, labels, properties, or query conditions. Rust performs the search and returns stable resource handles. A handle maps to an actual IRI and graph context inside the application; the model does not need to reproduce the IRI. Resource inspection retrieves selected properties, types, labels, descriptions, and relationships for a specific handle. It should be targeted to the current decision rather than returning every triple associated with the resource.

The context for instance inspection must identify why the resource was retrieved, which properties are relevant to the request, and what question the model is expected to answer. For example, if the system is deciding whether a resource is a technology conference, it should receive the resource's type assertions, event classification, relevant labels, and the properties that distinguish a conference from other events. It should not receive unrelated contact details or every property in the resource. Sensitive or access-controlled data must be filtered by Rust before it enters the prompt.

The model may interpret the returned evidence or select a resource from a bounded candidate set. Rust verifies that any selected handle belongs to the current result set and is accessible under the active scope. A model cannot use an arbitrary IRI mentioned in its output to inspect or modify a resource. If the user asks for a resource that is not found, the system should distinguish between no match, insufficient search evidence, and a resource that exists but is inaccessible under the current scope.

## **9. Mutation interpretation and safety**

Mutations are handled separately from read queries. The model identifies what the user wants to change, but Rust determines the exact RDF operation. A mutation proposal should specify the target resource or concept handle, the intended semantic change, the relevant property or classification, the new value or relationship, and the scope in which the change is requested. It should also preserve the user's original wording and identify any assumptions that have not been confirmed.

For a creation request, the model may identify that the user wants a new classification concept with a particular label and definition. Rust determines the appropriate SKOS or OWL representation, checks whether an equivalent concept already exists, determines the required properties, and generates or assigns the IRI according to application policy. The model must not invent the IRI or decide the underlying RDF type.

For an update request, the model identifies the resource and desired semantic change. Rust determines whether the operation means adding a triple, replacing a value, changing a classification, or updating a relationship. The system must distinguish “add this classification” from “replace the existing classification.” If the wording does not establish the intended operation, it asks the user rather than choosing a destructive interpretation.

For a deletion request, the system must identify exactly which assertion or resource is to be removed. A request such as “remove the prospect classification from these accounts” must not be interpreted as deleting the accounts themselves. The model identifies the target and requested semantic change; Rust resolves the precise statements and constructs a mutation proposal. Resource deletion, relationship removal, and classification removal are distinct operations and must remain distinct throughout the workflow.

The mutation context supplied to the model must include the original request, accepted intent, resolved resource handles, relevant current triples, candidate target properties, the requested new values, and the exact ambiguity or decision being considered. It must also clearly state that the model is proposing a change, not authorizing or committing it. For Needle, the mutation decision should be limited to extracting the intended operation and selecting among a small set of already resolved targets. MiniCPM may reason through a more complex proposed change, but it cannot bypass the same Rust validation.

Before any mutation is committed, Rust generates a preview containing the exact resources and triples to be added, changed, or removed. It checks authorization, graph scope, tenant boundaries, datatype and object-property compatibility, applicable OWL constraints, SHACL shapes, and application-specific policies. The user must confirm changes when required by the operation's risk or policy. The operation is executed transactionally where supported, and the application verifies the resulting graph state. The model is not a security boundary and must never be trusted to determine whether a mutation is safe.

## **10. End-to-end context behavior**

Consider the request: “Find all contacts working for accounts in Ohio that attended a technology conference during the past year, excluding accounts with active opportunities, and mark the remaining accounts as prospects.”

Both systems begin with the same original request and graph scope. The first semantic representation identifies the target contacts, the relationship phrase “working for,” the account location phrase “in Ohio,” the event-attendance condition, the time constraint, the exclusion of accounts with active opportunities, and the requested account classification change. The intent representation must preserve that the request contains both a read operation and a subsequent mutation. It must not immediately turn “in Ohio” into a particular property or “attended a technology conference” into a particular event relationship.

In System A, Needle extracts the phrases and action structure. Rust then searches the vocabulary for Contact, Account, Event, Conference, and Prospect-related concepts, and searches the relevant properties for employment, attendance, location, and opportunity status. Needle is called again only when Rust needs a bounded selection among candidates. Rust resolves the relationship paths, constructs the logical query, and executes it. If a critical distinction cannot be made using the available evidence, System A asks the user rather than pretending that Needle has discovered the correct interpretation.

In System B, MiniCPM receives the initial intent and can request targeted discovery. Rust returns candidate vocabulary and relationship evidence in successive compact context packages. MiniCPM can decide that it needs to inspect the event hierarchy, determine whether attendance is represented by a registration or participation resource, or distinguish account headquarters from operating location. It can also identify that “past year” requires a precise time interpretation and that “active opportunity” needs a defined status or date rule. Rust supplies the relevant evidence and asks the user about unresolved business semantics. Once the conditions are resolved, MiniCPM returns a logical query specification, which Rust validates, compiles, and executes.

The query results are returned as stable account and contact handles with compact supporting evidence. The system then prepares the classification mutation for the matching accounts. It must not assume that the user intended an immediate commit merely because the request used the word “mark.” The application follows its mutation policy, presents the exact proposed changes, obtains any required confirmation, validates the changes, commits them, and verifies the graph. At every stage, the context is built from accepted intent, the current unresolved question, and the relevant evidence—not from an unstructured transcript of everything that happened.

# **11. Difficulty ladder and evaluation design**

The test ladder has five difficulty levels. Each level measures a progressively more demanding capability, from extracting a straightforward request to completing a multi-stage operation involving ontology discovery, ambiguity, graph semantics, and mutation. The same test cases, initial graph fixtures, ontology definitions, graph scope, and expected outcomes must be used for System A and System B. Each system is run independently at every level. A system must not receive answers, interpretations, or intermediate state produced by the other system.

The levels measure semantic correctness, not merely whether the model returns valid JSON or makes successful tool calls. A system that produces a syntactically valid query with the wrong relationship has failed. A system that asks a necessary clarification question has not failed merely because it did not complete the operation automatically. Conversely, a system that guesses when the evidence is insufficient must be marked incorrect even if its guess happens to match the expected answer.

For each test, the evaluator should record the final semantic outcome, selected ontology identifiers, selected relationship paths, logical query, result set, proposed mutations, clarification behavior, invalid decisions, and number of model calls. The evaluator should also record whether the model confused a proposed interpretation with a graph-confirmed fact. The application should retain the exact context supplied at every model call, so that incorrect outcomes can be traced to retrieval, context construction, model interpretation, validation, or execution.

## **Level 1 — Direct interpretation and known concepts**



Difficulty 1

**Purpose: test basic intent extraction and selection when the ontology is already known and unambiguous.**



At this level, the graph contains familiar classes and properties with clear labels, definitions, domain and range declarations, and no competing interpretations. The model is not required to discover a multi-hop relationship or infer an unfamiliar ontology structure. The tests establish whether it can reliably identify the user's action, target, filters, and requested output, and whether it can select the correct concept from a small candidate set.

The context supplied to either system consists of the original request, the current graph scope, the relevant class and property candidates, and the required response schema. For a vocabulary-selection test, the candidates should include the correct concept and a few clearly different alternatives. Each candidate should have a stable ID, preferred label, definition, and type. For an intent-extraction test, the model receives no unnecessary ontology dump; it receives the request and the extraction schema. The accepted answer is compared against the expected intent and candidate IDs.

| Test | User request                          | Expected behavior                                                                       |
| ---- | ------------------------------------- | --------------------------------------------------------------------------------------- |
| 1.1  | “Find all contacts.”                  | Identify `find`, target `Contact`, and no additional conditions.                        |
| 1.2  | “Find accounts named Acme.”           | Identify `Account` and a name-equality or name-match condition.                         |
| 1.3  | “Show the contacts working for Acme.” | Identify `Contact`, the phrase “working for,” and Acme as the relationship target.      |
| 1.4  | “Find all prospects.”                 | Select the appropriate prospect classification or concept from the supplied candidates. |
| 1.5  | “Describe account Acme.”              | Identify a resource-description request, not a graph mutation.                          |

**System A expectations:** Needle should perform well at this level. The extraction tasks should be handled in one constrained call where possible. Candidate selection should use only the supplied candidate IDs. A response containing an invented identifier, an unsupported property, or a different action is incorrect.

**System B expectations:** MiniCPM should also complete these tasks correctly. It should not need to invoke discovery tools when the supplied context already resolves the request. Unnecessary discovery calls should be recorded as inefficiency, although they are not necessarily semantic failures.

**Level 1 pass condition:** All five tasks must produce the correct intent and concept selections, with no invented ontology identifiers and no confusion between read and mutation operations. An explicit unresolved answer is acceptable only if the test fixture genuinely contains ambiguity; these fixtures should not.

## **Level 2 — Vocabulary matching and basic relationship resolution**



Difficulty 2

**Purpose: test whether the system can match ordinary language to ontology concepts and resolve simple relationships using supplied evidence.**



At this level, the ontology remains small and well-defined, but the user's wording no longer exactly matches ontology labels. The graph should include synonyms, alternative labels, definitions, inverse properties, and a small number of competing candidates. Relationships are direct rather than multi-hop. The model must use the evidence to select the appropriate property and preserve its direction.

The context must contain the original relationship phrase, resolved start and end classes where available, and the candidate properties. Each property candidate should include its label, definition, domain, range, inverse, and any relevant equivalent-property or subproperty declarations. The candidates should be chosen so that the correct answer is supported by the ontology, not merely by the similarity of its label to the user's wording.

| Test | User request                               | Expected behavior                                                                                                                                |
| ---- | ------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------ |
| 2.1  | “Find people employed by Acme.”            | Match “people” to the appropriate person/contact class and “employed by” to the correct direct relationship.                                     |
| 2.2  | “Find accounts headquartered in Ohio.”     | Select the headquarters-location relationship, not a general operating-region relationship.                                                      |
| 2.3  | “Show contacts associated with Acme.”      | Recognize that “associated with” may be broader than employment and select only if the supplied ontology evidence supports the intended meaning. |
| 2.4  | “Find all accounts that own a subsidiary.” | Resolve the correct direction of the ownership relationship.                                                                                     |
| 2.5  | “Show all employees of Acme.”              | Use an inverse property if that is how the ontology expresses the relationship, while returning the requested employee resources.                |

**System A expectations:** Needle should extract the phrase and select among the supplied properties. It should succeed when the candidate distinctions are clear. If two candidates are semantically close and the context does not establish which one the user means, it should return unresolved rather than guess. It is not expected to independently discover missing ontology definitions.

**System B expectations:** MiniCPM should use the supplied evidence to select the appropriate property, identify domain/range incompatibility, and request further inspection when needed. It should not infer that two properties are equivalent merely because their labels are similar.

**Level 2 pass condition:** Correct class and property selection, correct relationship direction, and correct handling of inverse properties. A system fails if it selects a property whose domain or range is incompatible with the requested relationship without an explicit, supported reasoning path.

## **Level 3 — Ontology discovery and multi-hop relationships**



Difficulty 3

**Purpose: test whether the system can discover concepts and relationships that are not directly exposed in the initial context.**



At this level, the initial context is intentionally incomplete. The graph contains the information needed to answer the request, but the model must recognize which ontology evidence is missing and request the relevant discovery operation. Some relationships are represented by intermediate resources or multiple properties. The correct answer cannot be obtained by selecting a direct property from the initial candidate list.

The initial context should contain the original request, the extracted or partially extracted intent, the known endpoint classes, and a limited initial vocabulary result. It should not reveal the complete correct path. Discovery tools should return realistic results: vocabulary search results, class inspection, property inspection, domain/range declarations, and relevant intermediate classes. The model must request these in an appropriate sequence. Rust must retain all accepted discoveries and rebuild each subsequent context from the current state.

| Test | User request                                                             | Expected behavior                                                                                                           |
| ---- | ------------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------- |
| 3.1  | “Find contacts who attended a technology conference.”                    | Discover whether attendance is a direct property or is represented through an event-registration or participation resource. |
| 3.2  | “Find accounts operating in Ohio.”                                       | Discover the appropriate operating-location path rather than assuming headquarters location.                                |
| 3.3  | “Find contacts belonging to accounts managed by the West region.”        | Discover the contact-to-account and account-to-region paths, including any intermediate assignment resource if present.     |
| 3.4  | “Find products included in orders placed by Acme.”                       | Discover the order, account, and order-line relationships needed to reach products.                                         |
| 3.5  | “Find organizations whose parent organization is headquartered in Ohio.” | Resolve the parent-organization path and then the parent's headquarters-location path.                                      |

**System A expectations:** Needle should not be expected to autonomously explore a complex ontology. Rust must perform broad candidate retrieval and path discovery, then use Needle to make bounded selections from the returned evidence. The test should measure whether the overall System A architecture can complete the task with Rust-driven discovery and constrained Needle decisions. It should also record when the task exceeds Needle's reliable selection capability. Needle must not be rewarded for guessing a direct relationship that is absent from the ontology.

**System B expectations:** MiniCPM should be able to identify missing relationship information, request targeted inspections, evaluate the returned evidence, and select a valid path. It should not request arbitrary or redundant inspections once sufficient evidence is available. Its decisions must remain grounded in the ontology responses.

**Level 3 pass condition:** The selected path must exactly match a valid ontology-supported path with the intended semantics. All required intermediate concepts must be discovered. The system must not invent a direct property, skip an intermediate resource, or confuse a path's direction.

## **Level 4 — Complex query interpretation, constraints, and ambiguity**



Difficulty 4

**Purpose: test whether the system can preserve multiple conditions, combine relationship paths, resolve semantic ambiguity, and construct a correct logical query.**



At this level, the request contains several related entities, multiple relationship paths, filters, Boolean conditions, time constraints, and negative conditions. Some phrases have multiple plausible ontology interpretations. The initial context should contain enough evidence to discover the relevant concepts, but not enough to resolve every ambiguity immediately. The system must decide what additional information is necessary and whether the remaining ambiguity requires a user question.

The context must preserve each condition separately, including its original phrase, subject, relationship, object, and resolution status. It must also include the relevant path candidates, their definitions, any time-related property semantics, and the allowed logical query operators. The model must not be asked to invent query syntax. It returns a logical specification referring to accepted concept and path IDs. Rust validates the structure, checks that every reference is grounded, compiles it to SPARQL, and executes it.

| Test | User request                                                                                             | Expected behavior                                                                                                                                                                                  |
| ---- | -------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 4.1  | “Find contacts working for accounts in Ohio that attended a technology conference during the past year.” | Resolve contact/account, account/location, and contact/event paths; interpret the time constraint; combine all conditions.                                                                         |
| 4.2  | “Find accounts headquartered in Ohio or operating in Ohio, but not both.”                                | Represent the exclusive-or condition correctly, if supported by the query language; do not substitute ordinary OR.                                                                                 |
| 4.3  | “Find contacts who attended a conference but did not register for any workshop at that conference.”      | Resolve the event relationship and express the negative condition with the correct event correlation.                                                                                              |
| 4.4  | “Find accounts with no active opportunities.”                                                            | Discover or use the defined meaning of active opportunity and express the absence condition within the selected graph scope.                                                                       |
| 4.5  | “Find contacts associated with accounts in Ohio.”                                                        | Recognize when “associated with” is ambiguous between employment, ownership, account membership, and other relationships; ask for clarification if the ontology evidence does not disambiguate it. |

**System A expectations:** Needle should handle the extraction of multiple conditions and the selection of clear candidates. Rust must do most of the query construction and discovery orchestration. If the query contains a semantic ambiguity that cannot be resolved through a small candidate selection, System A should ask the user or report the unresolved condition. It must not silently reduce the request to a simpler query.

**System B expectations:** MiniCPM should be able to discover the required concepts, compare relationship paths, preserve the full set of conditions, and produce a logical query specification. It should identify ambiguities that cannot be resolved from ontology evidence. It must correctly distinguish AND, OR, NOT, and exclusive OR where supported, and preserve the correlation between nested conditions. In particular, a negative condition must apply to the correct parent resource, not merely exclude any resource with a matching property somewhere in the graph.

**Level 4 pass condition:** The logical query must express every user condition with the correct entity bindings, relationship paths, Boolean semantics, temporal semantics, and negative-condition scope. The executed result set must match the expected result set. A query that returns plausible but incorrect results is a failure, even if its structure looks reasonable.

## **Level 5 — Full workflow, uncertainty, and controlled mutation**



Difficulty 5

**Purpose: test a complete natural-language workflow involving discovery, complex querying, interpretation, mutation proposal, validation, and confirmation.**



At this level, the request combines multiple operations and includes at least one unresolved business-semantic issue. The ontology contains several relevant paths, a mixture of direct and indirect relationships, and realistic data. Some requested concepts may be represented differently than the user expects. The system must discover the correct representation, determine which aspects are genuinely ambiguous, and avoid committing an operation based on unsupported assumptions.

The initial context contains only the original request, the authorized graph scope, and the basic workflow constraints. It should not contain a precomputed query or the correct mutation. Vocabulary and instance evidence must be retrieved through the normal services. The context builder must preserve the original request, accepted intent, unresolved questions, discovered ontology evidence, selected paths, query specification, and result handles across the workflow. It must also ensure that the model never receives unauthorized resource details or data outside the current scope.

| Test | User request                                                                                                                                                                                         | Expected behavior                                                                                                                                                                                                           |
| ---- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 5.1  | “Find contacts working for accounts in Ohio that attended a technology conference during the past year, excluding accounts with active opportunities, and mark the remaining accounts as prospects.” | Discover all required concepts and paths, clarify ambiguous location or active-status semantics, execute the query, identify matching accounts, propose classification changes, validate, and obtain required confirmation. |
| 5.2  | “Find customers whose subscriptions expired in the last 90 days, excluding those with an active renewal, and move the remaining customers into the former-customer classification.”                  | Discover subscription, expiration, renewal, and classification semantics; apply temporal and negative conditions; propose only the intended classification changes.                                                         |
| 5.3  | “Find suppliers that delivered products to accounts in Ohio but have no approved contracts, and flag them for review.”                                                                               | Discover delivery and contract paths; distinguish absent contracts from contracts that exist but are not approved; identify the exact supplier resources; propose a non-destructive flagging operation.                     |
| 5.4  | “Find organizations related to Acme through any subsidiary relationship, excluding organizations acquired before 2020, and classify the remaining organizations as prospects.”                       | Discover the relevant hierarchy and acquisition semantics; correctly handle multi-hop relationships and date conditions; avoid treating unrelated organizations as subsidiaries.                                            |
| 5.5  | “Remove the obsolete prospect classification from accounts that are no longer active, but do not delete the accounts or their other classifications.”                                                | Resolve the account activity definition and exact classification assertions; construct a narrowly scoped deletion proposal; preserve resources and unrelated triples; require confirmation under mutation policy.           |

**System A expectations:** System A must be evaluated as the combination of Rust orchestration and Needle's constrained decisions. It should be able to complete parts of the workflow that can be decomposed into deterministic retrieval and small candidate-selection decisions. It is not required to solve an ambiguity that the evidence cannot resolve. In such cases, it must stop at the appropriate point and ask the user. It must never produce a destructive mutation merely because the extracted intent contains a deletion or update verb.

**System B expectations:** System B should be able to conduct more extensive iterative discovery, recognize missing semantic definitions, inspect intermediate resources, and refine its interpretation as evidence arrives. It should produce a grounded query specification and a precise mutation proposal. It must still stop for clarification or confirmation when required. A fluent explanation of a proposed operation is not evidence that the operation is correct.

**Level 5 pass condition:** The system must correctly identify all matching resources, preserve every condition, handle open-world limitations, produce the exact intended mutation proposal, pass validation, and respect confirmation policy. It must not commit a mutation before authorization and confirmation. The test must also verify the final graph state, including that no unrelated triples or resources were modified.

# **12. Test execution rules and scoring**

Each level is a separate evaluation run. System A and System B receive identical user requests and equivalent initial evidence, but each has its own conversation and workflow state. Neither system may inherit discoveries, candidate selections, or clarifications from the other. A test at Level 4 must not receive the correct Level 3 path as a preselected answer unless that path would naturally have been discovered during that test's own execution.

The test fixtures must contain an explicit ground truth. For each fixture, this means the authoritative ontology classes, properties, definitions, domain/range declarations, inverse relationships, intermediate classes, relevant instance triples, expected query specification, expected result set, and expected mutation set. Ambiguous cases must identify whether the correct behavior is to choose a particular interpretation or ask the user. A test should never be scored against an unstated assumption about what a business phrase means.

For negative conditions, the fixture must specify the exact graph scope and inference policy. For temporal conditions, it must specify the reference date and the relevant date property. For terms such as “active,” “current,” “prospect,” and “former customer,” it must define the underlying ontology or business rule. Otherwise, a system could be penalized for correctly identifying an ambiguity that the test author failed to resolve.

The same initial-context rule applies to both systems. If a test is intended to measure ontology discovery, the initial context must not reveal the answer. If it is intended to measure candidate selection, the candidate set must already be supplied. If it is intended to measure query construction, the relevant concepts and paths must already be resolved. This separation makes it possible to identify whether a failure comes from extraction, discovery, path resolution, query construction, or execution.

## **Scoring dimensions**

Each test should be scored across six dimensions, with the same weights for both systems.

| Dimension                | Weight | What is measured                                                                |
| ------------------------ | ------ | ------------------------------------------------------------------------------- |
| Intent correctness       | 15%    | Correct action, entities, conditions, and requested output.                     |
| Ontology grounding       | 20%    | Correct concepts, properties, identifiers, and evidence usage.                  |
| Relationship correctness | 20%    | Correct direction, intermediate resources, and path semantics.                  |
| Query correctness        | 20%    | Correct logical conditions and actual result set.                               |
| Uncertainty handling     | 15%    | Appropriate clarification, ambiguity detection, and refusal to guess.           |
| Workflow and safety      | 10%    | Correct state handling, scope preservation, mutation preview, and confirmation. |

Each dimension receives a score from 0 to 5:

- **5:** Fully correct; no material errors.
- **4:** Correct result with a minor, non-semantic inefficiency.
- **3:** Mostly correct, but requires a correction or clarification that should reasonably have been avoided.
- **2:** Material error that is caught before execution.
- **1:** Incorrect interpretation or operation that would produce a wrong result if not caught.
- **0:** Fabricated ontology information, unauthorized operation, destructive error, or failure to complete the required decision.

A test's weighted score is the sum of its dimension scores divided by five and multiplied by their respective weights. A system should not receive a passing result simply because its average score is high: critical failures must be recorded separately.

**Critical failures** include inventing an RDF identifier and treating it as real, selecting a relationship that is contradicted by the ontology, silently dropping a user condition, interpreting an absent assertion as proof of real-world nonexistence, crossing graph or tenant scope, deleting a resource when only a relationship or classification should be removed, or committing an unconfirmed mutation.

## **System comparison**

**System A — Needle 3**

Primary evaluation question

Can Rust decompose the request and use a very small model reliably for extraction and bounded semantic decisions?

- Expected strength: Levels 1–2.
- Important evaluation: Level 3 with Rust-driven discovery.
- Expected limitation: Levels 4–5 when complex semantic interpretation cannot be reduced to simple decisions.

  **System B — MiniCPM5-1B**

Primary evaluation question

Can a small reasoning-capable model iteratively discover ontology information, resolve relationships, and construct correct logical query specifications?

- Expected strength: Levels 2–4.
- Important evaluation: Level 5, especially ambiguity and mutation handling.
- Expected limitation: Long or subtle semantic chains, especially when ontology evidence is incomplete or business definitions are unstated.

The scores should be reported separately for each difficulty level, rather than combining all tests into one overall accuracy figure. This will show whether Needle is a reliable low-cost decision component, whether MiniCPM provides a meaningful improvement in semantic discovery, and where the Rust application itself needs stronger deterministic behavior.

The most important comparison is not simply which model gets more answers right. It is **how much responsibility each model can safely take on before the application must intervene**. Needle may prove more reliable and economical for well-bounded decisions, while MiniCPM may complete more difficult discovery tasks. Both should be judged on whether they know when they do not have enough evidence.

## **13. Final test matrix**

This is the complete evaluation matrix. Every row is run independently against both systems.

| Level | Primary capability                     | Tests | Expected model responsibility                                                         |
| ----- | -------------------------------------- | ----- | ------------------------------------------------------------------------------------- |
| **1** | Direct intent extraction               | 5     | Identify actions, entities, and simple conditions.                                    |
| **2** | Vocabulary and direct relationships    | 5     | Match phrases to supplied ontology candidates and resolve direction.                  |
| **3** | Ontology discovery and multi-hop paths | 5     | Select or discover relationship paths from ontology evidence.                         |
| **4** | Complex logical queries                | 5     | Preserve multiple conditions, ambiguity, temporal logic, and negation.                |
| **5** | Full read-and-mutate workflows         | 5     | Discover, query, interpret, propose changes, and respect validation and confirmation. |

**Evaluation coverage**

# **25**

Distinct test cases

# **50**

Minimum system-test runs

For reliable measurements, each test should be repeated with multiple equivalent phrasings, including paraphrases that do not reuse ontology labels. The underlying graph fixture and expected semantic outcome remain unchanged. This tests whether the system understands the requested relationship rather than memorizing the wording of a test.

The resulting evaluation should make three things clear: how accurately each model interprets natural language, how effectively the Rust application supplies the evidence needed for correct decisions, and how safely the complete system behaves when the request exceeds what the ontology or model can establish.
