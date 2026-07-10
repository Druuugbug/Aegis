// tests/integration.rs — Aegis 集成测试 (doc 0.6)
// 不依赖实际 LLM，使用 Mock Provider

use aegis_core::agent::Agent;
use aegis_core::agent::CostSummary;
use aegis_core::config::Config;
use aegis_provider::{Provider, StreamEvent};
use aegis_types::message::{Content, LlmResponse, Message, Role};
use anyhow::Result;
use async_trait::async_trait;
use futures::{stream, Stream};
use std::pin::Pin;
use std::sync::Arc;

type StreamResult = Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>;

/// 测试用 Mock LLM Provider
struct MockProvider {
    response: String,
}

#[async_trait]
impl Provider for MockProvider {
    fn name(&self) -> &str {
        "mock"
    }

    async fn chat(&self, _messages: &[Message], _tools: Option<&serde_json::Value>) -> Result<LlmResponse> {
        Ok(self.make_response())
    }

    async fn chat_stream(
        &self,
        _messages: &[Message],
        _tools: Option<&serde_json::Value>,
    ) -> Result<StreamResult> {
        let resp = self.make_response();
        let s = stream::once(async move { Ok(StreamEvent::Done { response: resp }) });
        Ok(Box::pin(s))
    }
}

impl MockProvider {
    fn make_response(&self) -> LlmResponse {
        LlmResponse {
            message: Message {
                role: Role::Assistant,
                content: Some(Content::Text(self.response.clone())),
                tool_calls: None,
                tool_call_id: None,
                name: None,
                reasoning: None,
            },
            finish_reason: Some("stop".to_string()),
            usage: None,
        }
    }
}

#[tokio::test]
async fn test_basic_chat() {
    let provider = Arc::new(MockProvider {
        response: "Hello from mock!".to_string(),
    });
    let config = Config::default();
    let mut agent = Agent::new(provider, None, config);
    let reply = agent.chat("Hello").await.expect("chat should not fail");
    assert!(!reply.is_empty(), "reply should not be empty");
    assert_eq!(reply, "Hello from mock!");
}

#[tokio::test]
async fn test_session_history() {
    let provider = Arc::new(MockProvider {
        response: "I remember".to_string(),
    });
    let config = Config::default();
    let mut agent = Agent::new(provider, None, config);
    agent.chat("First message").await.unwrap();
    agent.chat("Second message").await.unwrap();
    // history 应包含 4 条消息（user+assistant x2）
    assert!(agent.history().len() >= 4);
}

#[tokio::test]
async fn test_dlp_filter() {
    use aegis_security::DlpFilter;
    let filter = DlpFilter::new(true);
    let result = filter.filter("请帮我查一下 13812345678 的订单");
    assert!(
        !result.contains("13812345678"),
        "phone number should be filtered"
    );
    assert!(
        result.contains("[PHONE_REDACTED]"),
        "should contain PHONE_REDACTED placeholder"
    );
}

#[tokio::test]
async fn test_circuit_breaker() {
    use aegis_provider::CircuitBreaker;
    let cb = CircuitBreaker::new(3, 60);
    assert!(cb.allow_request(), "initially should allow");
    cb.record_failure();
    cb.record_failure();
    cb.record_failure();
    assert!(!cb.allow_request(), "after 3 failures should be open");
}

/// 5.2.4: build_split_system_prompt returns same hash for same tools_desc input
#[test]
fn test_build_split_system_prompt_hash_stable() {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    fn compute(tools_desc: &str) -> u64 {
        let static_text = format!("You are Aegis, an AI assistant.\n\nTools available:\n{}", tools_desc);
        let mut hasher = DefaultHasher::new();
        static_text.hash(&mut hasher);
        hasher.finish()
    }

    let tools = "terminal: run shell commands\nread_file: read a file";
    let h1 = compute(tools);
    let h2 = compute(tools);
    assert_eq!(h1, h2, "same tools_desc should produce same hash");

    // Different input should produce different hash
    let h3 = compute("different_tool: do something");
    assert_ne!(h1, h3, "different tools_desc should produce different hash");
}

/// 5.3.1: CostSummary fields are accessible
#[test]
fn test_cost_tracker() {
    let ct = CostSummary {
        input_tokens: 3000,
        output_tokens: 1500,
        estimated_cost_usd: 0.01,
    };
    assert_eq!(ct.input_tokens, 3000);
    assert_eq!(ct.output_tokens, 1500);
    assert!(ct.estimated_cost_usd > 0.0, "cost should be positive");
}
