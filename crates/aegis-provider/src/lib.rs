//! # aegis-provider
//!
//! Unified LLM provider abstraction for Aegis.
//!
//! Supports 200+ models through a common [`Provider`] trait with:
//! - **Fallback chains**: auto-switch on failure (e.g., Claude -> GPT -> Gemini)
//! - **Credential pools**: round-robin API keys for rate limit management
//! - **Circuit breaker**: cascade failure prevention with jitter
//! - **Multi-provider**: route across providers with priority ordering
//!
//! ## Supported Providers
//! OpenAI, Anthropic, Google, xAI, Mistral, Groq, Moonshot, MiniMax,
//! Baidu, Zhipu, DeepSeek, OpenRouter, and any OpenAI-compatible endpoint.

mod anthropic;
pub mod circuit_breaker;
pub mod cost;
pub mod credential_pool;
pub mod endurance;
pub mod fallback;
pub mod minimax;
pub mod multi_provider;
mod openai;
pub mod rate_limit;
mod traits;

pub use anthropic::AnthropicProvider;
pub use circuit_breaker::CircuitBreaker;
pub use cost::{
    BillingKind, CostConfidence, CostEstimate, CostSource, CostTier, ModelRoute,
    RouteCheapnessEstimate, Router,
};
pub use credential_pool::{CredentialPool, RotationStrategy};
pub use endurance::EnduringProvider;
pub use fallback::FallbackChain;
pub use minimax::MiniMaxOutcome;
pub use multi_provider::{FallbackProvider, RoundRobinProvider};
pub use openai::OpenAiProvider;
pub use traits::{Provider, SplitSystemPrompt, StreamEvent};
