//! Model integration (Phase 4): Needle 3 is the tool router only.
//!
//! [`ModelAdapter`] is the seam between the agent loop and inference.
//! Production uses [`needle::NeedleAdapter`] (the single `cactus-rs`
//! dependency for inference; no custom FFI). Tests use [`MockModel`].

pub mod needle;

pub use needle::NeedleAdapter;
#[cfg(feature = "live-model")]
pub use needle::{LiveEmbedder, LiveEngine, LiveModelHandle};

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub name: String,
    pub args: serde_json::Value,
    pub id: String,
    #[serde(default)]
    pub assistant_content: Option<String>,
}

impl ToolCall {
    pub fn new(name: impl Into<String>, args: serde_json::Value, id: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            args,
            id: id.into(),
            assistant_content: None,
        }
    }
}

/// One model turn: tool calls or a final answer.
#[derive(Debug, Clone, PartialEq)]
pub enum ModelCompletion {
    ToolCalls(Vec<ToolCall>),
    Done(String),
}

#[derive(Debug, Error)]
pub enum ModelError {
    #[error("engine error: {0}")]
    Engine(String),
    #[error("envelope error: {0}")]
    Envelope(String),
}

/// Minimal adapter the agent loop programs against.
pub trait ModelAdapter: Send {
    fn complete(&mut self, user_text: &str) -> Result<ModelCompletion, ModelError>;
    /// Feed structured tool results back; the next `complete` sees them.
    /// Default implementations (e.g. real Needle) may accumulate internally
    /// and ignore this; mocks record it for assertions.
    fn submit_tool_results(
        &mut self,
        _results: &[crate::agent::ToolResult],
    ) -> Result<(), ModelError> {
        Ok(())
    }
    fn embed(&mut self, text: &str) -> Result<Vec<f32>, ModelError>;
    fn reset(&mut self);
}

/// Scripted mock: returns the scripted completions in order.
#[derive(Debug, Default)]
pub struct MockModel {
    script: Vec<ModelCompletion>,
    index: usize,
    pub seen_inputs: Vec<String>,
    pub submitted: Vec<usize>,
}

impl MockModel {
    pub fn new(script: Vec<ModelCompletion>) -> Self {
        Self {
            script,
            index: 0,
            seen_inputs: vec![],
            submitted: vec![],
        }
    }

    pub fn done(text: &str) -> Self {
        Self::new(vec![ModelCompletion::Done(text.to_string())])
    }
}

impl ModelAdapter for MockModel {
    fn complete(&mut self, user_text: &str) -> Result<ModelCompletion, ModelError> {
        self.seen_inputs.push(user_text.to_string());
        if self.index < self.script.len() {
            let out = self.script[self.index].clone();
            self.index += 1;
            Ok(out)
        } else {
            Ok(ModelCompletion::Done(
                "mock exhausted; stopping".to_string(),
            ))
        }
    }

    fn submit_tool_results(
        &mut self,
        results: &[crate::agent::ToolResult],
    ) -> Result<(), ModelError> {
        self.submitted.push(results.len());
        Ok(())
    }

    fn embed(&mut self, _text: &str) -> Result<Vec<f32>, ModelError> {
        Ok(vec![0.0; 8])
    }

    fn reset(&mut self) {
        self.index = 0;
        self.seen_inputs.clear();
        self.submitted.clear();
    }
}
