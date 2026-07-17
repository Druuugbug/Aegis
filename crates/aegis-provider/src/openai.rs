use crate::rate_limit::is_rate_limited;
use crate::traits::{Provider, StreamEvent, StreamResult};
use aegis_types::message::{Content, LlmResponse, Message, Role, ToolCall, Usage};
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::Value;
use std::time::Duration;
use thiserror::Error;
use tracing::{debug, warn};

// ── Error classification ──

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("authentication error: {0}")]
    Auth(String),
    #[error("rate limited (retry after {retry_after_secs:?}s)")]
    RateLimit { retry_after_secs: Option<u64> },
    /// Quota / usage window exhausted (e.g. MiniMax `base_resp` 2056). Retryable
    /// by *waiting* the full `retry_after_secs` until the window resets, rather
    /// than a short exponential backoff.
    #[error("quota exhausted (retry after {retry_after_secs:?}s)")]
    QuotaExhausted { retry_after_secs: Option<u64> },
    /// Insufficient balance (e.g. MiniMax `base_resp` 1008). Not retryable —
    /// waiting does not help; the account needs to be topped up.
    #[error("insufficient balance")]
    InsufficientBalance,
    #[error("server error {status}: {body}")]
    Server { status: u16, body: String },
    #[error("context length exceeded")]
    ContextLength,
    #[error("timeout after {0}s")]
    Timeout(u64),
    #[error("network error: {0}")]
    Network(String),
}

impl ProviderError {
    fn from_status(status: u16, body: &str, retry_after: Option<u64>) -> Self {
        match status {
            401 | 403 => Self::Auth(body.to_string()),
            429 => Self::RateLimit {
                retry_after_secs: retry_after,
            },
            _ if body.contains("context_length_exceeded") => Self::ContextLength,
            _ => Self::Server {
                status,
                body: body.to_string(),
            },
        }
    }

    /// Whether this error is worth retrying.
    ///
    /// `QuotaExhausted` is retryable in the sense that waiting until the reset
    /// window will succeed; `InsufficientBalance` is not (top-up required).
    fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::RateLimit { .. }
                | Self::QuotaExhausted { .. }
                | Self::Server { .. }
                | Self::Timeout(_)
                | Self::Network(_)
        )
    }
}

// ── Provider ──

pub struct OpenAiProvider {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
    model: String,
    max_tokens: u32,
    timeout: Duration,
    max_retries: u32,
    /// Timezone offset (seconds from UTC) used to compute MiniMax fixed-window
    /// quota reset times. Defaults to [`crate::minimax::DEFAULT_TZ_OFFSET_SECS`].
    quota_tz_offset_secs: i64,
}

impl OpenAiProvider {
    /// Creates a new `instance`.
    pub fn new(
        api_key: String,
        base_url: String,
        model: String,
        max_tokens: u32,
        timeout_secs: u64,
        max_retries: u32,
    ) -> Self {
        // Historically we set `.timeout(timeout_secs)` at the client level.
        // In reqwest, `ClientBuilder::timeout` is the *entire request*
        // deadline — including reading the streaming response body. Long
        // generations (5000-word articles with reasoning + tool calls on
        // MiniMax-M3 / Anthropic thinking) legitimately need more than 120s
        // of wall-clock to finish streaming, and were being killed
        // mid-stream with `error decoding response body`.
        //
        // Split the two concerns:
        //   * `connect_timeout` — fail fast if the endpoint is unreachable.
        //   * `read_timeout`    — per-read stall detection; if the server
        //                         stops sending data for `timeout_secs`,
        //                         abort. Keeps the "hung connection"
        //                         guarantee without capping total duration.
        //   * total request timeout is applied per-request on the
        //     *non-streaming* path via `RequestBuilder::timeout` (see
        //     `do_request`). The streaming path relies on `read_timeout`
        //     plus the existing 30s SSE stall check in `chat_stream`.
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(Duration::from_secs(timeout_secs))
            .build()
            .expect("failed to build HTTP client");
        Self {
            client,
            api_key,
            base_url: base_url.trim_end_matches('/').to_string(),
            model,
            max_tokens,
            timeout: Duration::from_secs(timeout_secs),
            max_retries,
            quota_tz_offset_secs: crate::minimax::DEFAULT_TZ_OFFSET_SECS,
        }
    }

