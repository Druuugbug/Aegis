use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::traits::{Provider, StreamResult};
use aegis_types::message::{LlmResponse, Message};

/// Speculative execution Provider: sends request to both providers, takes the fastest response.
pub struct HedgeProvider {
    primary: Arc<dyn Provider>,
    secondary: Arc<dyn Provider>,
    /// How long to wait before sending secondary request.
    delay: Duration,
}

impl HedgeProvider {
    /// Creates a new `instance`.
    pub fn new(primary: Arc<dyn Provider>, secondary: Arc<dyn Provider>, delay: Duration) -> Self {
        Self { primary, secondary, delay }
    }
}

#[async_trait]
impl Provider for HedgeProvider {
    fn name(&self) -> &str {
        self.primary.name()
    }

    fn supports_streaming(&self) -> bool {
        self.primary.supports_streaming()
    }

    fn supports_tools(&self) -> bool {
        self.primary.supports_tools()
    }

    async fn chat(
        &self,
        messages: &[Message],
        tools: Option<&serde_json::Value>,
    ) -> anyhow::Result<LlmResponse> {
        let primary = self.primary.clone();
        let secondary = self.secondary.clone();
        let delay = self.delay;
        let messages_owned: Vec<Message> = messages.to_vec();
        let tools_owned = tools.cloned();

        let msgs1 = messages_owned.clone();
        let tools1 = tools_owned.clone();
        let primary_fut = async move {
            primary.chat(&msgs1, tools1.as_ref()).await
        };

        let msgs2 = messages_owned.clone();
        let tools2 = tools_owned.clone();
        let secondary_fut = async move {
            tokio::time::sleep(delay).await;
            secondary.chat(&msgs2, tools2.as_ref()).await
        };

        tokio::select! {
            res = primary_fut => res,
            res = secondary_fut => res,
        }
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: Option<&serde_json::Value>,
    ) -> anyhow::Result<StreamResult> {
        let primary = self.primary.clone();
        let secondary = self.secondary.clone();
        let delay = self.delay;
        let messages_owned: Vec<Message> = messages.to_vec();
        let tools_owned = tools.cloned();

        let msgs1 = messages_owned.clone();
        let tools1 = tools_owned.clone();
        let msgs2 = messages_owned.clone();
        let tools2 = tools_owned.clone();

        // Try primary immediately; secondary after delay. Return whichever starts first.
        let primary_stream_fut = async move {
            primary.chat_stream(&msgs1, tools1.as_ref()).await
        };
        let secondary_stream_fut = async move {
            tokio::time::sleep(delay).await;
            secondary.chat_stream(&msgs2, tools2.as_ref()).await
        };

        tokio::select! {
            res = primary_stream_fut => res,
            res = secondary_stream_fut => res,
        }
    }
}
