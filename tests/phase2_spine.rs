//! Phase 2 coverage report: which bench requests the spine classifier
//! covers, which fall back loudly. Keyless and deterministic —
//! classification involves no model.
//!
//! The value is the printed matched/unmatched table plus the pinned
//! ladder mapping; counts are reported, not thresholded, so the
//! `unmatched-request` rate is measured honestly as the classifier grows.

use needle_rdf_agent::ladder_spec::ladder;
use needle_rdf_agent::planner::spine::spine_for_request;
use std::collections::BTreeMap;

fn bench_requests() -> Vec<(String, String)> {
    let mut out = vec![];
    let entries = std::fs::read_dir("tests/bench_cases").expect("bench cases dir");
    let mut paths: Vec<_> = entries.map(|e| e.unwrap().path()).collect();
    paths.sort();
    for path in paths {
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let raw = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        out.push((
            v.get("id").and_then(|i| i.as_str()).unwrap_or("?").to_string(),
            v.get("request").and_then(|r| r.as_str()).unwrap_or("").to_string(),
        ));
    }
    out
}

#[test]
fn classifier_covers_ladder_exactly_and_reports_bench() {
    // Ladder rungs pin exact spine levels.
    for rung in ladder() {
        let spine = spine_for_request(rung.request)
            .unwrap_or_else(|| panic!("ladder L{} must match", rung.level));
        assert_eq!(spine.level, rung.level, "ladder L{}", rung.level);
    }
    // Bench corpus: report, don't threshold.
    let cases = bench_requests();
    assert_eq!(cases.len(), 100, "bench Set 1 size");
    let mut matched: BTreeMap<&str, usize> = BTreeMap::new();
    let mut unmatched = vec![];
    for (id, request) in &cases {
        match spine_for_request(request) {
            Some(spine) => {
                *matched.entry(spine.name).or_default() += 1;
            }
            None => unmatched.push(id.clone()),
        }
    }
    let total_matched: usize = matched.values().sum();
    println!("SPINE COVERAGE: {total_matched}/{} matched", cases.len());
    for (name, n) in &matched {
        println!("  spine {name}: {n}");
    }
    println!("  unmatched ({}): {}", unmatched.len(), unmatched.join(", "));
}
