//! Application configuration and hard limits (Phase 1).
//!
//! These caps are enforced at the repository layer so later phases
//! (bounded graph traversal, 8-iteration agent loop) inherit them.

use serde::{Deserialize, Serialize};

/// Hard limits for the whole application. Phase 1 enforces the
/// read-path caps inside the RDF repository; the agent-loop caps
/// (`max_iterations`, `max_ops_per_request`) are defined here so
/// Phase 4 can rely on them without changing this file's shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppLimits {
    /// Maximum Needle tool iterations per incoming request (DESIGN §6).
    pub max_iterations: usize,
    /// Maximum repository operations per incoming request.
    pub max_ops_per_request: usize,
    /// Maximum triples returned by a single repository read.
    pub max_triples_per_read: usize,
    /// Maximum resources returned by a single read call.
    pub max_resources_per_read: usize,
    /// Maximum traversal paths followed in one read call.
    pub max_traversal_paths: usize,
    /// Default traversal depth for resource reads (0 = direct props only).
    pub default_read_depth: usize,
    /// Default page size for paginated reads.
    pub default_page_limit: usize,
}

impl Default for AppLimits {
    fn default() -> Self {
        Self {
            max_iterations: 8,
            max_ops_per_request: 32,
            max_triples_per_read: 500,
            max_resources_per_read: 50,
            max_traversal_paths: 200,
            default_read_depth: 1,
            default_page_limit: 50,
        }
    }
}

impl AppLimits {
    /// Effective limit for a read: caller-supplied `requested` clamped to
    /// `[1, max_triples_per_read]`. `None` yields the default page limit.
    pub fn effective_read_limit(&self, requested: Option<usize>) -> usize {
        let r = requested.unwrap_or(self.default_page_limit);
        r.max(1).min(self.max_triples_per_read)
    }

    /// Load overrides from environment variables (`NEEDLE_*`), falling back
    /// to defaults. Unknown / unparsable values are ignored.
    pub fn from_env() -> Self {
        let mut limits = Self::default();
        let get = |k: &str| std::env::var(k).ok()?.parse::<usize>().ok();
        if let Some(v) = get("NEEDLE_MAX_ITERATIONS") {
            limits.max_iterations = v.max(1);
        }
        if let Some(v) = get("NEEDLE_MAX_OPS") {
            limits.max_ops_per_request = v.max(1);
        }
        if let Some(v) = get("NEEDLE_MAX_TRIPLES_PER_READ") {
            limits.max_triples_per_read = v.max(1);
        }
        if let Some(v) = get("NEEDLE_MAX_RESOURCES_PER_READ") {
            limits.max_resources_per_read = v.max(1);
        }
        if let Some(v) = get("NEEDLE_MAX_TRAVERSAL_PATHS") {
            limits.max_traversal_paths = v.max(1);
        }
        limits
    }
}

/// Top-level application configuration (Phase 1 subset).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// Named-graph used when callers do not specify one.
    pub default_graph: String,
    pub limits: AppLimits,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            default_graph: "urn:needle:default".to_string(),
            limits: AppLimits::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_design_values() {
        let l = AppLimits::default();
        assert_eq!(l.max_iterations, 8);
        assert!(l.max_triples_per_read >= l.default_page_limit);
    }

    #[test]
    fn effective_limit_clamps() {
        let l = AppLimits::default();
        assert_eq!(l.effective_read_limit(None), l.default_page_limit);
        assert_eq!(l.effective_read_limit(Some(0)), 1);
        assert_eq!(
            l.effective_read_limit(Some(usize::MAX)),
            l.max_triples_per_read
        );
    }
}