    /// Override the timezone offset (seconds from UTC) used to compute MiniMax
    /// fixed 5-hour window reset times. Only relevant for MiniMax token plans;
    /// harmless for other OpenAI-compatible providers.
    pub fn with_quota_window_tz(mut self, offset_secs: i64) -> Self {
        self.quota_tz_offset_secs = offset_secs;
        self
    }

    fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }

    /// Inspect a (HTTP-successful) response body for a MiniMax `base_resp`
    /// error. MiniMax's OpenAI-compatible endpoint often returns `HTTP 200`
    /// with the real status in `base_resp.status_code`, so this must run on the
    /// success path before parsing `choices`.
    ///
    /// Returns `None` for standard OpenAI bodies (no `base_resp`) or success,
    /// so it is safe against real OpenAI responses.
    fn detect_minimax_error(&self, body: &str) -> Option<ProviderError> {
        let outcome = crate::minimax::classify_body(body)?;
        minimax_outcome_to_error(outcome, self.quota_tz_offset_secs)
    }

    /// True when this endpoint is MiniMax (fixed 5-hour quota windows), detected
    /// from the base URL. Relays reselling MiniMax may need explicit config.
    fn quota_dialect_is_minimax(&self) -> bool {
        self.base_url.contains("minimax")
    }

    /// Build a structured error for a rate-limited / quota response.
    ///
    /// Covers both MiniMax's native `base_resp` (which can surface even on a
    /// non-200 response) and the standard OpenAI-style `HTTP 429`. When a 429
    /// carries no usable `Retry-After` and this is MiniMax, the wait is computed
    /// from the next fixed 5-hour window boundary rather than guessing 60s.
    fn rate_limit_error(&self, headers: &reqwest::header::HeaderMap, text: &str) -> ProviderError {
        // MiniMax may embed base_resp even on a non-200 response.
        if let Some(err) = self.detect_minimax_error(text) {
            return err;
        }
        // Standard OpenAI-style: honor an explicit reset hint if the server sent one.
        if let Some(d) = crate::rate_limit::parse_retry_after_opt(headers) {
            return ProviderError::RateLimit {
                retry_after_secs: Some(d.as_secs()),
            };
        }
        // No reset hint. MiniMax fixed windows send a bare 429 → compute the boundary.
        if self.quota_dialect_is_minimax() {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let wait = crate::minimax::wait_until_next_window(
                now,
                self.quota_tz_offset_secs,
                Duration::from_secs(30),
            );
            return ProviderError::QuotaExhausted {
                retry_after_secs: Some(wait.as_secs()),
            };
        }
        ProviderError::RateLimit {
            retry_after_secs: None,
        }
    }

    fn build_body(&self, messages: &[Message], tools: Option<&Value>, stream: bool) -> Value {
        let msgs: Vec<Value> = messages.iter().map(msg_to_openai_value).collect();
        let mut body = serde_json::json!({
            "model": self.model,
            "messages": msgs,
            "max_tokens": self.max_tokens,
            "stream": stream,
        });
        if let Some(t) = tools {
            body["tools"] = t.clone();
        }
        body
    }

    /// Send request with retries + exponential backoff + jitter.
    async fn request_with_retry(
        &self,
        body: &Value,
    ) -> Result<(String, reqwest::StatusCode, reqwest::header::HeaderMap)> {
        let mut last_err = None;
        for attempt in 0..=self.max_retries {
            if attempt > 0 {
                let base = Duration::from_secs(1 << attempt.min(6));
                let jitter = Duration::from_millis(rand::random::<u64>() % 1000);
                let delay = base + jitter;
                warn!(
                    attempt,
                    delay_ms = delay.as_millis() as u64,
                    "retrying LLM request"
                );
                tokio::time::sleep(delay).await;
            }

            match self.do_request(body).await {
                Ok((text, status, headers)) => {
                    if status.is_server_error() && attempt < self.max_retries {
                        debug!(attempt, status = status.as_u16(), "server error, retrying");
                        last_err = Some(anyhow::anyhow!(
                            "server error {}: {}",
                            status.as_u16(),
                            &text[..text.len().min(200)]
                        ));
                        continue;
                    }
                    return Ok((text, status, headers));
                }
                Err(e) => {
                    let pe = classify_error(&e, self.timeout.as_secs());
                    if !pe.is_retryable() || attempt == self.max_retries {
                        return Err(e);
                    }
                    debug!(attempt, error = %pe, "retryable error");
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("request failed")))
    }

    async fn stream_request_with_retry(&self, body: &Value) -> Result<reqwest::Response> {
        let mut last_err = None;
        for attempt in 0..=self.max_retries {
            if attempt > 0 {
                let base = Duration::from_secs(1 << attempt.min(6));
                let jitter = Duration::from_millis(rand::random::<u64>() % 1000);
                let delay = base + jitter;
                warn!(
                    attempt,
                    delay_ms = delay.as_millis() as u64,
                    "retrying streaming request"
                );
                tokio::time::sleep(delay).await;
            }

            let resp = match self
                .client
                .post(self.endpoint())
                .bearer_auth(&self.api_key)
                .json(body)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    let err = anyhow::Error::from(e).context("sending streaming request");
                    if attempt == self.max_retries {
                        return Err(err);
                    }
                    last_err = Some(err);
                    continue;
                }
            };

            if resp.status().is_server_error() && attempt < self.max_retries {
                let status = resp.status().as_u16();
                let text = resp.text().await.unwrap_or_default();
                debug!(attempt, status, "stream server error, retrying");
                last_err = Some(anyhow::anyhow!(
                    "server error {}: {}",
                    status,
                    &text[..text.len().min(200)]
                ));
                continue;
            }

            if !resp.status().is_success() {
                let status = resp.status();
                let headers = resp.headers().clone();
                let text = resp.text().await.unwrap_or_default();
                if status.as_u16() == 401 {
                    anyhow::bail!(
                        "API key unauthorized (401). Please check your API key configuration."
                    );
                }
                if is_rate_limited(status.as_u16()) {
                    if attempt < self.max_retries {
                        last_err = Some(anyhow::anyhow!(self.rate_limit_error(&headers, &text)));
                        continue;
                    }
                    anyhow::bail!(self.rate_limit_error(&headers, &text));
                }
                if let Some(err) = self.detect_minimax_error(&text) {
                    anyhow::bail!(err);
                }
                let pe = ProviderError::from_status(status.as_u16(), &text, None);
                anyhow::bail!(pe);
            }

            return Ok(resp);
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("streaming request failed")))
    }

    async fn do_request(
        &self,
        body: &Value,
    ) -> Result<(String, reqwest::StatusCode, reqwest::header::HeaderMap)> {
        // Non-streaming: apply the full-request timeout at the request
        // level (the client no longer sets a total-request timeout because
        // that would kill legitimate long streams — see `Self::new`).
        let resp = self
            .client
            .post(self.endpoint())
            .timeout(self.timeout)
            .bearer_auth(&self.api_key)
            .json(body)
            .send()
            .await
            .context("sending request to LLM")?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let text = resp.text().await.context("reading LLM response")?;
        Ok((text, status, headers))
    }

    fn parse_choice(choice: &Value) -> Message {
        let msg = &choice["message"];
        let content = msg["content"]
            .as_str()
            .map(|s| Content::Text(s.to_string()));
        let reasoning = msg
            .get("reasoning_content")
            .or_else(|| msg.get("reasoning"))
            .and_then(|v| v.as_str())
            .map(String::from);
        let tool_calls = Self::parse_tool_calls(msg.get("tool_calls"));

        Message {
            role: Role::Assistant,
            content,
            tool_calls,
            tool_call_id: None,
            name: None,
            reasoning,
        }
    }

    fn parse_tool_calls(val: Option<&Value>) -> Option<Vec<ToolCall>> {
        val.and_then(|tc| tc.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| {
                        Some(ToolCall {
                            id: t["id"].as_str()?.to_string(),
                            name: t["function"]["name"].as_str()?.to_string(),
                            arguments: t["function"]["arguments"].as_str()?.to_string(),
                        })
                    })
                    .collect()
            })
            .filter(|v: &Vec<ToolCall>| !v.is_empty())
    }

    fn parse_usage(data: &Value) -> Option<Usage> {
        let u = data.get("usage")?;
        Some(Usage {
            input_tokens: u["prompt_tokens"].as_u64().unwrap_or(0) as u32,
            output_tokens: u["completion_tokens"].as_u64().unwrap_or(0) as u32,
            cache_read_tokens: u
                .get("prompt_tokens_details")
                .and_then(|d| d["cached_tokens"].as_u64())
                .unwrap_or(0) as u32,
            cache_write_tokens: 0,
            reasoning_tokens: u
                .get("completion_tokens_details")
                .and_then(|d| d["reasoning_tokens"].as_u64())
                .unwrap_or(0) as u32,
        })
    }
}

