//! Local LLM runtime (Plan 2 Phase 1): `dotenvy` + keyless `agentstream` backend.
//!
//! All non-Needle LLM traffic goes through `agentstream`'s OpenAI-compatible
//! backend. Needle 3 stays on `cactus-rs` and never touches this module.
//!
//! Environment contract (loaded via `dotenvy::dotenv()` before any read):
//! - `OPENAI_BASE_URL` — e.g. `http://localhost:8080`. Normalized to end in
//!   `/v1` because `agentstream` posts to `{base}/chat/completions`.
//! - `OPENAI_MODEL` — model id sent with every non-Needle request. Single
//!   source of truth; blank/unset falls back to `"default-local"` + warn.
//! - `OPENAI_API_KEY` — optional for local endpoints. When missing/empty and
//!   the base URL is local (localhost / loopback / LAN / `*.local`), a
//!   placeholder bearer (`"local-no-key"`) is used so startup and CI work
//!   with no key configured. When missing/empty and the base URL is remote
//!   (e.g. `api.openai.com`), a typed `MissingKey` error is returned.
//! - `AGENTSTREAM_TIMEOUT` (or `OPENAI_TIMEOUT`) — request timeout in seconds,
//!   default 120.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Fallback model id when `OPENAI_MODEL` is unset/blank.
pub const DEFAULT_MODEL: &str = "default-local";
/// Fallback base URL when `OPENAI_BASE_URL` is unset/blank.
pub const DEFAULT_BASE_URL: &str = "http://localhost:8080/v1";
/// Placeholder bearer sent to local servers that ignore auth.
pub const PLACEHOLDER_KEY: &str = "local-no-key";
/// Default request timeout in seconds.
pub const DEFAULT_TIMEOUT_SECS: u64 = 120;

pub const ENV_BASE_URL: &str = "OPENAI_BASE_URL";
pub const ENV_MODEL: &str = "OPENAI_MODEL";
pub const ENV_API_KEY: &str = "OPENAI_API_KEY";
pub const ENV_TIMEOUT: &str = "AGENTSTREAM_TIMEOUT";
pub const ENV_TIMEOUT_ALT: &str = "OPENAI_TIMEOUT";

/// Whether the configured backend carries a real key or runs keyless-local.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum KeyMode {
    KeyPresent,
    #[default]
    AbsentLocal,
}

/// Resolved non-Needle LLM configuration. The API key is intentionally NOT
/// logged; the custom `Debug` impl redacts it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmConfig {
    /// Normalized base URL ending in `/v1` (no trailing slash beyond that).
    pub base_url: String,
    /// Model id for every non-Needle request (from `OPENAI_MODEL`).
    pub model: String,
    pub key_mode: KeyMode,
    /// Effective bearer token (real key or [`PLACEHOLDER_KEY`]).
    #[serde(skip_serializing)]
    pub api_key: String,
    pub timeout_secs: u64,
}

impl std::fmt::Debug for LlmConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmConfig")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("key_mode", &self.key_mode)
            .field("api_key", &"<redacted>")
            .field("timeout_secs", &self.timeout_secs)
            .finish()
    }
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            model: DEFAULT_MODEL.to_string(),
            key_mode: KeyMode::AbsentLocal,
            api_key: PLACEHOLDER_KEY.to_string(),
            timeout_secs: DEFAULT_TIMEOUT_SECS,
        }
    }
}

/// Typed failure for LLM configuration.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum LlmError {
    #[error(
        "OPENAI_API_KEY is not set and OPENAI_BASE_URL points at a remote host ({base_url}). For hosted OpenAI set OPENAI_API_KEY; for a local endpoint use a localhost/LAN base URL or set a key."
    )]
    MissingKey { base_url: String },
    #[error("invalid timeout value {raw:?}: must be a positive integer number of seconds")]
    BadTimeout { raw: String },
}

impl LlmConfig {
    /// Redacted one-line summary for startup logs.
    pub fn summary(&self) -> String {
        format!(
            "base_url={} model={} key_mode={:?} timeout_secs={}",
            self.base_url, self.model, self.key_mode, self.timeout_secs
        )
    }
}

