//! `cactus-rs` adapter (Phase 4 + live model): the only inference dependency.
//!
//! One process-wide [`LiveEngine`] owns the single `cactus-rs` `Needle`
//! instance (the engine is a process singleton and not thread-safe for
//! concurrent conversations). Both the agent tool-router
//! ([`LiveModelHandle`]) and the search vector encoder
//! ([`LiveEmbedder`]) share the same `Arc<LiveEngine>` so there is only
//! ever one `Needle` per process — a second `build()` would return
//! `Error::EngineBusy`.
//!
//! The engine links a closed-source static archive (LLVM libc++ plus
//! downloaded binaries) and is therefore gated behind the `live-model`
//! cargo feature. Default `cargo test` builds and runs fully against
//! [`MockModel`](super::MockModel) with no weights; passing
//! `--features live-model` compiles the real adapter below. Set
//! `NEEDLE_WEIGHTS_PATH` to a `needle3.cact` file or let
//! `Weights::fetch()` use the pinned, checksum-verified cache
//! (`~/.cache/cactus-rs`). Live integration tests are gated behind
//! `RUN_MODEL_TESTS=1` so CI without weights stays green.
//!
//! No custom FFI anywhere: inference goes through `cactus-rs` only.

#[cfg(feature = "live-model")]
mod live {
    use crate::agent::ToolResult;
    use crate::model::{ModelCompletion, ModelError, ToolCall};
    use std::sync::{Arc, Mutex};

    pub use cactus_rs::needle::{CompleteOptions, Tool as NeedleTool, Weights};

    /// Process-wide engine: the single `Needle` behind a mutex.
    /// `cactus-rs` serializes engine calls internally with a process lock,
    /// and every method here takes the mutex briefly per call, so sharing
    /// between the agent loop and the search embedder is deadlock-free
    /// (the agent never holds the lock across tool dispatch).
    pub struct LiveEngine {
        needle: Mutex<cactus_rs::needle::Needle>,
    }

    impl LiveEngine {
        pub fn build(
            weights: Weights,
            system: &str,
            tools: Vec<crate::tools::ToolDef>,
        ) -> Result<Arc<Self>, ModelError> {
            let mut builder = cactus_rs::needle::Needle::builder(weights).system(system);
            for t in tools {
                builder = builder.tool(NeedleTool::new(t.name, t.description, t.schema));
            }
            let needle = builder
                .build()
                .map_err(|e| ModelError::Engine(e.to_string()))?;
            Ok(Arc::new(Self {
                needle: Mutex::new(needle),
            }))
        }

        fn lock(&self) -> Result<std::sync::MutexGuard<'_, cactus_rs::needle::Needle>, ModelError> {
            self.needle
                .lock()
                .map_err(|e| ModelError::Engine(format!("live engine lock poisoned: {e}")))
        }

        /// Resolve weights: `NEEDLE_WEIGHTS_PATH` file first, else the
        /// pinned `Weights::fetch()` cache (downloads the 35 MB archive
        /// once, SHA-256 verified).
        pub fn resolve_weights() -> Result<Weights, ModelError> {
            if let Ok(p) = std::env::var("NEEDLE_WEIGHTS_PATH") {
                if !p.trim().is_empty() {
                    return Weights::from_file(p.trim())
                        .map_err(|e| ModelError::Engine(format!("weights file: {e}")));
                }
            }
            Weights::fetch().map_err(|e| {
                ModelError::Engine(format!(
                    "weights fetch (set NEEDLE_WEIGHTS_PATH or allow download): {e}"
                ))
            })
        }

        /// Default system prompt: tool router over the RDF graph, not a narrator.
        pub fn default_system_prompt() -> String {
            "You control an RDF knowledge graph with five tools: search, read, create, update, delete. \
             Discover URIs with search, inspect them with read, then act. \
             Never invent URIs for update/delete; use only URIs returned by search/read. \
             Keep arguments minimal and typed exactly as each schema requires."
                .to_string()
        }

        /// Build from the environment: resolve weights, use
        /// `NEEDLE_SYSTEM` or the default prompt, register `tool_defs()`.
        pub fn build_from_env() -> Result<Arc<Self>, ModelError> {
            let weights = Self::resolve_weights()?;
            let system = std::env::var("NEEDLE_SYSTEM")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(Self::default_system_prompt);
            Self::build(weights, &system, crate::tools::tool_defs())
        }

        pub fn complete_turn(&self, user_text: &str) -> Result<ModelCompletion, ModelError> {
            let completion = self
                .lock()?
                .complete(user_text)
                .map_err(|e| ModelError::Engine(e.to_string()))?;
            Ok(convert(&completion))
        }

        pub fn embed_text(&self, text: &str) -> Result<Vec<f32>, ModelError> {
            let mut guard = self.lock()?;
            let mut v = guard
                .embed(text)
                .map_err(|e| ModelError::Engine(e.to_string()))?;
            crate::search::l2_normalize(&mut v);
            Ok(v)
        }

        pub fn embedding_dimension(&self) -> Result<usize, ModelError> {
            self.lock()?
                .embedding_dimension()
                .map_err(|e| ModelError::Engine(e.to_string()))
        }

        pub fn reset_engine(&self) {
            if let Ok(mut guard) = self.needle.lock() {
                guard.reset();
            }
        }

