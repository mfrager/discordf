//! Confirmation gate + dry-run summaries (Phase 2).

use crate::ontology::shacl::ValidationResult;
use crate::rdf::{Quad, TripleDelta};

/// Destructive operations require an explicit confirmation token.
/// Any token starting with this prefix counts as confirmation;
/// `None` means "no confirmation supplied".
pub const REQUIRED_CONFIRMATION_PREFIX: &str = "confirm-";

/// Destructive = removes at least one quad (resource delete, predicate wipe,
/// graph clear, or any update that removes). Adds alone are not destructive.
pub fn is_destructive(to_add: &[Quad], to_remove: &[Quad]) -> bool {
    let _ = to_add;
    !to_remove.is_empty()
}

/// True when the supplied token satisfies the destructive gate.
pub fn confirmation_satisfied(token: Option<&str>) -> bool {
    matches!(token, Some(t) if t.starts_with(REQUIRED_CONFIRMATION_PREFIX))
}

/// Human-readable dry-run summary (no triples committed).
pub fn dry_run_summary(delta: &TripleDelta, validation: &ValidationResult) -> String {
    format!(
        "dry-run: would add {} quad(s), remove {} quad(s); validation {} ({} violation(s))",
        delta.added.len(),
        delta.removed.len(),
        if validation.conforms {
            "passed"
        } else {
            "FAILED"
        },
        validation.violations.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rdf::{GraphName, Term};

    fn q() -> Quad {
        Quad::new(
            Term::iri("http://example.com/A").unwrap(),
            Term::iri("http://example.com/p").unwrap(),
            Term::literal("v"),
            GraphName::tenant("acme").unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn adds_only_are_not_destructive() {
        assert!(!is_destructive(&[q()], &[]));
    }

    #[test]
    fn any_remove_is_destructive_and_needs_token() {
        assert!(is_destructive(&[], &[q()]));
        assert!(is_destructive(&[q()], &[q()]));
        assert!(!confirmation_satisfied(None));
        assert!(!confirmation_satisfied(Some("nope")));
        assert!(confirmation_satisfied(Some("confirm-req-1")));
    }
}
