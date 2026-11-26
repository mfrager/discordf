# **Rust implementation plan: Needle 3 RDF/OWL agent**

## **1. Overall design**

Build a Rust application that provides a natural-language interface to an OWL/RDF knowledge store. Needle 3 is the tool-calling model responsible for interpreting user requests, selecting operations, supplying arguments, and deciding what additional information it needs. Rust provides the execution environment, tool registry, RDF storage interface, search engine, validation, and transaction management.

Keep the architecture deliberately small. There should be one agent execution loop, one collection of registered tools, one RDF repository abstraction, and one search subsystem. Avoid introducing a conventional agent framework, planner, workflow engine, or separate reasoning loop. Needle should determine the next action from the current request and the results of previous tool calls.

Use Oxigraph as the initial RDF implementation. It provides a native Rust RDF store, SPARQL support, named graphs, and transactional storage. Keep the storage implementation behind a repository interface so that the agent and tools are not coupled to Oxigraph.

## **2. Rust project structure**

Organize the project into modules for the agent, model integration, tools, RDF storage, search, ontology management, and execution policy. The agent module owns the conversation and execution lifecycle. The model module handles Needle 3 inference and tool-call responses. The tools module defines the available functions and their schemas. The RDF module implements resource-level CRUD operations. The search module handles semantic and lexical discovery. The ontology module provides OWL vocabulary information and SHACL validation. The policy module handles authorization, confirmation, and mutation restrictions.

Keep the public interfaces between these modules small. In particular, tools should depend on an RDF repository trait rather than directly accessing Oxigraph. This allows the RDF implementation to change without changing tool definitions or model prompts.

## **3. Needle 3 integration**

Integrate Needle 3 through the Cactus-supported Rust interface if available for the selected deployment target. Otherwise, implement a small adapter around its supported inference interface. Avoid creating an elaborate model abstraction layer. The adapter only needs to submit messages, register tool definitions, receive structured tool calls, submit tool results, and obtain the final response.

The model adapter should expose a simple asynchronous interface that accepts the conversation and tool definitions and returns either a tool-call response or a completed response. Normalize model responses into an internal representation containing the tool name, parsed arguments, call identifier, and any accompanying assistant content.

Keep the Needle-specific implementation isolated. The rest of the application should not need to know anything about Cactus response formats, tokenization, inference parameters, or model-specific message conventions.

## **4. Tool definitions**

Register five tools with Needle: `search`, `read`, `create`, `update`, and `delete`. Give each tool a concise description and a strict JSON argument schema. Tool descriptions should explain when to use the tool, what its arguments mean, and what information it returns. Avoid putting extensive ontology documentation into the tool descriptions.

The search tool discovers resources. The read tool retrieves RDF descriptions. The create tool creates resources and their initial properties. The update tool changes existing properties and relationships. The delete tool removes resources or selected triples. Each tool should have a single corresponding Rust handler.

Use a common tool registry that maps a tool name to its argument schema and execution handler. When Needle returns a tool call, the application resolves the tool name through this registry, validates the arguments, executes the handler, and serializes its result back into the model conversation.

## **5. RDF repository**

Implement an RDF repository abstraction with methods for searching, reading, creating, updating, and deleting resources. Use Oxigraph as the first implementation, with RDF terms represented using proper RDF types rather than ordinary strings.

Treat IRIs, blank nodes, language-tagged literals, and typed literals distinctly. A relationship between two resources must be represented as an IRI object, not a string literal. This distinction is essential for subsequent graph traversal and ontology reasoning.

Represent the knowledge store as an RDF dataset with named graphs. Use named graphs to distinguish projects, tenants, ontologies, or other knowledge contexts. Keep ontology definitions separate from ordinary application data where practical.

The repository should support atomic mutations. Each update or delete operation should execute within an appropriate transaction boundary, validate the resulting graph, and report exactly which triples changed.

## **6. Needle execution loop**

Use the `cactus-rs` crate rather than implementing the Cactus FFI yourself. There is now a Rust binding for Needle 3, published on crates.io, which exposes typed tool definitions, structured calls, embeddings, and grounding validation. The current documented dependency is `cactus-rs = "0.2"`.&#x20;

