# **Design update: Contextual, ontology-aware RDF agent**

The original design is too CRUD-centric and too dependent on simple URI discovery. It works for straightforward operations, but it will struggle with requests that require understanding an ontology, constructing multi-condition graph queries, discovering relevant vocabulary, and then manipulating a specific subset of the graph.

I would evolve it from a five-tool CRUD interface into a **contextual RDF operations interface**. Needle 3 remains the tool-calling controller, but receives a more expressive set of operations and a persistent operational context.

## **1. Limitations of the original design**

The original five tools have several limitations.

**Search conflates different kinds of discovery.** Finding an OWL class, locating a property definition, and finding an instance of that class are different operations. Vocabulary discovery should be independent of instance discovery. Otherwise, the model can confuse ontology definitions with actual data.

**Read with a fixed traversal depth is insufficient.** Complex graph questions require following specific predicates, filtering relationships, joining resources, and traversing paths with different constraints. Arbitrary depth traversal is not a substitute for graph queries.

**CRUD is too resource-oriented.** RDF is fundamentally a graph of statements, not a collection of documents with properties. Some operations need to manipulate individual triples, relationships, or sets of resources selected by a query.

**There is no persistent operational context.** A sequence of operations needs to share a consistent understanding of the relevant ontology, namespaces, named graphs, resource types, and previously discovered URIs.

**There is no query-construction capability.** Needle needs to be able to construct queries from vocabulary discoveries, execute them, inspect their results, and use those results to determine subsequent operations.

**The original execution model is too linear.** A complex request may require alternating between vocabulary search, instance search, ontology inspection, query execution, and graph mutations. The agent must be able to interleave these operations rather than follow a predefined search-then-read-then-update sequence.

## **2. Introduce an operational context**

Every tool should operate within an explicit `GraphContext`. This becomes the shared context for the entire request and can be refined as Needle discovers more information.

A context should include the applicable dataset, named graphs, namespace mappings, ontology resources, inference settings, tenant or project boundaries, and transaction identity. It should also support a focused subset of the graph, represented by explicit URI sets, query-defined resource sets, or graph patterns.

For example, Needle might establish a context for the CRM ontology, restrict instance operations to the Savvy CRM named graph, and identify the Contact and Account classes as relevant vocabulary.

The important distinction is that context should not merely be a search filter. It must also define the scope of subsequent reads, queries, and mutations. Every CRUD operation should inherit this context unless it explicitly overrides it.

Context should be represented as a structured object rather than repeated natural-language instructions. The application should maintain and validate it between tool calls.

## **3. Expand the tool interface**

I would use the following tools:

| Tool                | Responsibility                                                       |
| ------------------- | -------------------------------------------------------------------- |
| `search_vocabulary` | Find OWL classes, properties, restrictions, and ontology definitions |
| `search_instances`  | Find actual RDF resources and instances                              |
| `read`              | Inspect resources, vocabulary definitions, and relationships         |
| `query`             | Execute complex graph queries                                        |
| `create`            | Create vocabulary or instance resources and triples                  |
| `update`            | Modify resources, relationships, or selected triples                 |
| `delete`            | Delete resources, triples, or query-selected subsets                 |
| `validate`          | Check ontology and SHACL constraints                                 |

The two search tools should be deliberately distinct.

`search_vocabulary` searches ontology definitions, including classes, object properties, datatype properties, annotation properties, domains, ranges, restrictions, and subclass relationships.

`search_instances` searches actual resources, optionally constrained by RDF type, namespace, named graph, or other known properties.

Both return URIs and compact descriptions that can be used in subsequent Needle calls.

This separation is particularly important when Needle needs to discover a class, inspect its properties, and then find instances of that class.

## **4. Add a proper graph query interface**

The most important addition is `query`.

It should support complex queries involving multiple graph patterns, joins, optional relationships, filters, unions, and property paths.

I would use SPARQL 1.1 as the underlying query language rather than inventing a proprietary graph query language.

The tool should accept a query, bindings, and an explicit graph context.

For example:

```
SELECT ?contact ?account ?email
WHERE {
  GRAPH <urn:savvy:crm> {
    ?contact a <urn:savvy:Contact> ;
             <urn:savvy:belongsTo> ?account ;
             <urn:savvy:email> ?email .

    ?account a <urn:savvy:Account> .
  }
  FILTER(CONTAINS(LCASE(STR(?email)), "example"))
}
LIMIT 50
```

Needle should be allowed to construct this query after discovering the relevant classes and predicates through vocabulary search.

