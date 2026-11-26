//! Embeddings (Phase 3): normalized text + pluggable encoder + cache.
//!
//! Prod uses `Needle::embed()` 3072-d via `cactus-rs` (Phase 4 wires it).
//! Tests use [`HashEmbedder`] (deterministic, no weights). Vectors are
//! L2-normalized; cache is keyed by text hash to avoid re-embedding.

use std::collections::HashMap;
use std::sync::Mutex;

/// Text normalization: `label + " " + comment + " " + types + " " + relations`.
pub fn normalize_text(
    label: &str,
    comment: &str,
    types: &[String],
    relations: &[String],
) -> String {
    let mut parts = Vec::new();
    if !label.trim().is_empty() {
        parts.push(label.trim().to_string());
    }
    if !comment.trim().is_empty() {
        parts.push(comment.trim().to_string());
    }
    for t in types {
        let short = short_name(t);
        if !short.is_empty() {
            parts.push(short);
        }
    }
    for r in relations {
        if !r.trim().is_empty() {
            parts.push(r.trim().to_string());
        }
    }
    parts.join(" ")
}

/// Local name: after last `/`, `#` or `:`.
pub fn short_name(uri: &str) -> String {
    uri.rsplit(['/', '#', ':'])
        .next()
        .unwrap_or(uri)
        .to_string()
}

/// Lowercase alphanumeric tokens.
pub fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase())
        .collect()
}

/// Canonicalize known synonyms so the stub vector encoder captures
/// semantic matches that lexical BM25 misses (e.g. physician↔doctor).
/// Production `Needle::embed()` does this via model semantics; the stub
/// needs the explicit map for the hybrid-recall fixture.
pub fn canonical_token(tok: &str) -> &str {
    match tok {
        "physician" | "doctor" | "dr" | "medic" => "doctor",
        "automobile" | "car" | "vehicle" => "car",
        "corp" | "corporation" | "company" | "inc" | "incorporated" => "company",
        "fruit" | "apple" | "apples" => tok, // keep fruit/apple distinct; handled by types
        "café" | "cafe" => "cafe",
        _ => tok,
    }
}

pub fn l2_normalize(v: &mut [f32]) {
    let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 1e-9 {
        for x in v.iter_mut() {
            *x /= n;
        }
    }
}

pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    // Inputs are L2-normalized → cosine = dot product.
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

/// Pluggable embedder (prod: Needle 3072-d; tests: stub).
pub trait Embedder: Send + Sync {
    fn embed(&self, text: &str) -> Vec<f32>;
    fn dim(&self) -> usize;
}

/// Deterministic hashing embedder: token-hash histogram + L2 norm.
/// No model weights; seeded only by text. Handles ascii-folding
/// (`café`→`cafe`) and the synonym map above.
#[derive(Debug, Clone)]
pub struct HashEmbedder {
    dim: usize,
}

impl HashEmbedder {
    pub fn new(dim: usize) -> Self {
        Self { dim: dim.max(8) }
    }
    pub fn default_dim() -> Self {
        Self::new(128)
    }
}

impl Embedder for HashEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }
    fn embed(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0f32; self.dim];
        for tok in tokenize(text) {
            let folded = tok
                .chars()
                .map(|c| match c {
                    'é' | 'è' | 'ê' | 'ë' => 'e',
                    'á' | 'à' | 'â' | 'ä' => 'a',
                    'í' | 'ì' | 'î' | 'ï' => 'i',
                    'ó' | 'ò' | 'ô' | 'ö' => 'o',
                    'ú' | 'ù' | 'û' | 'ü' => 'u',
                    'ç' => 'c',
                    'ñ' => 'n',
                    _ => c,
                })
                .collect::<String>();
            let canon = canonical_token(&folded);
            // FNV-1a hash → bucket.
            let mut h: u64 = 0xcbf29ce484222325;
            for b in canon.bytes() {
                h ^= b as u64;
                h = h.wrapping_mul(0x100000001b3);
            }
            // Spread unigrams + bigrams: also hash token length salt.
            let bucket = (h % self.dim as u64) as usize;
            v[bucket] += 1.0;
            let bucket2 = ((h >> 16) % self.dim as u64) as usize;
            v[bucket2] += 0.5;
        }
        l2_normalize(&mut v);
        v
    }
}

/// Cache keyed by text hash (FNV-1a hex) to avoid re-embedding.
#[derive(Debug, Default)]
pub struct EmbeddingCache {
    inner: Mutex<HashMap<String, Vec<f32>>>,
}

impl EmbeddingCache {
    pub fn key(text: &str) -> String {
        let mut h: u64 = 0xcbf29ce484222325;
        for b in text.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        format!("{h:016x}")
    }

    pub fn get_or_embed(&self, embedder: &dyn Embedder, text: &str) -> Vec<f32> {
        let k = Self::key(text);
        if let Some(v) = self.inner.lock().unwrap().get(&k) {
            return v.clone();
        }
        let v = embedder.embed(text);
        self.inner.lock().unwrap().insert(k, v.clone());
        v
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_is_deterministic_and_normalized() {
        let e = HashEmbedder::default_dim();
        let a = e.embed("Hello Doctor");
        let b = e.embed("Hello Doctor");
        assert_eq!(a, b);
        let n: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((n - 1.0).abs() < 1e-5);
    }

    #[test]
    fn synonyms_map_together() {
        let e = HashEmbedder::default_dim();
        let sim_same = cosine_similarity(&e.embed("physician"), &e.embed("doctor"));
        let sim_diff = cosine_similarity(&e.embed("physician"), &e.embed("banana"));
        assert!(sim_same > sim_diff, "{sim_same} vs {sim_diff}");
    }
}