fn classify_error(e: &anyhow::Error, timeout_secs: u64) -> ProviderError {
    let msg = e.to_string();
    if msg.contains("timed out") || msg.contains("timeout") {
        ProviderError::Timeout(timeout_secs)
    } else {
        ProviderError::Network(msg)
    }
}

/// Map a classified MiniMax `base_resp` outcome to a [`ProviderError`].
///
/// Shared by the non-streaming (`chat`) and streaming (`chat_stream`) paths.
/// `Ok` maps to `None` (proceed normally). For usage-window exhaustion the wait
/// until the next fixed 5-hour window is computed from `tz_offset_secs`.
fn minimax_outcome_to_error(
    outcome: crate::minimax::MiniMaxOutcome,
    tz_offset_secs: i64,
) -> Option<ProviderError> {
    use crate::minimax::{wait_until_next_window, MiniMaxOutcome};
    match outcome {
        MiniMaxOutcome::Ok => None,
        MiniMaxOutcome::UsageWindowExhausted => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let wait = wait_until_next_window(now, tz_offset_secs, Duration::from_secs(30));
            Some(ProviderError::QuotaExhausted {
                retry_after_secs: Some(wait.as_secs()),
            })
        }
        MiniMaxOutcome::TransientRateLimit { .. } => Some(ProviderError::RateLimit {
            retry_after_secs: None,
        }),
        MiniMaxOutcome::InsufficientBalance => Some(ProviderError::InsufficientBalance),
        MiniMaxOutcome::TransientServer { code } => Some(ProviderError::Server {
            status: 200,
            body: format!("minimax base_resp status_code {code}"),
        }),
        MiniMaxOutcome::Other { code, msg } => Some(ProviderError::Server {
            status: 200,
            body: format!("minimax base_resp status_code {code}: {msg}"),
        }),
    }
}

