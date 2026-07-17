use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response, Sse},
    routing::{get, post},
    Json, Router,
};
use futures::StreamExt;
use std::sync::Arc;
use std::time::Duration;
use tokio_stream::wrappers::ReceiverStream;

use crate::{
    auth::ChainAuthProvider,
    task_manager::TaskManager,
    types::{
        AgentCard, JsonRpcRequest, JsonRpcResponse, TaskCancelParams, TaskGetParams, TaskSendParams,
    },
};

pub struct A2AServer {
    pub agent_card: AgentCard,
    pub task_manager: Arc<dyn TaskManager>,
    pub auth: Option<Arc<ChainAuthProvider>>,
}

impl A2AServer {
    /// Creates a new `instance`.
    pub fn new(agent_card: AgentCard, task_manager: Arc<dyn TaskManager>) -> Self {
        Self {
            agent_card,
            task_manager,
            auth: None,
        }
    }

    /// Enables authentication for this instance.
    pub fn with_auth(mut self, auth: Arc<ChainAuthProvider>) -> Self {
        self.auth = Some(auth);
        self
    }

    /// Starts the HTTP server on the given address.
    pub async fn serve(self, addr: &str) -> anyhow::Result<()> {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        tracing::info!("A2A server listening on {}", addr);
        let router = Arc::new(self).router();
        axum::serve(listener, router).await?;
        Ok(())
    }

    fn router(self: Arc<Self>) -> Router {
        Router::new()
            .route("/", post(handle_jsonrpc))
            .route("/.well-known/agent.json", get(handle_agent_card))
            .route("/.well-known/agent-card.json", get(handle_agent_card))
            .route("/health", get(handle_health))
            .with_state(self)
    }
}

async fn handle_health() -> impl IntoResponse {
    Json(serde_json::json!({"status": "ok"}))
}

async fn handle_agent_card(State(server): State<Arc<A2AServer>>) -> impl IntoResponse {
    Json(server.agent_card.clone())
}

async fn handle_jsonrpc(
    State(server): State<Arc<A2AServer>>,
    headers: HeaderMap,
    Json(req): Json<JsonRpcRequest>,
) -> Response {
    // Auth check
    if let Some(auth) = &server.auth {
        if !auth.verify(&headers) {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    }

    match req.method.as_str() {
        "tasks/send" | "message/send" => {
            let params: TaskSendParams = match parse_params(&req) {
                Ok(p) => p,
                Err(e) => return json_err(req.id, -32602, e).into_response(),
            };
            match server.task_manager.on_send(params).await {
                Ok(task) => json_ok(req.id, task).into_response(),
                Err(e) => json_err(req.id, -32000, e.to_string()).into_response(),
            }
        }
        "tasks/get" => {
            let params: TaskGetParams = match parse_params(&req) {
                Ok(p) => p,
                Err(e) => return json_err(req.id, -32602, e).into_response(),
            };
            match server.task_manager.on_get(params).await {
                Ok(task) => json_ok(req.id, task).into_response(),
                Err(e) => json_err(req.id, -32000, e.to_string()).into_response(),
            }
        }
        "tasks/cancel" => {
            let params: TaskCancelParams = match parse_params(&req) {
                Ok(p) => p,
                Err(e) => return json_err(req.id, -32602, e).into_response(),
            };
            match server.task_manager.on_cancel(params).await {
                Ok(task) => json_ok(req.id, task).into_response(),
                Err(e) => json_err(req.id, -32000, e.to_string()).into_response(),
            }
        }
        "tasks/sendSubscribe" | "message/stream" | "tasks/resubscribe" => {
            let params: TaskSendParams = match parse_params(&req) {
                Ok(p) => p,
                Err(e) => return json_err(req.id, -32602, e).into_response(),
            };
            match server.task_manager.on_subscribe(params).await {
                Ok(event_stream) => {
                    let (tx, rx) = tokio::sync::mpsc::channel(64);
                    tokio::spawn(async move {
                        let mut stream = event_stream;
                        let mut batch: Vec<crate::types::TaskEvent> = Vec::new();
                        let mut interval = tokio::time::interval(Duration::from_millis(50));
                        loop {
                            tokio::select! {
                                Some(event) = stream.next() => {
                                    batch.push(event);
                                    if batch.len() >= 5 {
                                        for e in batch.drain(..) {
                                            let data = serde_json::to_string(&e).unwrap_or_default();
                                            if tx.send(Ok::<_, std::convert::Infallible>(
                                                axum::response::sse::Event::default().data(data)
                                            )).await.is_err() {
                                                break;
                                            }
                                        }
                                    }
                                }
                                _ = interval.tick() => {
                                    for e in batch.drain(..) {
                                        let data = serde_json::to_string(&e).unwrap_or_default();
                                        if tx.send(Ok::<_, std::convert::Infallible>(
                                            axum::response::sse::Event::default().data(data)
                                        )).await.is_err() {
                                            break;
                                        }
                                    }
                                }
                                else => break,
                            }
                        }
                    });
                    let sse_stream = ReceiverStream::new(rx);
                    Sse::new(sse_stream).into_response()
                }
                Err(e) => json_err(req.id, -32000, e.to_string()).into_response(),
            }
        }
        "agent/getAuthenticatedExtendedCard" => json_ok(req.id, &server.agent_card).into_response(),
        _ => json_err(req.id, -32601, "Method not found").into_response(),
    }
}

fn parse_params<T: serde::de::DeserializeOwned>(req: &JsonRpcRequest) -> Result<T, String> {
    let params = req.params.clone().unwrap_or(serde_json::Value::Null);
    serde_json::from_value(params).map_err(|e| e.to_string())
}

fn json_ok<T: serde::Serialize>(id: Option<serde_json::Value>, val: T) -> Json<JsonRpcResponse> {
    let result = serde_json::to_value(val).unwrap_or(serde_json::Value::Null);
    Json(JsonRpcResponse::ok(id, result))
}

fn json_err(
    id: Option<serde_json::Value>,
    code: i32,
    msg: impl Into<String>,
) -> Json<JsonRpcResponse> {
    Json(JsonRpcResponse::err(id, code, msg))
}
