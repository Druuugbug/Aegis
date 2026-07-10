use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use serde_json::Value;
use tokio::sync::Mutex;
use tracing::{info, warn};

// ─── Core Types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct McpRequest {
    pub method: String,
    pub params: Value,
    pub id: Option<Value>,
    pub meta: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct McpResponse {
    pub result: Option<Value>,
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
}

// ─── Middleware Types ─────────────────────────────────────────────────────────

pub type Next = Box<
    dyn FnOnce(McpRequest) -> Pin<Box<dyn Future<Output = McpResponse> + Send>>
        + Send,
>;

pub type Middleware = Arc<
    dyn Fn(McpRequest, Next) -> Pin<Box<dyn Future<Output = McpResponse> + Send>>
        + Send
        + Sync,
>;

// ─── MiddlewareStack ──────────────────────────────────────────────────────────

pub struct MiddlewareStack {
    middlewares: Vec<Middleware>,
}

impl Default for MiddlewareStack {
    fn default() -> Self {
        Self::new()
    }
}

impl MiddlewareStack {
    /// Create an empty middleware stack.
    pub fn new() -> Self {
        Self { middlewares: Vec::new() }
    }

    /// Add a middleware to the end of the stack.
    pub fn use_middleware(&mut self, mw: Middleware) {
        self.middlewares.push(mw);
    }

    /// Execute a request through all middlewares and the final handler, in onion order.
    pub async fn execute(&self, req: McpRequest, handler: Next) -> McpResponse {
        // Build the onion: wrap handler with middlewares in reverse order
        let mut next: Next = handler;
        for mw in self.middlewares.iter().rev() {
            let mw = mw.clone();
            let prev = next;
            next = Box::new(move |r: McpRequest| {
                let fut = mw(r, prev);
                Box::pin(fut) as Pin<Box<dyn Future<Output = McpResponse> + Send>>
            });
        }
        next(req).await
    }
}

// ─── Built-in Middlewares ─────────────────────────────────────────────────────

/// Create a middleware that logs every MCP request and response at info level.
pub fn logging_middleware() -> Middleware {
    Arc::new(|req: McpRequest, next: Next| {
        Box::pin(async move {
            info!(method = %req.method, id = ?req.id, "MCP request");
            let resp = next(req).await;
            if let Some(ref e) = resp.error {
                warn!(code = e.code, message = %e.message, "MCP error response");
            } else {
                info!("MCP response ok");
            }
            resp
        })
    })
}

/// Create a middleware that rejects requests without a valid Bearer token.
pub fn auth_middleware(token: String) -> Middleware {
    Arc::new(move |req: McpRequest, next: Next| {
        let token = token.clone();
        Box::pin(async move {
            if let Some(provided) = req.meta.get("authorization") {
                if provided == &format!("Bearer {token}") {
                    return next(req).await;
                }
            }
            McpResponse {
                result: None,
                error: Some(JsonRpcError {
                    code: -32001,
                    message: "Unauthorized".to_string(),
                }),
            }
        })
    })
}

/// Create a middleware that throttles requests to the given rate (requests per second).
pub fn rate_limit_middleware(requests_per_second: u32) -> Middleware {
    let interval = std::time::Duration::from_micros(1_000_000 / requests_per_second.max(1) as u64);
    let last = Arc::new(Mutex::new(std::time::Instant::now()));
    Arc::new(move |req: McpRequest, next: Next| {
        let last = last.clone();
        Box::pin(async move {
            let mut guard = last.lock().await;
            let now = std::time::Instant::now();
            if now < *guard + interval {
                let wait = (*guard + interval) - now;
                tokio::time::sleep(wait).await;
            }
            *guard = std::time::Instant::now();
            drop(guard);
            next(req).await
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_req(method: &str) -> McpRequest {
        McpRequest {
            method: method.to_string(),
            params: json!({}),
            id: Some(json!(1)),
            meta: HashMap::new(),
        }
    }

    fn ok_handler() -> Next {
        Box::new(|_req: McpRequest| {
            Box::pin(async move {
                McpResponse {
                    result: Some(json!({"ok": true})),
                    error: None,
                }
            })
        })
    }

    #[tokio::test]
    async fn test_no_middleware_passes_through() {
        let stack = MiddlewareStack::new();
        let resp = stack.execute(make_req("test"), ok_handler()).await;
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());
    }

    #[tokio::test]
    async fn test_auth_middleware_rejects_without_token() {
        let mut stack = MiddlewareStack::new();
        stack.use_middleware(auth_middleware("secret123".to_string()));
        let resp = stack.execute(make_req("test"), ok_handler()).await;
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[tokio::test]
    async fn test_auth_middleware_accepts_valid_token() {
        let mut stack = MiddlewareStack::new();
        stack.use_middleware(auth_middleware("secret123".to_string()));
        let mut req = make_req("test");
        req.meta.insert("authorization".to_string(), "Bearer secret123".to_string());
        let resp = stack.execute(req, ok_handler()).await;
        assert!(resp.result.is_some());
    }

    #[tokio::test]
    async fn test_middleware_execution_order() {
        let mut stack = MiddlewareStack::new();
        // First middleware adds a header
        stack.use_middleware(Arc::new(|mut req: McpRequest, next: Next| {
            Box::pin(async move {
                req.meta.insert("x-added".to_string(), "yes".to_string());
                next(req).await
            })
        }));
        // Second middleware checks the header was added
        stack.use_middleware(Arc::new(|req: McpRequest, next: Next| {
            Box::pin(async move {
                if req.meta.get("x-added").map(|s| s.as_str()) == Some("yes") {
                    next(req).await
                } else {
                    McpResponse {
                        result: None,
                        error: Some(JsonRpcError { code: -1, message: "missing header".to_string() }),
                    }
                }
            })
        }));
        let resp = stack.execute(make_req("test"), ok_handler()).await;
        assert!(resp.result.is_some());
    }

    #[tokio::test]
    async fn test_logging_middleware_does_not_block() {
        let mut stack = MiddlewareStack::new();
        stack.use_middleware(logging_middleware());
        let resp = stack.execute(make_req("test"), ok_handler()).await;
        assert!(resp.result.is_some());
    }
}
