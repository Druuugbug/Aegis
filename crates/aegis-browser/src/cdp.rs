use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio::sync::{oneshot, Mutex};
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct Pending {
    tx: oneshot::Sender<Value>,
}

pub struct CdpConnection {
    writer: Arc<Mutex<futures::stream::SplitSink<WsStream, Message>>>,
    next_id: Arc<std::sync::atomic::AtomicU32>,
    pending: Arc<Mutex<HashMap<u32, Pending>>>,
    _reader_handle: tokio::task::JoinHandle<()>,
}

impl CdpConnection {
    pub async fn connect(ws_url: &str) -> Result<Self> {
        let (ws, _) = connect_async(ws_url)
            .await
            .with_context(|| format!("failed to connect CDP at {ws_url}"))?;

        let (writer, reader) = ws.split();
        let writer = Arc::new(Mutex::new(writer));
        let next_id = Arc::new(std::sync::atomic::AtomicU32::new(1));
        let pending: Arc<Mutex<HashMap<u32, Pending>>> = Arc::new(Mutex::new(HashMap::new()));

        let pending_clone = pending.clone();
        let _reader_handle = tokio::spawn(async move {
            Self::read_loop(reader, pending_clone).await;
        });

        Ok(Self {
            writer,
            next_id,
            pending,
            _reader_handle,
        })
    }

    async fn read_loop(
        mut reader: futures::stream::SplitStream<WsStream>,
        pending: Arc<Mutex<HashMap<u32, Pending>>>,
    ) {
        while let Some(Ok(msg)) = reader.next().await {
            let Message::Text(text) = msg else {
                continue;
            };
            let Ok(val) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if let Some(id) = val.get("id").and_then(|v| v.as_u64()) {
                let mut map = pending.lock().await;
                if let Some(p) = map.remove(&(id as u32)) {
                    let _ = p.tx.send(val);
                }
            }
        }
    }

    pub async fn send(&self, method: &str, params: Value) -> Result<Value> {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let msg = json!({
            "id": id,
            "method": method,
            "params": params,
        });

        let (tx, rx) = oneshot::channel();
        {
            let mut map = self.pending.lock().await;
            map.insert(id, Pending { tx });
        }

        {
            let mut w = self.writer.lock().await;
            w.send(Message::Text(msg.to_string()))
                .await
                .context("CDP send failed")?;
        }

        let response = tokio::time::timeout(std::time::Duration::from_secs(30), rx)
            .await
            .context("CDP response timeout")??;

        if let Some(err) = response.get("error") {
            anyhow::bail!("CDP error: {}", err);
        }

        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    }
}

impl Drop for CdpConnection {
    fn drop(&mut self) {
        self._reader_handle.abort();
    }
}
