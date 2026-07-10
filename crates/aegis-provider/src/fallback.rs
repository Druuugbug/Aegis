use crate::circuit_breaker::CircuitBreaker;
use crate::traits::{Provider, StreamResult};
use aegis_types::message::{LlmResponse, Message};
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

/// Checks if an error message indicates a transient server-side failure
/// that warrants trying the next provider.
fn is_fallback_error(err: &anyhow::Error) -> bool {
    let msg = format!("{err:#}");
    let lower = msg.to_lowercase();
    lower.contains("503")
        || lower.contains("502")
        || lower.contains("500")
        || lower.contains("service unavailable")
        || lower.contains("bad gateway")
        || lower.contains("429")
        || lower.contains("rate limit")
        || lower.contains("overloaded")
}

/// A provider that tries each provider in order, falling back on transient errors.
pub struct FallbackChain {
    providers: Vec<Arc<dyn Provider>>,
    breakers: Vec<CircuitBreaker>,
}

impl FallbackChain {
    /// Creates a new `instance`.
    pub fn new(providers: Vec<Arc<dyn Provider>>) -> Self {
        assert!(!providers.is_empty(), "FallbackChain requires at least one provider");
        let breakers = providers
            .iter()
            .map(|_| CircuitBreaker::new(3, 60))
            .collect();
        Self { providers, breakers }
    }
}

#[async_trait]
impl Provider for FallbackChain {
    fn name(&self) -> &str {
        "fallback-chain"
    }

    async fn chat(
        &self,
        messages: &[Message],
        tools: Option<&serde_json::Value>,
    ) -> Result<LlmResponse> {
        let mut last_err = None;
        let mut all_open = true;
        for (i, (provider, breaker)) in self.providers.iter().zip(self.breakers.iter()).enumerate() {
            if !breaker.allow_request() {
                tracing::info!(
                    "Provider '{}' is circuit-broken (state: {}), skipping",
                    provider.name(),
                    breaker.state_name()
                );
                continue;
            }
            all_open = false;
            match provider.chat(messages, tools).await {
                Ok(resp) => {
                    breaker.record_success();
                    return Ok(resp);
                }
                Err(e) if is_fallback_error(&e) => {
                    tracing::warn!(
                        "Provider '{}' failed with fallback error (trying next): {e:#}",
                        provider.name()
                    );
                    breaker.record_failure();
                    if i + 1 < self.providers.len() {
                        tracing::info!(
                            "Falling back to provider '{}'",
                            self.providers[i + 1].name()
                        );
                    }
                    last_err = Some(e);
                }
                Err(e) => {
                    breaker.record_failure();
                    return Err(e);
                }
            }
        }
        if all_open {
            return Err(anyhow::anyhow!(
                "All providers are circuit-broken (Open state), no provider available"
            ));
        }
        Err(last_err.expect("FallbackChain has at least one provider"))
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: Option<&serde_json::Value>,
    ) -> Result<StreamResult> {
        let mut last_err = None;
        let mut all_open = true;
        for (i, (provider, breaker)) in self.providers.iter().zip(self.breakers.iter()).enumerate() {
            if !breaker.allow_request() {
                tracing::info!(
                    "Provider '{}' is circuit-broken (state: {}), skipping",
                    provider.name(),
                    breaker.state_name()
                );
                continue;
            }
            all_open = false;
            match provider.chat_stream(messages, tools).await {
                Ok(stream) => {
                    breaker.record_success();
                    return Ok(stream);
                }
                Err(e) if is_fallback_error(&e) => {
                    tracing::warn!(
                        "Provider '{}' stream failed with fallback error (trying next): {e:#}",
                        provider.name()
                    );
                    breaker.record_failure();
                    if i + 1 < self.providers.len() {
                        tracing::info!(
                            "Falling back to provider '{}'",
                            self.providers[i + 1].name()
                        );
                    }
                    last_err = Some(e);
                }
                Err(e) => {
                    breaker.record_failure();
                    return Err(e);
                }
            }
        }
        if all_open {
            return Err(anyhow::anyhow!(
                "All providers are circuit-broken (Open state), no provider available"
            ));
        }
        Err(last_err.expect("FallbackChain has at least one provider"))
    }

    fn supports_streaming(&self) -> bool {
        self.providers.iter().any(|p| p.supports_streaming())
    }

