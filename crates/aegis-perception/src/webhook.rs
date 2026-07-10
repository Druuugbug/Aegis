use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// 简单的 HTTP Webhook 接收器
pub struct WebhookServer {
    pub port: u16,
}

impl WebhookServer {
    /// Creates a new `instance`.
    pub fn new(port: u16) -> Self {
        Self { port }
    }

    /// 启动监听，每次收到 POST 请求时调用 callback
    pub async fn serve<F, Fut>(&self, callback: F) -> Result<()>
    where
        F: Fn(String) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let listener = TcpListener::bind(format!("0.0.0.0:{}", self.port)).await?;
        tracing::info!("Webhook server listening on port {}", self.port);
        loop {
            let (mut socket, addr) = listener.accept().await?;
            tracing::debug!("Webhook request from {addr}");
            let mut buf = vec![0u8; 8192];
            let n = socket.read(&mut buf).await.unwrap_or(0);
            let raw = String::from_utf8_lossy(&buf[..n]).to_string();
            // 提取 HTTP body（简单分隔第一个\r\n\r\n后的内容）
            let body = if let Some(pos) = raw.find("\r\n\r\n") {
                raw[pos + 4..].to_string()
            } else {
                raw.clone()
            };
            let resp = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK";
            let _ = socket.write_all(resp).await;
            callback(body).await;
        }
    }
}
