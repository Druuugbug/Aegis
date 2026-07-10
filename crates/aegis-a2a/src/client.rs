use std::pin::Pin;
use std::time::Duration;

use futures::Stream;
use reqwest::Client;
use reqwest_eventsource::{Event, EventSource};

use crate::types::*;

pub struct A2AClient {
    base_url: String,
    client: Client,
    token: Option<String>,
}

impl A2AClient {
    /// Creates a new `instance`.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            client: Client::new(),
            token: None,
        }
    }

    /// Sets a bearer token for authentication.
    pub fn with_bearer_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    fn build_request(&self, method: &str, params: serde_json::Value) -> reqwest::RequestBuilder {
        let body = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(serde_json::Value::Number(1.into())),
            method: method.into(),
            params: Some(params),
        };
        let mut req = self.client.post(&self.base_url).json(&body);
        if let Some(token) = &self.token {
            req = req.bearer_auth(token);
        }
        req
    }

    async fn rpc<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<T> {
        let resp = self
            .build_request(method, params)
            .send()
            .await?
            .json::<JsonRpcResponse>()
            .await?;

        if let Some(err) = resp.error {
            return Err(anyhow::anyhow!("RPC error {}: {}", err.code, err.message));
        }

        let result = resp.result.ok_or_else(|| anyhow::anyhow!("No result in response"))?;
        serde_json::from_value(result).map_err(|e| anyhow::anyhow!("Deserialize error: {}", e))
    }

    /// Submits a new task to the agent.
    pub async fn submit(&self, params: TaskSendParams) -> anyhow::Result<Task> {
        let v = serde_json::to_value(params)?;
        self.rpc("message/send", v).await
    }

    /// Retrieves a task by its parameters.
    pub async fn get(&self, params: TaskGetParams) -> anyhow::Result<Task> {
        let v = serde_json::to_value(params)?;
        self.rpc("tasks/get", v).await
    }

    /// Cancels a running task.
    pub async fn cancel(&self, params: TaskCancelParams) -> anyhow::Result<Task> {
        let v = serde_json::to_value(params)?;
        self.rpc("tasks/cancel", v).await
    }

    /// Subscribes to task status updates.
    pub async fn subscribe(
        &self,
        params: TaskSendParams,
    ) -> anyhow::Result<Pin<Box<dyn Stream<Item = anyhow::Result<TaskEvent>> + Send>>> {
        let body = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(serde_json::Value::Number(1.into())),
            method: "message/stream".into(),
            params: Some(serde_json::to_value(params)?),
        };

        let mut req_builder = self
            .client
            .post(&self.base_url)
            .json(&body);

        if let Some(token) = &self.token {
            req_builder = req_builder.bearer_auth(token);
        }

        let mut es = EventSource::new(req_builder)
            .map_err(|e| anyhow::anyhow!("EventSource error: {}", e))?;

        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let mut retry_count = 0u32;
        const MAX_RETRIES: u32 = 3;

        tokio::spawn(async move {
            use futures::StreamExt;
            loop {
                match es.next().await {
                    Some(Ok(Event::Message(msg))) => {
                        retry_count = 0;
                        match serde_json::from_str::<TaskEvent>(&msg.data) {
                            Ok(event) => {
                                if tx.send(Ok(event)).await.is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                let _ = tx.send(Err(anyhow::anyhow!("Parse error: {}", e))).await;
                            }
                        }
                    }
                    Some(Ok(Event::Open)) => {}
                    Some(Err(e)) => {
                        retry_count += 1;
                        if retry_count > MAX_RETRIES {
                            let _ = tx.send(Err(anyhow::anyhow!("SSE error after {} retries: {}", MAX_RETRIES, e))).await;
                            break;
                        }
                        let backoff = Duration::from_millis(100 * (1 << retry_count));
                        tokio::time::sleep(backoff).await;
                    }
                    None => break,
                }
            }
        });

        let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        Ok(Box::pin(stream))
    }
}
