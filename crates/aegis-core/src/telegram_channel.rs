use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde::Deserialize;
use std::collections::VecDeque;

use crate::channel::{Channel, InboundMessage, OutboundMessage};

pub struct TelegramChannel {
    bot_token: String,
    client: reqwest::Client,
    offset: i64,
    pending: VecDeque<InboundMessage>,
}

#[derive(Debug, Deserialize)]
struct TelegramUser {
    id: i64,
}

#[derive(Debug, Deserialize)]
struct TelegramChat {
    id: i64,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct TelegramMessage {
    message_id: i64,
    from: Option<TelegramUser>,
    chat: TelegramChat,
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TelegramUpdate {
    update_id: i64,
    message: Option<TelegramMessage>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct TelegramResponse<T> {
    ok: bool,
    result: Option<T>,
}

impl TelegramChannel {
    /// Create a new Telegram channel client with the given bot token.
    pub fn new(bot_token: impl Into<String>) -> Self {
        Self {
            bot_token: bot_token.into(),
            client: reqwest::Client::new(),
            offset: 0,
            pending: VecDeque::new(),
        }
    }

    fn base_url(&self) -> String {
        format!("https://api.telegram.org/bot{}", self.bot_token)
    }
}

#[async_trait]
impl Channel for TelegramChannel {
    fn name(&self) -> &str {
        "telegram"
    }

    async fn connect(&mut self) -> Result<()> {
        let url = format!("{}/getMe", self.base_url());
        let resp = self.client.get(&url).send().await?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(anyhow!(
                "Telegram getMe failed: {}",
                resp.status()
            ))
        }
    }

    async fn recv(&mut self) -> Result<InboundMessage> {
        if let Some(msg) = self.pending.pop_front() {
            return Ok(msg);
        }

        let url = format!(
            "{}/getUpdates?offset={}&timeout=30",
            self.base_url(),
            self.offset
        );
        let resp = self.client.get(&url).send().await?;
        let body: TelegramResponse<Vec<TelegramUpdate>> = resp.json().await?;

        let updates = body.result.unwrap_or_default();
        for update in updates {
            self.offset = update.update_id + 1;
            if let Some(msg) = update.message {
                if let Some(text) = msg.text {
                    self.pending.push_back(InboundMessage {
                        channel: "telegram".to_string(),
                        user_id: msg.from.map(|u| u.id.to_string()).unwrap_or_default(),
                        chat_id: msg.chat.id.to_string(),
                        text,
                    });
                }
            }
        }

        self.pending
            .pop_front()
            .ok_or_else(|| anyhow!("no new messages"))
    }

    async fn send(&self, msg: OutboundMessage) -> Result<()> {
        let base = self.base_url();
        if let Some(message_id) = msg.edit_message_id {
            let url = format!("{}/editMessageText", base);
            let body = serde_json::json!({
                "chat_id": msg.chat_id,
                "message_id": message_id.parse::<i64>().unwrap_or(0),
                "text": msg.text,
            });
            let resp = self.client.post(&url).json(&body).send().await?;
            if !resp.status().is_success() {
                return Err(anyhow!(
                    "Telegram editMessageText failed: {}",
                    resp.status()
                ));
            }
        } else {
            let url = format!("{}/sendMessage", base);
            let body = serde_json::json!({
                "chat_id": msg.chat_id,
                "text": msg.text,
            });
            let resp = self.client.post(&url).json(&body).send().await?;
            if !resp.status().is_success() {
                return Err(anyhow!(
                    "Telegram sendMessage failed: {}",
                    resp.status()
                ));
            }
        }
        Ok(())
    }
}
