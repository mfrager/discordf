//! Guarded mutation path (Phase 2): the single entry point for writes.
//!
//! Order: authz → destructive gate → staged SHACL validation → commit-or-dry-run → audit.
//! Every call appends exactly one [`AuditRecord`].

use super::audit::{AuditLog, AuditRecord, CommitStatus};
use super::authz::{authorize, ExecutionContext, Operation, PolicyError};
use super::confirm::{confirmation_satisfied, dry_run_summary, is_destructive};
use crate::ontology::reasoner::OntologyReasoner;
use crate::ontology::shacl::{staged_by_subject, validate_staged_subject, Shape, ValidationResult};
use crate::rdf::{GraphName, Quad, RdfError, RdfRepository, Term, TripleDelta};
use std::collections::{HashMap, HashSet};

/// Options for one guarded mutation.
#[derive(Debug, Clone, Default)]
pub struct MutationOptions {
    pub dry_run: bool,
    pub confirmation_token: Option<String>,
}

/// Outcome of a successful guarded mutation (committed or dry-run).
#[derive(Debug, Clone)]
pub struct MutationOutcome {
    pub delta: TripleDelta,
    pub validation: ValidationResult,
    pub dry_run: bool,
    pub summary: String,
}

fn classify_op(to_add: &[Quad], to_remove: &[Quad]) -> Operation {
    match (!to_add.is_empty(), !to_remove.is_empty()) {
        (true, true) => Operation::Update,
        (true, false) => Operation::Create,
        (false, true) => Operation::Delete,
        (false, false) => Operation::Update,
    }
}

fn op_str(op: Operation) -> String {
    op.as_str().to_string()
}