    fn supports_tools(&self) -> bool {
        self.providers.iter().any(|p| p.supports_tools())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::StreamEvent;
    use aegis_types::message::{Content, LlmResponse, Message, Role, Usage};
    use async_trait::async_trait;
    use futures::stream;

    struct OkProvider {
        name: &'static str,
    }

    #[async_trait]
    impl Provider for OkProvider {
        fn name(&self) -> &str {
            self.name
        }
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: Option<&serde_json::Value>,
        ) -> Result<LlmResponse> {
            Ok(LlmResponse {
                message: Message {
                    role: Role::Assistant,
                    content: Some(Content::Text(format!("response from {}", self.name))),
                    tool_calls: None,
                    tool_call_id: None,
                    name: Some(self.name.to_string()),
                    reasoning: None,
                },
                finish_reason: Some("end_turn".into()),
                usage: Some(Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    ..Default::default()
                }),
            })
        }
        async fn chat_stream(
            &self,
            _messages: &[Message],
            _tools: Option<&serde_json::Value>,
        ) -> Result<StreamResult> {
            let events: Vec<Result<StreamEvent>> = vec![];
            Ok(Box::pin(stream::iter(events)))
        }
    }

    struct FailProvider {
        name: &'static str,
        error_msg: &'static str,
    }

    #[async_trait]
    impl Provider for FailProvider {
        fn name(&self) -> &str {
            self.name
        }
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: Option<&serde_json::Value>,
        ) -> Result<LlmResponse> {
            Err(anyhow::anyhow!("{}", self.error_msg))
        }
        async fn chat_stream(
            &self,
            _messages: &[Message],
            _tools: Option<&serde_json::Value>,
        ) -> Result<StreamResult> {
            Err(anyhow::anyhow!("{}", self.error_msg))
        }
    }

    fn make_messages() -> Vec<Message> {
        vec![Message::user("hello")]
    }

    fn provider_name(resp: &LlmResponse) -> String {
        resp.message.name.clone().unwrap_or_default()
    }

    #[tokio::test]
    async fn test_first_provider_ok() {
        let chain = FallbackChain::new(vec![
            Arc::new(OkProvider { name: "p1" }) as Arc<dyn Provider>,
            Arc::new(OkProvider { name: "p2" }) as Arc<dyn Provider>,
        ]);
        let resp = chain.chat(&make_messages(), None).await.unwrap();
        assert_eq!(provider_name(&resp), "p1");
    }

    #[tokio::test]
    async fn test_fallback_on_503() {
        let chain = FallbackChain::new(vec![
            Arc::new(FailProvider {
                name: "p1",
                error_msg: "HTTP 503 Service Unavailable",
            }) as Arc<dyn Provider>,
            Arc::new(OkProvider { name: "p2" }) as Arc<dyn Provider>,
        ]);
        let resp = chain.chat(&make_messages(), None).await.unwrap();
        assert_eq!(provider_name(&resp), "p2");
    }

    #[tokio::test]
    async fn test_no_fallback_on_auth_error() {
        let chain = FallbackChain::new(vec![
            Arc::new(FailProvider {
                name: "p1",
                error_msg: "401 Unauthorized: invalid API key",
            }) as Arc<dyn Provider>,
            Arc::new(OkProvider { name: "p2" }) as Arc<dyn Provider>,
        ]);
        let result = chain.chat(&make_messages(), None).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("401"));
    }

    #[tokio::test]
    async fn test_fallback_on_rate_limit() {
        let chain = FallbackChain::new(vec![
            Arc::new(FailProvider {
                name: "p1",
                error_msg: "429 rate limit exceeded",
            }) as Arc<dyn Provider>,
            Arc::new(OkProvider { name: "p2" }) as Arc<dyn Provider>,
        ]);
        let resp = chain.chat(&make_messages(), None).await.unwrap();
        assert_eq!(provider_name(&resp), "p2");
    }

    #[tokio::test]
    async fn test_all_providers_fail_returns_last_error() {
        let chain = FallbackChain::new(vec![
            Arc::new(FailProvider {
                name: "p1",
                error_msg: "503 unavailable",
            }) as Arc<dyn Provider>,
            Arc::new(FailProvider {
                name: "p2",
                error_msg: "502 bad gateway",
            }) as Arc<dyn Provider>,
        ]);
        let result = chain.chat(&make_messages(), None).await;
        assert!(result.is_err());
    }
}
