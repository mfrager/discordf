//! Mutation safety + authorization (Phase 2).
//!
//! - [`ExecutionContext`] travels with every request (principal, tenant,
//!   allowed graphs, permitted ops).
//! - [`authz::authorize`] is deny-by-default.
//! - [`confirm`] gates destructive ops behind an explicit token and renders
//!   dry-run summaries.
//! - [`audit`] appends exactly one record per mutation attempt.
//! - [`gate::execute_guarded`] is the single mutation path that enforces
//!   authz → destructive gate → SHACL validation → commit-or-dry-run → audit.

pub mod audit;
pub mod authz;
pub mod confirm;
pub mod gate;

pub use audit::{AuditLog, AuditRecord, CommitStatus};
pub use authz::{ExecutionContext, Operation, PolicyError};
pub use confirm::{dry_run_summary, is_destructive, REQUIRED_CONFIRMATION_PREFIX};
pub use gate::{execute_guarded, MutationOptions, MutationOutcome};