/// Normalize a base URL: trim whitespace/trailing slashes; append `/v1`
/// when no `/v1` suffix is present (agentstream posts to
/// `{base}/chat/completions`).
pub fn normalize_base_url(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return DEFAULT_BASE_URL.to_string();
    }
    if trimmed.ends_with("/v1") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/v1")
    }
}

/// True when `url` targets a local/private endpoint where a missing API key
/// is acceptable: `localhost`, `127.*`, `::1`, `10.*`, `172.16-31.*`,
/// `192.168.*`, `*.local`, or unix-socket style values.
pub fn is_local_url(url: &str) -> bool {
    let lower = url.trim().to_lowercase();
    // Explicit unix-socket style endpoints are always local.
    if lower.starts_with("unix:") || lower.starts_with("unix://") {
        return true;
    }
    // Strip scheme, then take host[:port].
    let after_scheme = lower.split("://").last().unwrap_or(&lower);
    let host = after_scheme
        .split('/')
        .next()
        .unwrap_or("")
        .split('@')
        .last()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .trim_matches(|c| c == '[' || c == ']');
    if host == "localhost" || host == "127.0.0.1" || host == "::1" {
        return true;
    }
    if host.ends_with(".local") || host.ends_with(".localhost") {
        return true;
    }
    if host.starts_with("10.") || host.starts_with("192.168.") {
        return true;
    }
    // 172.16.0.0/12
    if let Some(rest) = host.strip_prefix("172.") {
        if let Some(second) = rest.split('.').next().and_then(|s| s.parse::<u8>().ok()) {
            if (16..=31).contains(&second) {
                return true;
            }
        }
    }
    // Bare loopback without scheme.
    if host.is_empty() && (lower.contains("localhost") || lower.contains("127.0.0.1")) {
        return true;
    }
    false
}

/// Resolve the model id: trimmed `OPENAI_MODEL`, or [`DEFAULT_MODEL`] fallback.
pub fn resolve_model(raw: Option<&str>) -> String {
    let model = raw.unwrap_or("").trim().to_string();
    if model.is_empty() {
        tracing::warn!(
            "OPENAI_MODEL is not set; falling back to {DEFAULT_MODEL:?}. Set OPENAI_MODEL in .env to the local model id."
        );
        DEFAULT_MODEL.to_string()
    } else {
        model
    }
}

fn resolve_timeout(raw: Option<&str>) -> Result<u64, LlmError> {
    match raw.map(str::trim) {
        None | Some("") => Ok(DEFAULT_TIMEOUT_SECS),
        Some(s) => {
            let parsed: u64 = s
                .parse()
                .map_err(|_| LlmError::BadTimeout { raw: s.to_string() })?;
            if parsed == 0 {
                return Err(LlmError::BadTimeout { raw: s.to_string() });
            }
            Ok(parsed)
        }
    }
}

/// Pure constructor used by [`load_llm_config`] and directly by tests.
///
/// `api_key`: `None`/blank means "no key configured".
pub fn config_from_vars(
    base_url: Option<&str>,
    model: Option<&str>,
    api_key: Option<&str>,
    timeout: Option<&str>,
) -> Result<LlmConfig, LlmError> {
    let base_url = match base_url.map(str::trim) {
        None | Some("") => DEFAULT_BASE_URL.to_string(),
        Some(raw) => normalize_base_url(raw),
    };
    let model = resolve_model(model);
    let timeout_secs = resolve_timeout(timeout)?;
    let key = api_key.map(str::trim).unwrap_or("").to_string();

    if !key.is_empty() {
        return Ok(LlmConfig {
            base_url,
            model,
            key_mode: KeyMode::KeyPresent,
            api_key: key,
            timeout_secs,
        });
    }
    if is_local_url(&base_url) {
        tracing::info!(key_mode = "absent-local", "OPENAI_API_KEY not set; using placeholder bearer for local endpoint");
        return Ok(LlmConfig {
            base_url,
            model,
            key_mode: KeyMode::AbsentLocal,
            api_key: PLACEHOLDER_KEY.to_string(),
            timeout_secs,
        });
    }
    Err(LlmError::MissingKey { base_url })
}