[image](https://www.google.com/s2/favicons?domain=https://docs.rs\&sz=32)

Rust+1



Build a single `Needle` instance at application startup using its builder, load the Needle 3 weights, supply the system instructions, and register the five tool definitions with JSON schemas. The Rust binding uses a process-wide model instance, so design the application around one persistent Needle instance rather than attempting to create a separate model for each conversation. The model is not thread-safe internally, and its weights cannot be unloaded or replaced while active.&#x20;

[image](https://www.google.com/s2/favicons?domain=https://docs.rs\&sz=32)

Rust+1



For every incoming request, call `Needle::complete()` with the user's text. Inspect the returned `Completion` and determine whether it contains tool calls or a completed response. For each tool call, parse its arguments into a Rust structure using Serde, dispatch it through the tool registry, and execute the corresponding operation. Validate the returned arguments and use Needle's grounding information to reject unsupported or invented argument values before executing sensitive operations.

After executing a tool, feed its result into the next Needle turn so that it can decide what to do next. Preserve the original request and accumulated tool results throughout the conversation. Continue until Needle returns no additional tool calls or the configured iteration limit is reached. The Rust binding exposes `complete()` rather than the Python `agent.run()` convenience loop, so implement this small orchestration loop yourself. Needle's conversation accumulates successive turns, and `reset()` can start a new conversation while retaining the configured tool definitions.&#x20;

[image](https://www.google.com/s2/favicons?domain=https://docs.rs\&sz=32)

Rust+1



Use a maximum of eight tool iterations initially, with a separate maximum number of operations per request. Return compact, structured tool results rather than large serialized RDF graphs. If a tool fails, return a structured error containing the tool name, error category, and useful explanation. Let Needle decide whether to retry, search elsewhere, or explain the failure.

## **7. Search and URI discovery**

Implement search as a separate subsystem rather than embedding search logic inside the RDF CRUD handlers. Its responsibility is to turn a natural-language query into a ranked collection of resource URIs. The search interface should accept a query, optional context, optional namespace, and result limit.

Use Needle's own embedding functionality as one of the search encoders. The Rust binding exposes `Needle::embed()`, producing 3,072-dimensional vectors. This gives you a common model for interpreting user queries and creating resource embeddings.&#x20;

[image](https://www.google.com/s2/favicons?domain=https://docs.rs\&sz=32)

Rust+1



For the initial implementation, combine lexical search with vector similarity. Lexical search should cover labels, comments, local names, descriptions, and ontology annotations. Vector search should compare the user's query embedding against indexed resource embeddings. Apply namespace and named-graph context filters before returning candidates, then combine the scores into a ranked result set.

Index each resource using its URI, RDF types, labels, descriptions, and selected relationships. Generate embeddings from a normalized textual representation of those fields. Whenever a resource is created, updated, or deleted, update its search-index entry. Keep the RDF store authoritative, and treat the search index as a disposable derived structure that can be rebuilt.

Return only the most relevant candidates, including URI, label, RDF type, short description, and relevance score. Needle can then decide which URI to read. Do not automatically expand every search result into a large graph traversal.

## **8. Resource reading and graph traversal**

Implement `read` as a resource-oriented interface rather than a general-purpose SPARQL execution tool. It should accept one or more URIs, an optional context, an optional list of predicates, and a traversal depth.

At depth zero or one, return the resource's directly asserted properties. At greater depths, follow selected relationships and include compact descriptions of connected resources. Apply limits to the number of returned triples, resources, and traversal paths to prevent unexpectedly large responses.

Include the RDF type, label, comment, and relevant relationships in the default result. Preserve the distinction between literal values and resource references. For references, always return their actual URIs so Needle can pass them into subsequent read calls.

Do not ask Needle to formulate SPARQL. If more advanced graph queries are needed later, implement them as additional application-level operations with narrowly defined argument schemas.

## **9. OWL ontology and SHACL validation**

Implement ontology management separately from ordinary resource CRUD. Load ontology graphs during initialization and make their relevant definitions available to the repository and validation services.

The ontology service should resolve class hierarchies, property characteristics, domains, ranges, inverse properties, equivalent classes, and subclass relationships. Where possible, use an OWL reasoning implementation to compute inferred types and relationships. Keep inferred statements separate from explicitly asserted statements.

Use SHACL shapes to validate resources before committing mutations. For example, a Contact resource might require a name, permit multiple email addresses, and require its account relationship to reference an Account resource. The Rust application should enforce these rules rather than relying on Needle to remember ontology constraints.

When validation fails, return the actual constraint violations to Needle. It can then request missing information, correct an operation, or explain why the requested change cannot be made.

## **10. Mutation safety and authorization**

Keep mutation authorization outside the model. Needle should be able to propose a create, update, or delete operation, but the Rust application determines whether that operation is permitted. Associate each request with an execution context containing the authenticated principal, tenant, allowed named graphs, and permitted operations.

Implement a dry-run mode for mutations. Before committing, calculate the affected triples, run validation, and return a mutation summary. For ordinary changes, the application can commit automatically when authorization and validation succeed. For destructive operations, require explicit confirmation from the user. Use Oxigraph transactions or an equivalent atomic operation boundary to avoid partially applied changes.

Record every mutation in an audit log containing the request identifier, principal, target URI, graph, operation, affected triples, validation result, and commit status. Never treat a model-generated success message as proof of a committed operation.

## **11. Conversation management and application lifecycle**

Keep Needle's conversation management separate from the user's application conversation history. Store user messages, tool calls, tool results, and final responses in the application database. This provides durable history and allows conversations to be reconstructed.

There is an important deployment constraint: the current Rust binding maintains one process-wide Needle instance and one accumulating model conversation. Consequently, do not assume that separate concurrent HTTP requests can each own an independent Needle instance. Start with a dedicated agent worker that serializes inference requests. Reset the model conversation between independent user sessions and supply the relevant conversation context again. If independent model instances are eventually necessary, use separate worker processes.

Needle currently does not provide token streaming. Treat each inference call as a completed operation and send progress updates from the application layer if the interface needs them.&#x20;

[image](https://www.google.com/s2/favicons?domain=https://docs.rs\&sz=32)

Rust+1



The final user-facing response should be assembled by the application. Needle is primarily a structured tool-calling engine, not a general conversational response generator. When it returns no more tool calls, the application should summarize the completed operations, their results, and any remaining errors. If more sophisticated natural-language synthesis becomes necessary, that can be delegated to a separate larger model without changing the RDF tool architecture.

## **12. Testing and implementation sequence**

Start with the RDF repository and its unit tests. Verify that resources can be created, read, updated, and deleted, that named graphs remain isolated, and that typed literals and IRI relationships are preserved. Test transactions, SHACL validation, and ontology loading before introducing Needle.

Next, implement the five tools independently and test them with ordinary Rust calls. Give each tool a collection of representative arguments and expected results. This ensures that problems can be isolated to either the RDF implementation or the model.

Then integrate Needle and build a benchmark of natural-language requests. Include simple searches, ambiguous searches, relationship discovery, resource creation, property updates, multi-step operations, and requests that should be rejected. Record tool selection accuracy, argument accuracy, URI accuracy, operation ordering, and final graph correctness.

Once the initial system works, add embeddings and the search index. Compare lexical retrieval against vector retrieval and their combination. Finally, introduce OWL reasoning, SHACL validation, authorization, audit logging, and more advanced graph traversal.

For the first usable release, avoid sophisticated planning and autonomous ontology modification. The goal is to establish a dependable cycle in which Needle discovers a resource, reads it, performs an authorized operation, receives the result, and determines whether another operation is needed.

## **Recommended initial Rust dependencies**

Use `cactus-rs` for Needle 3, `oxigraph` for RDF storage, `serde` and `serde_json` for structured tool arguments and results, `tokio` for asynchronous application execution, `axum` for an HTTP API if needed, and `tracing` for observability. Add a vector index only after the basic search implementation is working. Use a SHACL validation implementation compatible with your chosen RDF stack, and keep the reasoning engine behind the ontology service interface.

**The intended result is a small Rust application in which Needle is the decision-making tool router, Oxigraph is the authoritative knowledge store, and search is the URI discovery mechanism.** The application, not the LLM, owns correctness, transactions, validation, authorization, and persistence.

The most important implementation decision is to keep Needle's job narrow: interpret the request, choose the next operation, and supply its arguments. Everything that requires deterministic graph semantics belongs in Rust.