/// Convert our Message to OpenAI JSON value, handling Content enum.
fn msg_to_openai_value(m: &Message) -> Value {
    // Tool-result messages must use the OpenAI-compatible shape:
    //   {"role":"tool","tool_call_id":"<id>","content":"<string>"}
    // Internally the id lives inside a `ToolResult` content block and the
    // message-level `tool_call_id` is None, so translate here. Without this,
    // OpenAI-compatible providers (e.g. MiniMax) can't pair the result with the
    // assistant's tool_call and return an empty next turn.
    if m.role == Role::Tool {
        let mut v = serde_json::json!({
            "role": "tool",
            "content": m.text(),
        });
        if let Some(id) = m.tool_result_id() {
            v["tool_call_id"] = Value::String(id);
        }
        return v;
    }

    let mut v = serde_json::json!({ "role": m.role });
    match &m.content {
        Some(Content::Text(s)) => {
            v["content"] = Value::String(s.clone());
        }
        Some(Content::Blocks(blocks)) => {
            v["content"] = serde_json::to_value(blocks).unwrap_or(Value::Null);
        }
        None => {}
    }
    if let Some(tc) = &m.tool_calls {
        v["tool_calls"] = serde_json::json!(tc
            .iter()
            .map(|t| serde_json::json!({
                "id": t.id, "type": "function",
                "function": { "name": t.name, "arguments": t.arguments }
            }))
            .collect::<Vec<_>>());
    }
    if let Some(id) = &m.tool_call_id {
        v["tool_call_id"] = Value::String(id.clone());
    }
    if let Some(n) = &m.name {
        v["name"] = Value::String(n.clone());
    }
    v
}

