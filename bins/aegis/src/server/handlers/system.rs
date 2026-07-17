use crate::server::error::ApiError;
use crate::server::state::{AppState, ChatMessage};
use axum::extract::State;
use axum::response::sse::{Event, Sse};
use axum::Json;
use futures::stream::{self, Stream};
use futures::FutureExt;
use serde::Serialize;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

/// Health check response.
#[derive(Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub version: &'static str,
}

/// Version info response.
#[derive(Serialize)]
pub struct VersionResponse {
    pub version: &'static str,
    pub model: String,
}

/// Liveness probe — always returns 200 OK.
pub async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
    })
}

/// Returns version and current model info.
pub async fn version(State(state): State<Arc<AppState>>) -> Json<VersionResponse> {
    Json(VersionResponse {
        version: env!("CARGO_PKG_VERSION"),
        model: state.config.model.default.clone(),
    })
}

/// Chat endpoint — sends a message to the agent and returns the response.
#[derive(serde::Deserialize)]
pub struct ChatRequest {
    pub message: String,
}

/// Chat response.
#[derive(Serialize)]
pub struct ChatResponse {
    pub response: String,
}

/// Processes a chat message and returns the full response.
///
/// Sends the message to the agent thread via channel and waits for the reply.
pub async fn chat(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ChatRequest>,
) -> Result<Json<ChatResponse>, ApiError> {
    if req.message.trim().is_empty() {
        return Err(ApiError::BadRequest("message cannot be empty".into()));
    }
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    state
        .agent_tx
        .send(ChatMessage {
            content: req.message,
            reply: reply_tx,
        })
        .await
        .map_err(|_| ApiError::AgentError("agent thread unavailable".into()))?;
    let response = reply_rx
        .await
        .map_err(|_| ApiError::AgentError("agent dropped response".into()))?;
    Ok(Json(ChatResponse { response }))
}

/// SSE streaming chat endpoint.
///
/// Sends the full response as a single SSE `message` event, then a `done` event.
/// Future versions will support token-by-token streaming as the provider
/// supports streaming responses.
pub async fn chat_stream(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ChatRequest>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    if req.message.trim().is_empty() {
        return Err(ApiError::BadRequest("message cannot be empty".into()));
    }

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    state
        .agent_tx
        .send(ChatMessage {
            content: req.message,
            reply: reply_tx,
        })
        .await
        .map_err(|_| ApiError::AgentError("agent thread unavailable".into()))?;

    // Build SSE stream: wait for the reply, then emit events
    let sse_stream = async move {
        match reply_rx.await {
            Ok(response) => {
                let data = serde_json::json!({"content": response}).to_string();
                let events: Vec<Result<Event, Infallible>> = vec![
                    Ok(Event::default().event("message").data(&data)),
                    Ok(Event::default().event("done").data("")),
                ];
                stream::iter(events)
            }
            Err(_) => {
                let data = serde_json::json!({"error": "agent dropped response"}).to_string();
                let events: Vec<Result<Event, Infallible>> =
                    vec![Ok(Event::default().event("error").data(&data))];
                stream::iter(events)
            }
        }
    }
    .flatten_stream();

    Ok(Sse::new(sse_stream).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_response_serde() {
        let resp = HealthResponse {
            status: "ok",
            version: "1.0.0",
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"status\":\"ok\""));
        assert!(json.contains("\"version\":\"1.0.0\""));
    }

    #[test]
    fn chat_request_deserialize() {
        let json = r#"{"message":"hello"}"#;
        let req: ChatRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.message, "hello");
    }

    #[test]
    fn chat_request_empty_fails_validation() {
        let req: ChatRequest = serde_json::from_str(r#"{"message":""}"#).unwrap();
        assert!(req.message.trim().is_empty());
    }

    #[test]
    fn version_response_serde() {
        let resp = VersionResponse {
            version: "2.0.0",
            model: "gpt-4o".into(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("gpt-4o"));
    }

    #[test]
    fn chat_response_serde() {
        let resp = ChatResponse {
            response: "hello!".into(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("hello!"));
    }
}