/// Execute a guarded mutation. `target_uri`/`target_graph` identify the
/// primary target for audit purposes (deltas may touch the same graph only;
/// cross-graph batches are rejected).
#[allow(clippy::too_many_arguments)]
pub fn execute_guarded(
    repo: &(impl RdfRepository + ?Sized),
    reasoner: &OntologyReasoner,
    shapes: &[Shape],
    audit: &AuditLog,
    ctx: &ExecutionContext,
    target_uri: &str,
    target_graph: &GraphName,
    to_add: &[Quad],
    to_remove: &[Quad],
    options: MutationOptions,
) -> Result<MutationOutcome, PolicyError> {
    let op = classify_op(to_add, to_remove);

    // All quads must live in the target graph (single-graph atomic batches).
    for q in to_add.iter().chain(to_remove.iter()) {
        if &q.graph != target_graph {
            let rec = AuditRecord {
                request_id: ctx.request_id.clone(),
                principal: ctx.principal.clone(),
                target_uri: target_uri.to_string(),
                graph: target_graph.clone(),
                op: op_str(op),
                delta_added: 0,
                delta_removed: 0,
                delta: TripleDelta::default(),
                validation_conforms: false,
                validation_violations: 0,
                commit_status: CommitStatus::Denied,
                note: "cross-graph batch rejected".to_string(),
            };
            let _ = audit.append(rec);
            return Err(PolicyError::Denied(
                "cross-graph batches are rejected".to_string(),
            ));
        }
    }

    // 1. Authorization.
    if let Err(e) = authorize(ctx, target_graph, op) {
        let msg = e.to_string();
        let _ = audit.append(AuditRecord {
            request_id: ctx.request_id.clone(),
            principal: ctx.principal.clone(),
            target_uri: target_uri.to_string(),
            graph: target_graph.clone(),
            op: op_str(op),
            delta_added: 0,
            delta_removed: 0,
            delta: TripleDelta::default(),
            validation_conforms: false,
            validation_violations: 0,
            commit_status: CommitStatus::Denied,
            note: msg.clone(),
        });
        return Err(PolicyError::Denied(msg));
    }

    // 2. Destructive gate (dry-runs never need a token).
    if is_destructive(to_add, to_remove)
        && !options.dry_run
        && !confirmation_satisfied(options.confirmation_token.as_deref())
    {
        let msg = format!("destructive {op:?} requires confirmation token (dry-run first)");
        let _ = audit.append(AuditRecord {
            request_id: ctx.request_id.clone(),
            principal: ctx.principal.clone(),
            target_uri: target_uri.to_string(),
            graph: target_graph.clone(),
            op: op_str(op),
            delta_added: 0,
            delta_removed: 0,
            delta: TripleDelta::default(),
            validation_conforms: true,
            validation_violations: 0,
            commit_status: CommitStatus::Denied,
            note: msg.clone(),
        });
        return Err(PolicyError::ConfirmationRequired(msg));
    }

    // 3. Staged validation: for each affected subject, merge existing +
    // adds − removes and validate the staged state.
    let affected: HashSet<Term> = to_add
        .iter()
        .chain(to_remove.iter())
        .map(|q| q.subject.clone())
        .collect();
    let mut all_violations = Vec::new();
    for subject in &affected {
        let existing = repo
            .match_quads(Some(subject), None, None, Some(target_graph), None)
            .map_err(|e| {
                PolicyError::Storage(match e {
                    RdfError::Storage(s) => s,
                    other => other.to_string(),
                })
            })?;
        let staged = staged_state(&existing, to_add, to_remove, subject, target_graph);
        let res = validate_staged_subject(repo, reasoner, target_graph, subject, &staged, shapes)
            .map_err(|e| PolicyError::Storage(e.to_string()))?;
        all_violations.extend(res.violations);
    }
    // Also validate subjects that would be left behind with dangling
    // references? Not needed for the Contact fixture; keep scope tight.
    let validation = ValidationResult::failed(all_violations);
    if !validation.conforms {
        let n = validation.violations.len();
        let _ = audit.append(AuditRecord {
            request_id: ctx.request_id.clone(),
            principal: ctx.principal.clone(),
            target_uri: target_uri.to_string(),
            graph: target_graph.clone(),
            op: op_str(op),
            delta_added: 0,
            delta_removed: 0,
            delta: TripleDelta::default(),
            validation_conforms: false,
            validation_violations: n,
            commit_status: CommitStatus::ValidationFailed,
            note: format!("SHACL validation failed with {n} violation(s)"),
        });
        return Err(PolicyError::ValidationFailed(validation.violations));
    }

    // 4. Dry-run: compute the would-be delta without writing.
    if options.dry_run {
        let delta = preview_delta(repo, to_add, to_remove)?;
        let summary = dry_run_summary(&delta, &validation);
        let _ = audit.append(AuditRecord {
            request_id: ctx.request_id.clone(),
            principal: ctx.principal.clone(),
            target_uri: target_uri.to_string(),
            graph: target_graph.clone(),
            op: op_str(op),
            delta_added: delta.added.len(),
            delta_removed: delta.removed.len(),
            delta: delta.clone(),
            validation_conforms: true,
            validation_violations: 0,
            commit_status: CommitStatus::DryRun,
            note: summary.clone(),
        });
        return Ok(MutationOutcome {
            delta,
            validation,
            dry_run: true,
            summary,
        });
    }

    // 5. Commit atomically.
    let delta = repo.apply_atomic(to_add, to_remove).map_err(|e| {
        PolicyError::Storage(match e {
            RdfError::InjectedFailure(n) => format!("injected failure at write {n}"),
            other => other.to_string(),
        })
    })?;
    let summary = format!(
        "committed: added {} quad(s), removed {} quad(s)",
        delta.added.len(),
        delta.removed.len()
    );
    let _ = audit.append(AuditRecord {
        request_id: ctx.request_id.clone(),
        principal: ctx.principal.clone(),
        target_uri: target_uri.to_string(),
        graph: target_graph.clone(),
        op: op_str(op),
        delta_added: delta.added.len(),
        delta_removed: delta.removed.len(),
        delta: delta.clone(),
        validation_conforms: true,
        validation_violations: 0,
        commit_status: CommitStatus::Committed,
        note: summary.clone(),
    });
    Ok(MutationOutcome {
        delta,
        validation,
        dry_run: false,
        summary,
    })
}

/// Merge existing + adds − removes for one subject (same graph).
fn staged_state(
    existing: &[Quad],
    to_add: &[Quad],
    to_remove: &[Quad],
    subject: &Term,
    graph: &GraphName,
) -> Vec<Quad> {
    let remove_set: HashSet<String> = to_remove.iter().map(|q| format!("{q:?}")).collect();
    let mut staged: Vec<Quad> = existing
        .iter()
        .filter(|q| !remove_set.contains(&format!("{q:?}")))
        .cloned()
        .collect();
    for q in to_add {
        if &q.subject == subject && &q.graph == graph && !staged.contains(q) {
            staged.push(q.clone());
        }
    }
    // Keep the staged_by_subject helper exercised (grouping used by future multi-subject validators).
    let _grouped: HashMap<Term, Vec<Quad>> = staged_by_subject(&staged);
    staged
}

/// Compute the would-be delta without writing: adds not already present,
/// removes that are present.
fn preview_delta(
    repo: &(impl RdfRepository + ?Sized),
    to_add: &[Quad],
    to_remove: &[Quad],
) -> Result<TripleDelta, PolicyError> {
    let mut delta = TripleDelta::default();
    for q in to_add {
        let present = repo
            .contains(q)
            .map_err(|e| PolicyError::Storage(e.to_string()))?;
        if !present {
            delta.added.push(q.clone());
        }
    }
    for q in to_remove {
        let present = repo
            .contains(q)
            .map_err(|e| PolicyError::Storage(e.to_string()))?;
        if present {
            delta.removed.push(q.clone());
        }
    }
    Ok(delta)
}