        pub fn grounding_ok(completion: &cactus_rs::needle::Completion) -> bool {
            completion.validation().is_grounded()
        }
    }

    /// Agent-facing handle: `ModelAdapter` over the shared engine.
    pub struct LiveModelHandle {
        pub engine: Arc<LiveEngine>,
    }

    impl LiveModelHandle {
        pub fn new(engine: Arc<LiveEngine>) -> Self {
            Self { engine }
        }
    }

    impl super::super::ModelAdapter for LiveModelHandle {
        fn complete(&mut self, user_text: &str) -> Result<ModelCompletion, ModelError> {
            self.engine.complete_turn(user_text)
        }

        fn submit_tool_results(&mut self, _results: &[ToolResult]) -> Result<(), ModelError> {
            // No-op by design: the agent loop feeds tool results back as the
            // next `complete()` input (`"continue. tool_results: ..."`), which
            // is the single engine turn for that iteration. Calling
            // `complete()` here as well would double every turn, waste the
            // token budget, and pollute the conversation prefix.
            Ok(())
        }

        fn embed(&mut self, text: &str) -> Result<Vec<f32>, ModelError> {
            self.engine.embed_text(text)
        }

        fn reset(&mut self) {
            self.engine.reset_engine();
        }
    }

    /// Search-facing embedder: 3072-d `Needle::embed()` over the shared
    /// engine, L2-normalized, usable as `Box<dyn Embedder>`.
    pub struct LiveEmbedder {
        pub engine: Arc<LiveEngine>,
        dim: usize,
    }

    impl LiveEmbedder {
        pub fn new(engine: Arc<LiveEngine>) -> Result<Self, ModelError> {
            let dim = engine.embedding_dimension()?;
            Ok(Self { engine, dim })
        }
    }

    impl crate::search::Embedder for LiveEmbedder {
        fn embed(&self, text: &str) -> Vec<f32> {
            self.engine
                .embed_text(text)
                .unwrap_or_else(|_| vec![0.0; self.dim])
        }
        fn dim(&self) -> usize {
            self.dim
        }
    }

    /// Back-compat alias: the old `NeedleAdapter` name now refers to the
    /// shared-engine handle. `build()` resolves weights from the
    /// environment so existing call sites keep compiling.
    pub type NeedleAdapter = LiveModelHandle;

    impl NeedleAdapter {
        pub fn build(
            weights_path: &str,
            system: &str,
            tools: Vec<crate::tools::ToolDef>,
        ) -> Result<Self, ModelError> {
            let weights = if weights_path.trim().is_empty() {
                LiveEngine::resolve_weights()?
            } else {
                Weights::from_file(weights_path).map_err(|e| ModelError::Engine(e.to_string()))?
            };
            let engine = LiveEngine::build(weights, system, tools)?;
            Ok(Self::new(engine))
        }

        pub fn grounding_ok(completion: &cactus_rs::needle::Completion) -> bool {
            LiveEngine::grounding_ok(completion)
        }
    }

    fn convert(completion: &cactus_rs::needle::Completion) -> ModelCompletion {
        let calls: Vec<ToolCall> = completion
            .calls()
            .iter()
            .enumerate()
            .map(|(i, c)| ToolCall {
                name: c.name().to_string(),
                args: c.arguments().clone(),
                id: format!("call-{i}"),
                assistant_content: completion.reasoning().map(|s| s.to_string()),
            })
            .collect();
        if calls.is_empty() {
            ModelCompletion::Done(completion.reasoning().unwrap_or("done").to_string())
        } else {
            ModelCompletion::ToolCalls(calls)
        }
    }

    #[allow(dead_code)]
    pub fn _assert_complete_options() {
        let _ = CompleteOptions::new();
    }
}

#[cfg(feature = "live-model")]
pub use live::{
    CompleteOptions, LiveEmbedder, LiveEngine, LiveModelHandle, NeedleAdapter, NeedleTool, Weights,
};

#[cfg(not(feature = "live-model"))]
mod stub {
    use crate::agent::ToolResult;
    use crate::model::{ModelCompletion, ModelError, ToolCall};

    /// Compile-time stand-in when `live-model` is off. All methods fail
    /// closed with a clear message; tests use [`MockModel`](super::MockModel).
    pub struct NeedleAdapter {
        _private: (),
    }

    impl NeedleAdapter {
        pub fn build(
            _weights_path: &str,
            _system: &str,
            _tools: Vec<crate::tools::ToolDef>,
        ) -> Result<Self, ModelError> {
            Err(ModelError::Engine(
                "live model not enabled in this build (rebuild with --features live-model)"
                    .to_string(),
            ))
        }
    }

    impl super::super::ModelAdapter for NeedleAdapter {
        fn complete(&mut self, _user_text: &str) -> Result<ModelCompletion, ModelError> {
            Err(ModelError::Engine(
                "live model not enabled in this build".to_string(),
            ))
        }
        fn submit_tool_results(&mut self, _results: &[ToolResult]) -> Result<(), ModelError> {
            Ok(())
        }
        fn embed(&mut self, _text: &str) -> Result<Vec<f32>, ModelError> {
            Err(ModelError::Engine(
                "live model not enabled in this build".to_string(),
            ))
        }
        fn reset(&mut self) {}
    }

    #[allow(dead_code)]
    pub fn _unused(_t: ToolCall) {}
}

#[cfg(not(feature = "live-model"))]
pub use stub::NeedleAdapter;
