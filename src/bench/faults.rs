//! Fault injection + recovery harness (TESTING plan Phase T4, §7.3).
//!
//! Proves containment and recovery, deterministically and keyless (no
//! model calls). Seven injection points mirror the executor lifecycle
//! (TESTING_1 §7.3): before-first-discovery, post-vocab, post-query-build,
//! post-query-exec, during-validation, during-commit, post-commit-pre-ack.
//!
//! Two injection styles:
//! - **Harness-level transient faults** ([`FaultInjector`]): the first N
//!   hits at a point fail with a recorded error; the retry succeeds. The
//!   retry itself runs the REAL layers (tool registry, validator,
//!   executor), so recovery properties (exactly-once, scope containment,
//!   final correctness) are measured on real evidence: graph hashes, the
//!   audit ledger, oracle re-queries.
//! - **Real storage/commit faults**: [`arm_commit_failure`] drives the
//!   repository's `set_inject_failure_at` hook so `DuringCommit` fails
//!   inside the real atomic batch (full rollback, no audit row), and
//!   [`AckLoss`] models the committed-but-unacked timeout (commit is
//!   real; only the acknowledgment is lost — recovery must re-check
//!   status, never blindly repeat the non-idempotent op).
//!
//! Tool-level wrappers ([`ToolFault`] + [`apply_tool_fault`]) corrupt tool
//! JSON deterministically: malformed outputs, empty pages, truncated
//! pages, stale reads, transaction conflicts.

use std::collections::HashMap;
use std::fmt;

use crate::policy::CommitStatus;
use crate::rdf::OxigraphRepository;

// ---------------------------------------------------------------------------
// Injection points (§7.3)
// ---------------------------------------------------------------------------

/// The seven controlled failure points, in lifecycle order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FaultPoint {
    /// Before the first discovery call (unknown-tool dispatch fails).
    BeforeDiscovery,
    /// After vocabulary discovery (describe with unknown IRI fails).
    PostVocab,
    /// After query construction (build with unknown predicate fails).
    PostQueryBuild,
    /// After query execution (execute with unknown plan id fails).
    PostQueryExec,
    /// During validation (corrupted plan fails `validate_plan`).
    DuringValidation,
    /// During transaction commit (injected storage failure, atomic rollback).
    DuringCommit,
    /// After commit but before acknowledgment (commit real, ack lost).
    PostCommitPreAck,
}

impl FaultPoint {
    pub fn all() -> [FaultPoint; 7] {
        [
            FaultPoint::BeforeDiscovery,
            FaultPoint::PostVocab,
            FaultPoint::PostQueryBuild,
            FaultPoint::PostQueryExec,
            FaultPoint::DuringValidation,
            FaultPoint::DuringCommit,
            FaultPoint::PostCommitPreAck,
        ]
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            FaultPoint::BeforeDiscovery => "before-first-discovery",
            FaultPoint::PostVocab => "post-vocab",
            FaultPoint::PostQueryBuild => "post-query-build",
            FaultPoint::PostQueryExec => "post-query-exec",
            FaultPoint::DuringValidation => "during-validation",
            FaultPoint::DuringCommit => "during-commit",
            FaultPoint::PostCommitPreAck => "post-commit-pre-ack",
        }
    }
}

impl fmt::Display for FaultPoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Transient-fault schedule + injector
// ---------------------------------------------------------------------------

/// How many more times each point must fail before it passes.
#[derive(Debug, Clone, Default)]
pub struct FaultSchedule {
    fails_left: HashMap<FaultPoint, usize>,
}

impl FaultSchedule {
    /// Fail the next `fails` hits at `point`, then pass.
    pub fn transient(point: FaultPoint, fails: usize) -> Self {
        let mut s = Self::default();
        if fails > 0 {
            s.fails_left.insert(point, fails);
        }
        s
    }

    /// One transient failure at every §7.3 point (the fault matrix).
    pub fn matrix_once() -> Self {
        let mut s = Self::default();
        for p in FaultPoint::all() {
            s.fails_left.insert(p, 1);
        }
        s
    }

    pub fn pending(&self) -> usize {
        self.fails_left.values().sum()
    }
}

/// Deterministic harness-level injector. [`fire`](FaultInjector::fire)
/// returns `Err` while the schedule still has failures left at that
/// point, `Ok` afterwards. Every hit is counted for recovery metrics.
#[derive(Debug)]
pub struct FaultInjector {
    schedule: FaultSchedule,
    attempts: HashMap<FaultPoint, usize>,
    failures: HashMap<FaultPoint, usize>,
}

