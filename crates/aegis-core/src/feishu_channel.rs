use anyhow::{anyhow, Result};
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::channel::{Channel, InboundMessage, OutboundMessage};

pub struct FeishuChannel {
    app_id: String,
    app_secret: String,
    receive_id: String,
    base: String,
    client: reqwest::Client,
    token_cache: Arc<Mutex<Option<(String, tokio::time::Instant)>>>,
}

impl FeishuChannel {
    /// Create a new Feishu (Lark) channel client with app credentials and receive ID.
    pub fn new(
        app_id: impl Into<String>,
        app_secret: impl Into<String>,
        receive_id: impl Into<String>,
    ) -> Self {
        Self::with_base(app_id, app_secret, receive_id, "https://open.feishu.cn")
    }

    /// Like [`FeishuChannel::new`] but with a custom open-platform base URL
    /// (China `https://open.feishu.cn` or Lark International
    /// `https://open.larksuite.com`).
    pub fn with_base(
        app_id: impl Into<String>,
        app_secret: impl Into<String>,
        receive_id: impl Into<String>,
        base: impl Into<String>,
    ) -> Self {
        Self {
            app_id: app_id.into(),
            app_secret: app_secret.into(),
            receive_id: receive_id.into(),
            base: {
                let b: String = base.into();
                b.trim_end_matches('/').to_string()
            },
            client: reqwest::Client::new(),
            token_cache: Arc::new(Mutex::new(None)),
        }
    }

    async fn get_tenant_access_token(&self) -> Result<String> {
        let mut cache = self.token_cache.lock().await;
        if let Some((ref token, expiry)) = *cache {
            if tokio::time::Instant::now() < expiry {
                return Ok(token.clone());
            }
        }

        let body = serde_json::json!({
            "app_id": self.app_id,
            "app_secret": self.app_secret,
        });
        let resp = self
            .client
            .post(format!("{}/open-apis/auth/v3/tenant_access_token/internal", self.base))
            .json(&body)
            .send()
            .await?;

        let data: serde_json::Value = resp.json().await?;
        let code = data["code"].as_i64().unwrap_or(-1);
        if code != 0 {
            return Err(anyhow!("Feishu token error: {}", data));
        }
        let token = data["tenant_access_token"]
            .as_str()
            .ok_or_else(|| anyhow!("missing tenant_access_token"))?
            .to_string();

        let ttl = std::time::Duration::from_secs(7200 - 60); // ~2h with margin
        *cache = Some((token.clone(), tokio::time::Instant::now() + ttl));
        Ok(token)
    }
}

#[async_trait]
impl Channel for FeishuChannel {
    fn name(&self) -> &str {
        "feishu"
    }

    async fn connect(&mut self) -> Result<()> {
        self.get_tenant_access_token().await?;
        Ok(())
    }

    async fn recv(&mut self) -> Result<InboundMessage> {
        // Feishu uses event subscription (webhook); polling not supported here.
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        Err(anyhow!("no new messages"))
    }

    async fn send(&self, msg: OutboundMessage) -> Result<()> {
        let token = self.get_tenant_access_token().await?;
        let url = format!(
            "{}/open-apis/im/v1/messages?receive_id_type={}",
            self.base,
            if self.receive_id.starts_with("oc_") { "chat_id" } else { "open_id" }
        );
        let body = serde_json::json!({
            "receive_id": self.receive_id,
            "msg_type": "text",
            "content": serde_json::json!({"text": msg.text}).to_string(),
        });
        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", token))
            .json(&body)
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(anyhow!("Feishu send failed: {}", resp.status()));
        }
        Ok(())
    }
}
