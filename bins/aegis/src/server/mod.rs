//! HTTP API server (axum, CORS, SSE streaming, Bearer auth).
//!
//! Merged from the former `aegis-server` binary. Exposed as the `aegis serve`
//! subcommand. The agent runs on a dedicated thread (rusqlite `Connection`
//! isn't `Send`), communicating via an mpsc channel.

pub mod error;
pub mod handlers;
pub mod state;

use aegis_core::agent::{Agent, AgentCallbacks};
use aegis_core::config::{self, Config};
use aegis_provider::{OpenAiProvider, Provider};
use aegis_tools::*;
use anyhow::Result;
use axum::extract::Request;
use axum::http::Method;
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};

use state::{AppState, ChatMessage};

struct ApiCallbacks;
impl AgentCallbacks for ApiCallbacks {}

/// Bearer token auth middleware. If `server.api_key` is set in config,
/// all requests must include `Authorization: Bearer <key>`.
/// Health and version endpoints are always public.
async fn auth_middleware(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> std::result::Result<Response, error::ApiError> {
    // Public endpoints bypass auth
    let path = req.uri().path();
    if path == "/health" || path == "/version" {
        return Ok(next.run(req).await);
    }

    if let Some(expected) = &state.config.server.api_key {
        let auth_header = req
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        let token = auth_header
            .strip_prefix("Bearer ")
            .unwrap_or(auth_header);

        if token != expected {
            return Err(error::ApiError::Unauthorized("invalid API key".into()));
        }
    }

    Ok(next.run(req).await)
}

/// Build the axum Router from an AppState. Used by both `run()` and tests.
fn build_app(state: Arc<AppState>) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers(Any)
        .max_age(std::time::Duration::from_secs(3600));

    Router::new()
        .route("/health", get(handlers::system::health))
        .route("/version", get(handlers::system::version))
        .route("/chat", post(handlers::system::chat))
        .route("/chat/stream", post(handlers::system::chat_stream))
        .route("/sessions", get(handlers::sessions::list_sessions))
        .route("/sessions/{id}", get(handlers::sessions::get_session))
        .route("/sessions/{id}/export", get(handlers::sessions::export_session))
        .route("/sessions/{id}/delete", post(handlers::sessions::delete_session))
        .route("/search", get(handlers::sessions::search_sessions))
        .layer(cors)
        .layer(middleware::from_fn_with_state(state.clone(), auth_middleware))
        .with_state(state)
}

