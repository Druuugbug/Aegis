use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde::Deserialize;

use crate::channel::{Channel, InboundMessage, OutboundMessage};

pub struct DiscordChannel {
    bot_token: String,
    channel_id: String,
    webhook_url: Option<String>,
    client: reqwest::Client,
    last_message_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DiscordUser {
    id: String,
    bot: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct DiscordMessage {
    id: String,
    author: DiscordUser,
    content: String,
}

impl DiscordChannel {
    /// Create a new Discord channel client with bot token and channel ID.
    pub fn new(
        bot_token: impl Into<String>,
        channel_id: impl Into<String>,
    ) -> Self {
        Self {
            bot_token: bot_token.into(),
            channel_id: channel_id.into(),
            webhook_url: None,
            client: reqwest::Client::new(),
            last_message_id: None,
        }
    }

    /// Set a webhook URL for sending messages (uses webhook instead of bot API).
    pub fn with_webhook(mut self, url: impl Into<String>) -> Self {
        self.webhook_url = Some(url.into());
        self
    }
}

#[async_trait]
impl Channel for DiscordChannel {
    fn name(&self) -> &str {
        "discord"
    }

    async fn connect(&mut self) -> Result<()> {
        let resp = self
            .client
            .get("https://discord.com/api/v10/users/@me")
            .header("Authorization", format!("Bot {}", self.bot_token))
            .send()
            .await?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(anyhow!(
                "Discord connect failed: {}",
                resp.status()
            ))
        }
    }

    async fn recv(&mut self) -> Result<InboundMessage> {
        let mut url = format!(
            "https://discord.com/api/v10/channels/{}/messages?limit=5",
            self.channel_id
        );
        if let Some(ref after) = self.last_message_id {
            url.push_str(&format!("&after={}", after));
        }

        let resp = self
            .client
            .get(&url)
            .header("Authorization", format!("Bot {}", self.bot_token))
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(anyhow!(
                "Discord fetch messages failed: {}",
                resp.status()
            ));
        }

        let messages: Vec<DiscordMessage> = resp.json().await?;
        let mut new_messages: Vec<InboundMessage> = Vec::new();

        for msg in messages {
            if msg.author.bot.unwrap_or(false) {
                continue;
            }
            new_messages.push(InboundMessage {
                channel: "discord".to_string(),
                user_id: msg.author.id.clone(),
                chat_id: self.channel_id.clone(),
                text: msg.content,
            });
            // Track the latest message id seen
            match &self.last_message_id {
                Some(last) if msg.id > *last => {
                    self.last_message_id = Some(msg.id);
                }
                None => {
                    self.last_message_id = Some(msg.id);
                }
                _ => {}
            }
        }

        if let Some(first) = new_messages.into_iter().next() {
            return Ok(first);
        }

        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        Err(anyhow!("no new messages"))
    }

    async fn send(&self, msg: OutboundMessage) -> Result<()> {
        if let Some(ref webhook) = self.webhook_url {
            let body = serde_json::json!({
                "content": msg.text,
            });
            let resp = self.client.post(webhook).json(&body).send().await?;
            if !resp.status().is_success() {
                return Err(anyhow!(
                    "Discord webhook send failed: {}",
                    resp.status()
                ));
            }
        } else {
            let url = format!(
                "https://discord.com/api/v10/channels/{}/messages",
                self.channel_id
            );
            let body = serde_json::json!({
                "content": msg.text,
            });
            let resp = self
                .client
                .post(&url)
                .header("Authorization", format!("Bot {}", self.bot_token))
                .json(&body)
                .send()
                .await?;
            if !resp.status().is_success() {
                return Err(anyhow!(
                    "Discord send message failed: {}",
                    resp.status()
                ));
            }
        }
        Ok(())
    }
}
