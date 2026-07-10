use crate::rate_limit::{is_rate_limited, parse_retry_after};
use crate::traits::{Provider, StreamEvent, StreamResult};
use aegis_types::message::{Content, LlmResponse, Message, Role, ToolCall, Usage};
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::Value;
use std::time::Duration;

pub struct AnthropicProvider {
    client: reqwest::Client,
    api_key: String,
    model: String,
    max_tokens: u32,
    base_url: String,
    /// Total-request timeout for the non-streaming path. Applied via
    /// `RequestBuilder::timeout` per-request because the client itself no
    /// longer sets a total timeout (would prematurely kill long streaming
    /// responses; see `Self::new_with_base_url`).
    request_timeout: Duration,
}

impl AnthropicProvider {
    /// Creates a new `instance`.
    pub fn new(api_key: String, model: String, max_tokens: u32, timeout_secs: u64) -> Self {
        Self::new_with_base_url(api_key, model, max_tokens, timeout_secs, "https://api.anthropic.com".to_string())
    }

    /// New with base url.
    pub fn new_with_base_url(api_key: String, model: String, max_tokens: u32, timeout_secs: u64, base_url: String) -> Self {
        // See `openai::OpenAiProvider::new` for the rationale: `.timeout()`
        // at the client level would kill legitimate long streaming
        // generations mid-body with `error decoding response body`. Use
        // per-read + connect timeouts; enforce the total-request timeout
        // per-request in the non-streaming path only.
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(Duration::from_secs(timeout_secs))
            .build()
            .expect("failed to build HTTP client");
        Self {
            client,
            api_key,
            model,
            max_tokens,
            base_url: base_url.trim_end_matches('/').to_string(),
            request_timeout: Duration::from_secs(timeout_secs),
        }
    }

