use crate::channel::{MessageEvent, OutboundMessage, PlatformAdapter};
use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;

/// HTTP webhook 模式的 PlatformAdapter 实现。
/// 通过 tokio mpsc channel 桥接 axum HTTP handler 和 Agent。
pub struct HttpPlatformAdapter {
    /// 接收入站消息的 receiver（从 HTTP handler 发来）
    inbound_rx: mpsc::Receiver<MessageEvent>,
    /// 发送出站消息的 sender（给 HTTP handler 取走返回给调用方）
    outbound_tx: mpsc::Sender<OutboundMessage>,
}

/// Handle for the HTTP handler side: inject inbound messages, drain outbound replies.
pub struct HttpAdapterHandle {
    /// HTTP handler 用这个发消息进来
    pub inbound_tx: mpsc::Sender<MessageEvent>,
    /// HTTP handler 用这个取出 agent 的回复
    pub outbound_rx: mpsc::Receiver<OutboundMessage>,
}

impl HttpPlatformAdapter {
    /// Create a new HTTP adapter and its handle, connected via mpsc channels.
    pub fn new() -> (Self, HttpAdapterHandle) {
        let (inbound_tx, inbound_rx) = mpsc::channel(32);
        let (outbound_tx, outbound_rx) = mpsc::channel(32);
        (
            Self {
                inbound_rx,
                outbound_tx,
            },
            HttpAdapterHandle {
                inbound_tx,
                outbound_rx,
            },
        )
    }
}

#[async_trait]
impl PlatformAdapter for HttpPlatformAdapter {
    fn platform_name(&self) -> &str {
        "http"
    }

    async fn connect(&mut self) -> Result<()> {
        Ok(())
    }

    async fn disconnect(&mut self) -> Result<()> {
        Ok(())
    }

    async fn send(&self, msg: &OutboundMessage) -> Result<Option<String>> {
        let id = uuid::Uuid::new_v4().to_string();
        self.outbound_tx.send(msg.clone()).await?;
        Ok(Some(id))
    }

    async fn send_typing(&self, _chat_id: &str) -> Result<()> {
        Ok(())
    }

    async fn edit_message(&self, _chat_id: &str, _message_id: &str, _text: &str) -> Result<()> {
        Ok(())
    }

    fn interrupt_session(&self, _session_key: &str) {}

    async fn recv(&mut self) -> Result<MessageEvent> {
        self.inbound_rx
            .recv()
            .await
            .ok_or_else(|| anyhow::anyhow!("channel closed"))
    }
}