#[async_trait]
impl Provider for OpenAiProvider {
    fn name(&self) -> &str {
        "openai"
    }

    async fn chat(&self, messages: &[Message], tools: Option<&Value>) -> Result<LlmResponse> {
        let body = self.build_body(messages, tools, false);
        let (text, status, headers) = self.request_with_retry(&body).await?;

        if !status.is_success() {
            if status.as_u16() == 401 {
                anyhow::bail!(
                    "API key unauthorized (401). Please check your API key configuration."
                );
            }
            if is_rate_limited(status.as_u16()) {
                anyhow::bail!(self.rate_limit_error(&headers, &text));
            }
            // MiniMax may return a non-200 with the real status in `base_resp`.
            if let Some(err) = self.detect_minimax_error(&text) {
                anyhow::bail!(err);
            }
            let pe = ProviderError::from_status(status.as_u16(), &text, None);
            anyhow::bail!(pe);
        }

        // MiniMax (OpenAI-compatible) can return HTTP 200 with the real status
        // in `base_resp.status_code`; detect quota/rate-limit/hard errors before
        // parsing choices. No-op for standard OpenAI responses.
        if let Some(err) = self.detect_minimax_error(&text) {
            anyhow::bail!(err);
        }

        let data: Value = serde_json::from_str(&text).context("parsing LLM JSON")?;
        let message = Self::parse_choice(&data["choices"][0]);
        let finish_reason = data["choices"][0]["finish_reason"]
            .as_str()
            .map(String::from);
        let usage = Self::parse_usage(&data);
        Ok(LlmResponse {
            message,
            finish_reason,
            usage,
        })
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: Option<&Value>,
    ) -> Result<StreamResult> {
        let body = self.build_body(messages, tools, true);

        let resp = self.stream_request_with_retry(&body).await?;

        let (tx, rx) = tokio::sync::mpsc::channel::<Result<StreamEvent>>(64);

        let quota_tz_offset_secs = self.quota_tz_offset_secs;
        tokio::spawn(async move {
            use futures::StreamExt;
            let mut byte_stream = resp.bytes_stream();
            // Buffer for incomplete SSE lines across chunks
            let mut buf = String::new();
            let mut content = String::new();
            let mut reasoning = String::new();
            // Inline <think> tag state machine: models like MiniMax-M3 embed
            // reasoning inside the content field wrapped in <think>...</think>.
            // We detect this and route those chunks as Reasoning events.
            let mut in_think = false;
            let mut tag_buf = String::new();
            let mut tc_parts: std::collections::BTreeMap<u32, (String, String, String)> =
                Default::default();
            let mut finish_reason: Option<String> = None;
            let mut usage: Option<Usage> = None;

            let stall_timeout = std::time::Duration::from_secs(30);
            loop {
                let next = tokio::time::timeout(stall_timeout, byte_stream.next()).await;
                let chunk = match next {
                    Err(_) => {
                        let _ = tx
                            .send(Err(anyhow::anyhow!("stream stall: no data for 30s")))
                            .await;
                        return;
                    }
                    Ok(None) => break,
                    Ok(Some(c)) => c,
                };
                let bytes = match chunk {
                    Ok(b) => b,
                    Err(e) => {
                        let _ = tx.send(Err(anyhow::anyhow!("{e}"))).await;
                        return;
                    }
                };
                buf.push_str(&String::from_utf8_lossy(&bytes));

                // Process complete lines; keep incomplete trailing data in buf
                while let Some(pos) = buf.find('\n') {
                    let line = buf[..pos].trim_end_matches('\r').to_string();
                    buf = buf[pos + 1..].to_string();

                    let data = match line.strip_prefix("data: ") {
                        Some(d) if d.trim() == "[DONE]" => {
                            // Flush any remaining tag_buf content
                            if !tag_buf.is_empty() {
                                if in_think {
                                    reasoning.push_str(&tag_buf);
                                    let _ = tx
                                        .send(Ok(StreamEvent::Reasoning(std::mem::take(
                                            &mut tag_buf,
                                        ))))
                                        .await;
                                } else {
                                    content.push_str(&tag_buf);
                                    let _ = tx
                                        .send(Ok(StreamEvent::Delta(std::mem::take(&mut tag_buf))))
                                        .await;
                                }
                            }
                            // Emit Done and exit
                            let tool_calls = if tc_parts.is_empty() {
                                None
                            } else {
                                Some(
                                    tc_parts
                                        .into_values()
                                        .map(|(id, name, args)| ToolCall {
                                            id,
                                            name,
                                            arguments: args,
                                        })
                                        .collect(),
                                )
                            };
                            let _ = tx
                                .send(Ok(StreamEvent::Done {
                                    response: LlmResponse {
                                        message: Message {
                                            role: Role::Assistant,
                                            content: if content.is_empty() {
                                                None
                                            } else {
                                                Some(Content::Text(content))
                                            },
                                            tool_calls,
                                            tool_call_id: None,
                                            name: None,
                                            reasoning: if reasoning.is_empty() {
                                                None
                                            } else {
                                                Some(reasoning)
                                            },
                                        },
                                        finish_reason,
                                        usage,
                                    },
                                }))
                                .await;
                            return;
                        }
                        Some(d) => d,
                        _ => continue,
                    };

                    let json: Value = match serde_json::from_str(data) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };

                    // MiniMax streams the real status in `base_resp`; a non-zero
                    // code (quota/rate-limit/hard error) can appear in a data
                    // chunk. Surface it as a stream error instead of silently
                    // ending with an empty message. No-op for standard OpenAI.
                    if let Some(outcome) = crate::minimax::classify_value(&json) {
                        if let Some(err) = minimax_outcome_to_error(outcome, quota_tz_offset_secs) {
                            let _ = tx.send(Err(anyhow::Error::new(err))).await;
                            return;
                        }
                    }

                    let delta = &json["choices"][0]["delta"];

                    // Content — with inline <think> tag detection.
                    // Think blocks are routed to reasoning; the rest to content.
                    if let Some(c) = delta["content"].as_str() {
                        if !c.is_empty() {
                            tag_buf.push_str(c);
                            while !tag_buf.is_empty() {
                                if in_think {
                                    if let Some(end) = tag_buf.find("</think>") {
                                        let thinking = &tag_buf[..end];
                                        if !thinking.is_empty() {
                                            reasoning.push_str(thinking);
                                            let _ = tx
                                                .send(Ok(StreamEvent::Reasoning(
                                                    thinking.to_string(),
                                                )))
                                                .await;
                                        }
                                        tag_buf = tag_buf[end + "</think>".len()..].to_string();
                                        in_think = false;
                                    } else if tag_buf.ends_with('<')
                                        || tag_buf.ends_with("</")
                                        || tag_buf.ends_with("</t")
                                        || tag_buf.ends_with("</th")
                                        || tag_buf.ends_with("</thi")
                                        || tag_buf.ends_with("</thin")
                                        || tag_buf.ends_with("</think")
                                    {
                                        break;
                                    } else {
                                        reasoning.push_str(&tag_buf);
                                        let _ = tx
                                            .send(Ok(StreamEvent::Reasoning(std::mem::take(
                                                &mut tag_buf,
                                            ))))
                                            .await;
                                    }
                                } else {
                                    if let Some(start) = tag_buf.find("<think>") {
                                        let before = &tag_buf[..start];
                                        if !before.is_empty() {
                                            content.push_str(before);
                                            let _ = tx
                                                .send(Ok(StreamEvent::Delta(before.to_string())))
                                                .await;
                                        }
                                        tag_buf = tag_buf[start + "<think>".len()..].to_string();
                                        in_think = true;
                                    } else if tag_buf.ends_with('<')
                                        || tag_buf.ends_with("<t")
                                        || tag_buf.ends_with("<th")
                                        || tag_buf.ends_with("<thi")
                                        || tag_buf.ends_with("<thin")
                                        || tag_buf.ends_with("<think")
                                    {
                                        break;
                                    } else {
                                        content.push_str(&tag_buf);
                                        let _ = tx
                                            .send(Ok(StreamEvent::Delta(std::mem::take(
                                                &mut tag_buf,
                                            ))))
                                            .await;
                                    }
                                }
                            }
                        }
                    }

                    // Reasoning
                    let r = delta
                        .get("reasoning_content")
                        .or_else(|| delta.get("reasoning"))
                        .and_then(|v| v.as_str());
                    if let Some(r) = r {
                        if !r.is_empty() {
                            reasoning.push_str(r);
                            let _ = tx.send(Ok(StreamEvent::Reasoning(r.to_string()))).await;
                        }
                    }

                    // Tool calls
                    if let Some(tcs) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                        for tc in tcs {
                            let idx = tc["index"].as_u64().unwrap_or(0) as u32;
                            let entry = tc_parts.entry(idx).or_default();
                            if let Some(id) = tc["id"].as_str() {
                                entry.0 = id.to_string();
                            }
                            if let Some(name) = tc["function"]["name"].as_str() {
                                entry.1 = name.to_string();
                                let _ = tx
                                    .send(Ok(StreamEvent::ToolGenStarted(name.to_string())))
                                    .await;
                            }
                            if let Some(args) = tc["function"]["arguments"].as_str() {
                                entry.2.push_str(args);
                            }
                        }
                    }

                    // Finish reason
                    if let Some(fr) = json["choices"][0]["finish_reason"].as_str() {
                        finish_reason = Some(fr.to_string());
                    }

                    // Usage (some providers send it in the last chunk)
                    if let Some(u) = json.get("usage").filter(|u| u.is_object()) {
                        usage = Some(Usage {
                            input_tokens: u["prompt_tokens"].as_u64().unwrap_or(0) as u32,
                            output_tokens: u["completion_tokens"].as_u64().unwrap_or(0) as u32,
                            ..Default::default()
                        });
                    }
                }
            }

            // Flush any remaining tag_buf content
            if !tag_buf.is_empty() {
                if in_think {
                    reasoning.push_str(&tag_buf);
                    let _ = tx
                        .send(Ok(StreamEvent::Reasoning(std::mem::take(&mut tag_buf))))
                        .await;
                } else {
                    content.push_str(&tag_buf);
                    let _ = tx
                        .send(Ok(StreamEvent::Delta(std::mem::take(&mut tag_buf))))
                        .await;
                }
            }

            // Stream ended without [DONE] — emit what we have
            let tool_calls = if tc_parts.is_empty() {
                None
            } else {
                Some(
                    tc_parts
                        .into_values()
                        .map(|(id, name, args)| ToolCall {
                            id,
                            name,
                            arguments: args,
                        })
                        .collect(),
                )
            };
            let _ = tx
                .send(Ok(StreamEvent::Done {
                    response: LlmResponse {
                        message: Message {
                            role: Role::Assistant,
                            content: if content.is_empty() {
                                None
                            } else {
                                Some(Content::Text(content))
                            },
                            tool_calls,
                            tool_call_id: None,
                            name: None,
                            reasoning: if reasoning.is_empty() {
                                None
                            } else {
                                Some(reasoning)
                            },
                        },
                        finish_reason,
                        usage,
                    },
                }))
                .await;
        });

        Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

    fn provider(base_url: &str) -> OpenAiProvider {
        OpenAiProvider::new(
            "test-key".to_string(),
            base_url.to_string(),
            "MiniMax-M3".to_string(),
            1024,
            30,
            3,
        )
    }

    fn headers_with(key: &str, val: &str) -> HeaderMap {
        let mut m = HeaderMap::new();
        m.insert(
            HeaderName::from_bytes(key.as_bytes()).unwrap(),
            HeaderValue::from_str(val).unwrap(),
        );
        m
    }

    // Confirmed: MiniMax's OpenAI-compatible endpoint returns HTTP 429 on quota
    // exhaustion. These tests pin the `rate_limit_error` behavior for that path.

    #[test]
    fn minimax_429_without_retry_after_computes_fixed_window() {
        let p = provider("https://api.minimax.io/v1");
        let err = p.rate_limit_error(&HeaderMap::new(), "");
        match err {
            ProviderError::QuotaExhausted {
                retry_after_secs: Some(s),
            } => {
                assert!(s > 0, "computed window wait must be positive");
                // Boundaries are at most 5h apart; allow the 30s safety margin.
                assert!(
                    s <= 5 * 3600 + 60,
                    "wait must be within one 5h window + margin"
                );
            }
            other => panic!("expected QuotaExhausted, got {other:?}"),
        }
    }

    #[test]
    fn any_429_with_retry_after_header_uses_it_verbatim() {
        // An explicit Retry-After always wins over the computed window.
        let p = provider("https://api.minimax.io/v1");
        let err = p.rate_limit_error(&headers_with("retry-after", "120"), "");
        match err {
            ProviderError::RateLimit {
                retry_after_secs: Some(s),
            } => assert_eq!(s, 120),
            other => panic!("expected RateLimit(120), got {other:?}"),
        }
    }

    #[test]
    fn minimax_429_carrying_base_resp_2056_is_quota_exhausted() {
        let p = provider("https://api.minimax.io/v1");
        let body = r#"{"base_resp":{"status_code":2056,"status_msg":"usage limit exceeded"}}"#;
        let err = p.rate_limit_error(&HeaderMap::new(), body);
        assert!(matches!(
            err,
            ProviderError::QuotaExhausted {
                retry_after_secs: Some(_)
            }
        ));
    }

    #[test]
    fn minimax_429_carrying_base_resp_1008_is_insufficient_balance() {
        let p = provider("https://api.minimax.io/v1");
        let body = r#"{"base_resp":{"status_code":1008,"status_msg":"insufficient balance"}}"#;
        let err = p.rate_limit_error(&HeaderMap::new(), body);
        assert!(matches!(err, ProviderError::InsufficientBalance));
        assert!(
            !err.is_retryable(),
            "insufficient balance must not be retryable"
        );
    }

    #[test]
    fn non_minimax_429_without_hint_is_plain_rate_limit() {
        // A non-MiniMax endpoint with no reset hint must not compute a window.
        let p = provider("https://api.openai.com/v1");
        let err = p.rate_limit_error(&HeaderMap::new(), "");
        assert!(matches!(
            err,
            ProviderError::RateLimit {
                retry_after_secs: None
            }
        ));
    }

    #[test]
    fn retryability_of_quota_variants() {
        assert!(ProviderError::QuotaExhausted {
            retry_after_secs: Some(100)
        }
        .is_retryable());
        assert!(!ProviderError::InsufficientBalance.is_retryable());
    }

    #[test]
    fn with_quota_window_tz_overrides_default() {
        let p = provider("https://api.minimax.io/v1").with_quota_window_tz(0);
        assert_eq!(p.quota_tz_offset_secs, 0);
    }
}
