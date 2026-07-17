use crate::traits::{Provider, StreamResult};
use aegis_types::message::{LlmResponse, Message};
use anyhow::Result;
use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

/// A provider with sticky failover: starts from the last successful provider,
/// rotates on failure, and sticks to the one that succeeds.
pub struct FallbackProvider {
    providers: Vec<Box<dyn Provider>>,
    current: Mutex<usize>,
    max_retries: usize,
}

impl FallbackProvider {
    /// Creates a new `instance`.
    pub fn new(providers: Vec<Box<dyn Provider>>) -> Self {
        assert!(
            !providers.is_empty(),
            "FallbackProvider requires at least one provider"
        );
        let max_retries = providers.len();
        Self {
            providers,
            current: Mutex::new(0),
            max_retries,
        }
    }

    fn advance(&self, cur: usize) -> usize {
        (cur + 1) % self.providers.len()
    }
}

#[async_trait]
impl Provider for FallbackProvider {
    fn name(&self) -> &str {
        "fallback"
    }

    async fn chat(
        &self,
        messages: &[Message],
        tools: Option<&serde_json::Value>,
    ) -> Result<LlmResponse> {
        let start = *self.current.lock().expect("lock poisoned");
        let mut idx = start;
        for _ in 0..self.max_retries {
            match self.providers[idx].chat(messages, tools).await {
                Ok(resp) => {
                    *self.current.lock().expect("lock poisoned") = idx;
                    return Ok(resp);
                }
                Err(e) => {
                    tracing::warn!(
                        "FallbackProvider: '{}' failed: {e:#}, rotating",
                        self.providers[idx].name()
                    );
                    idx = self.advance(idx);
                }
            }
        }
        *self.current.lock().expect("lock poisoned") = idx;
        Err(anyhow::anyhow!("all providers exhausted"))
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: Option<&serde_json::Value>,
    ) -> Result<StreamResult> {
        let start = *self.current.lock().expect("lock poisoned");
        let mut idx = start;
        for _ in 0..self.max_retries {
            match self.providers[idx].chat_stream(messages, tools).await {
                Ok(stream) => {
                    *self.current.lock().expect("lock poisoned") = idx;
                    return Ok(stream);
                }
                Err(e) => {
                    tracing::warn!(
                        "FallbackProvider: '{}' stream failed: {e:#}, rotating",
                        self.providers[idx].name()
                    );
                    idx = self.advance(idx);
                }
            }
        }
        *self.current.lock().expect("lock poisoned") = idx;
        Err(anyhow::anyhow!("all providers exhausted"))
    }
}

/// Round-robin load-balancing provider. Each call rotates to the next provider.
/// Uses AtomicUsize for lock-free concurrency.
pub struct RoundRobinProvider {
    providers: Vec<Box<dyn Provider>>,
    current: AtomicUsize,
}

impl RoundRobinProvider {
    /// Creates a new `instance`.
    pub fn new(providers: Vec<Box<dyn Provider>>) -> Self {
        assert!(
            !providers.is_empty(),
            "RoundRobinProvider requires at least one provider"
        );
        Self {
            providers,
            current: AtomicUsize::new(0),
        }
    }

    fn next_index(&self) -> usize {
        let idx = self.current.fetch_add(1, Ordering::Relaxed);
        idx % self.providers.len()
    }
}

#[async_trait]
impl Provider for RoundRobinProvider {
    fn name(&self) -> &str {
        "round-robin"
    }

