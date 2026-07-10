//! SimpleX Chat channel: connects to an already-running `simplex-chat` CLI
//! process over its local WebSocket control API (via the `simploxide-client`
//! crate, `websocket` feature only — no `ffi`, no bundled CLI binary; see
//! `docs/simplex-aegis-comms-assessment.md` §4/§8 for the full rationale).
//!
//! Architecture (outbound-only, like Feishu `mode = "ws"`):
//!   SimpleX user  <-->  SMP relay network  <-->  simplex-chat CLI (local)
//!                                                       ^
//!                                                       | ws://127.0.0.1:PORT
//!                                                       v
//!                                                  Aegis gateway
//!
//! Aegis never listens on a public port for this channel: the CLI process
//! dials *out* to the SMP relay servers, and Aegis dials the CLI's *local*
//! control WebSocket. The CLI is a separately managed process (installed and
//! started by the operator, not spawned or bundled by Aegis) — this keeps
//! the integration on the Apache-2.0/MIT side of `simploxide-client`'s
//! conditional license (see assessment doc §8.2/§8.4).
//!
//! Security: the CLI's WebSocket control API has **no authentication and no
//! transport encryption** by design (upstream docs). It MUST stay bound to
//! loopback. [`GatewaySimplexConfig`] (in `aegis-core::config`) enforces this
//! at the config-validation layer.
//!
//! `simploxide_client::id::ChatId` (SimpleX's real chat identity — a direct
//! contact, group, or local note-to-self) does not implement `Serialize`, so
//! it cannot round-trip through the generic [`OutboundMessage::chat_id`]
//! `String` field used by the rest of the gateway. [`ChatIdRegistry`] below
//! is the bridge: the gateway's `serve_simplex` event loop registers each
//! inbound chat's real `ChatId` under a stable string key (used as the
//! session's `chat_id`), and [`SimplexChannel::send`] looks it back up when
//! replying. The registry is process-local, in-memory only — SimpleX chat
//! identities are re-derived from the CLI's own state on each inbound event,
//! so nothing durable is lost if the gateway restarts.

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use simploxide_client::id::ChatId;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::channel::{Channel, InboundMessage, OutboundMessage};

/// Stable string key <-> real [`ChatId`] bridge (see module docs).
#[derive(Clone, Default)]
pub struct ChatIdRegistry {
    inner: Arc<Mutex<HashMap<String, ChatId>>>,
}

impl ChatIdRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `chat_id` under `key`, returning `key` for convenience.
    pub fn register(&self, key: String, chat_id: ChatId) -> String {
        self.inner.lock().unwrap().insert(key.clone(), chat_id);
        key
    }

    /// Look up the real [`ChatId`] for a session's string key.
    pub fn get(&self, key: &str) -> Option<ChatId> {
        self.inner.lock().unwrap().get(key).copied()
    }

    /// Deterministic string key for a [`ChatId`] (used as the aegis session's
    /// `chat_id` — stable across events for the same contact/group/scope).
    pub fn key_for(chat_id: ChatId) -> String {
        match chat_id {
            ChatId::Direct(id) => format!("direct:{id:?}"),
            ChatId::Group { id, scope } => format!("group:{id:?}:{scope:?}"),
            ChatId::Local(id) => format!("local:{id:?}"),
        }
    }
}

/// Outbound-send half of the SimpleX channel. Wraps a [`simploxide_client`]
/// `Bot` handle plus a [`ChatIdRegistry`] so replies can be routed back to
/// the correct contact/group through the same control connection that the
/// gateway's event loop is reading from.
///
/// `recv()` is intentionally unimplemented (returns an error): SimpleX's
/// control API is event-stream based (`EventStream`), not poll-based, so the
/// gateway drives inbound messages directly from `serve_simplex`'s event
/// loop rather than through this trait's `recv()`. This struct exists so the
/// *outbound* path can reuse the generic [`OutboundMessage`] shape, matching
/// how the other channels' `send()` call sites are structured.
pub struct SimplexChannel {
    bot: simploxide_client::ws::Bot,
    registry: ChatIdRegistry,
}

impl SimplexChannel {
    /// Wrap an already-connected [`simploxide_client::ws::Bot`] handle and
    /// the shared [`ChatIdRegistry`] populated by `serve_simplex`.
    pub fn new(bot: simploxide_client::ws::Bot, registry: ChatIdRegistry) -> Self {
        Self { bot, registry }
    }
}

#[async_trait]
impl Channel for SimplexChannel {
    fn name(&self) -> &str {
        "simplex"
    }

    async fn connect(&mut self) -> Result<()> {
        // Connection is established before this struct is constructed (see
        // `serve_simplex` in the gateway, which owns the BotBuilder handshake
        // so it can share the resulting `Bot`/`EventStream` pair).
        Ok(())
    }

    async fn recv(&mut self) -> Result<InboundMessage> {
        Err(anyhow!(
            "SimplexChannel::recv is not used; inbound messages are driven by \
             the gateway's serve_simplex event loop (see gateway.rs)"
        ))
    }

    async fn send(&self, msg: OutboundMessage) -> Result<()> {
        let chat_id = self.registry.get(&msg.chat_id).ok_or_else(|| {
            anyhow!(
                "simplex: unknown chat_id {:?} (no inbound event registered it yet)",
                msg.chat_id
            )
        })?;

        self.bot
            .send_msg(chat_id, msg.text)
            .await
            .map_err(|e| anyhow!("simplex: send_msg failed: {e:?}"))?;
        Ok(())
    }
}
