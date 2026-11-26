//! Tantivy lexical index + vector sidecar (Phase 3).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};

use serde::{Deserialize, Serialize};
use tantivy::collector::TopDocs;
use tantivy::directory::MmapDirectory;
use tantivy::query::QueryParser;
use tantivy::schema::{
    Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, Value, FAST, STORED, STRING,
};
use tantivy::tokenizer::{AsciiFoldingFilter, LowerCaser, SimpleTokenizer, TextAnalyzer};
use tantivy::{doc, Index, IndexReader, IndexWriter, ReloadPolicy, Term};

use super::embeddings::{
    cosine_similarity, normalize_text, short_name, Embedder, EmbeddingCache, HashEmbedder,
};
use super::rank::{rank_hybrid, Candidate, Provenance, SearchMode};
use super::{SearchError, SearchService};
use crate::rdf::{GraphName, RdfRepository, Term as RdfTerm};

/// Tokenizer choice (recorded per plan):
/// - `lower_ascii` = SimpleTokenizer + LowerCaser + AsciiFoldingFilter.
///   Gives case-insensitivity + ascii-folding (`café`→`cafe`).
/// - Local names use the same analyzer (no edge-ngram/CJK in v1;
///   edge-ngram was considered but rejected to keep the index small and
///   ranking deterministic; local-name matching works via tokenized
///   `local_name` field, e.g. `JohnSmith` is pre-split into `john smith`).
pub const TOKENIZER_NAME: &str = "lower_ascii";

fn text_options() -> TextOptions {
    let indexing = TextFieldIndexing::default().set_tokenizer(TOKENIZER_NAME);
    TextOptions::default()
        .set_indexing_options(indexing)
        .set_stored()
}

fn register_tokenizers(index: &Index) {
    let analyzer = TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(LowerCaser)
        .filter(AsciiFoldingFilter)
        .build();
    index.tokenizers().register(TOKENIZER_NAME, analyzer);
}

#[derive(Debug, Clone)]
struct Fields {
    uri: Field,
    graph: Field,
    type_uris: Field,
    label: Field,
    comment: Field,
    local_name: Field,
    annotations: Field,
    embedding_id: Field,
}

fn build_schema() -> (Schema, Fields) {
    let mut b = Schema::builder();
    let uri = b.add_text_field("uri", STRING | STORED | FAST);
    let graph = b.add_text_field("graph", STRING | STORED | FAST);
    let type_uris = b.add_text_field("type_uris", STRING | STORED);
    let label = b.add_text_field("label", text_options());
    let comment = b.add_text_field("comment", text_options());
    let local_name = b.add_text_field("local_name", text_options());
    let annotations = b.add_text_field("annotations", text_options());
    let embedding_id = b.add_text_field("embedding_id", STRING | STORED);
    let schema = b.build();
    (
        schema,
        Fields {
            uri,
            graph,
            type_uris,
            label,
            comment,
            local_name,
            annotations,
            embedding_id,
        },
    )
}

/// Document to index (one per resource URI + graph).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchDocument {
    pub uri: String,
    pub graph: String,
    pub type_uris: Vec<String>,
    pub label: String,
    pub comment: String,
    pub local_name: String,
    pub annotations: String,
}

impl SearchDocument {
    pub fn embedding_id(&self) -> String {
        format!("{}||{}", self.graph, self.uri)
    }
    pub fn normalized_text(&self) -> String {
        normalize_text(
            &self.label,
            &self.comment,
            &self.type_uris,
            std::slice::from_ref(&self.annotations),
        )
    }
}

#[derive(Debug, Clone)]
struct LexHit {
    uri: String,
    graph: String,
    label: String,
    type_: String,
    comment: String,
    score: f32,
}

/// Tantivy-backed index with a vector sidecar.
pub struct TantivyIndex {
    index: Index,
    fields: Fields,
    writer: Mutex<IndexWriter>,
    reader: IndexReader,
    vectors: RwLock<HashMap<String, Vec<f32>>>,
    cache: EmbeddingCache,
    embedder: Box<dyn Embedder>,
    alpha: f32,
    dir: Option<PathBuf>,
}