    async fn chat(
        &self,
        messages: &[Message],
        tools: Option<&serde_json::Value>,
    ) -> Result<LlmResponse> {
        let idx = self.next_index();
        self.providers[idx].chat(messages, tools).await
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: Option<&serde_json::Value>,
    ) -> Result<StreamResult> {
        let idx = self.next_index();
        self.providers[idx].chat_stream(messages, tools).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::StreamEvent;
    use aegis_types::message::{Content, LlmResponse, Message, Role, Usage};
    use futures::stream;

    struct MockProvider {
        id: &'static str,
        fail: bool,
    }

    #[async_trait]
    impl Provider for MockProvider {
        fn name(&self) -> &str {
            self.id
        }
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: Option<&serde_json::Value>,
        ) -> Result<LlmResponse> {
            if self.fail {
                return Err(anyhow::anyhow!("mock error from {}", self.id));
            }
            Ok(LlmResponse {
                message: Message {
                    role: Role::Assistant,
                    content: Some(Content::Text(format!("ok from {}", self.id))),
                    tool_calls: None,
                    tool_call_id: None,
                    name: Some(self.id.to_string()),
                    reasoning: None,
                },
                finish_reason: Some("stop".into()),
                usage: Some(Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                    ..Default::default()
                }),
            })
        }
        async fn chat_stream(
            &self,
            _messages: &[Message],
            _tools: Option<&serde_json::Value>,
        ) -> Result<StreamResult> {
            if self.fail {
                return Err(anyhow::anyhow!("mock stream error from {}", self.id));
            }
            let events: Vec<Result<StreamEvent>> = vec![];
            Ok(Box::pin(stream::iter(events)))
        }
    }

    fn msgs() -> Vec<Message> {
        vec![Message::user("hi")]
    }

    fn resp_name(r: &LlmResponse) -> &str {
        r.message.name.as_deref().unwrap_or("")
    }

    #[tokio::test]
    async fn fallback_uses_first_provider_when_healthy() {
        let fp = FallbackProvider::new(vec![
            Box::new(MockProvider {
                id: "a",
                fail: false,
            }),
            Box::new(MockProvider {
                id: "b",
                fail: false,
            }),
        ]);
        let r = fp.chat(&msgs(), None).await.unwrap();
        assert_eq!(resp_name(&r), "a");
    }

    #[tokio::test]
    async fn fallback_rotates_on_failure() {
        let fp = FallbackProvider::new(vec![
            Box::new(MockProvider {
                id: "a",
                fail: true,
            }),
            Box::new(MockProvider {
                id: "b",
                fail: false,
            }),
        ]);
        let r = fp.chat(&msgs(), None).await.unwrap();
        assert_eq!(resp_name(&r), "b");
    }

    #[tokio::test]
    async fn fallback_sticky_routing() {
        let fp = FallbackProvider::new(vec![
            Box::new(MockProvider {
                id: "a",
                fail: true,
            }),
            Box::new(MockProvider {
                id: "b",
                fail: false,
            }),
            Box::new(MockProvider {
                id: "c",
                fail: false,
            }),
        ]);
        // First call: a fails, lands on b
        let r = fp.chat(&msgs(), None).await.unwrap();
        assert_eq!(resp_name(&r), "b");
        // Second call: starts from b (sticky)
        let r = fp.chat(&msgs(), None).await.unwrap();
        assert_eq!(resp_name(&r), "b");
    }

    #[tokio::test]
    async fn fallback_all_fail() {
        let fp = FallbackProvider::new(vec![
            Box::new(MockProvider {
                id: "a",
                fail: true,
            }),
            Box::new(MockProvider {
                id: "b",
                fail: true,
            }),
        ]);
        let err = fp.chat(&msgs(), None).await.unwrap_err();
        assert!(err.to_string().contains("all providers exhausted"));
    }

    #[tokio::test]
    async fn round_robin_rotates() {
        let rr = RoundRobinProvider::new(vec![
            Box::new(MockProvider {
                id: "x",
                fail: false,
            }),
            Box::new(MockProvider {
                id: "y",
                fail: false,
            }),
            Box::new(MockProvider {
                id: "z",
                fail: false,
            }),
        ]);
        assert_eq!(resp_name(&rr.chat(&msgs(), None).await.unwrap()), "x");
        assert_eq!(resp_name(&rr.chat(&msgs(), None).await.unwrap()), "y");
        assert_eq!(resp_name(&rr.chat(&msgs(), None).await.unwrap()), "z");
        assert_eq!(resp_name(&rr.chat(&msgs(), None).await.unwrap()), "x");
    }
}
