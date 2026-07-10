use aegis_types::message::{LlmResponse, Message};
use anyhow::Result;
use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;

/// A streaming event from the LLM.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// Text delta.
    Delta(String),
    /// Reasoning/thinking delta.
    Reasoning(String),
    /// Tool call being generated (name known).
    ToolGenStarted(String),
    /// Stream finished.
    Done { response: LlmResponse },
}

pub type StreamResult = Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>;

/// Split system prompt for provider cache optimization.
///
/// Providers that support prompt caching (e.g., Anthropic) can use `static_part`
/// as the cacheable prefix and `dynamic_part` as the per-turn varying suffix.
/// Providers that don't support caching simply concatenate both parts.
pub struct SplitSystemPrompt {
    pub static_part: String,
    pub dynamic_part: String,
}

impl SplitSystemPrompt {
    /// Creates a new `instance`.
    pub fn new(static_part: impl Into<String>, dynamic_part: impl Into<String>) -> Self {
        Self {
            static_part: static_part.into(),
            dynamic_part: dynamic_part.into(),
        }
    }

    /// Combined system prompt (for providers that don't support split).
    pub fn combined(&self) -> String {
        if self.dynamic_part.is_empty() {
            self.static_part.clone()
        } else if self.static_part.is_empty() {
            self.dynamic_part.clone()
        } else {
            format!("{}\n\n{}", self.static_part, self.dynamic_part)
        }
    }

    /// Estimated token count (rough: 1 token per 4 chars).
    pub fn estimated_tokens(&self) -> usize {
        (self.static_part.len() + self.dynamic_part.len()) / 4
    }
}

/// LLM provider interface.
#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;

    /// Non-streaming chat.
    async fn chat(
        &self,
        messages: &[Message],
        tools: Option<&serde_json::Value>,
    ) -> Result<LlmResponse>;

    /// Streaming chat. Returns a stream of events.
    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: Option<&serde_json::Value>,
    ) -> Result<StreamResult>;

    /// Streaming chat with split system prompt for cache optimization.
    /// Default implementation: concatenate and delegate to `chat_stream`.
    async fn chat_stream_split(
        &self,
        messages: &[Message],
        tools: Option<&serde_json::Value>,
        system: &SplitSystemPrompt,
    ) -> Result<StreamResult> {
        // Inject combined system prompt as a system message
        let combined = system.combined();
        let mut messages_with_system = Vec::with_capacity(messages.len() + 1);
        messages_with_system.push(Message::system(&combined));
        messages_with_system.extend_from_slice(messages);
        self.chat_stream(&messages_with_system, tools).await
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    /// Whether this provider/model supports tool/function calling.
    fn supports_tools(&self) -> bool {
        true
    }

    /// Whether this provider supports native prompt caching
    /// (e.g., Anthropic cache_control).
    fn supports_prompt_caching(&self) -> bool {
        false
    }
}