impl TantivyIndex {
    fn with_index(
        index: Index,
        dir: Option<PathBuf>,
        embedder: Box<dyn Embedder>,
        alpha: f32,
    ) -> Result<Self, SearchError> {
        register_tokenizers(&index);
        let schema = index.schema();
        // Recover field handles from the live schema (same names as build_schema).
        let fields = Fields {
            uri: schema
                .get_field("uri")
                .map_err(|e| SearchError::Tantivy(e.to_string()))?,
            graph: schema
                .get_field("graph")
                .map_err(|e| SearchError::Tantivy(e.to_string()))?,
            type_uris: schema
                .get_field("type_uris")
                .map_err(|e| SearchError::Tantivy(e.to_string()))?,
            label: schema
                .get_field("label")
                .map_err(|e| SearchError::Tantivy(e.to_string()))?,
            comment: schema
                .get_field("comment")
                .map_err(|e| SearchError::Tantivy(e.to_string()))?,
            local_name: schema
                .get_field("local_name")
                .map_err(|e| SearchError::Tantivy(e.to_string()))?,
            annotations: schema
                .get_field("annotations")
                .map_err(|e| SearchError::Tantivy(e.to_string()))?,
            embedding_id: schema
                .get_field("embedding_id")
                .map_err(|e| SearchError::Tantivy(e.to_string()))?,
        };
        let writer = index.writer(50_000_000)?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        Ok(Self {
            index,
            fields,
            writer: Mutex::new(writer),
            reader,
            vectors: RwLock::new(HashMap::new()),
            cache: EmbeddingCache::default(),
            embedder,
            alpha,
            dir,
        })
    }

    /// In-RAM index with the default stub embedder.
    pub fn in_ram() -> Result<Self, SearchError> {
        Self::in_ram_with_embedder(Box::new(HashEmbedder::default_dim()))
    }

    pub fn in_ram_with_embedder(embedder: Box<dyn Embedder>) -> Result<Self, SearchError> {
        let (schema, _) = build_schema();
        let index = Index::create_in_ram(schema);
        Self::with_index(index, None, embedder, 0.5)
    }

    /// Open-or-create a persistent index dir. Vectors sidecar lives at
    /// `<dir>/vectors.json` (Tantivy dir itself holds lexical data).
    pub fn open_or_create(
        path: impl AsRef<Path>,
        embedder: Box<dyn Embedder>,
    ) -> Result<Self, SearchError> {
        let path = path.as_ref().to_path_buf();
        std::fs::create_dir_all(&path).map_err(|e| SearchError::Storage(e.to_string()))?;
        let (schema, _) = build_schema();
        let dir = MmapDirectory::open(&path).map_err(|e| SearchError::Tantivy(e.to_string()))?;
        let index = Index::open_or_create(dir, schema)?;
        let this = Self::with_index(index, Some(path), embedder, 0.5)?;
        this.load_vectors()?;
        Ok(this)
    }

    pub fn set_alpha(&mut self, alpha: f32) {
        self.alpha = alpha.clamp(0.0, 1.0);
    }

    pub fn doc_count(&self) -> usize {
        self.vectors.read().unwrap().len()
    }

    fn persist_vectors(&self) -> Result<(), SearchError> {
        if let Some(dir) = &self.dir {
            let p = dir.join("vectors.json");
            let map = self.vectors.read().unwrap();
            let json =
                serde_json::to_string(&*map).map_err(|e| SearchError::Storage(e.to_string()))?;
            std::fs::write(p, json).map_err(|e| SearchError::Storage(e.to_string()))?;
        }
        Ok(())
    }

    fn load_vectors(&self) -> Result<(), SearchError> {
        if let Some(dir) = &self.dir {
            let p = dir.join("vectors.json");
            if p.exists() {
                let raw =
                    std::fs::read_to_string(p).map_err(|e| SearchError::Storage(e.to_string()))?;
                let map: HashMap<String, Vec<f32>> =
                    serde_json::from_str(&raw).map_err(|e| SearchError::Storage(e.to_string()))?;
                *self.vectors.write().unwrap() = map;
            }
        }
        Ok(())
    }

    fn embedding_id(uri: &str, graph: &str) -> String {
        format!("{graph}||{uri}")
    }

