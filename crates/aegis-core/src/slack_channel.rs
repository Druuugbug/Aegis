use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde::Deserialize;
use std::collections::VecDeque;

use crate::channel::{Channel, InboundMessage, OutboundMessage};

pub struct SlackChannel {
    bot_token: String,
    channel_id: String,
    client: reqwest::Client,
    last_ts: Option<String>,
    pending: VecDeque<InboundMessage>,
}

#[derive(Debug, Deserialize)]
struct SlackMessage {
    user: Option<String>,
    text: Option<String>,
    ts: Option<String>,
    /// Present on messages posted by a bot/app (incl. our own replies).
    #[serde(default)]
    bot_id: Option<String>,
    /// e.g. "bot_message". Used to skip non-human messages.
    #[serde(default)]
    subtype: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SlackHistoryResponse {
    ok: bool,
    messages: Option<Vec<SlackMessage>>,
}

#[derive(Debug, Deserialize)]
struct SlackPostResponse {
    ok: bool,
    error: Option<String>,
}

impl SlackChannel {
    /// Create a new Slack channel client with bot token and channel ID.
    pub fn new(bot_token: impl Into<String>, channel_id: impl Into<String>) -> Self {
        Self {
            bot_token: bot_token.into(),
            channel_id: channel_id.into(),
            client: reqwest::Client::new(),
            last_ts: None,
            pending: VecDeque::new(),
        }
    }
}

#[async_trait]
impl Channel for SlackChannel {
    fn name(&self) -> &str {
        "slack"
    }

    async fn connect(&mut self) -> Result<()> {
        let resp = self
            .client
            .post("https://slack.com/api/auth.test")
            .bearer_auth(&self.bot_token)
            .send()
            .await?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(anyhow!("Slack auth.test failed: {}", resp.status()))
        }
    }

    async fn recv(&mut self) -> Result<InboundMessage> {
        if let Some(msg) = self.pending.pop_front() {
            return Ok(msg);
        }

        let mut url = format!(
            "https://slack.com/api/conversations.history?channel={}&limit=10",
            self.channel_id
        );
        if let Some(ts) = &self.last_ts {
            url.push_str(&format!("&oldest={}", ts));
        }

        let resp = self
            .client
            .get(&url)
            .bearer_auth(&self.bot_token)
            .send()
            .await?;
        let body: SlackHistoryResponse = resp.json().await?;

        if !body.ok {
            return Err(anyhow!("Slack conversations.history failed"));
        }

        let mut messages = body.messages.unwrap_or_default();
        // API returns newest first; reverse to process oldest first
        messages.reverse();

        for msg in messages {
            // Always advance the cursor (even for skipped messages).
            if let Some(ts) = msg.ts.clone() {
                self.last_ts = Some(ts);
            }
            // Skip bot/app messages — including our OWN replies — so the bot
            // doesn't re-ingest what it just posted and reply in a loop.
            if msg.bot_id.is_some() || msg.subtype.as_deref() == Some("bot_message") {
                continue;
            }
            if let Some(text) = msg.text {
                self.pending.push_back(InboundMessage {
                    channel: "slack".to_string(),
                    user_id: msg.user.unwrap_or_default(),
                    chat_id: self.channel_id.clone(),
                    text,
                });
            }
        }

        self.pending
            .pop_front()
            .ok_or_else(|| anyhow!("no new messages"))
    }

    async fn send(&self, msg: OutboundMessage) -> Result<()> {
        let body = serde_json::json!({
            "channel": msg.chat_id,
            "text": msg.text,
        });
        let resp = self
            .client
            .post("https://slack.com/api/chat.postMessage")
            .bearer_auth(&self.bot_token)
            .json(&body)
            .send()
            .await?;
        let result: SlackPostResponse = resp.json().await?;
        if !result.ok {
            return Err(anyhow!(
                "Slack chat.postMessage failed: {}",
                result.error.unwrap_or_default()
            ));
        }
        Ok(())
    }
}