/// Load `.env` (if present) then resolve [`LlmConfig`] from the environment.
///
/// Never panics; missing key on a local endpoint is not an error.
pub fn load_llm_config() -> Result<LlmConfig, LlmError> {
    let _ = dotenvy::dotenv();
    config_from_vars(
        std::env::var(ENV_BASE_URL).ok().as_deref(),
        std::env::var(ENV_MODEL).ok().as_deref(),
        std::env::var(ENV_API_KEY).ok().as_deref(),
        std::env::var(ENV_TIMEOUT)
            .ok()
            .or_else(|| std::env::var(ENV_TIMEOUT_ALT).ok())
            .as_deref(),
    )
}

/// Build an `agentstream` OpenAI-compatible backend from a resolved config.
///
/// Uses `openai_explicit` (never `openai_from_env`, which requires a key).
pub fn build_backend(
    config: &LlmConfig,
) -> Arc<dyn agentstream::backends::LLMBackend> {
    agentstream::backends::BackendFactory::openai_explicit(
        config.api_key.clone(),
        Some(config.base_url.clone()),
        Some(config.model.clone()),
        config.timeout_secs,
    )
}

/// Thin `AgentStream` constructor for non-Needle planner calls.
pub fn build_agent(
    backend: Arc<dyn agentstream::backends::LLMBackend>,
    model: &str,
    max_tokens: u32,
) -> agentstream::agent::AgentStream {
    let model = if model.trim().is_empty() {
        DEFAULT_MODEL
    } else {
        model.trim()
    };
    let config = agentstream::agent::AgentConfig::new(model, max_tokens);
    agentstream::agent::AgentStream::new(config, backend)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_normalization_appends_v1() {
        assert_eq!(
            normalize_base_url("http://localhost:8080"),
            "http://localhost:8080/v1"
        );
        assert_eq!(
            normalize_base_url("http://localhost:8080/"),
            "http://localhost:8080/v1"
        );
        assert_eq!(
            normalize_base_url("http://localhost:8080/v1"),
            "http://localhost:8080/v1"
        );
        assert_eq!(
            normalize_base_url("http://localhost:8080/v1/"),
            "http://localhost:8080/v1"
        );
        assert_eq!(
            normalize_base_url("  https://api.openai.com/v1  "),
            "https://api.openai.com/v1"
        );
        assert_eq!(normalize_base_url(""), DEFAULT_BASE_URL);
    }

    #[test]
    fn dotenvy_load_with_only_base_url_yields_absent_local() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env_path = dir.path().join(".env");
        std::fs::write(
            &env_path,
            "OPENAI_BASE_URL=http://localhost:8080\nOPENAI_MODEL=test-local-model\n",
        )
        .expect("write .env");
        let _ = dotenvy::from_path(&env_path).expect("load .env");
        // Simulate what load_llm_config() reads after dotenvy, without
        // depending on ambient process env for the key.
        let cfg = config_from_vars(
            std::env::var(ENV_BASE_URL).ok().as_deref(),
            std::env::var(ENV_MODEL).ok().as_deref(),
            None,
            None,
        )
        .expect("local config without key must succeed");
        assert_eq!(cfg.key_mode, KeyMode::AbsentLocal);
        assert_eq!(cfg.model, "test-local-model");
        assert_eq!(cfg.base_url, "http://localhost:8080/v1");
        // Clean up env vars this test introduced so other tests are unaffected.
        std::env::remove_var(ENV_BASE_URL);
        std::env::remove_var(ENV_MODEL);
    }

    #[test]
    fn key_gating_local_ok_remote_missing_key_errors_key_present_ok() {
        // Local host + no key -> ok (placeholder).
        let local = config_from_vars(Some("http://localhost:8080"), Some("m"), None, None)
            .expect("local without key");
        assert_eq!(local.key_mode, KeyMode::AbsentLocal);
        assert_eq!(local.api_key, PLACEHOLDER_KEY);

        // Loopback + LAN variants also local.
        for url in [
            "http://127.0.0.1:8080",
            "http://10.0.0.5:8080/v1",
            "http://192.168.1.10:8080",
            "http://mybox.local:8080",
        ] {
            let cfg = config_from_vars(Some(url), Some("m"), None, None)
                .unwrap_or_else(|e| panic!("{url} should be local: {e}"));
            assert_eq!(cfg.key_mode, KeyMode::AbsentLocal, "{url}");
        }

        // Remote host + no key -> MissingKey.
        let err = config_from_vars(Some("https://api.openai.com/v1"), Some("m"), None, None)
            .expect_err("remote without key must fail");
        assert!(matches!(err, LlmError::MissingKey { .. }), "{err}");

        // Key present -> ok regardless of host.
        let remote = config_from_vars(
            Some("https://api.openai.com/v1"),
            Some("m"),
            Some("sk-real"),
            None,
        )
        .expect("remote with key");
        assert_eq!(remote.key_mode, KeyMode::KeyPresent);
        assert_eq!(remote.api_key, "sk-real");

        let local_keyed =
            config_from_vars(Some("http://localhost:8080"), Some("m"), Some("sk-x"), None)
                .expect("local with key");
        assert_eq!(local_keyed.key_mode, KeyMode::KeyPresent);
    }

    #[test]
    fn model_fallback_never_panics() {
        assert_eq!(resolve_model(None), DEFAULT_MODEL);
        assert_eq!(resolve_model(Some("")), DEFAULT_MODEL);
        assert_eq!(resolve_model(Some("   ")), DEFAULT_MODEL);
        assert_eq!(resolve_model(Some(" minicpm5-1b ")), "minicpm5-1b");

        let cfg = config_from_vars(Some("http://localhost:8080"), None, None, None)
            .expect("fallback config");
        assert_eq!(cfg.model, DEFAULT_MODEL);
    }

    #[test]
    fn timeout_parsing() {
        let cfg = config_from_vars(
            Some("http://localhost:8080"),
            Some("m"),
            None,
            Some("7"),
        )
        .expect("timeout 7");
        assert_eq!(cfg.timeout_secs, 7);
        assert!(config_from_vars(
            Some("http://localhost:8080"),
            Some("m"),
            None,
            Some("0")
        )
        .is_err());
        assert!(config_from_vars(
            Some("http://localhost:8080"),
            Some("m"),
            None,
            Some("abc")
        )
        .is_err());
    }

    #[test]
    fn debug_redacts_api_key() {
        let cfg = config_from_vars(
            Some("http://localhost:8080"),
            Some("m"),
            Some("sk-super-secret"),
            None,
        )
        .expect("keyed");
        let dbg = format!("{cfg:?}");
        assert!(!dbg.contains("sk-super-secret"), "{dbg}");
        assert!(dbg.contains("<redacted>"), "{dbg}");
    }

    #[test]
    fn backend_carries_configured_model() {
        let cfg = config_from_vars(
            Some("http://localhost:8080"),
            Some("phase1-test-model"),
            None,
            Some("5"),
        )
        .expect("config");
        let backend = build_backend(&cfg);
        assert_eq!(backend.name(), "openai");
        assert_eq!(backend.default_model(), Some("phase1-test-model"));
    }

    #[tokio::test]
    async fn keyless_round_trip_against_stub_http_server() {
        // Minimal OpenAI-compatible stub: enough for agentstream's
        // `{base}/chat/completions` non-streaming path.
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            axum::routing::post(|| async {
                axum::Json(serde_json::json!({
                    "id": "chatcmpl-test",
                    "object": "chat.completion",
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": "hello-local"},
                        "finish_reason": "stop"
                    }],
                    "usage": {}
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stub");
        let addr = listener.local_addr().expect("addr");
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        // Give the listener a moment to accept (bind is already complete, but
        // the accept loop needs a tick on loaded CI runners).
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let cfg = config_from_vars(
            Some(&format!("http://{addr}")),
            Some("stub-model"),
            None, // no key on purpose
            Some("10"),
        )
        .expect("keyless local stub config");
        assert_eq!(cfg.key_mode, KeyMode::AbsentLocal);
        let backend = build_backend(&cfg);
        let mut agent = build_agent(backend, &cfg.model, 64);
        let resp = agent
            .send("hi")
            .await
            .expect("keyless stub round-trip must succeed");
        assert_eq!(resp.text, "hello-local");

        server.abort();
    }
}