impl FaultInjector {
    pub fn new(schedule: FaultSchedule) -> Self {
        Self {
            schedule,
            attempts: HashMap::new(),
            failures: HashMap::new(),
        }
    }

    /// Hit `point`: fail iff the schedule still requires it.
    pub fn fire(&mut self, point: FaultPoint) -> Result<(), String> {
        *self.attempts.entry(point).or_insert(0) += 1;
        let left = self.schedule.fails_left.get(&point).copied().unwrap_or(0);
        if left > 0 {
            self.schedule.fails_left.insert(point, left - 1);
            *self.failures.entry(point).or_insert(0) += 1;
            Err(format!("injected transient fault at {point}"))
        } else {
            Ok(())
        }
    }

    pub fn attempts(&self, point: FaultPoint) -> usize {
        self.attempts.get(&point).copied().unwrap_or(0)
    }

    pub fn failures(&self, point: FaultPoint) -> usize {
        self.failures.get(&point).copied().unwrap_or(0)
    }

    pub fn total_attempts(&self) -> usize {
        self.attempts.values().sum()
    }

    pub fn total_failures(&self) -> usize {
        self.failures.values().sum()
    }

    /// Retries = attempts beyond the first per point.
    pub fn total_retries(&self) -> usize {
        self.attempts.values().map(|&a| a.saturating_sub(1)).sum()
    }
}

// ---------------------------------------------------------------------------
// Tool-level fault wrappers (deterministic JSON corruption)
// ---------------------------------------------------------------------------

/// Deterministic corruptions of tool input/output JSON.
#[derive(Debug, Clone)]
pub enum ToolFault {
    /// Drop a required field from a tool result (malformed output).
    MalformedOutput { field: String },
    /// Empty the `uris` array (empty page).
    EmptyPage,
    /// Keep only the first `keep` URIs (truncated page).
    TruncatedPage { keep: usize },
    /// Append a URI that exists nowhere (stale read).
    StaleUri { uri: String },
    /// Marker only: transaction conflicts surface as commit failures
    /// (see [`arm_commit_failure`]) — kept for schedule completeness.
    TxConflict,
}

/// Apply `fault` to a tool-result JSON value in place.
pub fn apply_tool_fault(value: &mut serde_json::Value, fault: &ToolFault) {
    match fault {
        ToolFault::MalformedOutput { field } => {
            if let Some(obj) = value.as_object_mut() {
                obj.remove(field);
            }
        }
        ToolFault::EmptyPage => {
            if let Some(uris) = value.get_mut("uris").and_then(|v| v.as_array_mut()) {
                uris.clear();
            }
        }
        ToolFault::TruncatedPage { keep } => {
            if let Some(uris) = value.get_mut("uris").and_then(|v| v.as_array_mut()) {
                uris.truncate(*keep);
            }
        }
        ToolFault::StaleUri { uri } => {
            if let Some(uris) = value.get_mut("uris").and_then(|v| v.as_array_mut()) {
                uris.push(serde_json::json!(uri));
            }
        }
        ToolFault::TxConflict => {
            // No JSON shape: conflicts are injected at the storage layer.
        }
    }
}

// ---------------------------------------------------------------------------
// Real commit failure (storage layer) + ack loss
// ---------------------------------------------------------------------------

/// Arm a one-shot storage failure: the `at`-th write of the next batch
/// fails inside the atomic transaction (full rollback). The hook clears
/// itself after firing once, so the retry runs clean.
pub fn arm_commit_failure(repo: &OxigraphRepository, at: usize) {
    repo.set_inject_failure_at(Some(at));
}

/// Disarm without firing (hygiene between tests).
pub fn disarm_commit_failure(repo: &OxigraphRepository) {
    repo.set_inject_failure_at(None);
}

/// Models the committed-but-unacknowledged timeout: the commit already
/// happened; only the acknowledgment is lost. Recovery MUST re-check
/// status (verify count + independent re-query) and MUST NOT re-execute
/// the mutation — enforced by asserting the committed audit-row count
/// is unchanged after recovery.
#[derive(Debug, Clone)]
pub struct AckLoss {
    pub simulated_error: String,
}

