//! Ranking (Phase 3): lexical BM25 + vector cosine hybrid fusion.

use serde::{Deserialize, Serialize};

/// Where a candidate came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Provenance {
    Lexical,
    Vector,
    Hybrid,
}

/// Compact search hit — only allowed fields, never triples.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub uri: String,
    pub label: String,
    #[serde(rename = "type")]
    pub type_: String,
    pub description: String,
    pub score: f32,
    pub provenance: Provenance,
}

/// Which encoder(s) to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchMode {
    Lexical,
    Vector,
    Hybrid,
}

/// Fuse lexical (BM25) + vector (cosine) scores.
///
/// Both inputs are `uri → score`. Scores are min-max normalized to [0,1]
/// then combined as `alpha * lexical + (1 - alpha) * vector`.
/// Items present in both inputs are marked [`Provenance::Hybrid`].
pub fn rank_hybrid(
    lexical: &[(String, f32)],
    vector: &[(String, f32)],
    alpha: f32,
    limit: usize,
) -> Vec<(String, f32, Provenance)> {
    use std::collections::{HashMap, HashSet};
    let alpha = alpha.clamp(0.0, 1.0);
    let normalize = |items: &[(String, f32)]| -> HashMap<String, f32> {
        if items.is_empty() {
            return HashMap::new();
        }
        let min = items.iter().map(|(_, s)| *s).fold(f32::INFINITY, f32::min);
        let max = items
            .iter()
            .map(|(_, s)| *s)
            .fold(f32::NEG_INFINITY, f32::max);
        let span = (max - min).max(1e-9);
        items
            .iter()
            .map(|(u, s)| (u.clone(), (s - min) / span))
            .collect()
    };
    let ln = normalize(lexical);
    let vn = normalize(vector);
    let mut uris: HashSet<String> = HashSet::new();
    for (u, _) in lexical.iter().chain(vector.iter()) {
        uris.insert(u.clone());
    }
    let mut out: Vec<(String, f32, Provenance)> = uris
        .into_iter()
        .map(|u| {
            let l = ln.get(&u).copied().unwrap_or(0.0);
            let v = vn.get(&u).copied().unwrap_or(0.0);
            let prov = match (ln.contains_key(&u), vn.contains_key(&u)) {
                (true, true) => Provenance::Hybrid,
                (true, false) => Provenance::Lexical,
                (false, true) => Provenance::Vector,
                (false, false) => Provenance::Lexical,
            };
            (u, alpha * l + (1.0 - alpha) * v, prov)
        })
        .collect();
    out.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    out.truncate(limit.max(1));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fusion_is_deterministic_and_prefers_both() {
        let lex = vec![("a".to_string(), 1.0), ("b".to_string(), 2.0)];
        let vec_ = vec![("b".to_string(), 0.9), ("c".to_string(), 0.1)];
        let r1 = rank_hybrid(&lex, &vec_, 0.5, 10);
        let r2 = rank_hybrid(&lex, &vec_, 0.5, 10);
        assert_eq!(r1, r2);
        // `b` in both → Hybrid and ranked first (max in both normalized).
        assert_eq!(r1[0].0, "b");
        assert_eq!(r1[0].2, Provenance::Hybrid);
    }
}