    fn build_body(
        &self,
        messages: &[Message],
        tools: Option<&Value>,
        stream: bool,
    ) -> (Option<String>, Value) {
        let mut system_text = None;
        let mut msgs = Vec::new();

        for m in messages {
            match m.role {
                Role::System => {
                    system_text = Some(m.text());
                }
                Role::User => {
                    msgs.push(serde_json::json!({ "role": "user", "content": m.text() }));
                }
                Role::Assistant => {
                    if m.has_tool_calls() {
                        let blocks: Vec<Value> = m.tool_calls.as_ref().expect("has_tool_calls was true").iter().map(|tc| {
                            let input: Value = serde_json::from_str(&tc.arguments).unwrap_or(Value::Object(Default::default()));
                            serde_json::json!({ "type": "tool_use", "id": tc.id, "name": tc.name, "input": input })
                        }).collect();
                        let mut content = blocks;
                        if !m.text().is_empty() {
                            content
                                .insert(0, serde_json::json!({ "type": "text", "text": m.text() }));
                        }
                        msgs.push(serde_json::json!({ "role": "assistant", "content": content }));
                    } else {
                        msgs.push(serde_json::json!({ "role": "assistant", "content": m.text() }));
                    }
                }
                Role::Tool => {
                    msgs.push(serde_json::json!({
                        "role": "user",
                        "content": [{
                            "type": "tool_result",
                            "tool_use_id": m.tool_result_id().unwrap_or_default(),
                            "content": m.text(),
                        }]
                    }));
                }
            }
        }

        let mut body = serde_json::json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "messages": msgs,
            "stream": stream,
        });
        if let Some(sys) = &system_text {
            // Prompt caching: mark the system block as an ephemeral cache
            // breakpoint. Anthropic caches the `tools → system` prefix up to
            // here, so repeated calls with the same prefix (e.g. every iteration
            // of a tool loop) read from cache at ~0.1x input price. Below the
            // minimum cacheable length Anthropic just won't cache (no error).
            body["system"] = serde_json::json!([{
                "type": "text",
                "text": sys,
                "cache_control": { "type": "ephemeral" }
            }]);
        }
        if let Some(t) = tools {
            // Convert OpenAI tools format to Anthropic
            if let Some(arr) = t.as_array() {
                let anthropic_tools: Vec<Value> = arr
                    .iter()
                    .filter_map(|tool| {
                        let f = tool.get("function")?;
                        Some(serde_json::json!({
                            "name": f["name"],
                            "description": f["description"],
                            "input_schema": f["parameters"],
                        }))
                    })
                    .collect();
                body["tools"] = Value::Array(anthropic_tools);
            }
        }
        (system_text, body)
    }

    fn parse_response(data: &Value) -> LlmResponse {
        let mut text = String::new();
        let mut tool_calls = Vec::new();
        let mut reasoning = None;

        if let Some(content) = data["content"].as_array() {
            for block in content {
                match block["type"].as_str() {
                    Some("text") => {
                        if let Some(t) = block["text"].as_str() {
                            text.push_str(t);
                        }
                    }
                    Some("tool_use") => {
                        tool_calls.push(ToolCall {
                            id: block["id"].as_str().unwrap_or("").to_string(),
                            name: block["name"].as_str().unwrap_or("").to_string(),
                            arguments: block["input"].to_string(),
                        });
                    }
                    Some("thinking") => {
                        if let Some(t) = block["thinking"].as_str() {
                            reasoning = Some(t.to_string());
                        }
                    }
                    _ => {}
                }
            }
        }

        let usage = data.get("usage").map(|u| Usage {
            input_tokens: u["input_tokens"].as_u64().unwrap_or(0) as u32,
            output_tokens: u["output_tokens"].as_u64().unwrap_or(0) as u32,
            cache_read_tokens: u
                .get("cache_read_input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32,
            cache_write_tokens: u
                .get("cache_creation_input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32,
            reasoning_tokens: 0,
        });

        LlmResponse {
            message: Message {
                role: Role::Assistant,
                content: if text.is_empty() {
                    None
                } else {
                    Some(Content::Text(text))
                },
                tool_calls: if tool_calls.is_empty() {
                    None
                } else {
                    Some(tool_calls)
                },
                tool_call_id: None,
                name: None,
                reasoning,
            },
            finish_reason: data["stop_reason"].as_str().map(String::from),
            usage,
        }
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    async fn chat(&self, messages: &[Message], tools: Option<&Value>) -> Result<LlmResponse> {
        let (_, body) = self.build_body(messages, tools, false);
        let resp = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .timeout(self.request_timeout)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .context("sending Anthropic request")?;

        let status = resp.status();
        let headers = resp.headers().clone();
        let text = resp.text().await.context("reading Anthropic response")?;
        if !status.is_success() {
            if status.as_u16() == 401 {
                anyhow::bail!("API key unauthorized (401). Please check your API key configuration.");
            }
            if is_rate_limited(status.as_u16()) {
                let duration = parse_retry_after(&headers);
                anyhow::bail!("rate limited, retry after {:?}", duration);
            }
            anyhow::bail!("Anthropic API error {status}: {text}");
        }

        let data: Value = serde_json::from_str(&text).context("parsing Anthropic JSON")?;
        Ok(Self::parse_response(&data))
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: Option<&Value>,
    ) -> Result<StreamResult> {
        let (_, body) = self.build_body(messages, tools, true);
        let resp = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .context("sending Anthropic stream")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let headers = resp.headers().clone();
            let text = resp.text().await.unwrap_or_default();
            if status.as_u16() == 401 {
                anyhow::bail!("API key unauthorized (401). Please check your API key configuration.");
            }
            if is_rate_limited(status.as_u16()) {
                let duration = parse_retry_after(&headers);
                anyhow::bail!("rate limited, retry after {:?}", duration);
            }
            anyhow::bail!("Anthropic API error {status}: {text}");
        }

        let (tx, rx) = tokio::sync::mpsc::channel::<Result<StreamEvent>>(64);

        tokio::spawn(async move {
            use futures::StreamExt;
            let mut byte_stream = resp.bytes_stream();
            let mut buf = String::new();
            let mut content = String::new();
            let mut reasoning = String::new();
            let mut tool_calls: Vec<ToolCall> = Vec::new();
            let mut current_tool: Option<(String, String, String)> = None; // (id, name, json_buf)
            let mut usage: Option<Usage> = None;
            let mut stop_reason: Option<String> = None;

            while let Some(chunk) = byte_stream.next().await {
                let bytes = match chunk {
                    Ok(b) => b,
                    Err(e) => {
                        let _ = tx.send(Err(anyhow::anyhow!("{e}"))).await;
                        return;
                    }
                };
                buf.push_str(&String::from_utf8_lossy(&bytes));

                while let Some(pos) = buf.find('\n') {
                    let line = buf[..pos].trim_end_matches('\r').to_string();
                    buf = buf[pos + 1..].to_string();

                    let data = match line.strip_prefix("data: ") {
                        Some(d) => d,
                        _ => continue,
                    };
                    let json: Value = match serde_json::from_str(data) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };

                    match json["type"].as_str() {
                        Some("content_block_start") => {
                            let cb = &json["content_block"];
                            if cb["type"].as_str() == Some("tool_use") {
                                let id = cb["id"].as_str().unwrap_or("").to_string();
                                let name = cb["name"].as_str().unwrap_or("").to_string();
                                let _ =
                                    tx.send(Ok(StreamEvent::ToolGenStarted(name.clone()))).await;
                                current_tool = Some((id, name, String::new()));
                            }
                        }
                        Some("content_block_delta") => {
                            let delta = &json["delta"];
                            match delta["type"].as_str() {
                                Some("text_delta") => {
                                    if let Some(t) = delta["text"].as_str() {
                                        content.push_str(t);
                                        let _ =
                                            tx.send(Ok(StreamEvent::Delta(t.to_string()))).await;
                                    }
                                }
                                Some("thinking_delta") => {
                                    if let Some(t) = delta["thinking"].as_str() {
                                        reasoning.push_str(t);
                                        let _ = tx
                                            .send(Ok(StreamEvent::Reasoning(t.to_string())))
                                            .await;
                                    }
                                }
                                Some("input_json_delta") => {
                                    if let Some(ref mut tool) = current_tool {
                                        if let Some(j) = delta["partial_json"].as_str() {
                                            tool.2.push_str(j);
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                        Some("content_block_stop") => {
                            if let Some((id, name, json_buf)) = current_tool.take() {
                                tool_calls.push(ToolCall {
                                    id,
                                    name,
                                    arguments: json_buf,
                                });
                            }
                        }
                        Some("message_delta") => {
                            if let Some(sr) = json["delta"]["stop_reason"].as_str() {
                                stop_reason = Some(sr.to_string());
                            }
                            if let Some(u) = json.get("usage") {
                                let out = u["output_tokens"].as_u64().unwrap_or(0) as u32;
                                // Merge: keep input/cache tokens captured at
                                // message_start instead of overwriting them.
                                if let Some(ref mut us) = usage {
                                    us.output_tokens = out;
                                } else {
                                    usage = Some(Usage {
                                        output_tokens: out,
                                        ..Default::default()
                                    });
                                }
                            }
                        }
                        Some("message_start") => {
                            if let Some(u) = json["message"].get("usage") {
                                usage = Some(Usage {
                                    input_tokens: u["input_tokens"].as_u64().unwrap_or(0) as u32,
                                    cache_read_tokens: u["cache_read_input_tokens"].as_u64().unwrap_or(0) as u32,
                                    cache_write_tokens: u["cache_creation_input_tokens"].as_u64().unwrap_or(0) as u32,
                                    ..Default::default()
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }

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
                            tool_calls: if tool_calls.is_empty() {
                                None
                            } else {
                                Some(tool_calls)
                            },
                            tool_call_id: None,
                            name: None,
                            reasoning: if reasoning.is_empty() {
                                None
                            } else {
                                Some(reasoning)
                            },
                        },
                        finish_reason: stop_reason,
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
    use aegis_types::message::Message;

    #[test]
    fn test_system_block_has_cache_control() {
        let p = AnthropicProvider::new("k".into(), "claude-test".into(), 100, 5);
        let msgs = vec![Message::system("system text"), Message::user("hi")];
        let (_, body) = p.build_body(&msgs, None, false);
        let sys = &body["system"];
        assert!(sys.is_array(), "system must be a content-block array for caching");
        assert_eq!(sys[0]["type"], "text");
        assert_eq!(sys[0]["text"], "system text");
        assert_eq!(sys[0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn test_no_system_no_field() {
        let p = AnthropicProvider::new("k".into(), "claude-test".into(), 100, 5);
        let msgs = vec![Message::user("hi")];
        let (_, body) = p.build_body(&msgs, None, false);
        assert!(body.get("system").is_none());
    }
}