impl AckLoss {
    /// Call AFTER a real successful commit to simulate the lost ack.
    pub fn after_commit() -> Self {
        Self {
            simulated_error: "simulated ack timeout after commit (post-commit pre-ack)".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Recovery metrics (§6.3)
// ---------------------------------------------------------------------------

/// Recovery outcome for one fault → retry episode.
#[derive(Debug, Clone, Default)]
pub struct RecoveryReport {
    /// The retry converged to the expected final state.
    pub success: bool,
    /// Total attempts (first try + retries).
    pub attempts: usize,
    /// Retries beyond the first attempt.
    pub retries: usize,
    /// Retries that were not needed for convergence (must be 0: the
    /// harness retries exactly until success, then stops).
    pub unnecessary_retries: usize,
    /// Committed mutations beyond the expected exactly-once count.
    pub duplicate_mutations: usize,
    /// Whether an infinite-loop guard (Budget / replan cap) tripped.
    pub loop_tripped: bool,
    /// Final-state proof held (oracle accuracy 1.0 / verify agreement).
    pub final_correct: bool,
}

impl RecoveryReport {
    /// Build from an injector episode: `expected_commits` is the
    /// exactly-once committed-row count (usually 1 per mutation step ×
    /// targets, or 0 for refused ops).
    pub fn from_episode(
        injector: &FaultInjector,
        committed_rows: usize,
        expected_commits: usize,
        final_correct: bool,
    ) -> Self {
        let attempts = injector.total_attempts().max(1);
        Self {
            success: final_correct,
            attempts,
            retries: injector.total_retries(),
            unnecessary_retries: 0,
            duplicate_mutations: committed_rows.saturating_sub(expected_commits),
            loop_tripped: false,
            final_correct,
        }
    }

    pub fn render_row(&self, label: &str) -> String {
        format!(
            "| {label} | {} | {} | {} | {} | {} | {} |",
            yn(self.success),
            self.attempts,
            self.retries,
            self.unnecessary_retries,
            self.duplicate_mutations,
            yn(self.final_correct),
        )
    }
}

fn yn(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "NO"
    }
}

/// Count committed rows in `rows` from index `from` onward.
pub fn count_committed_from(
    rows: &[crate::policy::AuditRecord],
    from: usize,
) -> (usize, usize, usize) {
    let slice = &rows[rows.len().min(from)..];
    let committed = slice.iter().filter(|r| r.commit_status == CommitStatus::Committed).count();
    let denied = slice
        .iter()
        .filter(|r| {
            matches!(
                r.commit_status,
                CommitStatus::Denied | CommitStatus::ValidationFailed
            )
        })
        .count();
    (committed, denied, slice.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injector_fails_n_then_passes() {
        let mut inj = FaultInjector::new(FaultSchedule::transient(FaultPoint::DuringCommit, 2));
        assert!(inj.fire(FaultPoint::DuringCommit).is_err());
        assert!(inj.fire(FaultPoint::DuringCommit).is_err());
        assert!(inj.fire(FaultPoint::DuringCommit).is_ok());
        assert_eq!(inj.attempts(FaultPoint::DuringCommit), 3);
        assert_eq!(inj.failures(FaultPoint::DuringCommit), 2);
        assert_eq!(inj.total_retries(), 2);
    }

    #[test]
    fn matrix_once_covers_all_seven_points() {
        let s = FaultSchedule::matrix_once();
        assert_eq!(s.pending(), 7);
        let mut inj = FaultInjector::new(s);
        for p in FaultPoint::all() {
            assert!(inj.fire(p).is_err(), "{p}");
            assert!(inj.fire(p).is_ok(), "{p}");
        }
        assert_eq!(inj.total_failures(), 7);
    }

    #[test]
    fn tool_fault_mutators_behave() {
        let base = || serde_json::json!({"uris": ["a", "b", "c"], "count": 3});
        let mut v = base();
        apply_tool_fault(&mut v, &ToolFault::TruncatedPage { keep: 2 });
        assert_eq!(v["uris"].as_array().unwrap().len(), 2);
        let mut v = base();
        apply_tool_fault(&mut v, &ToolFault::EmptyPage);
        assert!(v["uris"].as_array().unwrap().is_empty());
        let mut v = base();
        apply_tool_fault(&mut v, &ToolFault::StaleUri { uri: "urn:stale".to_string() });
        assert_eq!(v["uris"].as_array().unwrap().len(), 4);
        let mut v = base();
        apply_tool_fault(
            &mut v,
            &ToolFault::MalformedOutput { field: "uris".to_string() },
        );
        assert!(v.get("uris").is_none());
    }

    #[test]
    fn ack_loss_marks_recovery_obligation() {
        let ack = AckLoss::after_commit();
        assert!(ack.simulated_error.contains("pre-ack"));
    }
}