    /// Upsert one resource doc + embedding (call `commit()` after a batch).
    pub fn upsert(&self, doc: &SearchDocument) -> Result<(), SearchError> {
        let eid = doc.embedding_id();
        let text = doc.normalized_text();
        let vec = self.cache.get_or_embed(self.embedder.as_ref(), &text);
        self.vectors.write().unwrap().insert(eid.clone(), vec);

        let f = &self.fields;
        let mut td = doc!(
            f.uri => doc.uri.clone(),
            f.graph => doc.graph.clone(),
            f.embedding_id => eid.clone(),
            f.label => doc.label.clone(),
            f.comment => doc.comment.clone(),
            f.local_name => split_local(&doc.local_name),
            f.annotations => doc.annotations.clone(),
        );
        for t in &doc.type_uris {
            td.add_text(f.type_uris, t);
        }
        {
            let w = self.writer.lock().unwrap();
            w.delete_term(Term::from_field_text(f.embedding_id, &eid));
            w.add_document(td)?;
        }
        Ok(())
    }

    /// Delete one resource doc + vector (call `commit()` after).
    pub fn delete(&self, uri: &str, graph: &GraphName) -> Result<(), SearchError> {
        let eid = Self::embedding_id(uri, &graph.to_string());
        self.vectors.write().unwrap().remove(&eid);
        self.writer
            .lock()
            .unwrap()
            .delete_term(Term::from_field_text(self.fields.embedding_id, &eid));
        Ok(())
    }

    /// Commit writer + reload reader + persist sidecar.
    pub fn commit(&self) -> Result<(), SearchError> {
        self.writer.lock().unwrap().commit()?;
        self.reader.reload()?;
        self.persist_vectors()?;
        Ok(())
    }

    /// Full rebuild from the authoritative repo scan (disposable index).
    pub fn rebuild(&self, repo: &(impl RdfRepository + ?Sized)) -> Result<usize, SearchError> {
        self.writer.lock().unwrap().delete_all_documents()?;
        self.vectors.write().unwrap().clear();
        let docs = documents_from_repo(repo)?;
        let n = docs.len();
        for d in &docs {
            self.upsert(d)?;
        }
        self.commit()?;
        Ok(n)
    }

    /// Sync one resource from repo state (create/update → upsert; missing → delete).
    pub fn sync_resource(
        &self,
        repo: &(impl RdfRepository + ?Sized),
        uri: &str,
        graph: &GraphName,
    ) -> Result<(), SearchError> {
        let subject = RdfTerm::iri(uri.to_string())
            .map_err(|e| SearchError::InvalidArgument(e.to_string()))?;
        let quads = repo
            .match_quads(Some(&subject), None, None, Some(graph), None)
            .map_err(|e| SearchError::Storage(e.to_string()))?;
        if quads.is_empty() {
            self.delete(uri, graph)?;
        } else if let Some(d) = document_for_resource(uri, graph, &quads) {
            self.upsert(&d)?;
        }
        self.commit()?;
        Ok(())
    }

    fn lexical_hits(&self, query: &str, limit: usize) -> Result<Vec<LexHit>, SearchError> {
        if query.trim().is_empty() {
            return Ok(vec![]);
        }
        let searcher = self.reader.searcher();
        let qp = QueryParser::for_index(
            &self.index,
            vec![
                self.fields.label,
                self.fields.comment,
                self.fields.local_name,
                self.fields.annotations,
            ],
        );
        let q = match qp.parse_query(query) {
            Ok(q) => q,
            Err(_) => return Ok(vec![]),
        };
        let top = searcher.search(&*q, &TopDocs::with_limit(limit.max(10)))?;
        let mut out = Vec::new();
        for (score, addr) in top {
            let d: tantivy::TantivyDocument = searcher.doc(addr)?;
            let get = |f: Field| {
                d.get_first(f)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string()
            };
            let uri = get(self.fields.uri);
            if uri.is_empty() {
                continue;
            }
            out.push(LexHit {
                uri,
                graph: get(self.fields.graph),
                label: get(self.fields.label),
                type_: get(self.fields.type_uris),
                comment: get(self.fields.comment),
                score,
            });
        }
        Ok(out)
    }

    fn meta_by_eid(&self, eid: &str) -> Option<(String, String, String, String, String)> {
        // (uri, graph, label, type, description) via embedding_id term.
        let searcher = self.reader.searcher();
        let term = Term::from_field_text(self.fields.embedding_id, eid);
        use tantivy::query::TermQuery;
        let q = TermQuery::new(term, IndexRecordOption::Basic);
        let top = searcher.search(&q, &TopDocs::with_limit(1)).ok()?;
        let (_, addr) = top.into_iter().next()?;
        let d: tantivy::TantivyDocument = searcher.doc(addr).ok()?;
        let get = |f: Field| {
            d.get_first(f)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };
        Some((
            get(self.fields.uri),
            get(self.fields.graph),
            get(self.fields.label),
            get(self.fields.type_uris),
            get(self.fields.comment),
        ))
    }

