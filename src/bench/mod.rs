//! Benchmark harness foundation (TESTING plan Phase T1).
//!
//! Deterministic ground truth + independent scoring for the A/B/C/D
//! comparative benchmark. No model calls here: dataset builders, the
//! machine-readable case format, oracles (plan constraints, URI-set
//! precision/recall, graph-diff mutation accuracy), trace events, and
//! metrics/reporting. Planners run against this harness in T2–T5.

pub mod case;
pub mod dataset;
pub mod faults;
pub mod generator;
pub mod metrics;
pub mod oracle;
pub mod runner;
pub mod scale;
pub mod trace;

pub use case::{fixed_cases, load_cases_dir, validate_case, BenchmarkCase, Category, Difficulty};
pub use dataset::{BenchGraphs, Scale, Snapshot};
pub use metrics::{render_report_table, Aggregate, RunMetrics};
pub use oracle::CaseScore;
pub use trace::{Trace, TraceEvent};