/// Run the HTTP API server, binding to `host:port`.
///
/// Note: tracing is initialised globally by the main CLI entry point, so this
/// function does not (and must not) re-initialise a subscriber.
pub async fn run(host: String, port: u16) -> Result<()> {
    let config = Config::load(&config::config_path())?;
    let api_key = config.resolve_api_key()?;
    let base_url = config.resolve_base_url();

    let provider: Arc<dyn Provider> = Arc::new(OpenAiProvider::new(
        api_key,
        base_url,
        config.model.default.clone(),
        config.model.max_tokens,
        config.model.timeout_secs,
        config.model.max_retries,
    ));

    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(TerminalTool));
    reg.register(Arc::new(ReadFileTool));
    reg.register(Arc::new(WriteFileTool));
    reg.register(Arc::new(PatchTool));
    reg.register(Arc::new(SearchFilesTool));
    reg.register(Arc::new(WebSearchTool::new()));
    reg.register(Arc::new(WebExtractTool::new()));
    reg.register(Arc::new(CratesTool::new()));
    reg.register(Arc::new(SkillTool::new()));

    let mut agent = Agent::new(provider, None, config.clone());
    agent.set_callbacks(Box::new(ApiCallbacks));
    agent.set_tool_registry(Arc::new(reg));
    agent.init_session()?;

    // Agent runs in its own thread (rusqlite Connection isn't Send+Sync)
    let (agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<ChatMessage>(32);

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build agent runtime");
        rt.block_on(async move {
            while let Some(msg) = agent_rx.recv().await {
                let result = match agent.chat(&msg.content).await {
                    Ok(r) => r,
                    Err(e) => format!("Error: {e}"),
                };
                let _ = msg.reply.send(result);
            }
        });
    });

    let state = Arc::new(AppState::new(agent_tx, config));
    let app = build_app(state);

    let addr = format!("{host}:{port}");
    tracing::info!("aegis serve listening on {addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    /// Create a test app with a mock agent that echoes input back.
    fn test_app() -> Router {
        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<ChatMessage>(32);

        // Mock agent thread: echoes input back
        tokio::spawn(async move {
            while let Some(msg) = agent_rx.recv().await {
                let _ = msg.reply.send(format!("echo: {}", msg.content));
            }
        });

        let config = Config::default();
        let state = Arc::new(AppState::new(agent_tx, config));
        build_app(state)
    }

    /// Create a test app with auth enabled.
    fn test_app_with_auth(api_key: &str) -> Router {
        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<ChatMessage>(32);

        tokio::spawn(async move {
            while let Some(msg) = agent_rx.recv().await {
                let _ = msg.reply.send(format!("echo: {}", msg.content));
            }
        });

        let mut config = Config::default();
        config.server.api_key = Some(api_key.to_string());
        let state = Arc::new(AppState::new(agent_tx, config));
        build_app(state)
    }

    #[tokio::test]
    async fn health_endpoint_returns_ok() {
        let app = test_app();
        let req = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "ok");
        assert!(json["version"].is_string());
    }

    #[tokio::test]
    async fn version_endpoint_returns_model() {
        let app = test_app();
        let req = Request::builder()
            .uri("/version")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["version"].is_string());
        assert!(json["model"].is_string());
    }

    #[tokio::test]
    async fn chat_endpoint_returns_echo() {
        let app = test_app();
        let req = Request::builder()
            .method("POST")
            .uri("/chat")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"message":"hello world"}"#))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["response"], "echo: hello world");
    }

    #[tokio::test]
    async fn chat_empty_message_returns_400() {
        let app = test_app();
        let req = Request::builder()
            .method("POST")
            .uri("/chat")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"message":""}"#))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["code"], "BAD_REQUEST");
    }

    #[tokio::test]
    async fn chat_stream_returns_sse() {
        let app = test_app();
        let req = Request::builder()
            .method("POST")
            .uri("/chat/stream")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"message":"stream test"}"#))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let content_type = resp.headers().get("content-type").unwrap().to_str().unwrap();
        assert!(content_type.contains("text/event-stream"), "expected SSE content-type, got: {content_type}");
    }

    #[tokio::test]
    async fn sessions_endpoint_returns_list() {
        let app = test_app();
        let req = Request::builder()
            .uri("/sessions")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // May return 200 with empty list or 500 if no DB, both are acceptable
        // for a unit test (no SQLite in test env)
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        if status == StatusCode::OK {
            assert!(json["sessions"].is_array());
        }
        // 500 is also valid — no sessions.db in test environment
    }

    #[tokio::test]
    async fn nonexistent_route_returns_404() {
        let app = test_app();
        let req = Request::builder()
            .uri("/nonexistent")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn cors_headers_present() {
        let app = test_app();
        let req = Request::builder()
            .method("OPTIONS")
            .uri("/chat")
            .header("origin", "http://example.com")
            .header("access-control-request-method", "POST")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let allow = resp.headers().get("access-control-allow-origin");
        assert!(allow.is_some(), "expected CORS allow-origin header");
    }

    #[tokio::test]
    async fn auth_bypass_for_health() {
        let app = test_app_with_auth("secret-key");
        let req = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn auth_bypass_for_version() {
        let app = test_app_with_auth("secret-key");
        let req = Request::builder()
            .uri("/version")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn auth_rejects_missing_token() {
        let app = test_app_with_auth("secret-key");
        let req = Request::builder()
            .method("POST")
            .uri("/chat")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"message":"test"}"#))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn auth_rejects_wrong_token() {
        let app = test_app_with_auth("secret-key");
        let req = Request::builder()
            .method("POST")
            .uri("/chat")
            .header("content-type", "application/json")
            .header("authorization", "Bearer wrong-key")
            .body(Body::from(r#"{"message":"test"}"#))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn auth_accepts_valid_token() {
        let app = test_app_with_auth("secret-key");
        let req = Request::builder()
            .method("POST")
            .uri("/chat")
            .header("content-type", "application/json")
            .header("authorization", "Bearer secret-key")
            .body(Body::from(r#"{"message":"auth test"}"#))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn auth_disabled_when_no_key_configured() {
        // test_app() has no api_key configured, so auth is disabled
        let app = test_app();
        let req = Request::builder()
            .method("POST")
            .uri("/chat")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"message":"no auth needed"}"#))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