    fn vector_hits(&self, query: &str, limit: usize) -> Vec<(String, f32)> {
        // Returns (embedding_id, cosine).
        let qv = self.cache.get_or_embed(self.embedder.as_ref(), query);
        let map = self.vectors.read().unwrap();
        let mut scored: Vec<(String, f32)> = map
            .iter()
            .map(|(eid, v)| (eid.clone(), cosine_similarity(&qv, v)))
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit.max(10));
        scored
    }

    /// Mode-selectable search (used by recall benchmarks).
    pub fn search_with_mode(
        &self,
        query: &str,
        context_graph: Option<&GraphName>,
        namespace: Option<&str>,
        limit: Option<usize>,
        mode: SearchMode,
    ) -> Result<Vec<Candidate>, SearchError> {
        let limit = limit.unwrap_or(10).clamp(1, 100);
        let fetch = (limit.max(5) * 4).min(100);

        let passes_filters = |uri: &str, graph: &str| -> bool {
            if let Some(g) = context_graph {
                if graph != g.to_string() {
                    return false;
                }
            }
            if let Some(ns) = namespace {
                if !uri.starts_with(ns) {
                    return false;
                }
            }
            true
        };

        match mode {
            SearchMode::Lexical => {
                let hits = self.lexical_hits(query, fetch)?;
                Ok(hits
                    .into_iter()
                    .filter(|h| passes_filters(&h.uri, &h.graph))
                    .take(limit)
                    .map(|h| Candidate {
                        uri: h.uri,
                        label: h.label,
                        type_: h.type_,
                        description: h.comment,
                        score: h.score,
                        provenance: Provenance::Lexical,
                    })
                    .collect())
            }
            SearchMode::Vector => {
                let scored = self.vector_hits(query, fetch);
                let mut out = Vec::new();
                for (eid, s) in scored {
                    if let Some((uri, graph, label, typ, comment)) = self.meta_by_eid(&eid) {
                        if !passes_filters(&uri, &graph) {
                            continue;
                        }
                        out.push(Candidate {
                            uri,
                            label,
                            type_: typ,
                            description: comment,
                            score: s,
                            provenance: Provenance::Vector,
                        });
                        if out.len() >= limit {
                            break;
                        }
                    }
                }
                Ok(out)
            }
            SearchMode::Hybrid => {
                let lex_hits = self.lexical_hits(query, fetch)?;
                let lex_f: Vec<(String, f32)> = lex_hits
                    .iter()
                    .filter(|h| passes_filters(&h.uri, &h.graph))
                    .map(|h| (h.uri.clone(), h.score))
                    .collect();
                let vec_scored = self.vector_hits(query, fetch);
                let mut vec_f = Vec::new();
                for (eid, s) in &vec_scored {
                    if let Some((uri, graph, _, _, _)) = self.meta_by_eid(eid) {
                        if passes_filters(&uri, &graph) {
                            vec_f.push((uri, *s));
                        }
                    }
                }
                let fused = rank_hybrid(&lex_f, &vec_f, self.alpha, limit);
                // Resolve candidate display fields from whichever side has them.
                let lex_map: HashMap<&str, &LexHit> =
                    lex_hits.iter().map(|h| (h.uri.as_str(), h)).collect();
                let mut out = Vec::new();
                for (uri, score, prov) in fused {
                    if let Some(h) = lex_map.get(uri.as_str()) {
                        out.push(Candidate {
                            uri: uri.clone(),
                            label: h.label.clone(),
                            type_: h.type_.clone(),
                            description: h.comment.clone(),
                            score,
                            provenance: prov,
                        });
                    } else if let Some(eid) = self.eid_for_uri(&uri, context_graph) {
                        if let Some((u, _, label, typ, comment)) = self.meta_by_eid(&eid) {
                            out.push(Candidate {
                                uri: u,
                                label,
                                type_: typ,
                                description: comment,
                                score,
                                provenance: prov,
                            });
                        }
                    }
                }
                Ok(out)
            }
        }
    }

    fn eid_for_uri(&self, uri: &str, context_graph: Option<&GraphName>) -> Option<String> {
        if let Some(g) = context_graph {
            return Some(Self::embedding_id(uri, &g.to_string()));
        }
        // No graph filter: find any eid ending with this uri.
        let map = self.vectors.read().unwrap();
        for eid in map.keys() {
            if let Some((_, u)) = eid.split_once("||") {
                if u == uri {
                    return Some(eid.clone());
                }
            }
        }
        None
    }
}