The query service should validate syntax, enforce graph permissions, apply result limits, and reject unauthorized query forms. Initially, expose SELECT, ASK, and CONSTRUCT, while keeping SPARQL Update behind the controlled mutation tools.

This provides a substantial increase in expressiveness without requiring Needle to understand the internal storage implementation.

## **5. Make CRUD operations graph-oriented and contextual**

CRUD should support both resource-level operations and statement-level operations.

For example, `create` should support creating a class instance, defining an ontology property, or inserting a collection of triples. `update` should support replacing values, adding relationships, removing selected triples, and applying a graph patch. `delete` should support deleting specific statements, resources, or a query-selected set of resources.

Every operation should accept the same `GraphContext`, with optional explicit overrides.

Crucially, operations should be capable of targeting subsets of the graph. A subset could be specified by named graph, explicit URI collection, or a query result. This allows Needle to discover a set of resources using one operation and mutate exactly that set using another.

For complex changes, stage mutations within a transaction workspace. Validate the combined result before committing, rather than validating every intermediate change independently.

## **6. Allow Needle to construct an operational plan dynamically**

Do not implement a fixed workflow such as search, read, update. Instead, let Needle interleave operations as necessary, maintaining an operational context and a short plan in its conversation.

Consider this request:

"Find all CRM contacts belonging to accounts in Ohio, determine which ontology property identifies their account relationship, and add a regional classification to those contacts."

A possible execution sequence is:

1. `search_vocabulary`: Find the Contact and Account classes.
2. `read`: Inspect their ontology definitions.
3. `search_vocabulary`: Discover the property connecting Contact to Account and its domain and range.
4. `query`: Construct a SPARQL query that identifies the relevant contacts through their accounts and geographic properties.
5. `search_vocabulary`: Find an appropriate regional classification property or classification class.
6. `search_instances`: Determine whether the required Ohio classification resource already exists.
7. `create`: Create the classification resource if necessary.
8. `update`: Apply the classification relationship to the selected contacts.
9. `validate`: Check the resulting graph.
10. Commit the transaction and report the changes.

Notice that vocabulary discovery, instance discovery, queries, and mutations are interspersed. Needle decides what to do next based on the results of previous operations.

## **7. Rust implementation changes**

Keep the existing Rust architecture, but introduce a few additional abstractions.

`GraphContext` should be a first-class Rust structure shared across tool handlers. It should contain dataset and graph scope, namespaces, inference configuration, and transaction/workspace identity.

`OntologyService` should resolve vocabulary definitions and expose class/property metadata independently of instance storage.

`QueryService` should accept validated SPARQL queries and execute them against the repository.

`GraphRepository` should expose graph-oriented operations, including triple insertion, deletion, resource inspection, and transactional graph patches.

`SearchService` should maintain separate vocabulary and instance indexes, with both lexical and semantic retrieval.

`OperationContext` should maintain the current request's operational state, including discovered URIs, query results, selected graph subsets, and pending mutations.

The tool registry should inject the current `OperationContext` into each handler. Needle should not be responsible for manually carrying every discovered URI or reconstructing the context on each call.

## **8. The revised execution model**

The execution loop remains simple:

1. Send the user request and current operational context to Needle.
2. Needle selects an operation and produces structured arguments.
3. Rust validates the tool call and resolves its contextual scope.
4. Execute the operation and return a compact, structured result.
5. Update the operational context with discovered resources, query results, and pending changes.
6. Send the result back to Needle.
7. Repeat until Needle returns a final response.
8. Validate and commit pending mutations, subject to authorization and confirmation requirements.

The application should retain the original request, operation history, and transaction state throughout the process. Limit iterations, query complexity, graph traversal, and result sizes.

## **9. Architectural result**

The resulting system is substantially more expressive than the original CRUD agent.

It can discover vocabulary, understand ontology definitions, discover instances, construct complex SPARQL queries, select graph subsets, and perform contextualized mutations. These are all operations in one iterative tool-calling environment.

The critical design change is this:

**Move from a resource-oriented CRUD agent to a context-aware graph operations agent.**

Needle does not need to become an OWL reasoner or SPARQL engine. It needs to be able to discover the vocabulary, construct operations using that vocabulary, interpret query results, and choose the next operation.

Rust and the RDF services provide the deterministic semantics.

I would make `GraphContext`, `query`, and the separation of vocabulary search from instance search the three foundational changes. Those three additions will provide most of the expressive power needed for a general-purpose natural-language interface to MetaMind's RDF/OWL knowledge store.