impl SearchService for TantivyIndex {
    fn search(
        &self,
        query: &str,
        context_graph: Option<&GraphName>,
        namespace: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Vec<Candidate>, SearchError> {
        self.search_with_mode(query, context_graph, namespace, limit, SearchMode::Hybrid)
    }

    fn sync_resource(
        &self,
        repo: &dyn RdfRepository,
        uri: &str,
        graph: &GraphName,
    ) -> Result<(), SearchError> {
        TantivyIndex::sync_resource(self, repo, uri, graph)
    }

    fn rebuild_from_repo(&self, repo: &dyn RdfRepository) -> Result<usize, SearchError> {
        self.rebuild(repo)
    }
}

/// Split `JohnSmith` → `john smith` for the local-name field so
/// `john smith` queries match `ex:JohnSmith`.
fn split_local(local: &str) -> String {
    let mut out = String::new();
    for (i, c) in local.chars().enumerate() {
        if i > 0 && c.is_uppercase() {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

/// Well-known RDF predicates used when building docs.
const RDF_TYPE_STR: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

/// Build one [`SearchDocument`] from all quads of a resource, or `None`
/// when the resource has no usable URI subject.
pub fn document_for_resource(
    uri: &str,
    graph: &GraphName,
    quads: &[crate::rdf::Quad],
) -> Option<SearchDocument> {
    if quads.is_empty() {
        return None;
    }
    let mut label = String::new();
    let mut comment = String::new();
    let mut types = Vec::new();
    let mut annotations = Vec::new();
    for q in quads {
        let pred = q.predicate.as_iri().unwrap_or("");
        let local: &str = pred.rsplit(['/', '#']).next().unwrap_or(pred);
        match q.object.clone() {
            RdfTerm::Iri(o) => {
                if pred == RDF_TYPE_STR {
                    types.push(o);
                } else {
                    annotations.push(short_name(&o));
                }
            }
            RdfTerm::Literal { value, .. } => match local.to_lowercase().as_str() {
                "label" | "name" | "title" => {
                    if label.is_empty() {
                        label = value;
                    } else {
                        annotations.push(value);
                    }
                }
                "comment" | "description" | "abstract" => {
                    if comment.is_empty() {
                        comment = value;
                    } else {
                        annotations.push(value);
                    }
                }
                _ => annotations.push(value),
            },
            RdfTerm::BlankNode(b) => annotations.push(b),
        }
    }
    if label.is_empty() {
        label = short_name(uri);
    }
    Some(SearchDocument {
        uri: uri.to_string(),
        graph: graph.to_string(),
        type_uris: types,
        label: label.clone(),
        comment,
        local_name: short_name(uri),
        annotations: annotations.join(" "),
    })
}

/// Scan the repo and build one doc per (subject, graph).
pub fn documents_from_repo(
    repo: &(impl RdfRepository + ?Sized),
) -> Result<Vec<SearchDocument>, SearchError> {
    use std::collections::BTreeMap;
    let quads = repo
        .match_quads(None, None, None, None, None)
        .map_err(|e| SearchError::Storage(e.to_string()))?;
    // Group by (graph, subject-iri). Blank-node subjects are indexed too
    // (as `_:<id>` pseudo-URIs) so they stay searchable.
    let mut groups: BTreeMap<(String, String), Vec<crate::rdf::Quad>> = BTreeMap::new();
    for q in quads {
        // Skip ontology triples: the index covers app data, not schema.
        if q.graph.is_ontology() {
            continue;
        }
        let subj = match &q.subject {
            RdfTerm::Iri(s) => s.clone(),
            RdfTerm::BlankNode(b) => format!("_:{b}"),
            RdfTerm::Literal { .. } => continue,
        };
        groups
            .entry((q.graph.to_string(), subj))
            .or_default()
            .push(q);
    }
    let mut docs = Vec::new();
    for ((graph_s, uri), qs) in groups {
        let graph = if graph_s == "default" {
            GraphName::Default
        } else {
            GraphName::Named(graph_s.clone())
        };
        if let Some(d) = document_for_resource(&uri, &graph, &qs) {
            docs.push(d);
        }
    }
    Ok(docs)
}
